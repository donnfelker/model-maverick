//! `POST /v1/recommend` on the prompt path (bead 5.6; reference Part 5 §6.5, §13.5 and the
//! capture cases), on real Postgres with a scripted classifier provider: classification,
//! the cache, the fallback, the trace row without the prompt, the capture hook, and D2,
//! D4, and D5 end to end.

use std::collections::VecDeque;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use chrono::{Duration, NaiveDate, Utc};
use maverick_api::capture::Capturer;
use maverick_api::classify::{Classifier, Resolver, ResolverSettings};
use maverick_api::{
    ApiState, AuthState, HealthState, Identity, RateLimiter, RecommendConfig, StaticAuthBackend,
    public_router_with,
};
use maverick_domain::capture::{KeyRing, fingerprint};
use maverick_domain::keys;
use maverick_providers::{
    CompletionRequest, CompletionResponse, ProviderClient, ProviderError, ProviderErrorKind,
    ProviderFuture,
};
use maverick_store::catalog::{self, NewModelColumn};
use maverick_store::measurement::{self, AggregateUpsert};
use maverick_store::{JsonRawValue, MIGRATOR, corpus, tenancy};
use maverick_testdb::{Schema, TestDatabase};
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

const TENANT: &str = "ten_prompt";
const KEY: &str = "key_prompt";
const KEY2: &str = "key_prompt_two";
const CORPUS: &str = "2026-07-01";
const HMAC_KEY: [u8; 32] = [0; 32];
const MASTER_KEY: [u8; 32] = [0xC4; 32];
const COLUMN: &str = "claude-haiku-4-5/direct";
const ZR_COLUMN: &str = "claude-haiku-4-5/vertex";
const MARKER: &str = "ACME-CONTRACT-MARKER-8842";
const CONTRACT: &str = "Extract the parties, dates, and obligations from this contract: ACME-CONTRACT-MARKER-8842 between Acme Corp and Bravo LLC, signed 2026-01-15.";
const LEGAL: &str = r#"{"capability":"structured_extraction","difficulty":3,"domain_tags":["legal"],"confidence":0.91}"#;

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A classifier provider that answers a queue, then `then` forever (`None` fails).
#[derive(Clone)]
struct Scripted {
    queue: Arc<Mutex<VecDeque<Option<String>>>>,
    then: Arc<Mutex<Option<String>>>,
    calls: Arc<Mutex<usize>>,
}

impl Scripted {
    fn always(reply: Option<&str>) -> Self {
        Self {
            queue: Arc::default(),
            then: Arc::new(Mutex::new(reply.map(str::to_owned))),
            calls: Arc::default(),
        }
    }

    fn calls(&self) -> usize {
        *lock(&self.calls)
    }

    fn answer(&self, reply: Option<&str>) {
        *lock(&self.then) = reply.map(str::to_owned);
    }
}

impl ProviderClient for Scripted {
    fn complete<'a>(&'a self, _request: CompletionRequest) -> ProviderFuture<'a> {
        *lock(&self.calls) += 1;
        let reply = lock(&self.queue)
            .pop_front()
            .unwrap_or_else(|| lock(&self.then).clone());
        Box::pin(async move {
            match reply {
                Some(text) => Ok(CompletionResponse {
                    text,
                    tokens_in: 10,
                    tokens_out: 5,
                }),
                None => Err(ProviderError::new(
                    "anthropic",
                    ProviderErrorKind::ProviderError,
                )),
            }
        })
    }

    fn name(&self) -> &str {
        "anthropic"
    }
}

/// One test's database, classifier, and capture hook.
struct Env {
    _db: TestDatabase,
    pool: PgPool,
    classifier: Scripted,
    zero_classifier: Scripted,
    config: RecommendConfig,
}

impl Env {
    fn router(&self, key: &str) -> Router {
        let auth = AuthState::new(
            Arc::new(StaticAuthBackend::authenticated(Identity {
                tenant_id: TENANT.to_owned(),
                api_key_id: key.to_owned(),
            })),
            keys::parse_internal_tokens("ops:alpha").unwrap(),
            RateLimiter::new(10_000),
        );
        let health = HealthState::with_providers(self.pool.clone(), Vec::new());
        let api = ApiState::with_health(self.pool.clone(), health)
            .with_recommend_config(self.config.clone());
        public_router_with(auth, api)
    }

    async fn recommend_as(&self, key: &str, body: &str) -> (StatusCode, String) {
        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/recommend")
            .header(header::AUTHORIZATION, "Bearer mk_live_test")
            .body(Body::from(body.to_owned()))
            .unwrap();
        let response = self.router(key).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        eprintln!("POST /v1/recommend -> {status} {text}");
        (status, text)
    }

    async fn recommend(&self, body: &str) -> (StatusCode, String) {
        self.recommend_as(KEY, body).await
    }

    async fn count(&self, table: &str) -> i64 {
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!("select count(*) from {table}")))
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    async fn window(&self, key: Option<&str>, below: Option<f32>) -> String {
        let id = maverick_domain::ids::new(maverick_domain::ids::prefix::CAPTURE_WINDOW);
        maverick_store::capture::create_capture_window(
            &self.pool,
            &id,
            TENANT,
            key,
            below,
            Utc::now() - Duration::minutes(1),
            Utc::now() + Duration::minutes(60),
        )
        .await
        .unwrap();
        id
    }
}

fn prompt_body(prompt: &str) -> String {
    serde_json::json!({"workload": {"prompt": prompt}}).to_string()
}

fn parse(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("not JSON ({e}): {text}"))
}

fn night() -> NaiveDate {
    Utc::now().date_naive()
}

async fn column(pool: &PgPool, id: &str, model: &str, endpoint: &str, zero: bool) {
    let empty = JsonRawValue::from_string("{}".to_owned()).unwrap();
    let until = Utc::now() + Duration::days(30);
    catalog::create_model_column(
        pool,
        &NewModelColumn {
            id,
            provider: "anthropic",
            model_id: model,
            endpoint,
            settings: &empty,
            status: "active",
            pricing: &empty,
            quality_source_column_id: None,
            cold_start_until: None,
            deprecation_date: None,
            execution_aliases: None,
            retention_mode: zero.then_some("zero"),
            retention_verified_at: zero.then(Utc::now),
            retention_valid_until: zero.then_some(until),
            retention_source: None,
            execution_alias_retention: None,
            param_constraints: None,
        },
    )
    .await
    .unwrap();
}

async fn aggregate(pool: &PgPool, column: &str, capability: &str, tier: i16, dim: &str, ewma: f32) {
    measurement::upsert_aggregate(
        pool,
        &AggregateUpsert {
            model_column_id: column,
            capability,
            difficulty_tier: tier,
            dimension: dim,
            ewma,
            variance: 0.0,
            n: 20,
            measured: true,
            matrix_version: CORPUS,
            as_of: night(),
        },
    )
    .await
    .unwrap();
}

/// A tenant with two keys, an active corpus, and one pool column measured for the
/// capabilities the scripted classifier and the fallback name. `zero` makes the tenant
/// zero-retention and gives the pool column and the classifier column zero evidence.
async fn env(zero: bool) -> Option<Env> {
    let db = maverick_testdb::database(&SCHEMA).await?;
    let pool = db.pool().await;
    tenancy::create_tenant(&pool, TENANT, "Acme", &HMAC_KEY)
        .await
        .unwrap();
    for key in [KEY, KEY2] {
        tenancy::create_api_key(&pool, key, TENANT, key.as_bytes(), "abcd", "")
            .await
            .unwrap();
    }
    if zero {
        tenancy::upsert_tenant_constraints(&pool, TENANT, None, None, true, false)
            .await
            .unwrap();
    }
    corpus::upsert_corpus_version(&pool, CORPUS, "", 0, false)
        .await
        .unwrap();
    corpus::activate_corpus_version(&pool, CORPUS)
        .await
        .unwrap();
    column(&pool, "col_sonnet", "claude-sonnet-4-6", "direct", zero).await;
    column(&pool, "col_haiku", "claude-haiku-4-5", "direct", false).await;
    column(&pool, "col_haiku_zr", "claude-haiku-4-5", "vertex", true).await;
    tenancy::create_pool(
        &pool,
        "pool_default",
        TENANT,
        "default",
        &["col_sonnet".to_owned()],
        true,
    )
    .await
    .unwrap();
    for capability in [
        "structured_extraction",
        "multi_step_reasoning",
        "factual_qa",
    ] {
        for tier in 1..=5 {
            aggregate(&pool, "col_sonnet", capability, tier, "pass_rate", 0.95).await;
        }
        aggregate(&pool, "col_sonnet", capability, 3, "quality", 0.9).await;
        aggregate(&pool, "col_sonnet", capability, 3, "cost_per_run_usd", 0.01).await;
        aggregate(&pool, "col_sonnet", capability, 3, "latency_p95_ms", 900.0).await;
    }
    let classifier = Scripted::always(Some(LEGAL));
    let zero_classifier = Scripted::always(Some(LEGAL));
    let resolver = Resolver::new(ResolverSettings {
        column: COLUMN.to_owned(),
        zero_retention_column: Some(ZR_COLUMN.to_owned()),
        ..ResolverSettings::default()
    })
    .with_classifier(Classifier::new(Arc::new(classifier.clone()), COLUMN))
    .with_zero_retention_classifier(Classifier::new(
        Arc::new(zero_classifier.clone()),
        ZR_COLUMN,
    ));
    let capturer = Capturer::new(KeyRing::new(1, MASTER_KEY, None), 72).with_window_ttl(None);
    let config = RecommendConfig {
        classifier: Arc::new(resolver),
        capture: Some(Arc::new(capturer)),
        ..RecommendConfig::default()
    };
    Some(Env {
        _db: db,
        pool,
        classifier,
        zero_classifier,
        config,
    })
}

// parity: api.TestRecommendPromptFormClassified
#[tokio::test]
async fn prompt_form_is_classified_and_the_trace_keeps_no_prompt() {
    let Some(env) = env(false).await else { return };
    let (status, text) = env.recommend(&prompt_body(CONTRACT)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        text.starts_with(
            r#"{"classification":{"capability":"structured_extraction","difficulty":3,"domain_tags":["legal"],"high_stakes":true,"confidence":0.91,"method":"classified"},"recommendations":["#
        ),
        "{text}"
    );
    let body = parse(&text);
    let hash = fingerprint(&HMAC_KEY, CONTRACT.as_bytes());
    assert_eq!(body["prompt_hash"], hash.as_str());
    assert!(!body["recommendations"].as_array().unwrap().is_empty());
    assert!(body.get("classification_quota_exceeded").is_none());
    assert!(
        text.contains(&format!(
            r#""violations":[],"prompt_hash":"{hash}","recommendation_id":"#
        )),
        "prompt_hash sits between violations and recommendation_id: {text}"
    );
    assert_eq!(env.classifier.calls(), 1);

    let (prompt_hash, request, classification, candidates, response): (
        Option<String>,
        String,
        String,
        String,
        String,
    ) = sqlx::query_as(
        "select prompt_hash, request::text, classification::text, candidates::text, response::text
         from recommendations",
    )
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert_eq!(prompt_hash.as_deref(), Some(hash.as_str()));
    let stored: Value = serde_json::from_str(&request).unwrap();
    assert!(stored["workload"].get("prompt").is_none(), "{request}");
    for column in [&request, &classification, &candidates, &response] {
        assert!(
            !column.contains(MARKER),
            "the prompt reached the trace: {column}"
        );
    }
    assert_eq!(parse(&classification)["method"], "classified");
}

// parity: api.TestRecommendPromptSecondRequestHitsCache
#[tokio::test]
async fn the_second_request_hits_the_cache() {
    let Some(env) = env(false).await else { return };
    let (_, first) = env.recommend(&prompt_body(CONTRACT)).await;
    let (_, second) = env.recommend(&prompt_body(CONTRACT)).await;
    let (first, second) = (parse(&first), parse(&second));
    assert_eq!(first["classification"]["method"], "classified");
    assert_eq!(second["classification"]["method"], "cached");
    assert_eq!(env.classifier.calls(), 1);
    assert_eq!(first["prompt_hash"], second["prompt_hash"]);
    let without_method = |value: &Value| {
        let mut block = value["classification"].clone();
        block.as_object_mut().unwrap().remove("method");
        block
    };
    assert_eq!(without_method(&first), without_method(&second));
}

// parity: api.TestRecommendPromptClassifierOutageFallsBack
#[tokio::test]
async fn a_classifier_outage_answers_the_fallback() {
    let Some(env) = env(false).await else { return };
    env.classifier.answer(None);
    let (status, text) = env.recommend(&prompt_body(CONTRACT)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        text.starts_with(
            r#"{"classification":{"capability":"multi_step_reasoning","difficulty":3,"domain_tags":[],"high_stakes":false,"confidence":0,"method":"fallback_default"},"#
        ),
        "{text}"
    );
    let body = parse(&text);
    assert!(!body["recommendations"].as_array().unwrap().is_empty());
    assert!(
        body["prompt_hash"]
            .as_str()
            .unwrap()
            .starts_with("hmac_sha256:")
    );
    assert!(env.classifier.calls() >= 1);
}

// parity: api.TestRecommendPromptHighStakesTagDerivation
#[tokio::test]
async fn high_stakes_follows_the_tags() {
    for (tags, high_stakes) in [(r#"["medical"]"#, true), (r#"["cooking","travel"]"#, false)] {
        let Some(env) = env(false).await else { return };
        let output = format!(
            r#"{{"capability":"factual_qa","difficulty":3,"domain_tags":{tags},"confidence":0.9}}"#
        );
        env.classifier.answer(Some(&output));
        let (status, text) = env
            .recommend(&prompt_body(
                "Which dosage of ibuprofen is safe for an adult?",
            ))
            .await;
        assert_eq!(status, StatusCode::OK);
        let block = &parse(&text)["classification"];
        assert_eq!(block["high_stakes"], high_stakes, "{tags}");
        let want: Value = serde_json::from_str(tags).unwrap();
        assert_eq!(block["domain_tags"], want);
    }
}

// parity: api.TestRecommendPromptOverCharLimit422
// parity: api.TestRecommendPromptRejectsDeclaredFields
#[tokio::test]
async fn invalid_prompt_requests_never_reach_the_classifier() {
    let Some(env) = env(false).await else { return };
    let (status, text) = env.recommend(&prompt_body(&"a".repeat(500_001))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(parse(&text)["error"]["details"]["field"], "workload.prompt");
    for body in [
        r#"{"workload":{"prompt":"x","difficulty":2}}"#,
        r#"{"workload":{"prompt":"x","domain_tags":["legal"]}}"#,
    ] {
        let (status, _) = env.recommend(body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    }
    assert_eq!(env.classifier.calls(), 0);
    assert_eq!(env.count("recommendations").await, 0);
}

// parity: api.TestRecommendPromptAtCharLimitSurfacesTruncated
#[tokio::test]
async fn a_prompt_at_the_limit_is_truncated_and_capped() {
    let Some(env) = env(false).await else { return };
    let (status, text) = env.recommend(&prompt_body(&"a".repeat(500_000))).await;
    assert_eq!(status, StatusCode::OK);
    let block = &parse(&text)["classification"];
    assert_eq!(block["truncated"], true);
    assert_eq!(block["confidence"], 0.8);
    assert_eq!(block["method"], "classified");
}

/// A `debug_captures` row: id, recommendation, ciphertext, nonce, key version, window.
type CaptureRow = (String, String, Vec<u8>, Vec<u8>, i32, String);

/// Decrypt every capture with the test master key.
async fn captures(env: &Env) -> Vec<(String, String, String, String)> {
    let rows: Vec<CaptureRow> = sqlx::query_as(
        "select id, recommendation_id, ciphertext, nonce, capture_key_version,
                coalesce(window_id, '')
         from debug_captures order by id",
    )
    .fetch_all(&env.pool)
    .await
    .unwrap();
    let ring = KeyRing::new(1, MASTER_KEY, None);
    rows.into_iter()
        .map(|(id, recommendation, ciphertext, nonce, version, window)| {
            assert!(!String::from_utf8_lossy(&ciphertext).contains(MARKER));
            let nonce: [u8; 12] = nonce.try_into().expect("12-byte nonce");
            let prompt = ring
                .open(u32::try_from(version).unwrap(), &id, &nonce, &ciphertext)
                .expect("opens");
            (
                id,
                recommendation,
                String::from_utf8(prompt).unwrap(),
                window,
            )
        })
        .collect()
}

// parity: api.TestRecommendPromptCapturedUnderActiveWindow
#[tokio::test]
async fn an_active_window_captures_the_sealed_prompt() {
    let Some(env) = env(false).await else { return };
    let window = env.window(None, None).await;
    let (status, text) = env.recommend(&prompt_body(CONTRACT)).await;
    assert_eq!(status, StatusCode::OK);
    let recommendation = parse(&text)["recommendation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let captured = captures(&env).await;
    assert_eq!(captured.len(), 1);
    let (id, rec, prompt, window_id) = &captured[0];
    assert!(id.starts_with("cap_"), "{id}");
    assert_eq!(
        (rec, prompt.as_str(), window_id),
        (&recommendation, CONTRACT, &window)
    );
    let exact: bool = sqlx::query_scalar(
        "select c.expires_at = w.ends_at + interval '72 hours'
         from debug_captures c join capture_windows w on w.id = c.window_id",
    )
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert!(exact, "expires_at is the window end plus 72 h");
    assert_eq!(env.count("capture_access_log").await, 0);
}

// parity: api.TestRecommendPromptNotCapturedWithoutWindow
// parity: api.TestRecommendDeclaredPathNeverCaptured
#[tokio::test]
async fn no_window_or_the_declared_path_captures_nothing() {
    let Some(env) = env(false).await else { return };
    let (status, _) = env.recommend(&prompt_body(CONTRACT)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(env.count("debug_captures").await, 0);
    env.window(None, None).await;
    let (status, _) = env
        .recommend(r#"{"workload":{"capability":"structured_extraction"}}"#)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(env.count("debug_captures").await, 0);
}

// parity: api.TestCaptureWindowKeyScope
#[tokio::test]
async fn a_key_scoped_window_captures_only_that_key() {
    let Some(env) = env(false).await else { return };
    env.window(Some(KEY2), None).await;
    env.recommend_as(KEY, &prompt_body(CONTRACT)).await;
    assert_eq!(env.count("debug_captures").await, 0);
    env.recommend_as(KEY2, &prompt_body(CONTRACT)).await;
    assert_eq!(env.count("debug_captures").await, 1);
}

// parity: api.TestCaptureConfidenceBelowSampling
#[tokio::test]
async fn confidence_below_samples_only_less_confident_classifications() {
    let Some(env) = env(false).await else { return };
    let low = env.window(None, Some(0.5)).await;
    env.recommend(&prompt_body(CONTRACT)).await;
    assert_eq!(
        env.count("debug_captures").await,
        0,
        "0.91 is not below 0.5"
    );
    maverick_store::capture::delete_capture_window_for_tenant(&env.pool, &low, TENANT)
        .await
        .unwrap();
    env.window(None, Some(0.95)).await;
    env.recommend(&prompt_body(CONTRACT)).await;
    assert_eq!(env.count("debug_captures").await, 1);
}

// parity: api.TestCaptureZeroRetentionBeltAndSuspenders
#[tokio::test]
async fn a_zero_retention_tenant_is_never_captured() {
    let Some(env) = env(true).await else { return };
    env.window(None, None).await;
    let (status, text) = env.recommend(&prompt_body(CONTRACT)).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(parse(&text)["zero_retention"], true);
    assert_eq!(env.count("debug_captures").await, 0);
    assert_eq!(env.zero_classifier.calls(), 1);
    assert_eq!(env.classifier.calls(), 0);
}

/// The window cache: a window created after a request is seen only once the tenant's entry
/// is invalidated (the window endpoints do that).
#[tokio::test]
async fn the_window_cache_is_invalidated_per_tenant() {
    let Some(mut env) = env(false).await else {
        return;
    };
    let capturer = Arc::new(Capturer::new(KeyRing::new(1, MASTER_KEY, None), 72));
    env.config.capture = Some(Arc::clone(&capturer));
    env.recommend(&prompt_body(CONTRACT)).await;
    env.window(None, None).await;
    env.recommend(&prompt_body(CONTRACT)).await;
    assert_eq!(
        env.count("debug_captures").await,
        0,
        "the cached empty list"
    );
    capturer.invalidate(TENANT);
    env.recommend(&prompt_body(CONTRACT)).await;
    assert_eq!(env.count("debug_captures").await, 1);
}

/// DIVERGENCE: D2. A zero-retention tenant without a usable zero-retention classifier is
/// refused, and the prompt goes nowhere: no classifier call, no trace row, no cache row.
#[tokio::test]
async fn a_zero_retention_tenant_without_a_zero_retention_classifier_is_refused() {
    let d2 = parity_support::divergence!("D2");
    assert_eq!(d2.id(), "D2");
    let Some(mut env) = env(true).await else {
        return;
    };
    env.config.classifier = Arc::new(
        Resolver::new(ResolverSettings {
            column: COLUMN.to_owned(),
            zero_retention_column: None,
            ..ResolverSettings::default()
        })
        .with_classifier(Classifier::new(Arc::new(env.classifier.clone()), COLUMN)),
    );
    env.window(None, None).await;
    let (status, text) = env.recommend(&prompt_body(CONTRACT)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let error = &parse(&text)["error"];
    assert_eq!(error["code"], "zero_retention_requires_declared");
    assert_eq!(error["details"]["field"], "workload.prompt");
    assert_eq!(env.classifier.calls(), 0);
    for table in [
        "recommendations",
        "classification_cache",
        "debug_captures",
        "classifier_usage",
    ] {
        assert_eq!(env.count(table).await, 0, "{table}");
    }
    // The declared form still works for the same tenant.
    let (status, _) = env
        .recommend(r#"{"workload":{"capability":"structured_extraction"}}"#)
        .await;
    assert_eq!(status, StatusCode::OK);
}

/// DIVERGENCE: D4. Past the tenant's quota the response carries the fallback and, last,
/// `classification_quota_exceeded: true`.
#[tokio::test]
async fn a_spent_quota_is_flagged_on_the_response() {
    let d4 = parity_support::divergence!("D4");
    assert_eq!(d4.id(), "D4");
    let Some(env) = env(false).await else { return };
    tenancy::set_tenant_classification_quota(&env.pool, TENANT, Some(0))
        .await
        .unwrap();
    let (status, text) = env.recommend(&prompt_body(CONTRACT)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        text.ends_with(
            "\"zero_retention\":false,\"telemetry\":true,\"classification_quota_exceeded\":true}\n"
        ),
        "{text}"
    );
    assert_eq!(parse(&text)["classification"]["method"], "fallback_default");
    assert_eq!(env.classifier.calls(), 0);
}

/// DIVERGENCE: D5. The prompt form takes `workload.projection_version`, which keys the
/// cache; the declared form refuses it.
#[tokio::test]
async fn the_projection_version_is_accepted_on_the_prompt_form_only() {
    let d5 = parity_support::divergence!("D5");
    assert_eq!(d5.id(), "D5");
    let Some(env) = env(false).await else { return };
    let with_p1 = serde_json::json!({"workload": {"prompt": CONTRACT, "projection_version": "p1"}})
        .to_string();
    let (status, text) = env.recommend(&with_p1).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(parse(&text)["classification"]["method"], "classified");
    let (_, text) = env.recommend(&prompt_body(CONTRACT)).await;
    assert_eq!(
        parse(&text)["classification"]["method"],
        "classified",
        "another cache entry"
    );
    let (_, text) = env.recommend(&with_p1).await;
    assert_eq!(parse(&text)["classification"]["method"], "cached");
    let stored: String = sqlx::query_scalar(
        "select request::text from recommendations order by created_at desc, id desc limit 1",
    )
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert_eq!(parse(&stored)["workload"]["projection_version"], "p1");

    let (status, text) = env
        .recommend(r#"{"workload":{"capability":"factual_qa","projection_version":"p1"}}"#)
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        parse(&text)["error"]["details"]["field"],
        "workload.projection_version"
    );
}
