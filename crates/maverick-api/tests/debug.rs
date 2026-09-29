//! The debug-capture endpoints (beads 5.9 and 5.11; reference Part 4 §4.6, Part 5 §6.14
//! to §6.18, §7.11, §7.12, §13.9 to §13.11), on real Postgres with a scripted classifier:
//! windows, replay, the tenant and operator capture lists, prompt reads, and the access
//! log. Error bodies are the recorded Go bytes (`parity/blackbox/api-recorded.jsonl`);
//! success bodies are checked field by field because their ids and times differ per run.

use std::collections::VecDeque;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use maverick_api::capture::Capturer;
use maverick_api::classify::{Classifier, Resolver, ResolverSettings};
use maverick_api::{
    ApiState, AuthState, HealthState, Identity, RateLimiter, RecommendConfig, StaticAuthBackend,
    internal_router_with, public_router_with,
};
use maverick_domain::capture::KeyRing;
use maverick_domain::gojson::{self, JsonValue};
use maverick_domain::keys;
use maverick_providers::{
    CompletionRequest, CompletionResponse, ProviderClient, ProviderError, ProviderErrorKind,
    ProviderFuture,
};
use maverick_store::capture as store;
use maverick_store::catalog::{self, NewModelColumn};
use maverick_store::measurement::{self, AggregateUpsert};
use maverick_store::{JsonRawValue, MIGRATOR, corpus, tenancy};
use maverick_testdb::{Schema, TestDatabase};
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

const OWNER: &str = "ten_owner";
const KEY: &str = "key_owner";
const KEY2: &str = "key_owner_two";
const INTRUDER: &str = "ten_intruder";
const INTRUDER_KEY: &str = "key_intruder";
/// The internal token `<label>:<secret>`; the access-log actor is the label.
const OPS: &str = "ops:alpha";
const CORPUS: &str = "2026-07-01";
const MASTER_KEY: [u8; 32] = [0xC4; 32];
const COLUMN: &str = "claude-haiku-4-5/direct";
const ZR_COLUMN: &str = "claude-haiku-4-5/vertex";
const MARKER: &str = "CAPTURE-SECRET-MARKER-9917";
const PROMPT: &str = "Extract the parties, dates, and obligations from this contract: CAPTURE-SECRET-MARKER-9917 between Acme Corp and Bravo LLC, signed 2026-01-15.";
const LEGAL: &str = r#"{"capability":"structured_extraction","difficulty":3,"domain_tags":["legal"],"confidence":0.91}"#;
const CODE: &str =
    r#"{"capability":"code_generation","difficulty":4,"domain_tags":[],"confidence":0.88}"#;

const UNAUTHORIZED: &str = "{\"error\":{\"code\":\"unauthorized\",\"message\":\"missing or invalid API key\",\"details\":{}}}\n";
const NOT_FOUND: &str =
    "{\"error\":{\"code\":\"not_found\",\"message\":\"resource not found\",\"details\":{}}}\n";
const EXPIRED: &str = "{\"error\":{\"code\":\"capture_expired\",\"message\":\"capture has expired and its ciphertext is deleted or pending deletion\",\"details\":{}}}\n";
/// The recorded stored classification of [`LEGAL`] (jsonb key order).
const ORIGINAL: &str = r#"{"method":"classified","capability":"structured_extraction","confidence":0.91,"difficulty":3,"domain_tags":["legal"],"high_stakes":true}"#;
const REPLAYED_LEGAL: &str = r#"{"capability":"structured_extraction","difficulty":3,"domain_tags":["legal"],"high_stakes":true,"confidence":0.91,"method":"classified"}"#;

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A classifier provider that answers `then` forever (`None` fails).
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

/// One response.
#[derive(Debug)]
struct Reply {
    status: StatusCode,
    content_type: Option<String>,
    body: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.body))
    }
}

async fn send(router: Router, request: Request<Body>) -> Reply {
    let (method, uri) = (request.method().clone(), request.uri().clone());
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .map(|value| value.to_str().unwrap().to_owned());
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = String::from_utf8(bytes.to_vec()).unwrap();
    eprintln!("{method} {uri} -> {status} {body}");
    Reply {
        status,
        content_type,
        body,
    }
}

/// One test's database, classifiers, and capture hook.
struct Env {
    _db: TestDatabase,
    pool: PgPool,
    classifier: Scripted,
    zero_classifier: Scripted,
    config: RecommendConfig,
}

impl Env {
    fn router(&self, tenant: &str, key: &str) -> Router {
        let auth = AuthState::new(
            Arc::new(StaticAuthBackend::authenticated(Identity {
                tenant_id: tenant.to_owned(),
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

    /// A request as `tenant`/`key`; `None` sends no `Authorization` header.
    async fn call(
        &self,
        who: Option<(&str, &str)>,
        method: Method,
        uri: &str,
        body: &str,
    ) -> Reply {
        let mut request = Request::builder().method(method.clone()).uri(uri);
        if who.is_some() {
            request = request.header(header::AUTHORIZATION, "Bearer mk_live_test");
        }
        let (tenant, key) = who.unwrap_or((OWNER, KEY));
        send(
            self.router(tenant, key),
            request.body(Body::from(body.to_owned())).unwrap(),
        )
        .await
    }

    /// A `GET` on the internal listener with `token` (`None` sends no header).
    async fn operator(&self, uri: &str, token: Option<&str>) -> Reply {
        let auth = AuthState::new(
            Arc::new(StaticAuthBackend::authenticated(Identity {
                tenant_id: OWNER.to_owned(),
                api_key_id: KEY.to_owned(),
            })),
            keys::parse_internal_tokens(OPS).unwrap(),
            RateLimiter::new(10_000),
        );
        let health = HealthState::with_providers(self.pool.clone(), Vec::new());
        let api = ApiState::with_health(self.pool.clone(), health)
            .with_recommend_config(self.config.clone());
        let mut request = Request::builder().method(Method::GET).uri(uri);
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        send(
            internal_router_with(auth, api),
            request.body(Body::empty()).unwrap(),
        )
        .await
    }

    async fn owner(&self, method: Method, uri: &str, body: &str) -> Reply {
        self.call(Some((OWNER, KEY)), method, uri, body).await
    }

    async fn intruder(&self, method: Method, uri: &str, body: &str) -> Reply {
        self.call(Some((INTRUDER, INTRUDER_KEY)), method, uri, body)
            .await
    }

    /// Opens a 60-minute window through the API.
    async fn open_window(&self, tenant: &str, key: &str) -> String {
        let reply = self
            .call(
                Some((tenant, key)),
                Method::POST,
                "/v1/debug/capture-windows",
                r#"{"duration_minutes":60}"#,
            )
            .await;
        assert_eq!(reply.status, StatusCode::CREATED);
        reply.json()["id"].as_str().unwrap().to_owned()
    }

    /// A prompt recommend as `tenant`/`key`, and the capture it made (if any).
    async fn recommend(&self, tenant: &str, key: &str, prompt: &str) -> Option<Captured> {
        let body = serde_json::json!({"workload": {"prompt": prompt}}).to_string();
        let reply = self
            .call(Some((tenant, key)), Method::POST, "/v1/recommend", &body)
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        let recommendation_id = reply.json()["recommendation_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let row: Option<(String, Option<String>)> = sqlx::query_as(
            "select c.id, r.prompt_hash from debug_captures c
             join recommendations r on r.id = c.recommendation_id
             where c.recommendation_id = $1",
        )
        .bind(&recommendation_id)
        .fetch_optional(&self.pool)
        .await
        .unwrap();
        row.map(|(id, prompt_hash)| Captured {
            id,
            recommendation_id,
            prompt_hash: prompt_hash.unwrap(),
        })
    }

    /// A window and one captured prompt for the owner.
    async fn captured(&self) -> Captured {
        self.open_window(OWNER, KEY).await;
        self.recommend(OWNER, KEY, PROMPT).await.expect("captured")
    }

    async fn count(&self, table: &str) -> i64 {
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!("select count(*) from {table}")))
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    /// Every access-log row of a capture, in any tenant: (tenant, actor, action).
    async fn log(&self, capture_id: &str) -> Vec<(String, String, String)> {
        sqlx::query_as(
            "select tenant_id, actor, action from capture_access_log
             where capture_id = $1 order by id",
        )
        .bind(capture_id)
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }

    /// Makes a capture expire an hour ago.
    async fn expire(&self, capture_id: &str) {
        sqlx::query(
            "update debug_captures set expires_at = now() - interval '1 hour' where id = $1",
        )
        .bind(capture_id)
        .execute(&self.pool)
        .await
        .unwrap();
    }

    /// The `capture_expiry` job's work: hard delete plus a `delete` row per capture.
    async fn hard_delete(&self, capture_id: &str) {
        self.expire(capture_id).await;
        let deleted = store::delete_expired_debug_captures(&self.pool, Utc::now())
            .await
            .unwrap();
        assert_eq!(deleted.len(), 1);
        store::insert_capture_access_log(
            &self.pool,
            &maverick_domain::ids::new(maverick_domain::ids::prefix::CAPTURE_ACCESS),
            capture_id,
            &deleted[0].tenant_id,
            "capture_expiry",
            "delete",
        )
        .await
        .unwrap();
    }
}

#[derive(Debug, Clone)]
struct Captured {
    id: String,
    recommendation_id: String,
    prompt_hash: String,
}

fn error(code: &str, message: &str, details: &str) -> String {
    format!(
        "{{\"error\":{{\"code\":\"{code}\",\"message\":\"{message}\",\"details\":{details}}}}}\n"
    )
}

fn invalid(message: &str, details: &str) -> String {
    error("invalid_request", message, details)
}

fn time(value: &Value) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value.as_str().unwrap())
        .unwrap()
        .with_timezone(&Utc)
}

/// The member names of the object at `path` in the body, in wire order (serde_json's
/// `Value` sorts them). A number in `path` indexes an array.
fn keys_of(body: &str, path: &[&str]) -> Vec<String> {
    let mut value = gojson::unmarshal(body).unwrap();
    for step in path {
        value = match value {
            JsonValue::Object(members) => {
                members.into_iter().find(|(key, _)| key == step).unwrap().1
            }
            JsonValue::Array(items) => items.into_iter().nth(step.parse().unwrap()).unwrap(),
            other => panic!("no {step} in {other:?}"),
        };
    }
    let JsonValue::Object(members) = value else {
        panic!("not an object: {value:?}")
    };
    members.into_iter().map(|(key, _)| key).collect()
}

fn night() -> NaiveDate {
    Utc::now().date_naive()
}

async fn column(pool: &PgPool, id: &str, model: &str, endpoint: &str, zero: bool) {
    let empty = JsonRawValue::from_string("{}".to_owned()).unwrap();
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
            retention_valid_until: zero.then(|| Utc::now() + Duration::days(30)),
            retention_source: None,
            execution_alias_retention: None,
            param_constraints: None,
        },
    )
    .await
    .unwrap();
}

async fn aggregate(pool: &PgPool, capability: &str, tier: i16, dimension: &str, ewma: f32) {
    measurement::upsert_aggregate(
        pool,
        &AggregateUpsert {
            model_column_id: "col_sonnet",
            capability,
            difficulty_tier: tier,
            dimension,
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

/// Two tenants (the owner with two keys, the intruder with one), an active corpus, and
/// one pool column measured for the capabilities the classifier and the fallback name.
/// `zero` makes the owner zero-retention.
async fn env(zero: bool) -> Option<Env> {
    let db = maverick_testdb::database(&SCHEMA).await?;
    let pool = db.pool().await;
    for (tenant, tenant_keys) in [(OWNER, &[KEY, KEY2][..]), (INTRUDER, &[INTRUDER_KEY][..])] {
        tenancy::create_tenant(&pool, tenant, tenant, &[7; 32])
            .await
            .unwrap();
        for key in tenant_keys {
            tenancy::create_api_key(&pool, key, tenant, key.as_bytes(), "abcd", "")
                .await
                .unwrap();
        }
        tenancy::create_pool(
            &pool,
            &format!("pool_{tenant}"),
            tenant,
            "default",
            &["col_sonnet".to_owned()],
            true,
        )
        .await
        .unwrap_or_else(|e| panic!("pool: {e}"));
    }
    if zero {
        tenancy::upsert_tenant_constraints(&pool, OWNER, None, None, true, false)
            .await
            .unwrap();
    }
    corpus::upsert_corpus_version(&pool, CORPUS, "", 0, false)
        .await
        .unwrap();
    corpus::activate_corpus_version(&pool, CORPUS)
        .await
        .unwrap();
    column(&pool, "col_sonnet", "claude-sonnet-4-6", "direct", true).await;
    column(&pool, "col_haiku", "claude-haiku-4-5", "direct", false).await;
    column(&pool, "col_haiku_zr", "claude-haiku-4-5", "vertex", true).await;
    for capability in [
        "structured_extraction",
        "multi_step_reasoning",
        "code_generation",
    ] {
        for tier in 1..=5 {
            aggregate(&pool, capability, tier, "pass_rate", 0.95).await;
        }
        aggregate(&pool, capability, 3, "quality", 0.9).await;
        aggregate(&pool, capability, 3, "cost_per_run_usd", 0.01).await;
        aggregate(&pool, capability, 3, "latency_p95_ms", 900.0).await;
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

// ---- capture_test.go ----

// parity: api.TestCaptureEndpointsRequireAuth
#[tokio::test]
async fn window_and_replay_endpoints_require_a_key() {
    let Some(env) = env(false).await else { return };
    for (method, uri) in [
        (Method::POST, "/v1/debug/capture-windows"),
        (Method::DELETE, "/v1/debug/capture-windows/cw_x"),
        (Method::POST, "/v1/debug/replay/cap_x"),
    ] {
        let reply = env.call(None, method, uri, "").await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
        assert_eq!(reply.body, UNAUTHORIZED);
    }
}

// parity: api.TestCaptureWindowCreate201
#[tokio::test]
async fn a_window_opens_now_for_its_duration() {
    let Some(env) = env(false).await else { return };
    let before = Utc::now();
    let reply = env
        .owner(
            Method::POST,
            "/v1/debug/capture-windows",
            r#"{"duration_minutes":60}"#,
        )
        .await;
    assert_eq!(reply.status, StatusCode::CREATED);
    assert_eq!(reply.content_type.as_deref(), Some("application/json"));
    assert!(reply.body.ends_with("}\n"));
    let window = reply.json();
    assert_eq!(
        keys_of(&reply.body, &[]),
        [
            "id",
            "api_key_id",
            "confidence_below",
            "starts_at",
            "ends_at",
            "created_at"
        ]
    );
    assert!(window["id"].as_str().unwrap().starts_with("cw_"));
    assert!(window["api_key_id"].is_null() && window["confidence_below"].is_null());
    let starts = time(&window["starts_at"]);
    assert!((starts - before).num_milliseconds().abs() <= 1000);
    assert_eq!(time(&window["ends_at"]) - starts, Duration::minutes(60));
    assert_eq!(env.count("capture_windows").await, 1);
}

// parity: api.TestCaptureWindowCreateScopedFields
#[tokio::test]
async fn a_scoped_window_echoes_its_key_and_float32_bar() {
    let Some(env) = env(false).await else { return };
    let reply = env
        .owner(
            Method::POST,
            "/v1/debug/capture-windows",
            &format!(r#"{{"api_key_id":"{KEY2}","confidence_below":0.8,"duration_minutes":30}}"#),
        )
        .await;
    assert_eq!(reply.status, StatusCode::CREATED);
    // QUIRK: stored as float32 and echoed widened, as recorded.
    assert!(reply.body.contains(&format!(
        r#""api_key_id":"{KEY2}","confidence_below":0.800000011920929,"#
    )));
    let window = reply.json();
    assert_eq!(
        time(&window["ends_at"]) - time(&window["starts_at"]),
        Duration::minutes(30)
    );
}

// parity: api.TestCaptureWindowCreateValidation422
#[tokio::test]
async fn invalid_windows_are_422_in_the_reference_order() {
    let Some(env) = env(false).await else { return };
    let range = |d: &str| {
        invalid(
            "duration_minutes must be between 1 and 1440 (capture.max_window_minutes, spec §9.8)",
            &format!(r#"{{"duration_minutes":{d},"max_window_minutes":1440}}"#),
        )
    };
    let confidence = |c: &str| {
        invalid(
            "confidence_below must be a number in (0, 1]",
            &format!(r#"{{"confidence_below":{c}}}"#),
        )
    };
    let not_yours = |k: &str| {
        invalid(
            "api_key_id does not name one of your API keys",
            &format!(r#"{{"api_key_id":"{k}"}}"#),
        )
    };
    let cases = [
        (
            "{}".to_owned(),
            invalid(
                "duration_minutes is required",
                r#"{"field":"duration_minutes"}"#,
            ),
        ),
        (r#"{"duration_minutes":0}"#.to_owned(), range("0")),
        (r#"{"duration_minutes":-5}"#.to_owned(), range("-5")),
        (r#"{"duration_minutes":1441}"#.to_owned(), range("1441")),
        (
            r#"{"duration_minutes":60,"nope":true}"#.to_owned(),
            invalid(
                r#"malformed request body: json: unknown field \"nope\""#,
                "{}",
            ),
        ),
        (
            r#"{"confidence_below":0,"duration_minutes":60}"#.to_owned(),
            confidence("0"),
        ),
        (
            r#"{"confidence_below":1.5,"duration_minutes":60}"#.to_owned(),
            confidence("1.5"),
        ),
        (
            r#"{"api_key_id":"key_01UNKNOWN","duration_minutes":60}"#.to_owned(),
            not_yours("key_01UNKNOWN"),
        ),
        (
            format!(r#"{{"api_key_id":"{INTRUDER_KEY}","duration_minutes":60}}"#),
            not_yours(INTRUDER_KEY),
        ),
    ];
    for (body, want) in cases {
        let reply = env
            .owner(Method::POST, "/v1/debug/capture-windows", &body)
            .await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(reply.body, want, "{body}");
    }
    assert_eq!(env.count("capture_windows").await, 0);
    assert_eq!(env.count("debug_captures").await, 0);
}

// parity: api.TestCaptureWindowCreateZeroRetention409
#[tokio::test]
async fn a_zero_retention_tenant_cannot_open_a_window() {
    let Some(env) = env(true).await else { return };
    let reply = env
        .owner(
            Method::POST,
            "/v1/debug/capture-windows",
            r#"{"duration_minutes":60}"#,
        )
        .await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(
        reply.body,
        error(
            "zero_retention_conflict",
            "tenant is configured zero-retention; debug capture windows cannot be created (spec §9.8, G5)",
            "{}"
        )
    );
    assert_eq!(env.count("capture_windows").await, 0);
    // QUIRK Q8: the key check comes first, so an unknown key is still 422.
    let reply = env
        .owner(
            Method::POST,
            "/v1/debug/capture-windows",
            r#"{"api_key_id":"key_nope","duration_minutes":60}"#,
        )
        .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
}

// parity: api.TestCaptureWindowDeleteEndsEarly
#[tokio::test]
async fn deleting_a_window_ends_it_early() {
    let Some(env) = env(false).await else { return };
    let id = env.open_window(OWNER, KEY).await;
    let uri = format!("/v1/debug/capture-windows/{id}");
    let reply = env.owner(Method::DELETE, &uri, "").await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    assert_eq!(reply.body, "");
    assert_eq!(reply.content_type, None);
    assert!(env.recommend(OWNER, KEY, PROMPT).await.is_none());
    let reply = env.owner(Method::DELETE, &uri, "").await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.body, NOT_FOUND);
}

// parity: api.TestCaptureWindowDeleteCrossTenant404
#[tokio::test]
async fn another_tenants_window_cannot_be_deleted() {
    let Some(env) = env(false).await else { return };
    let id = env.open_window(OWNER, KEY).await;
    let reply = env
        .intruder(
            Method::DELETE,
            &format!("/v1/debug/capture-windows/{id}"),
            "",
        )
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.body, NOT_FOUND);
    assert_eq!(env.count("capture_windows").await, 1);
}

// parity: api.TestReplayMatch
#[tokio::test]
async fn a_replay_that_agrees_matches_and_is_logged() {
    let Some(env) = env(false).await else { return };
    let captured = env.captured().await;
    let calls = env.classifier.calls();
    let reply = env
        .owner(
            Method::POST,
            &format!("/v1/debug/replay/{}", captured.id),
            "{}",
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.body,
        format!(
            r#"{{"original_classification":{ORIGINAL},"replayed_classification":{REPLAYED_LEGAL},"match":true}}"#
        ) + "\n"
    );
    assert!(!reply.body.contains(MARKER));
    // The cache is bypassed: the classifier ran again.
    assert_eq!(env.classifier.calls(), calls + 1);
    assert_eq!(
        env.log(&captured.id).await,
        [(OWNER.to_owned(), KEY.to_owned(), "replay".to_owned())]
    );
}

// parity: api.TestReplayMismatch
#[tokio::test]
async fn a_replay_that_disagrees_does_not_match() {
    let Some(env) = env(false).await else { return };
    let captured = env.captured().await;
    env.classifier.answer(Some(CODE));
    let reply = env
        .owner(
            Method::POST,
            &format!("/v1/debug/replay/{}", captured.id),
            "",
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.body,
        format!(
            r#"{{"original_classification":{ORIGINAL},"replayed_classification":{{"capability":"code_generation","difficulty":4,"domain_tags":[],"high_stakes":false,"confidence":0.88,"method":"classified"}},"match":false}}"#
        ) + "\n"
    );
}

// parity: api.TestReplayClassifierFailureFallsBack
#[tokio::test]
async fn a_failed_replay_answers_the_fallback() {
    let Some(env) = env(false).await else { return };
    let captured = env.captured().await;
    env.classifier.answer(None);
    let reply = env
        .owner(
            Method::POST,
            &format!("/v1/debug/replay/{}", captured.id),
            "",
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.body,
        format!(
            r#"{{"original_classification":{ORIGINAL},"replayed_classification":{{"capability":"multi_step_reasoning","difficulty":3,"domain_tags":[],"high_stakes":false,"confidence":0,"method":"fallback_default"}},"match":false}}"#
        ) + "\n"
    );
}

// parity: api.TestReplayClassifierVersion
#[tokio::test]
async fn only_the_current_classifier_version_replays() {
    let Some(env) = env(false).await else { return };
    let captured = env.captured().await;
    let uri = format!("/v1/debug/replay/{}", captured.id);
    let reply = env
        .owner(
            Method::POST,
            &uri,
            r#"{"classifier_version":"claude-haiku-4-5/direct/v2"}"#,
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    let reply = env
        .owner(
            Method::POST,
            &uri,
            r#"{"classifier_version":"other-column/v0"}"#,
        )
        .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        reply.body,
        invalid(
            "only the current classifier_version can be replayed in v1",
            r#"{"classifier_version":"other-column/v0","current":"claude-haiku-4-5/direct/v2"}"#
        )
    );
    assert_eq!(env.log(&captured.id).await.len(), 1);
}

// parity: api.TestReplayNotFound404
#[tokio::test]
async fn replaying_an_unknown_capture_is_404() {
    let Some(env) = env(false).await else { return };
    let reply = env
        .owner(Method::POST, "/v1/debug/replay/cap_01UNKNOWN", "")
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.body, NOT_FOUND);
    assert_eq!(env.count("capture_access_log").await, 0);
}

// parity: api.TestReplayCrossTenant404
#[tokio::test]
async fn another_tenants_capture_does_not_replay() {
    let Some(env) = env(false).await else { return };
    let captured = env.captured().await;
    let calls = env.classifier.calls();
    let reply = env
        .intruder(
            Method::POST,
            &format!("/v1/debug/replay/{}", captured.id),
            "",
        )
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.body, NOT_FOUND);
    assert_eq!(env.classifier.calls(), calls);
    assert!(env.log(&captured.id).await.is_empty());
}

// parity: api.TestReplayExpiredButPresent410
#[tokio::test]
async fn an_expired_capture_does_not_replay_and_is_logged_read() {
    let Some(env) = env(false).await else { return };
    let captured = env.captured().await;
    env.expire(&captured.id).await;
    let reply = env
        .owner(
            Method::POST,
            &format!("/v1/debug/replay/{}", captured.id),
            "",
        )
        .await;
    assert_eq!(reply.status, StatusCode::GONE);
    assert_eq!(reply.body, EXPIRED);
    assert_eq!(
        env.log(&captured.id).await,
        [(OWNER.to_owned(), KEY.to_owned(), "read".to_owned())]
    );
}

// parity: api.TestReplayHardDeleted410
#[tokio::test]
async fn a_hard_deleted_capture_replays_410() {
    let Some(env) = env(false).await else { return };
    let captured = env.captured().await;
    env.hard_delete(&captured.id).await;
    let reply = env
        .owner(
            Method::POST,
            &format!("/v1/debug/replay/{}", captured.id),
            "",
        )
        .await;
    assert_eq!(reply.status, StatusCode::GONE);
    assert_eq!(reply.body, EXPIRED);
    assert_eq!(
        env.log(&captured.id).await.len(),
        1,
        "no row for the replay"
    );
}

// ---- capture_read_test.go ----

// parity: api.TestTenantCaptureReadEndpointsRequireAuth
#[tokio::test]
async fn capture_reads_require_a_key() {
    let Some(env) = env(false).await else { return };
    for uri in ["/v1/debug/captures", "/v1/debug/captures/cap_x/prompt"] {
        let reply = env.call(None, Method::GET, uri, "").await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
        assert_eq!(reply.body, UNAUTHORIZED);
    }
}

// parity: api.TestTenantListCapturesShape
#[tokio::test]
async fn the_capture_list_has_no_prompt_and_no_tenant() {
    let Some(env) = env(false).await else { return };
    let before = Utc::now();
    let window = env.open_window(OWNER, KEY).await;
    let captured = env.recommend(OWNER, KEY, PROMPT).await.unwrap();
    let reply = env.owner(Method::GET, "/v1/debug/captures", "").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(!reply.body.contains(MARKER) && !reply.body.contains(OWNER));
    let list = reply.json();
    assert_eq!(keys_of(&reply.body, &[]), ["captures", "next_cursor"]);
    assert!(list["next_cursor"].is_null());
    let entries = list["captures"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(
        keys_of(&reply.body, &["captures", "0"]),
        [
            "id",
            "window_id",
            "recommendation_id",
            "prompt_hash",
            "classification",
            "captured_at",
            "expires_at",
            "expired"
        ]
    );
    assert_eq!(entry["id"], captured.id.as_str());
    assert_eq!(entry["window_id"], window.as_str());
    assert_eq!(
        entry["recommendation_id"],
        captured.recommendation_id.as_str()
    );
    assert_eq!(entry["prompt_hash"], captured.prompt_hash.as_str());
    assert!(
        reply
            .body
            .contains(&format!(r#""classification":{ORIGINAL},"#))
    );
    assert_eq!(entry["expired"], false);
    assert!(time(&entry["captured_at"]) >= before - Duration::seconds(1));
    assert!(time(&entry["expires_at"]) > time(&entry["captured_at"]));
}

// parity: api.TestTenantListCapturesStrictlyScoped
#[tokio::test]
async fn each_tenant_lists_only_its_own_captures() {
    let Some(env) = env(false).await else { return };
    let owner = env.captured().await;
    let intruder_window = env.open_window(INTRUDER, INTRUDER_KEY).await;
    let intruder = env.recommend(INTRUDER, INTRUDER_KEY, PROMPT).await.unwrap();
    let ids = |reply: &Reply| -> Vec<String> {
        reply.json()["captures"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["id"].as_str().unwrap().to_owned())
            .collect()
    };
    let reply = env.owner(Method::GET, "/v1/debug/captures", "").await;
    assert_eq!(ids(&reply), [owner.id]);
    let reply = env.intruder(Method::GET, "/v1/debug/captures", "").await;
    assert_eq!(ids(&reply), [intruder.id]);
    let reply = env
        .owner(
            Method::GET,
            &format!("/v1/debug/captures?window_id={intruder_window}"),
            "",
        )
        .await;
    assert_eq!(reply.body, "{\"captures\":[],\"next_cursor\":null}\n");
    let reply = env
        .owner(
            Method::GET,
            &format!("/v1/debug/captures?tenant_id={INTRUDER}"),
            "",
        )
        .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        reply.body,
        invalid(
            r#"unknown query parameter \"tenant_id\""#,
            r#"{"parameter":"tenant_id"}"#
        )
    );
}

// parity: api.TestTenantListCapturesPagination
#[tokio::test]
async fn the_capture_list_pages_newest_first() {
    let Some(env) = env(false).await else { return };
    env.open_window(OWNER, KEY).await;
    let mut made = Vec::new();
    for suffix in [" A", " B", " C"] {
        made.push(
            env.recommend(OWNER, KEY, &format!("{PROMPT}{suffix}"))
                .await
                .unwrap()
                .id,
        );
    }
    let mut seen = Vec::new();
    let mut uri = "/v1/debug/captures?limit=1".to_owned();
    loop {
        let reply = env.owner(Method::GET, &uri, "").await;
        assert_eq!(reply.status, StatusCode::OK);
        let page = reply.json();
        let entries = page["captures"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        seen.push(entries[0]["id"].as_str().unwrap().to_owned());
        match page["next_cursor"].as_str() {
            Some(cursor) => uri = format!("/v1/debug/captures?limit=1&cursor={cursor}"),
            None => break,
        }
        assert!(seen.len() < 3, "a third page must not have a cursor");
    }
    made.reverse();
    assert_eq!(seen, made, "id descending");
    let reply = env
        .owner(Method::GET, "/v1/debug/captures?cursor=not%20a%20token", "")
        .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        reply.body,
        invalid(
            r#"cursor \"not a token\" must be an opaque pagination token"#,
            r#"{"parameter":"cursor","value":"not a token"}"#
        )
    );
    let reply = env
        .owner(Method::GET, "/v1/debug/captures?limit=201", "")
        .await;
    assert_eq!(
        reply.body,
        invalid(
            r#"limit \"201\" must be an integer between 1 and 200"#,
            r#"{"parameter":"limit","value":"201"}"#
        )
    );
}

// parity: api.TestTenantReadOwnPromptRoundTrip
#[tokio::test]
async fn a_tenant_reads_its_own_prompt_and_the_read_is_logged() {
    let Some(env) = env(false).await else { return };
    let captured = env.captured().await;
    let reply = env
        .owner(
            Method::GET,
            &format!("/v1/debug/captures/{}/prompt", captured.id),
            "",
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    let read = reply.json();
    assert_eq!(
        keys_of(&reply.body, &[]),
        [
            "id",
            "prompt",
            "prompt_hash",
            "recommendation_id",
            "classification",
            "captured_at",
            "expires_at"
        ]
    );
    assert_eq!(read["id"], captured.id.as_str());
    assert_eq!(read["prompt"], PROMPT);
    assert_eq!(read["prompt_hash"], captured.prompt_hash.as_str());
    assert_eq!(
        read["recommendation_id"],
        captured.recommendation_id.as_str()
    );
    assert!(
        reply
            .body
            .contains(&format!(r#""classification":{ORIGINAL},"#))
    );
    assert_eq!(
        env.log(&captured.id).await,
        [(OWNER.to_owned(), KEY.to_owned(), "read".to_owned())]
    );
}

// parity: api.TestTenantReadCrossTenant404IndistinguishableAndLogged
#[tokio::test]
async fn foreign_and_unknown_reads_are_the_same_404_and_logged_for_the_prober() {
    let Some(env) = env(false).await else { return };
    let captured = env.captured().await;
    let unknown = "cap_01UNKNOWNTENANTREAD000000";
    let mut bodies = Vec::new();
    for id in [captured.id.as_str(), unknown] {
        let reply = env
            .intruder(Method::GET, &format!("/v1/debug/captures/{id}/prompt"), "")
            .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        assert!(!reply.body.contains(MARKER));
        bodies.push(reply.body);
        assert_eq!(
            env.log(id).await,
            [(
                INTRUDER.to_owned(),
                INTRUDER_KEY.to_owned(),
                "read".to_owned()
            )]
        );
    }
    assert_eq!(bodies, [NOT_FOUND, NOT_FOUND]);
    let owner_rows = store::list_capture_access_log_by_capture(&env.pool, &captured.id, OWNER)
        .await
        .unwrap();
    assert!(owner_rows.is_empty());
}

// parity: api.TestTenantReadExpiredButPresent410
#[tokio::test]
async fn an_expired_prompt_read_is_410_and_logged() {
    let Some(env) = env(false).await else { return };
    let captured = env.captured().await;
    env.expire(&captured.id).await;
    let reply = env
        .owner(
            Method::GET,
            &format!("/v1/debug/captures/{}/prompt", captured.id),
            "",
        )
        .await;
    assert_eq!(reply.status, StatusCode::GONE);
    assert_eq!(reply.body, EXPIRED);
    assert_eq!(
        env.log(&captured.id).await,
        [(OWNER.to_owned(), KEY.to_owned(), "read".to_owned())]
    );
}

// parity: api.TestTenantReadHardDeleted410
#[tokio::test]
async fn a_hard_deleted_prompt_read_is_410_and_logged() {
    let Some(env) = env(false).await else { return };
    let captured = env.captured().await;
    env.hard_delete(&captured.id).await;
    let reply = env
        .owner(
            Method::GET,
            &format!("/v1/debug/captures/{}/prompt", captured.id),
            "",
        )
        .await;
    assert_eq!(reply.status, StatusCode::GONE);
    assert_eq!(reply.body, EXPIRED);
    assert_eq!(
        env.log(&captured.id).await,
        [
            (
                OWNER.to_owned(),
                "capture_expiry".to_owned(),
                "delete".to_owned()
            ),
            (OWNER.to_owned(), KEY.to_owned(), "read".to_owned()),
        ]
    );
}

// parity: api.TestTenantCaptureReadZeroRetention
#[tokio::test]
async fn a_zero_retention_tenant_has_no_captures() {
    let Some(env) = env(true).await else { return };
    let reply = env.owner(Method::GET, "/v1/debug/captures", "").await;
    assert_eq!(reply.body, "{\"captures\":[],\"next_cursor\":null}\n");
    let reply = env
        .owner(
            Method::GET,
            "/v1/debug/captures/cap_01NEVEREXISTED0000000000/prompt",
            "",
        )
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.body, NOT_FOUND);
}

// ---- internalv1_captures_test.go ----

const INTERNAL_UNAUTHORIZED: &str = "{\"error\":{\"code\":\"unauthorized\",\"message\":\"missing or invalid internal token\",\"details\":{}}}\n";

// parity: api.TestOperatorCaptureEndpointsRequireAuth
#[tokio::test]
async fn operator_capture_reads_require_the_internal_token() {
    let Some(env) = env(false).await else { return };
    for uri in [
        "/internal/v1/captures",
        "/internal/v1/captures/cap_x/prompt",
    ] {
        for token in [None, Some("ops:wrong")] {
            let reply = env.operator(uri, token).await;
            assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{uri} {token:?}");
            assert_eq!(reply.body, INTERNAL_UNAUTHORIZED);
        }
    }
}

// parity: api.TestOperatorListCapturesShape
#[tokio::test]
async fn the_operator_list_names_the_tenant_and_never_the_prompt() {
    let Some(env) = env(false).await else { return };
    let captured = env.captured().await;
    let reply = env
        .operator(
            &format!("/internal/v1/captures?tenant_id={OWNER}"),
            Some(OPS),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(!reply.body.contains(MARKER));
    assert_eq!(
        keys_of(&reply.body, &["captures", "0"]),
        [
            "id",
            "window_id",
            "tenant_id",
            "recommendation_id",
            "prompt_hash",
            "classification",
            "captured_at",
            "expires_at",
            "expired"
        ]
    );
    let list = reply.json();
    let entries = list["captures"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["id"], captured.id.as_str());
    assert_eq!(entries[0]["tenant_id"], OWNER);
    assert!(entries[0]["window_id"].as_str().unwrap().starts_with("cw_"));
    assert_eq!(entries[0]["prompt_hash"], captured.prompt_hash.as_str());
    assert!(
        reply
            .body
            .contains(&format!(r#""classification":{ORIGINAL},"#))
    );
    assert_eq!(entries[0]["expired"], false);
    assert!(list["next_cursor"].is_null());
    // The list is not access-logged.
    assert_eq!(env.count("capture_access_log").await, 0);
}

// parity: api.TestOperatorListCapturesFilters
#[tokio::test]
async fn the_operator_list_filters_by_tenant_and_window() {
    let Some(env) = env(false).await else { return };
    let owner = env.captured().await;
    let intruder_window = env.open_window(INTRUDER, INTRUDER_KEY).await;
    let intruder = env.recommend(INTRUDER, INTRUDER_KEY, PROMPT).await.unwrap();
    let ids = |reply: &Reply| -> Vec<String> {
        reply.json()["captures"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["id"].as_str().unwrap().to_owned())
            .collect()
    };
    let reply = env.operator("/internal/v1/captures", Some(OPS)).await;
    assert_eq!(ids(&reply), [intruder.id.clone(), owner.id.clone()]);
    let reply = env
        .operator(
            &format!("/internal/v1/captures?tenant_id={OWNER}"),
            Some(OPS),
        )
        .await;
    assert_eq!(ids(&reply), [owner.id]);
    let reply = env
        .operator(
            &format!("/internal/v1/captures?window_id={intruder_window}"),
            Some(OPS),
        )
        .await;
    assert_eq!(ids(&reply), [intruder.id]);
    let reply = env
        .operator("/internal/v1/captures?bogus=1", Some(OPS))
        .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        reply.body,
        invalid(
            r#"unknown query parameter \"bogus\""#,
            r#"{"parameter":"bogus"}"#
        )
    );
}

// parity: api.TestOperatorListCapturesPagination
#[tokio::test]
async fn the_operator_list_pages() {
    let Some(env) = env(false).await else { return };
    env.open_window(OWNER, KEY).await;
    let mut made = Vec::new();
    for suffix in [" A", " B", " C"] {
        made.push(
            env.recommend(OWNER, KEY, &format!("{PROMPT}{suffix}"))
                .await
                .unwrap()
                .id,
        );
    }
    let mut seen = Vec::new();
    let mut uri = format!("/internal/v1/captures?tenant_id={OWNER}&limit=1");
    loop {
        let reply = env.operator(&uri, Some(OPS)).await;
        assert_eq!(reply.status, StatusCode::OK);
        let page = reply.json();
        assert_eq!(page["captures"].as_array().unwrap().len(), 1);
        seen.push(page["captures"][0]["id"].as_str().unwrap().to_owned());
        match page["next_cursor"].as_str() {
            Some(cursor) => {
                uri = format!("/internal/v1/captures?tenant_id={OWNER}&limit=1&cursor={cursor}");
            }
            None => break,
        }
        assert!(seen.len() < 3, "a third page must not have a cursor");
    }
    made.reverse();
    assert_eq!(seen, made);
    let reply = env
        .operator(
            "/internal/v1/captures?cursor=not-valid-base64%20%20",
            Some(OPS),
        )
        .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        reply.body,
        invalid(
            r#"cursor \"not-valid-base64  \" must be an opaque pagination token"#,
            r#"{"parameter":"cursor","value":"not-valid-base64  "}"#
        )
    );
}

// parity: api.TestOperatorReadCapturePromptRoundTrip
#[tokio::test]
async fn the_operator_reads_any_prompt_and_the_read_is_logged() {
    let Some(env) = env(false).await else { return };
    let captured = env.captured().await;
    let reply = env
        .operator(
            &format!("/internal/v1/captures/{}/prompt", captured.id),
            Some(OPS),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        keys_of(&reply.body, &[]),
        [
            "id",
            "tenant_id",
            "prompt",
            "prompt_hash",
            "recommendation_id",
            "classification",
            "captured_at",
            "expires_at"
        ]
    );
    let read = reply.json();
    assert_eq!(read["id"], captured.id.as_str());
    assert_eq!(read["tenant_id"], OWNER);
    assert_eq!(read["prompt"], PROMPT);
    assert_eq!(read["prompt_hash"], captured.prompt_hash.as_str());
    assert_eq!(
        read["recommendation_id"],
        captured.recommendation_id.as_str()
    );
    assert!(
        reply
            .body
            .contains(&format!(r#""classification":{ORIGINAL},"#))
    );
    assert_eq!(
        env.log(&captured.id).await,
        [(OWNER.to_owned(), "ops".to_owned(), "read".to_owned())]
    );
}

// parity: api.TestOperatorReadCapturePromptExpiredButPresent410
#[tokio::test]
async fn an_expired_operator_read_is_410_and_logged() {
    let Some(env) = env(false).await else { return };
    let captured = env.captured().await;
    env.expire(&captured.id).await;
    let reply = env
        .operator(
            &format!("/internal/v1/captures/{}/prompt", captured.id),
            Some(OPS),
        )
        .await;
    assert_eq!(reply.status, StatusCode::GONE);
    assert_eq!(reply.body, EXPIRED);
    assert_eq!(
        env.log(&captured.id).await,
        [(OWNER.to_owned(), "ops".to_owned(), "read".to_owned())]
    );
}

// parity: api.TestOperatorReadCapturePromptHardDeleted410
#[tokio::test]
async fn a_hard_deleted_operator_read_is_410_under_the_captures_tenant() {
    let Some(env) = env(false).await else { return };
    let captured = env.captured().await;
    env.hard_delete(&captured.id).await;
    let reply = env
        .operator(
            &format!("/internal/v1/captures/{}/prompt", captured.id),
            Some(OPS),
        )
        .await;
    assert_eq!(reply.status, StatusCode::GONE);
    assert_eq!(reply.body, EXPIRED);
    assert_eq!(
        env.log(&captured.id).await,
        [
            (
                OWNER.to_owned(),
                "capture_expiry".to_owned(),
                "delete".to_owned()
            ),
            (OWNER.to_owned(), "ops".to_owned(), "read".to_owned()),
        ]
    );
}

// parity: api.TestOperatorReadCapturePromptUnknown404
#[tokio::test]
async fn an_unknown_operator_read_is_404_and_logged_without_a_tenant() {
    let Some(env) = env(false).await else { return };
    let id = "cap_01UNKNOWNOPERATORREAD0000";
    let reply = env
        .operator(&format!("/internal/v1/captures/{id}/prompt"), Some(OPS))
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.body, NOT_FOUND);
    assert_eq!(
        env.log(id).await,
        [(String::new(), "ops".to_owned(), "read".to_owned())]
    );
}

// parity: api.TestOperatorListZeroRetentionTenantHasNoCaptures
#[tokio::test]
async fn the_operator_sees_no_captures_for_a_zero_retention_tenant() {
    let Some(env) = env(true).await else { return };
    // The window endpoint refuses, and a prompt recommend never captures.
    let reply = env
        .owner(
            Method::POST,
            "/v1/debug/capture-windows",
            r#"{"duration_minutes":60}"#,
        )
        .await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    let reply = env
        .operator(
            &format!("/internal/v1/captures?tenant_id={OWNER}"),
            Some(OPS),
        )
        .await;
    assert_eq!(reply.body, "{\"captures\":[],\"next_cursor\":null}\n");
}

// parity: api.TestPublicListenerNeverLeaksForeignPromptText
#[tokio::test]
async fn the_public_listener_never_leaks_a_foreign_prompt() {
    let Some(env) = env(false).await else { return };
    let captured = env.captured().await;
    for uri in [
        format!("/v1/debug/captures/{}/prompt", captured.id),
        format!("/v1/debug/capture-windows/{}/prompt", captured.id),
    ] {
        let reply = env.intruder(Method::GET, &uri, "").await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        assert_eq!(reply.body, NOT_FOUND);
    }
    let reply = env.intruder(Method::GET, "/v1/debug/captures", "").await;
    assert_eq!(reply.body, "{\"captures\":[],\"next_cursor\":null}\n");
    let reply = env.owner(Method::GET, "/v1/debug/captures", "").await;
    assert!(reply.body.contains(&captured.id) && !reply.body.contains(MARKER));
    let reply = env
        .owner(
            Method::POST,
            &format!("/v1/debug/replay/{}", captured.id),
            "",
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(!reply.body.contains(MARKER));
}

// ---- Rust-only rules ----

/// Wrong methods and the routes' neighbors are the plain 404.
#[tokio::test]
async fn other_methods_are_404() {
    let Some(env) = env(false).await else { return };
    for (method, uri) in [
        (Method::GET, "/v1/debug/capture-windows"),
        (Method::PUT, "/v1/debug/capture-windows/cw_x"),
        (Method::GET, "/v1/debug/replay/cap_x"),
        (Method::POST, "/v1/debug/captures"),
        (Method::DELETE, "/v1/debug/captures/cap_x/prompt"),
    ] {
        let reply = env.owner(method, uri, "").await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(reply.body, NOT_FOUND);
    }
}

/// Opening and ending a window drop the tenant's cached windows, so the next prompt
/// sees the change even inside the 10 s cache lifetime.
#[tokio::test]
async fn window_changes_reach_the_cached_capture_decision() {
    let Some(mut env) = env(false).await else {
        return;
    };
    env.config.capture = Some(Arc::new(Capturer::new(
        KeyRing::new(1, MASTER_KEY, None),
        72,
    )));
    assert!(
        env.recommend(OWNER, KEY, PROMPT).await.is_none(),
        "caches no windows"
    );
    let window = env.open_window(OWNER, KEY).await;
    assert!(env.recommend(OWNER, KEY, PROMPT).await.is_some());
    let reply = env
        .owner(
            Method::DELETE,
            &format!("/v1/debug/capture-windows/{window}"),
            "",
        )
        .await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    assert!(env.recommend(OWNER, KEY, PROMPT).await.is_none());
    // Ending the window keeps its capture, with no window.
    let window_ids: Vec<Option<String>> =
        sqlx::query_scalar("select window_id from debug_captures")
            .fetch_all(&env.pool)
            .await
            .unwrap();
    assert_eq!(window_ids, [None]);
}

/// D18: a capture sealed before a key rotation opens with the previous key.
#[tokio::test]
async fn a_capture_opens_with_the_key_of_its_stored_version() {
    let Some(mut env) = env(false).await else {
        return;
    };
    let captured = env.captured().await;
    let uri = format!("/v1/debug/captures/{}/prompt", captured.id);
    let rotated = KeyRing::new(2, [0x5A; 32], Some((1, MASTER_KEY)));
    env.config.capture = Some(Arc::new(Capturer::new(rotated, 72)));
    let reply = env.owner(Method::GET, &uri, "").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json()["prompt"], PROMPT);
    // Without the previous key the capture cannot be opened, and nothing leaks.
    env.config.capture = Some(Arc::new(Capturer::new(
        KeyRing::new(2, [0x5A; 32], None),
        72,
    )));
    let reply = env.owner(Method::GET, &uri, "").await;
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(!reply.body.contains(MARKER));
}

/// The zero-retention rule holds on replay: a capture made before the tenant turned
/// zero-retention on is re-classified only by the zero-retention column (D2), and not at
/// all when that column has no valid evidence.
#[tokio::test]
async fn a_replay_for_a_zero_retention_tenant_never_reaches_an_unverified_column() {
    let Some(mut env) = env(false).await else {
        return;
    };
    let captured = env.captured().await;
    tenancy::upsert_tenant_constraints(&env.pool, OWNER, None, None, true, false)
        .await
        .unwrap();
    let uri = format!("/v1/debug/replay/{}", captured.id);
    let calls = env.classifier.calls();
    let reply = env.owner(Method::POST, &uri, "").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(
        reply
            .body
            .contains(&format!(r#""replayed_classification":{REPLAYED_LEGAL}"#))
    );
    assert_eq!(
        env.classifier.calls(),
        calls,
        "the default column is never called"
    );
    assert_eq!(env.zero_classifier.calls(), 1);

    // No zero-retention column: the fallback, and no call at all.
    env.config.classifier = Arc::new(
        Resolver::new(ResolverSettings {
            column: COLUMN.to_owned(),
            ..ResolverSettings::default()
        })
        .with_classifier(Classifier::new(Arc::new(env.classifier.clone()), COLUMN)),
    );
    let reply = env.owner(Method::POST, &uri, "").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains(r#""method":"fallback_default"}"#));
    assert_eq!(env.classifier.calls(), calls);
    assert_eq!(env.zero_classifier.calls(), 1);
}

/// A tenant's windows scope captures to its own keys, and a replay by a second key of
/// the same tenant is logged under that key.
#[tokio::test]
async fn access_rows_name_the_calling_key() {
    let Some(env) = env(false).await else { return };
    let captured = env.captured().await;
    let reply = env
        .call(
            Some((OWNER, KEY2)),
            Method::POST,
            &format!("/v1/debug/replay/{}", captured.id),
            "",
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        env.log(&captured.id).await,
        [(OWNER.to_owned(), KEY2.to_owned(), "replay".to_owned())]
    );
}
