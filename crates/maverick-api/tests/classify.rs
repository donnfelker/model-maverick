//! The prompt-path classifier and resolver (bead 5.6; reference Part 4 §3 and §9):
//! the classifier against a scripted provider, `ClientFor`, the classification cache, and
//! the resolver on real Postgres, with D2 (zero retention), D3 (total deadline), D4
//! (budget), and D5 (projection version in the cache key).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Duration;

use chrono::{DateTime, Utc};
use maverick_api::classify::{
    Classifier, Method, ResolveInput, Resolver, ResolverSettings, ZeroRetentionRefused, client_for,
};
use maverick_domain::capture::fingerprint;
use maverick_domain::classify::{self, Classification};
use maverick_providers::{
    CompletionRequest, CompletionResponse, ProviderClient, ProviderError, ProviderErrorKind,
    ProviderFuture,
};
use maverick_store::catalog::{self, ModelColumn, NewModelColumn};
use maverick_store::{JsonRawValue, MIGRATOR, decisions, tenancy};
use maverick_testdb::{Schema, TestDatabase};
use serde_json::Value;
use sqlx::PgPool;
use tokio::time::Instant;

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

const TENANT: &str = "ten_classify";
const OTHER_TENANT: &str = "ten_other";
const KEY: [u8; 32] = [0; 32];
const COLUMN: &str = "claude-haiku-4-5/direct";
const ZR_COLUMN: &str = "claude-haiku-4-5/vertex";
const VALID: &str = r#"{"capability":"structured_extraction","difficulty":3,"domain_tags":["legal"],"confidence":0.91}"#;

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---------------------------------------------------------------------------------------
// A scripted provider
// ---------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum Reply {
    Text(String),
    Fail,
    Hang,
}

fn text(reply: &str) -> Reply {
    Reply::Text(reply.to_owned())
}

/// Answers from a queue, then with `then` forever. Records every request.
#[derive(Clone)]
struct Scripted {
    queue: Arc<Mutex<VecDeque<Reply>>>,
    then: Reply,
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
}

impl Scripted {
    fn new(queue: Vec<Reply>, then: Reply) -> Self {
        Self {
            queue: Arc::new(Mutex::new(queue.into())),
            then,
            requests: Arc::default(),
        }
    }

    fn always(reply: Reply) -> Self {
        Self::new(Vec::new(), reply)
    }

    fn calls(&self) -> usize {
        lock(&self.requests).len()
    }

    fn classifier(&self, column: &str) -> Classifier {
        Classifier::new(Arc::new(self.clone()), column)
    }
}

impl ProviderClient for Scripted {
    fn complete<'a>(&'a self, request: CompletionRequest) -> ProviderFuture<'a> {
        lock(&self.requests).push(request);
        let reply = lock(&self.queue)
            .pop_front()
            .unwrap_or_else(|| self.then.clone());
        Box::pin(async move {
            match reply {
                Reply::Text(text) => Ok(CompletionResponse {
                    text,
                    tokens_in: 100,
                    tokens_out: 20,
                }),
                Reply::Fail => Err(ProviderError::new(
                    "anthropic",
                    ProviderErrorKind::ProviderError,
                )
                .with_status(503)),
                Reply::Hang => {
                    std::future::pending::<()>().await;
                    unreachable!("pending never resolves")
                }
            }
        })
    }

    fn name(&self) -> &str {
        "anthropic"
    }
}

fn soon() -> Instant {
    Instant::now() + Duration::from_secs(10)
}

fn legal() -> Classification {
    Classification {
        capability: "structured_extraction".to_owned(),
        difficulty: 3,
        domain_tags: vec!["legal".to_owned()],
        confidence: 0.91,
        truncated: false,
    }
}

// ---------------------------------------------------------------------------------------
// The classifier
// ---------------------------------------------------------------------------------------

// parity: classify.TestClassifyHappyPath
#[tokio::test]
async fn happy_path_sends_the_exact_request() {
    let provider = Scripted::always(text(VALID));
    let attempts = provider.classifier(COLUMN).classify("hi", soon()).await;
    assert_eq!(attempts.outcome, Ok(legal()));
    assert_eq!((attempts.calls, provider.calls()), (1, 1));
    assert_eq!((attempts.tokens_in, attempts.tokens_out), (100, 20));
    let request = lock(&provider.requests)[0].clone();
    assert_eq!(request.model, "claude-haiku-4-5");
    assert_eq!(request.temperature, 0.0);
    assert_eq!(request.max_tokens, 512);
    assert_eq!(request.system, classify::SYSTEM_PROMPT);
    assert_eq!(request.prompt, classify::fence("hi"));
    assert_eq!(
        request.settings.get("response_schema"),
        Some(&Value::Object(classify::response_schema()))
    );
}

// parity: classify.TestClassifyRetriesOnceOnFailure
// parity: classify.TestClassifyRetriesOnceOnInvalidOutput
#[tokio::test]
async fn one_retry_after_a_failure_or_invalid_output() {
    for first in [Reply::Fail, text("not json")] {
        let provider = Scripted::new(vec![first.clone()], text(VALID));
        let attempts = provider.classifier(COLUMN).classify("hi", soon()).await;
        assert_eq!(attempts.outcome, Ok(legal()), "{first:?}");
        assert_eq!(provider.calls(), 2, "{first:?}");
    }
    let provider = Scripted::always(Reply::Fail);
    let attempts = provider.classifier(COLUMN).classify("hi", soon()).await;
    assert!(attempts.outcome.is_err());
    assert_eq!(provider.calls(), 2, "never a third attempt");
}

/// DIVERGENCE: D3. One total deadline covers both attempts: a first attempt that hangs
/// uses all of it, so there is no retry (Go gave each attempt its own 5 s). A fast failure
/// still leaves time for the retry.
// parity: classify.TestClassifyTimeoutExhaustsBothAttempts
#[tokio::test]
async fn one_total_deadline_covers_both_attempts() {
    let d3 = parity_support::divergence!("D3");
    assert_eq!(d3.id(), "D3");
    let provider = Scripted::always(Reply::Hang);
    let started = std::time::Instant::now();
    let attempts = provider
        .classifier(COLUMN)
        .classify("hi", Instant::now() + Duration::from_millis(50))
        .await;
    let error = attempts.outcome.expect_err("times out");
    assert_eq!(
        error.to_string(),
        "classify: column claude-haiku-4-5/direct: anthropic: timeout: context deadline exceeded"
    );
    assert_eq!(provider.calls(), 1);
    assert!(started.elapsed() < Duration::from_secs(2));

    let provider = Scripted::new(vec![Reply::Fail], Reply::Hang);
    let attempts = provider
        .classifier(COLUMN)
        .classify("hi", Instant::now() + Duration::from_millis(50))
        .await;
    assert!(attempts.outcome.is_err());
    assert_eq!(
        provider.calls(),
        2,
        "the fast failure is retried inside the deadline"
    );
}

// parity: classify.TestClassifyInvalidOutputFails
// parity: classify.TestClassifyBoundaryOutputs
#[tokio::test]
async fn parse_rows_through_the_classifier() {
    let doc = parity_support::fixture("decision", "classify_parse.json").expect("fixture");
    for row in doc["cases"].as_array().expect("cases") {
        let id = row["id"].as_str().expect("id");
        let provider = Scripted::always(text(row["input"].as_str().expect("input")));
        let attempts = provider.classifier(COLUMN).classify("hi", soon()).await;
        if row["expected"]["ok"] == true {
            assert!(attempts.outcome.is_ok(), "{id}");
            assert_eq!(provider.calls(), 1, "{id}");
        } else {
            let error = attempts.outcome.expect_err(id);
            assert_eq!(provider.calls(), 2, "{id}: invalid output is retried once");
            assert!(
                error
                    .to_string()
                    .starts_with("classify: column claude-haiku-4-5/direct: ")
            );
        }
    }
}

/// The request carries the exact truncated fence, and a truncated classification caps
/// its confidence at 0.8 and says so.
// parity: classify.TestClassifyTruncation
#[tokio::test]
async fn truncation_reaches_the_request_and_caps_confidence() {
    let doc = parity_support::fixture("decision", "classify_truncate.json").expect("fixture");
    for row in doc["cases"].as_array().expect("cases") {
        let Some(fenced) = row["expected"]["fenced"].as_str() else {
            continue;
        };
        let max = usize::try_from(row["input"]["max_chars"].as_u64().expect("max")).unwrap();
        let prompt = row["input"]["prompt"].as_str().expect("prompt");
        let classifier = Scripted::always(text(VALID))
            .classifier(COLUMN)
            .with_max_input_chars(max);
        let (request, truncated) = classifier.request(prompt);
        assert_eq!(request.prompt, fenced, "{}", row["id"]);
        assert_eq!(
            truncated,
            row["expected"]["truncated"] == true,
            "{}",
            row["id"]
        );
    }
    let confident =
        r#"{"capability":"factual_qa","difficulty":1,"domain_tags":[],"confidence":0.95}"#;
    let long = "a".repeat(150);
    let capped = Scripted::always(text(confident))
        .classifier(COLUMN)
        .with_max_input_chars(100)
        .classify(&long, soon())
        .await
        .outcome
        .expect("classifies");
    assert_eq!((capped.confidence, capped.truncated), (0.8, true));
    let within = Scripted::always(text(VALID))
        .classifier(COLUMN)
        .with_max_input_chars(100)
        .classify(&"é".repeat(100), soon())
        .await
        .outcome
        .expect("classifies");
    assert_eq!((within.confidence, within.truncated), (0.91, false));
}

/// A deadline that has already passed makes no call (Go: a canceled context).
// parity: classify.TestClassifyStopsRetryingOnCanceledContext
#[tokio::test]
async fn a_passed_deadline_makes_no_call() {
    let provider = Scripted::always(text(VALID));
    let attempts = provider
        .classifier(COLUMN)
        .classify("hi", Instant::now())
        .await;
    assert_eq!(
        attempts.outcome.expect_err("no time").to_string(),
        "classify: column claude-haiku-4-5/direct: context deadline exceeded"
    );
    assert_eq!(provider.calls(), 0);
}

// parity: classify.TestClassifyErrorNamesColumn
#[tokio::test]
async fn the_error_names_the_column_and_wraps_the_provider_error() {
    let provider = Scripted::always(Reply::Fail);
    let error = provider
        .classifier(COLUMN)
        .classify("hi", soon())
        .await
        .outcome
        .expect_err("fails");
    let provider_error =
        ProviderError::new("anthropic", ProviderErrorKind::ProviderError).with_status(503);
    assert_eq!(
        error.to_string(),
        format!("classify: column claude-haiku-4-5/direct: {provider_error}")
    );
    assert_eq!(error.column, COLUMN);
}

fn model_column(provider: &str, model: &str, endpoint: &str) -> ModelColumn {
    let empty = || JsonRawValue::from_string("{}".to_owned()).unwrap();
    ModelColumn {
        id: format!("col_{model}_{endpoint}"),
        provider: provider.to_owned(),
        model_id: model.to_owned(),
        endpoint: endpoint.to_owned(),
        settings: empty(),
        status: "active".to_owned(),
        pricing: empty(),
        quality_source_column_id: None,
        cold_start_until: None,
        deprecation_date: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        execution_aliases: empty(),
        retention_mode: "unknown".to_owned(),
        retention_verified_at: None,
        retention_valid_until: None,
        retention_source: None,
        execution_alias_retention: empty(),
        param_constraints: empty(),
    }
}

// parity: classify.TestClientFor
#[test]
fn client_for_finds_the_catalog_column_and_its_provider() {
    let columns = [
        model_column("google", "gemini-3.1-flash", "vertex"),
        model_column("anthropic", "claude-haiku-4-5", "direct"),
    ];
    let anthropic: Arc<dyn ProviderClient> = Arc::new(Scripted::always(text(VALID)));
    let clients = HashMap::from([("anthropic".to_owned(), Arc::clone(&anthropic))]);
    let found = client_for(&columns, &clients, "claude-haiku-4-5/direct").expect("resolves");
    assert!(Arc::ptr_eq(&found, &anthropic));
    assert_eq!(
        client_for(&columns, &clients, "claude-haiku-4-5/vertex").err(),
        Some(
            "classify: no model column matches classifier column \"claude-haiku-4-5/vertex\""
                .to_owned()
        )
    );
    assert_eq!(
        client_for(&columns, &clients, "gemini-3.1-flash/vertex").err(),
        Some(
            "classify: no provider client for \"google\" (classifier column \
             \"gemini-3.1-flash/vertex\")"
                .to_owned()
        )
    );
    let no_slash = client_for(&columns, &clients, "no-slash")
        .err()
        .expect("error");
    assert_eq!(
        Some(no_slash),
        classify::parse_column_ref("no-slash").err(),
        "the domain's exact text"
    );
}

// ---------------------------------------------------------------------------------------
// The resolver on real Postgres
// ---------------------------------------------------------------------------------------

// parity: classify.TestMain
async fn database() -> Option<(TestDatabase, PgPool)> {
    let db = maverick_testdb::database(&SCHEMA).await?;
    let pool = db.pool().await;
    for tenant in [TENANT, OTHER_TENANT] {
        tenancy::create_tenant(&pool, tenant, tenant, &KEY)
            .await
            .unwrap();
    }
    Some((db, pool))
}

fn settings() -> ResolverSettings {
    ResolverSettings {
        column: COLUMN.to_owned(),
        zero_retention_column: Some(ZR_COLUMN.to_owned()),
        ..ResolverSettings::default()
    }
}

fn input<'a>(prompt: &'a str, columns: &'a [ModelColumn]) -> ResolveInput<'a> {
    ResolveInput {
        tenant_id: TENANT,
        prompt_hmac_key: &KEY,
        quota_override: None,
        zero_retention: false,
        prompt,
        projection_version: None,
        columns,
        now: Utc::now(),
    }
}

fn zr_input<'a>(prompt: &'a str, columns: &'a [ModelColumn]) -> ResolveInput<'a> {
    ResolveInput {
        zero_retention: true,
        ..input(prompt, columns)
    }
}

fn with_quota<'a>(prompt: &'a str, quota: i32) -> ResolveInput<'a> {
    ResolveInput {
        quota_override: Some(quota),
        ..input(prompt, &[])
    }
}

async fn cache_rows(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("select count(*) from classification_cache")
        .fetch_one(pool)
        .await
        .unwrap()
}

// parity: classify.TestCacheRoundTrip
// parity: classify.TestCacheConflictKeepsFirstWrite
#[tokio::test]
async fn the_cache_is_keyed_by_tenant_hash_and_version_and_keeps_the_first_write() {
    let Some((db, pool)) = database().await else {
        return;
    };
    let stored = r#"{"capability":"structured_extraction","difficulty":3,"domain_tags":["legal"],"confidence":0.8,"truncated":true}"#;
    let raw = JsonRawValue::from_string(stored.to_owned()).unwrap();
    let v1 = "claude-haiku-4-5/direct/v1";
    decisions::upsert_classification_cache(&pool, TENANT, "hmac_sha256:aa", v1, &raw)
        .await
        .unwrap();
    let hit = decisions::get_classification_cache(&pool, TENANT, "hmac_sha256:aa", v1)
        .await
        .unwrap();
    let value: Value = serde_json::from_str(hit.classification.get()).unwrap();
    assert_eq!(
        value["truncated"], true,
        "truncated survives the jsonb round trip"
    );
    for (tenant, hash, version) in [
        (TENANT, "hmac_sha256:aa", "claude-haiku-4-5/direct/v2"),
        (TENANT, "hmac_sha256:bb", v1),
        (OTHER_TENANT, "hmac_sha256:aa", v1),
    ] {
        let miss = decisions::get_classification_cache(&pool, tenant, hash, version).await;
        assert!(
            miss.is_err_and(|e| maverick_store::is_not_found(&e)),
            "{tenant} {hash} {version}"
        );
    }
    let second = JsonRawValue::from_string(r#"{"capability":"factual_qa"}"#.to_owned()).unwrap();
    decisions::upsert_classification_cache(&pool, TENANT, "hmac_sha256:aa", v1, &second)
        .await
        .unwrap();
    let kept = decisions::get_classification_cache(&pool, TENANT, "hmac_sha256:aa", v1)
        .await
        .unwrap();
    assert_eq!(kept.classification.get(), stored_jsonb(&pool, stored).await);
    db.close().await;
}

/// Postgres jsonb normalizes spacing; compare against its own rendering.
async fn stored_jsonb(pool: &PgPool, text: &str) -> String {
    sqlx::query_scalar::<_, String>("select $1::jsonb::text")
        .bind(text)
        .fetch_one(pool)
        .await
        .unwrap()
}

// parity: classify.TestResolveMethodTransitions
// parity: classify.TestResolveVersionBumpInvalidationStoreBacked
#[tokio::test]
async fn classified_then_cached_and_a_version_bump_is_a_new_namespace() {
    let Some((db, pool)) = database().await else {
        return;
    };
    let provider = Scripted::always(text(VALID));
    let resolver = Resolver::new(settings()).with_classifier(provider.classifier(COLUMN));
    let first = resolver
        .resolve(&pool, &input("the prompt", &[]))
        .await
        .unwrap();
    assert_eq!(first.method, Method::Classified);
    assert_eq!(first.classification, legal());
    assert_eq!(first.prompt_hash, fingerprint(&KEY, b"the prompt"));
    assert!(!first.quota_exceeded);
    let second = resolver
        .resolve(&pool, &input("the prompt", &[]))
        .await
        .unwrap();
    assert_eq!(second.method, Method::Cached);
    assert_eq!(second.classification, first.classification);
    assert_eq!(provider.calls(), 1);

    let bumped = Resolver::new(ResolverSettings {
        prompt_version: "v-bumped".to_owned(),
        ..settings()
    })
    .with_classifier(provider.classifier(COLUMN).with_prompt_version("v-bumped"));
    let third = bumped
        .resolve(&pool, &input("the prompt", &[]))
        .await
        .unwrap();
    assert_eq!(third.method, Method::Classified);
    let fourth = bumped
        .resolve(&pool, &input("the prompt", &[]))
        .await
        .unwrap();
    assert_eq!(fourth.method, Method::Cached);
    assert_eq!(provider.calls(), 2);
    let alerts: i64 = sqlx::query_scalar("select count(*) from alerts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(alerts, 0);
    db.close().await;
}

// parity: classify.TestResolveDifferentPromptsMiss
#[tokio::test]
async fn different_prompts_miss() {
    let Some((db, pool)) = database().await else {
        return;
    };
    let provider = Scripted::always(text(VALID));
    let resolver = Resolver::new(settings()).with_classifier(provider.classifier(COLUMN));
    let a = resolver
        .resolve(&pool, &input("prompt a", &[]))
        .await
        .unwrap();
    let b = resolver
        .resolve(&pool, &input("prompt b", &[]))
        .await
        .unwrap();
    assert_ne!(a.prompt_hash, b.prompt_hash);
    assert_eq!(
        (a.method, b.method),
        (Method::Classified, Method::Classified)
    );
    assert_eq!(provider.calls(), 2);
    db.close().await;
}

// parity: classify.TestResolveFallbackNotCached
#[tokio::test]
async fn a_fallback_is_never_cached() {
    let Some((db, pool)) = database().await else {
        return;
    };
    let provider = Scripted::new(vec![Reply::Fail, Reply::Fail], text(VALID));
    let resolver = Resolver::new(settings()).with_classifier(provider.classifier(COLUMN));
    let outage = resolver
        .resolve(&pool, &input("the prompt", &[]))
        .await
        .unwrap();
    assert_eq!(outage.method, Method::FallbackDefault);
    assert_eq!(outage.classification, Classification::fallback());
    assert_eq!(cache_rows(&pool).await, 0);
    let recovered = resolver
        .resolve(&pool, &input("the prompt", &[]))
        .await
        .unwrap();
    assert_eq!(recovered.method, Method::Classified);
    let again = resolver
        .resolve(&pool, &input("the prompt", &[]))
        .await
        .unwrap();
    assert_eq!(again.method, Method::Cached);
    db.close().await;
}

/// Every classifier failure is the fallback; every cache failure is a miss that still
/// classifies. The hash is always the prompt's fingerprint.
// parity: classify.TestResolveNeverFails
#[tokio::test]
async fn resolve_never_fails() {
    let Some((db, pool)) = database().await else {
        return;
    };
    let quick = ResolverSettings {
        deadline: Duration::from_millis(50),
        ..settings()
    };
    for (name, provider) in [
        ("provider failure", Scripted::always(Reply::Fail)),
        ("timeout", Scripted::always(Reply::Hang)),
        ("invalid output", Scripted::always(text("not json"))),
        (
            "schema violation",
            Scripted::always(text(
                r#"{"capability":"poetry","difficulty":3,"domain_tags":[],"confidence":0.5}"#,
            )),
        ),
    ] {
        let resolver = Resolver::new(quick.clone()).with_classifier(provider.classifier(COLUMN));
        let resolution = resolver
            .resolve(&pool, &input("the prompt", &[]))
            .await
            .unwrap();
        assert_eq!(resolution.method, Method::FallbackDefault, "{name}");
        assert_eq!(
            resolution.classification,
            Classification::fallback(),
            "{name}"
        );
        assert_eq!(
            resolution.prompt_hash,
            fingerprint(&KEY, b"the prompt"),
            "{name}"
        );
    }
    // No classifier client at all (serve could not resolve the column).
    let unconnected = Resolver::new(settings());
    let resolution = unconnected
        .resolve(&pool, &input("the prompt", &[]))
        .await
        .unwrap();
    assert_eq!(resolution.method, Method::FallbackDefault);

    // A corrupt cached value is a miss, and a real classification follows.
    let provider = Scripted::always(text(VALID));
    let resolver = Resolver::new(settings()).with_classifier(provider.classifier(COLUMN));
    let corrupt = JsonRawValue::from_string(r#"{"capability":5}"#.to_owned()).unwrap();
    decisions::upsert_classification_cache(
        &pool,
        TENANT,
        &fingerprint(&KEY, b"corrupt"),
        &classify::classifier_version(COLUMN, classify::PROMPT_VERSION),
        &corrupt,
    )
    .await
    .unwrap();
    let resolution = resolver
        .resolve(&pool, &input("corrupt", &[]))
        .await
        .unwrap();
    assert_eq!(resolution.method, Method::Classified);

    // A cache that cannot be read or written: still classified.
    sqlx::query("alter table classification_cache rename to classification_cache_gone")
        .execute(&pool)
        .await
        .unwrap();
    let resolution = resolver
        .resolve(&pool, &input("no cache", &[]))
        .await
        .unwrap();
    assert_eq!(resolution.method, Method::Classified);
    assert_eq!(resolution.classification, legal());
    db.close().await;
}

/// 20 failures in the window raise one critical `classifier_degraded` alert; later
/// failures in the same episode raise none.
// parity: classify.TestResolveDegradedAlert
#[tokio::test]
async fn sustained_failure_raises_one_degraded_alert() {
    let Some((db, pool)) = database().await else {
        return;
    };
    let provider = Scripted::always(Reply::Fail);
    let resolver = Resolver::new(settings()).with_classifier(provider.classifier(COLUMN));
    for i in 0..25 {
        let prompt = format!("prompt {i}");
        resolver.resolve(&pool, &input(&prompt, &[])).await.unwrap();
    }
    let rows: Vec<(String, String, String, String)> = sqlx::query_as(
        "select kind, severity, subject::text, message from alerts where kind = 'classifier_degraded'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    let (_, severity, subject, message) = &rows[0];
    assert_eq!(severity, "critical");
    let subject: Value = serde_json::from_str(subject).unwrap();
    assert_eq!(subject["classifier_version"], "claude-haiku-4-5/direct/v2");
    assert_eq!(subject["samples"], 20);
    assert_eq!(subject["window_seconds"], 900);
    assert_eq!(subject["failure_rate"], 1.0);
    assert_eq!(
        message,
        "classifier claude-haiku-4-5/direct/v2 failed 100.0% of calls over the last 15m0s \
         (threshold 5%); prompt-path requests are receiving the fallback classification"
    );
    db.close().await;
}

async fn catalog_column(
    pool: &PgPool,
    id: &str,
    model: &str,
    endpoint: &str,
    retention: Option<(&str, Option<DateTime<Utc>>)>,
    pricing: &str,
) {
    let empty = JsonRawValue::from_string("{}".to_owned()).unwrap();
    let pricing = JsonRawValue::from_string(pricing.to_owned()).unwrap();
    catalog::create_model_column(
        pool,
        &NewModelColumn {
            id,
            provider: "anthropic",
            model_id: model,
            endpoint,
            settings: &empty,
            status: "active",
            pricing: &pricing,
            quality_source_column_id: None,
            cold_start_until: None,
            deprecation_date: None,
            execution_aliases: None,
            retention_mode: retention.map(|(mode, _)| mode),
            retention_verified_at: retention.and_then(|(_, until)| until).map(|_| Utc::now()),
            retention_valid_until: retention.and_then(|(_, until)| until),
            retention_source: None,
            execution_alias_retention: None,
            param_constraints: None,
        },
    )
    .await
    .unwrap();
}

/// DIVERGENCE: D2. A zero-retention tenant classifies only on the zero-retention column,
/// and only while its zero evidence is valid; otherwise the prompt never leaves the
/// engine: no call, no cache row, no budget count.
#[tokio::test]
async fn zero_retention_tenants_use_only_a_verified_zero_retention_column() {
    let d2 = parity_support::divergence!("D2");
    assert_eq!(d2.id(), "D2");
    let Some((db, pool)) = database().await else {
        return;
    };
    let ordinary = Scripted::always(text(VALID));
    let zero = Scripted::always(text(VALID));
    let zr = |settings: ResolverSettings| {
        Resolver::new(settings)
            .with_classifier(ordinary.classifier(COLUMN))
            .with_zero_retention_classifier(zero.classifier(ZR_COLUMN))
    };
    // No zero-retention column configured.
    let unset = zr(ResolverSettings {
        zero_retention_column: None,
        ..settings()
    });
    assert_eq!(
        unset.resolve(&pool, &zr_input("secret prompt", &[])).await,
        Err(ZeroRetentionRefused)
    );

    // Configured, but its catalog column is missing, standard, or has expired evidence.
    let expired = Utc::now() - chrono::Duration::days(1);
    let valid = Utc::now() + chrono::Duration::days(30);
    catalog_column(
        &pool,
        "col_std",
        "claude-haiku-4-5",
        "vertex",
        Some(("standard", None)),
        "{}",
    )
    .await;
    let refused_columns = [
        Vec::new(),
        catalog::list_model_columns(&pool).await.unwrap(),
    ];
    for columns in &refused_columns {
        assert_eq!(
            zr(settings())
                .resolve(&pool, &zr_input("secret prompt", columns))
                .await,
            Err(ZeroRetentionRefused)
        );
    }
    sqlx::query("delete from model_columns")
        .execute(&pool)
        .await
        .unwrap();
    catalog_column(
        &pool,
        "col_exp",
        "claude-haiku-4-5",
        "vertex",
        Some(("zero", Some(expired))),
        "{}",
    )
    .await;
    let columns = catalog::list_model_columns(&pool).await.unwrap();
    assert_eq!(
        zr(settings())
            .resolve(&pool, &zr_input("secret prompt", &columns))
            .await,
        Err(ZeroRetentionRefused)
    );
    assert_eq!((ordinary.calls(), zero.calls()), (0, 0));
    assert_eq!(cache_rows(&pool).await, 0);
    let usage: i64 = sqlx::query_scalar("select count(*) from classifier_usage")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(usage, 0, "a refused prompt is not counted");

    // Valid zero evidence: the zero-retention classifier is the one called.
    sqlx::query("delete from model_columns")
        .execute(&pool)
        .await
        .unwrap();
    catalog_column(
        &pool,
        "col_zero",
        "claude-haiku-4-5",
        "vertex",
        Some(("zero", Some(valid))),
        "{}",
    )
    .await;
    let columns = catalog::list_model_columns(&pool).await.unwrap();
    let resolution = zr(settings())
        .resolve(&pool, &zr_input("secret prompt", &columns))
        .await
        .unwrap();
    assert_eq!(resolution.method, Method::Classified);
    assert_eq!((ordinary.calls(), zero.calls()), (0, 1));

    // Evidence valid but no client for the column: the fallback, and still no call.
    let unconnected = Resolver::new(settings()).with_classifier(ordinary.classifier(COLUMN));
    let resolution = unconnected
        .resolve(&pool, &zr_input("another", &columns))
        .await
        .unwrap();
    assert_eq!(resolution.method, Method::FallbackDefault);
    assert_eq!((ordinary.calls(), zero.calls()), (0, 1));
    db.close().await;
}

/// Two catalog columns for the zero-retention ref (same model and endpoint): `col_zr_1`
/// (settings `{}`) sorts first by id, so the classifier calls it; `col_zr_2` is a sibling
/// with thinking on.
async fn zero_retention_siblings(
    pool: &PgPool,
    first: (&str, Option<DateTime<Utc>>),
    sibling: (&str, Option<DateTime<Utc>>),
) {
    sqlx::query("delete from model_columns")
        .execute(pool)
        .await
        .unwrap();
    let pricing = JsonRawValue::from_string("{}".to_owned()).unwrap();
    for (id, settings, (mode, until)) in [
        ("col_zr_1", "{}", first),
        ("col_zr_2", r#"{"thinking":true}"#, sibling),
    ] {
        let settings = JsonRawValue::from_string(settings.to_owned()).unwrap();
        catalog::create_model_column(
            pool,
            &NewModelColumn {
                id,
                provider: "anthropic",
                model_id: "claude-haiku-4-5",
                endpoint: "vertex",
                settings: &settings,
                status: "active",
                pricing: &pricing,
                quality_source_column_id: None,
                cold_start_until: None,
                deprecation_date: None,
                execution_aliases: None,
                retention_mode: Some(mode),
                retention_verified_at: until.map(|_| Utc::now()),
                retention_valid_until: until,
                retention_source: None,
                execution_alias_retention: None,
                param_constraints: None,
            },
        )
        .await
        .unwrap();
    }
}

/// DIVERGENCE: D2 (bead model-maverick-a49j). The zero evidence must be on the column the
/// classifier calls, the first catalog column for the ref. A sibling with the same model
/// and endpoint does not clear it (spec §8.3: "its evidence").
#[tokio::test]
async fn zero_retention_evidence_must_be_on_the_column_the_classifier_calls() {
    let Some((db, pool)) = database().await else {
        return;
    };
    let zero = Scripted::always(text(VALID));
    let resolver =
        || Resolver::new(settings()).with_zero_retention_classifier(zero.classifier(ZR_COLUMN));
    let valid = Utc::now() + chrono::Duration::days(30);

    // The called column is standard; only its sibling holds zero evidence: refused.
    zero_retention_siblings(&pool, ("standard", None), ("zero", Some(valid))).await;
    let columns = catalog::list_model_columns(&pool).await.unwrap();
    assert_eq!(columns[0].id, "col_zr_1", "the catalog is ordered by id");
    assert_eq!(
        resolver()
            .resolve(&pool, &zr_input("secret prompt", &columns))
            .await,
        Err(ZeroRetentionRefused)
    );
    assert_eq!(zero.calls(), 0, "the prompt never left the engine");
    assert_eq!(cache_rows(&pool).await, 0);

    // The reverse: the called column holds the evidence, so it classifies.
    zero_retention_siblings(&pool, ("zero", Some(valid)), ("standard", None)).await;
    let columns = catalog::list_model_columns(&pool).await.unwrap();
    let resolution = resolver()
        .resolve(&pool, &zr_input("secret prompt", &columns))
        .await
        .unwrap();
    assert_eq!(resolution.method, Method::Classified);
    assert_eq!(zero.calls(), 1);
    db.close().await;
}

/// DIVERGENCE: D4. The tenant's daily quota and the global daily spend cap stop the
/// classifier call; the answer is the fallback with `quota_exceeded`. Cache hits never
/// count.
#[tokio::test]
async fn the_daily_quota_and_spend_cap_stop_classifier_calls() {
    let d4 = parity_support::divergence!("D4");
    assert_eq!(d4.id(), "D4");
    let Some((db, pool)) = database().await else {
        return;
    };
    let provider = Scripted::always(text(VALID));
    let resolver = Resolver::new(settings()).with_classifier(provider.classifier(COLUMN));
    assert_eq!(
        resolver
            .resolve(&pool, &with_quota("a", 1))
            .await
            .unwrap()
            .method,
        Method::Classified
    );
    let stopped = resolver.resolve(&pool, &with_quota("b", 1)).await.unwrap();
    assert_eq!(
        (stopped.method, stopped.quota_exceeded),
        (Method::FallbackDefault, true)
    );
    assert_eq!(stopped.classification, Classification::fallback());
    let cached = resolver.resolve(&pool, &with_quota("a", 1)).await.unwrap();
    assert_eq!(
        (cached.method, cached.quota_exceeded),
        (Method::Cached, false),
        "a cache hit needs no budget"
    );
    assert_eq!(provider.calls(), 1);
    assert!(
        resolver
            .resolve(&pool, &with_quota("c", 0))
            .await
            .unwrap()
            .quota_exceeded
    );
    assert_eq!(provider.calls(), 1);

    // The spend cap is global: 100 input and 20 output tokens at $1 and $5 per Mtok cost
    // $0.0002, past a $0.0001 cap, so the next miss of any tenant makes no call.
    catalog_column(
        &pool,
        "col_haiku",
        "claude-haiku-4-5",
        "direct",
        None,
        r#"{"input_per_mtok":1,"output_per_mtok":5,"currency":"USD","as_of":"2026-07-01","source":"list"}"#,
    )
    .await;
    let columns = catalog::list_model_columns(&pool).await.unwrap();
    let capped = Resolver::new(ResolverSettings {
        daily_spend_cap_usd: 0.0001,
        ..settings()
    })
    .with_classifier(provider.classifier(COLUMN));
    assert_eq!(
        capped
            .resolve(&pool, &input("d", &columns))
            .await
            .unwrap()
            .method,
        Method::Classified
    );
    let spent: String = sqlx::query_scalar(
        "select sum(spend_usd)::text from classifier_usage where tenant_id = $1",
    )
    .bind(TENANT)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(spent, "0.000200");
    let other = ResolveInput {
        tenant_id: OTHER_TENANT,
        ..input("e", &columns)
    };
    let stopped = capped.resolve(&pool, &other).await.unwrap();
    assert!(stopped.quota_exceeded, "the cap is global");
    assert_eq!(provider.calls(), 2);
    db.close().await;
}

/// DIVERGENCE: D5. The projection version joins the cache key, so the same prompt text
/// under another projection is a separate entry.
#[tokio::test]
async fn the_projection_version_separates_cache_entries() {
    let d5 = parity_support::divergence!("D5");
    assert_eq!(d5.id(), "D5");
    let Some((db, pool)) = database().await else {
        return;
    };
    let provider = Scripted::always(text(VALID));
    let resolver = Resolver::new(settings()).with_classifier(provider.classifier(COLUMN));
    let p1 = ResolveInput {
        projection_version: Some("p1"),
        ..input("same text", &[])
    };
    assert_eq!(
        resolver.resolve(&pool, &p1).await.unwrap().method,
        Method::Classified
    );
    assert_eq!(
        resolver
            .resolve(&pool, &input("same text", &[]))
            .await
            .unwrap()
            .method,
        Method::Classified
    );
    assert_eq!(
        resolver.resolve(&pool, &p1).await.unwrap().method,
        Method::Cached
    );
    assert_eq!(provider.calls(), 2);
    let versions: Vec<String> = sqlx::query_scalar(
        "select classifier_version from classification_cache order by classifier_version",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        versions,
        [
            "claude-haiku-4-5/direct/v2",
            "claude-haiku-4-5/direct/v2/p1"
        ]
    );
    db.close().await;
}

/// D3 through the resolver: a hung classifier answers the fallback within the deadline.
#[tokio::test]
async fn the_resolver_answers_within_the_deadline() {
    let Some((db, pool)) = database().await else {
        return;
    };
    let provider = Scripted::always(Reply::Hang);
    let resolver = Resolver::new(ResolverSettings {
        deadline: Duration::from_millis(100),
        ..settings()
    })
    .with_classifier(provider.classifier(COLUMN));
    let started = std::time::Instant::now();
    let resolution = resolver.resolve(&pool, &input("slow", &[])).await.unwrap();
    assert_eq!(resolution.method, Method::FallbackDefault);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(provider.calls(), 1);
    db.close().await;
}
