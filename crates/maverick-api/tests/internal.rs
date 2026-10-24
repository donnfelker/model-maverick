//! The internal provisioning seam on real Postgres (reference Part 5 §7 and §13.12; spec
//! §13; D4, D15). Error bodies are compared byte for byte with the recorded Go
//! exchanges in `parity/blackbox/api-recorded.jsonl`. Success bodies are checked field
//! by field, because the recordings ran against the full seed catalog (bead 5.15 replays
//! them whole).

use std::sync::{Arc, LazyLock, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use chrono::{DateTime, Duration, TimeZone, Utc};
use http_body_util::BodyExt;
use maverick_alerts::AlertNotifier;
use maverick_api::{
    ApiState, AuthState, HealthState, InternalSettings, PgAuthBackend, RateLimiter,
    RecommendConfig, internal_router_with, public_router_with,
};
use maverick_domain::keys;
use maverick_store::alerts::Alert;
use maverick_store::catalog::{NewModelColumn, create_model_column, get_model_column};
use maverick_store::measurement::{AggregateUpsert, upsert_aggregate};
use maverick_store::{JsonRawValue, MIGRATOR, tenancy};
use maverick_testdb::Schema;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

const OPS: &str = "ops:alpha";
const RAILS: &str = "rails:beta";
const UNAUTHORIZED: &str = "{\"error\":{\"code\":\"unauthorized\",\"message\":\"missing or invalid internal token\",\"details\":{}}}\n";

/// A recorded exchange.
struct Recorded {
    method: String,
    path: String,
    body: String,
    status: u16,
    response: String,
}

fn recorded(test: &str) -> Vec<Recorded> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../parity/blackbox/api-recorded.jsonl"
    );
    std::fs::read_to_string(path)
        .expect("recordings")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("row"))
        .filter(|row| row["test"] == test)
        .map(|row| Recorded {
            method: row["request"]["method"].as_str().unwrap().to_owned(),
            path: row["request"]["path"].as_str().unwrap().to_owned(),
            body: row["request"]["body"].as_str().unwrap().to_owned(),
            status: u16::try_from(row["response"]["status"].as_u64().unwrap()).unwrap(),
            response: row["response"]["body"].as_str().unwrap().to_owned(),
        })
        .collect()
}

/// The recorded response for `method path body` in `test`.
fn expected(test: &str, method: &str, path: &str, body: &str) -> (u16, String) {
    let row = recorded(test)
        .into_iter()
        .find(|r| r.method == method && r.path == path && r.body == body)
        .unwrap_or_else(|| panic!("no recorded {method} {path} {body} in {test}"));
    (row.status, row.response)
}

/// Records every alert it is told about.
#[derive(Default)]
struct Notified(Mutex<Vec<Alert>>);

impl AlertNotifier for Notified {
    fn notify(&self, alert: &Alert) {
        self.0.lock().unwrap().push(alert.clone());
    }
}

/// One event: its message and its fields in order.
type Line = (String, Vec<(String, String)>);

/// Collects every event.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<Line>>>);

struct Fields(String, Vec<(String, String)>);

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        } else {
            self.1.push((field.name().to_owned(), format!("{value:?}")));
        }
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.1.push((field.name().to_owned(), value.to_owned()));
    }
}

impl<S: Subscriber> Layer<S> for Captured {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut fields = Fields(String::new(), Vec::new());
        event.record(&mut fields);
        self.0.lock().unwrap().push((fields.0, fields.1));
    }
}

impl Captured {
    fn lines(&self, message: &str) -> Vec<Vec<(String, String)>> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, _)| m == message)
            .map(|(_, fields)| fields.clone())
            .collect()
    }

    fn audits(&self, action: &str) -> Vec<Vec<(String, String)>> {
        self.lines("internal provisioning write")
            .into_iter()
            .filter(|fields| fields.iter().any(|(k, v)| k == "action" && v == action))
            .collect()
    }
}

/// The keys appear in `text` in this order (a `Value` would sort them).
fn assert_key_order(text: &str, keys: &[&str]) {
    let positions: Vec<usize> = keys
        .iter()
        .map(|key| {
            text.find(&format!("\"{key}\":"))
                .unwrap_or_else(|| panic!("no {key} in {text}"))
        })
        .collect();
    assert!(positions.is_sorted(), "{keys:?} out of order in {text}");
}

fn field<'a>(fields: &'a [(String, String)], name: &str) -> Option<&'a str> {
    fields
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

struct Seam {
    internal: Router,
    public: Router,
    pool: PgPool,
    notified: Arc<Notified>,
}

fn clock() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 7, 6, 12, 0, 0).unwrap()
}

fn seam(pool: &PgPool) -> Seam {
    let notified = Arc::new(Notified::default());
    let auth = AuthState::new(
        Arc::new(PgAuthBackend::new(pool.clone())),
        keys::parse_internal_tokens(&format!("{OPS},{RAILS}")).unwrap(),
        RateLimiter::new(1000),
    );
    let recommend = RecommendConfig {
        alerts: Some(Arc::clone(&notified) as Arc<dyn AlertNotifier>),
        ..RecommendConfig::default()
    };
    let api = ApiState::with_health(pool.clone(), HealthState::detached())
        .with_recommend_config(recommend)
        .with_internal(InternalSettings {
            cold_start_nights: 5,
            default_classification_quota: 1000,
            clock: Arc::new(clock),
            ..InternalSettings::default()
        });
    Seam {
        internal: internal_router_with(auth.clone(), api.clone()),
        public: public_router_with(auth, api),
        pool: pool.clone(),
        notified,
    }
}

async fn send(
    router: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: &str,
) -> (u16, String) {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::from(body.to_owned())).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

impl Seam {
    async fn ops(&self, method: &str, path: &str, body: &str) -> (u16, String) {
        send(&self.internal, method, path, Some(OPS), body).await
    }

    async fn ops_json(&self, method: &str, path: &str, body: &str) -> (u16, Value) {
        let (status, text) = self.ops(method, path, body).await;
        assert!(text.ends_with('\n'), "{text}");
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    /// Replays a recorded request whose response carries no generated id or time.
    async fn replay(&self, test: &str, method: &str, path: &str, body: &str) {
        let (status, want) = expected(test, method, path, body);
        let (got_status, got) = self.ops(method, path, body).await;
        assert_eq!(
            (got_status, got.as_str()),
            (status, want.as_str()),
            "{method} {path} {body}"
        );
    }

    async fn column(&self, id: &str, status: &str) {
        column_with(&self.pool, id, status, "anthropic", "direct").await;
    }

    async fn create_tenant(&self, name: &str) -> String {
        let (status, body) = self
            .ops_json(
                "POST",
                "/internal/v1/tenants",
                &json!({ "name": name }).to_string(),
            )
            .await;
        assert_eq!(status, 201, "{body}");
        body["tenant"]["id"].as_str().unwrap().to_owned()
    }

    async fn issue_key(&self, tenant: &str) -> (String, String) {
        let (status, body) = self
            .ops_json(
                "POST",
                &format!("/internal/v1/tenants/{tenant}/api-keys"),
                r#"{"name":"primary"}"#,
            )
            .await;
        assert_eq!(status, 201);
        (
            body["id"].as_str().unwrap().to_owned(),
            body["key"].as_str().unwrap().to_owned(),
        )
    }

    async fn public(&self, method: &str, path: &str, key: &str, body: &str) -> (u16, String) {
        send(&self.public, method, path, Some(key), body).await
    }
}

async fn column_with(pool: &PgPool, id: &str, status: &str, provider: &str, endpoint: &str) {
    let empty = JsonRawValue::from_string("{}".to_owned()).unwrap();
    let model = format!("model-{id}");
    create_model_column(
        pool,
        &NewModelColumn {
            id,
            provider,
            model_id: &model,
            endpoint,
            settings: &empty,
            status,
            pricing: &empty,
            quality_source_column_id: None,
            cold_start_until: None,
            deprecation_date: None,
            execution_aliases: None,
            retention_mode: None,
            retention_verified_at: None,
            retention_valid_until: None,
            retention_source: None,
            execution_alias_retention: None,
            param_constraints: None,
        },
    )
    .await
    .unwrap();
}

macro_rules! db {
    () => {{
        let Some(db) = maverick_testdb::database(&SCHEMA).await else {
            return;
        };
        db
    }};
}

/// parity: api.TestInternalV1EveryEndpointRejectsMissingAndBadTokens
#[tokio::test]
async fn every_endpoint_rejects_missing_and_bad_tokens() {
    let db = db!();
    let pool = db.pool().await;
    let seam = seam(&pool);
    let endpoints = [
        ("POST", "/internal/v1/tenants"),
        ("GET", "/internal/v1/tenants/ten_x"),
        ("POST", "/internal/v1/tenants/ten_x/suspend"),
        ("POST", "/internal/v1/tenants/ten_x/activate"),
        ("POST", "/internal/v1/tenants/ten_x/api-keys"),
        ("DELETE", "/internal/v1/api-keys/key_x"),
        ("PUT", "/internal/v1/tenants/ten_x/pools"),
        ("PUT", "/internal/v1/tenants/ten_x/weights"),
        ("PUT", "/internal/v1/tenants/ten_x/constraints"),
        ("GET", "/internal/v1/alerts"),
        ("POST", "/internal/v1/columns"),
        ("PUT", "/internal/v1/columns/col_x"),
        ("POST", "/internal/v1/corpus/activate"),
        ("PUT", "/internal/v1/tenants/ten_x/limits"),
    ];
    for (method, path) in endpoints {
        for token in [None, Some("ops:wrong"), Some("garbage")] {
            let (status, body) = send(&seam.internal, method, path, token, "{}").await;
            assert_eq!(
                (status, body.as_str()),
                (401, UNAUTHORIZED),
                "{method} {path} {token:?}"
            );
        }
    }
}

/// parity: api.TestInternalV1EachRotationTokenAcceptedWithAuditActor
#[tokio::test]
async fn each_rotation_token_is_accepted_with_its_audit_actor() {
    let db = db!();
    let pool = db.pool().await;
    let seam = seam(&pool);
    let captured = Captured::default();
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(captured.clone()));
    for (token, actor) in [(OPS, "ops"), (RAILS, "rails")] {
        let (status, text) = send(
            &seam.internal,
            "POST",
            "/internal/v1/tenants",
            Some(token),
            r#"{"name":"Rotation"}"#,
        )
        .await;
        assert_eq!(status, 201, "{text}");
        let id = serde_json::from_str::<Value>(&text).unwrap()["tenant"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let audits = captured.audits("tenant.create");
        let last = audits.last().expect("audit line");
        assert_eq!(field(last, "actor"), Some(actor));
        assert_eq!(field(last, "tenant_id"), Some(id.as_str()));
        assert_eq!(field(last, "audit"), Some("true"));
    }
}

/// parity: api.TestInternalTenantCreateDefaults
#[tokio::test]
async fn tenant_create_defaults() {
    let db = db!();
    let pool = db.pool().await;
    let seam = seam(&pool);
    seam.column("col_a", "active").await;
    seam.column("col_b", "active").await;
    seam.column("col_cold", "cold_start").await;
    seam.column("col_retired", "retired").await;
    let captured = Captured::default();
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(captured.clone()));
    let (status, text) = seam
        .ops("POST", "/internal/v1/tenants", r#"{"name":"Acme Inc"}"#)
        .await;
    assert_eq!(status, 201);
    assert_key_order(
        &text,
        &["tenant", "pools", "weights", "constraints", "api_keys"],
    );
    assert_key_order(&text, &["id", "name", "status", "created_at", "updated_at"]);
    let body: Value = serde_json::from_str(&text).unwrap();
    let id = body["tenant"]["id"].as_str().unwrap();
    assert!(id.starts_with("ten_"));
    assert_eq!(body["tenant"]["name"], "Acme Inc");
    assert_eq!(body["tenant"]["status"], "active");
    assert_eq!(
        body["pools"],
        json!([{"name": "default", "model_column_ids": ["col_a", "col_b"], "is_default": true}])
    );
    assert_eq!(body["weights"], json!([]));
    assert_eq!(
        body["constraints"],
        json!({"max_cost_per_run": null, "max_latency_ms": null, "zero_retention": false,
               "telemetry_opt_out": false})
    );
    assert_eq!(body["api_keys"], json!([]));
    let tenant = tenancy::get_tenant(&pool, id).await.unwrap();
    assert_eq!(tenant.prompt_hmac_key.len(), 32);
    assert!(tenant.prompt_hmac_key.iter().any(|b| *b != 0));
    let audit = captured.audits("tenant.create");
    assert_eq!(field(&audit[0], "default_pool_columns"), Some("2"));
    assert_eq!(field(&audit[0], "name"), Some("Acme Inc"));
}

/// parity: api.TestInternalTenantCreateWithoutActiveColumnsHasNoPool
#[tokio::test]
async fn tenant_create_without_active_columns_has_no_pool() {
    let db = db!();
    let pool = db.pool().await;
    let seam = seam(&pool);
    seam.column("col_retired", "retired").await;
    let (status, body) = seam
        .ops_json("POST", "/internal/v1/tenants", r#"{"name":"Poolless"}"#)
        .await;
    assert_eq!(status, 201);
    assert_eq!(body["pools"], json!([]));
}

/// parity: api.TestInternalTenantCreateValidation
#[tokio::test]
async fn tenant_create_validation() {
    let db = db!();
    let pool = db.pool().await;
    let seam = seam(&pool);
    for body in ["", "{}", r#"{"name":""}"#, r#"{"name":"x","plan":"pro"}"#] {
        seam.replay(
            "TestInternalTenantCreateValidation",
            "POST",
            "/internal/v1/tenants",
            body,
        )
        .await;
    }
}

/// parity: api.TestInternalTenantGetSummary
#[tokio::test]
async fn tenant_get_summary() {
    let db = db!();
    let pool = db.pool().await;
    let seam = seam(&pool);
    seam.column("col_a", "active").await;
    let id = seam.create_tenant("Summary Tenant").await;
    let (status, body) = seam
        .ops_json("GET", &format!("/internal/v1/tenants/{id}"), "")
        .await;
    assert_eq!(status, 200);
    assert_eq!(body["tenant"]["name"], "Summary Tenant");
    assert_eq!(body["pools"][0]["name"], "default");
    seam.replay(
        "TestInternalTenantGetSummary",
        "GET",
        "/internal/v1/tenants/ten_00000000000000000000000000",
        "",
    )
    .await;
}

/// parity: api.TestInternalTenantSuspendActivateToggleReachesPublicListener
#[tokio::test]
async fn suspend_and_activate_reach_the_public_listener() {
    let db = db!();
    let pool = db.pool().await;
    let seam = seam(&pool);
    seam.column("col_a", "active").await;
    let id = seam.create_tenant("Suspend Me").await;
    let (_, key) = seam.issue_key(&id).await;
    let captured = Captured::default();
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(captured.clone()));
    assert_eq!(
        seam.public("GET", "/v1/config/pools", &key, "").await.0,
        200
    );
    let (status, body) = seam
        .ops_json("POST", &format!("/internal/v1/tenants/{id}/suspend"), "")
        .await;
    assert_eq!(
        (status, &body["tenant"]["status"]),
        (200, &json!("suspended"))
    );
    assert_eq!(
        seam.public("GET", "/v1/config/pools", &key, "").await.0,
        403
    );
    let (status, body) = seam
        .ops_json("POST", &format!("/internal/v1/tenants/{id}/activate"), "")
        .await;
    assert_eq!((status, &body["tenant"]["status"]), (200, &json!("active")));
    assert_eq!(
        seam.public("GET", "/v1/config/pools", &key, "").await.0,
        200
    );
    assert_eq!(captured.audits("tenant.suspend").len(), 1);
    assert_eq!(captured.audits("tenant.activate").len(), 1);
    for path in [
        "/internal/v1/tenants/ten_missing/suspend",
        "/internal/v1/tenants/ten_missing/activate",
    ] {
        seam.replay(
            "TestInternalTenantSuspendActivateToggleReachesPublicListener",
            "POST",
            path,
            "",
        )
        .await;
    }
}

/// parity: api.TestInternalAPIKeyRawReturnedExactlyOnce
#[tokio::test]
async fn api_key_raw_is_returned_exactly_once() {
    let db = db!();
    let pool = db.pool().await;
    let seam = seam(&pool);
    seam.column("col_a", "active").await;
    let id = seam.create_tenant("Key Tenant").await;
    let captured = Captured::default();
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(captured.clone()));
    let (status, body) = seam
        .ops_json(
            "POST",
            &format!("/internal/v1/tenants/{id}/api-keys"),
            r#"{"name":"rails-production"}"#,
        )
        .await;
    assert_eq!(status, 201);
    let key_id = body["id"].as_str().unwrap().to_owned();
    let raw = body["key"].as_str().unwrap().to_owned();
    assert!(key_id.starts_with("key_"));
    assert!(raw.starts_with("mk_live_") && raw.len() == 8 + 43);
    let issued = captured.audits("api_key.issue");
    assert_eq!(field(&issued[0], "api_key_id"), Some(key_id.as_str()));
    assert!(!format!("{issued:?}").contains(&raw[8..]));
    assert_eq!(
        seam.public("GET", "/v1/config/pools", &raw, "").await.0,
        200
    );

    let (_, summary) = seam
        .ops("GET", &format!("/internal/v1/tenants/{id}"), "")
        .await;
    assert!(!summary.contains(&raw[8..]));
    let summary: Value = serde_json::from_str(&summary).unwrap();
    let key = &summary["api_keys"][0];
    assert_eq!(key["name"], "rails-production");
    assert_eq!(key["status"], "active");
    assert_eq!(key["last4"], &raw[raw.len() - 4..]);
    assert_eq!(key["revoked_at"], Value::Null);

    let revoke = format!("/internal/v1/api-keys/{key_id}");
    let (status, first) = seam.ops("DELETE", &revoke, "").await;
    assert_eq!(status, 200);
    let revoked: Value = serde_json::from_str(&first).unwrap();
    assert_eq!(revoked["status"], "revoked");
    assert!(revoked["revoked_at"].is_string());
    assert!(!first.contains(&raw[8..]));
    assert_eq!(captured.audits("api_key.revoke").len(), 1);
    assert_eq!(
        seam.public("GET", "/v1/config/pools", &raw, "").await.0,
        401
    );
    let (status, second) = seam.ops("DELETE", &revoke, "").await;
    assert_eq!((status, second), (200, first));
    let test = "TestInternalAPIKeyRawReturnedExactlyOnce";
    seam.replay(test, "DELETE", "/internal/v1/api-keys/key_missing", "")
        .await;
    seam.replay(
        test,
        "POST",
        "/internal/v1/tenants/ten_missing/api-keys",
        r#"{"name":"x"}"#,
    )
    .await;
    let (status, body) = seam
        .ops("POST", &format!("/internal/v1/tenants/{id}/api-keys"), "{}")
        .await;
    assert_eq!(
        (status, body),
        expected(
            test,
            "POST",
            "/internal/v1/tenants/ten_01M3KQRCJM2WRNNWNYW981H0F6/api-keys",
            "{}"
        )
    );
}

/// parity: api.TestInternalConfigG4ParityByteExact
#[tokio::test]
async fn config_g4_parity_is_byte_exact() {
    let db = db!();
    let pool = db.pool().await;
    let seam = seam(&pool);
    seam.column("col_one", "active").await;
    seam.column("col_two", "active").await;
    let id = seam.create_tenant("Parity").await;
    let (_, key) = seam.issue_key(&id).await;
    // factual_qa tier 3 cost 5.0 on the default pool's first column only.
    tenancy::delete_pools_by_tenant(&pool, &id).await.unwrap();
    tenancy::create_pool(
        &pool,
        "pool_p",
        &id,
        "default",
        &["col_one".to_owned()],
        true,
    )
    .await
    .unwrap();
    upsert_aggregate(
        &pool,
        &AggregateUpsert {
            model_column_id: "col_one",
            capability: "factual_qa",
            difficulty_tier: 3,
            dimension: "cost_per_run_usd",
            ewma: 5.0,
            variance: 0.0,
            n: 3,
            measured: true,
            matrix_version: "2026-07-01",
            as_of: clock().date_naive(),
        },
    )
    .await
    .unwrap();
    let payloads = [
        (
            "pools",
            r#"{"pools":[{"name":"default","model_column_ids":["col_missing"],"is_default":true}]}"#,
        ),
        (
            "pools",
            r#"{"pools":[{"name":"default","model_column_ids":["col_one"],"is_default":false}]}"#,
        ),
        // D45: both seams refuse a pool name that the model grammar reserves.
        (
            "pools",
            r#"{"pools":[{"name":"auto","model_column_ids":["col_one"],"is_default":true}]}"#,
        ),
        (
            "weights",
            r#"{"weights":[{"capability":"factual_qa","w_quality":0.5,"w_cost":0.5,"w_latency":0.5,"quality_bar":0.8}]}"#,
        ),
        (
            "constraints",
            r#"{"constraints":{"max_cost_per_run":0.01,"max_latency_ms":null,"zero_retention":false}}"#,
        ),
    ];
    for (resource, payload) in payloads {
        let public = seam
            .public("PUT", &format!("/v1/config/{resource}"), &key, payload)
            .await;
        let internal = seam
            .ops(
                "PUT",
                &format!("/internal/v1/tenants/{id}/{resource}"),
                payload,
            )
            .await;
        assert_eq!(public, internal, "{resource} {payload}");
        assert_eq!(public.0, 422);
    }
    let captured = Captured::default();
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(captured.clone()));
    let (status, put) = seam
        .ops(
            "PUT",
            &format!("/internal/v1/tenants/{id}/pools"),
            r#"{"pools":[{"name":"default","model_column_ids":["col_one","col_two"],"is_default":true},{"name":"overflow","model_column_ids":["col_two"],"is_default":false}]}"#,
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(
        seam.public("GET", "/v1/config/pools", &key, "").await,
        (200, put)
    );
    assert_eq!(
        field(&captured.audits("tenant.pools.replace")[0], "pools"),
        Some("2")
    );
    let (status, _) = seam
        .ops(
            "PUT",
            &format!("/internal/v1/tenants/{id}/weights"),
            r#"{"weights":[{"capability":"factual_qa","w_quality":0.6,"w_cost":0.3,"w_latency":0.1,"quality_bar":0.85}]}"#,
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(
        field(&captured.audits("tenant.weights.replace")[0], "weights"),
        Some("1")
    );
    let (status, body) = seam
        .ops(
            "PUT",
            &format!("/internal/v1/tenants/{id}/constraints"),
            r#"{"constraints":{"max_cost_per_run":10,"max_latency_ms":60000,"zero_retention":false}}"#,
        )
        .await;
    assert_eq!(
        (status, body.as_str()),
        (
            200,
            "{\"constraints\":{\"max_cost_per_run\":10,\"max_latency_ms\":60000,\"zero_retention\":false,\"telemetry_opt_out\":false}}\n"
        )
    );
    assert_eq!(captured.audits("tenant.constraints.replace").len(), 1);
    // D39: the seam writes `telemetry_opt_out` through the same payload, the tenant reads
    // it back, and the same audit line covers it.
    let _d39 = parity_support::divergence!("D39");
    let opted_out = "{\"constraints\":{\"max_cost_per_run\":10,\"max_latency_ms\":60000,\"zero_retention\":false,\"telemetry_opt_out\":true}}\n";
    let (status, body) = seam
        .ops(
            "PUT",
            &format!("/internal/v1/tenants/{id}/constraints"),
            r#"{"constraints":{"max_cost_per_run":10,"max_latency_ms":60000,"telemetry_opt_out":true}}"#,
        )
        .await;
    assert_eq!((status, body.as_str()), (200, opted_out));
    assert_eq!(
        seam.public("GET", "/v1/config/constraints", &key, "").await,
        (200, opted_out.to_owned())
    );
    assert_eq!(captured.audits("tenant.constraints.replace").len(), 2);
    for resource in ["pools", "weights", "constraints"] {
        seam.replay(
            "TestInternalConfigG4ParityByteExact",
            "PUT",
            &format!("/internal/v1/tenants/ten_missing/{resource}"),
            "{}",
        )
        .await;
    }
}

/// parity: api.TestInternalColumnCreateEntersColdStart
#[tokio::test]
async fn column_create_enters_cold_start() {
    let db = db!();
    let pool = db.pool().await;
    let seam = seam(&pool);
    let test = "TestInternalColumnCreateEntersColdStart";
    let captured = Captured::default();
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(captured.clone()));
    let rows = recorded(test);
    let create = &rows[0];
    let (status, text) = seam.ops("POST", "/internal/v1/columns", &create.body).await;
    assert_eq!(status, 201, "{text}");
    assert_key_order(
        &text,
        &[
            "column_id",
            "provider",
            "model_id",
            "endpoint",
            "settings",
            "execution_aliases",
            "status",
            "pricing",
            "quality_source_column_id",
            "cold_start_until",
            "deprecation_date",
            "created_at",
            "updated_at",
        ],
    );
    let body: Value = serde_json::from_str(&text).unwrap();
    let id = body["column_id"].as_str().unwrap().to_owned();
    assert!(id.starts_with("col_"));
    assert_eq!(body["status"], "cold_start");
    assert_eq!(body["cold_start_until"], "2026-07-11");
    assert_eq!(body["settings"], json!({"thinking": true}));
    let want: Value = serde_json::from_str(&create.response).unwrap();
    assert_eq!(body["pricing"], want["pricing"]);
    assert_eq!(
        body.as_object().unwrap().len(),
        want.as_object().unwrap().len()
    );
    assert_eq!(
        field(&captured.audits("column.create")[0], "column_id"),
        Some(id.as_str())
    );
    // The duplicate and every invalid body answer the recorded 422 byte for byte.
    for row in rows.iter().filter(|r| r.status == 422) {
        let (status, body) = seam.ops("POST", "/internal/v1/columns", &row.body).await;
        assert_eq!(
            (status, body.as_str()),
            (422, row.response.as_str()),
            "{}",
            row.body
        );
    }
    // A second column naming the first as its quality source.
    let second = rows.last().unwrap();
    let body = second.body.replace("col_01M3KQRCK1Y5M2SXMDJPYEXSSM", &id);
    let (status, created) = seam.ops_json("POST", "/internal/v1/columns", &body).await;
    assert_eq!(status, 201);
    assert_eq!(created["quality_source_column_id"], id.as_str());
}

/// parity: api.TestInternalColumnUpdateStatusPricingDeprecation
#[tokio::test]
async fn column_update_status_pricing_deprecation() {
    let db = db!();
    let pool = db.pool().await;
    let seam = seam(&pool);
    let test = "TestInternalColumnUpdateStatusPricingDeprecation";
    seam.column("col_u", "cold_start").await;
    let (status, body) = seam
        .ops_json("PUT", "/internal/v1/columns/col_u", "{}")
        .await;
    assert_eq!(status, 200);
    assert_eq!(body["status"], "cold_start");
    assert_eq!(body["execution_aliases"], json!({}));
    let update = &recorded(test)[1];
    let (status, body) = seam
        .ops_json("PUT", "/internal/v1/columns/col_u", &update.body)
        .await;
    assert_eq!(status, 200);
    assert_eq!(body["status"], "deprecated");
    assert_eq!(body["pricing"]["input_per_mtok"], 2.5);
    assert_eq!(body["deprecation_date"], "2026-12-31");
    seam.replay(test, "PUT", "/internal/v1/columns/col_missing", "{}")
        .await;
    let (status, body) = seam
        .ops(
            "PUT",
            "/internal/v1/columns/col_u",
            r#"{"status":"sunset"}"#,
        )
        .await;
    assert_eq!(
        (status, body),
        expected(
            test,
            "PUT",
            "/internal/v1/columns/col_01M3KQRCK5PR806BZ88XTZT7XY",
            r#"{"status":"sunset"}"#
        )
    );
}

/// parity: api.TestInternalColumnExecutionAliasesCreateAndUpdate
#[tokio::test]
async fn column_execution_aliases_create_and_update() {
    let db = db!();
    let pool = db.pool().await;
    let seam = seam(&pool);
    let rows = recorded("TestInternalColumnExecutionAliasesCreateAndUpdate");
    let captured = Captured::default();
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(captured.clone()));
    let (status, body) = seam
        .ops_json("POST", "/internal/v1/columns", &rows[0].body)
        .await;
    assert_eq!(status, 201);
    assert_eq!(
        body["execution_aliases"],
        json!({"openrouter": "anthropic/claude-sonnet-4-6"})
    );
    let id = body["column_id"].as_str().unwrap().to_owned();
    let path = format!("/internal/v1/columns/{id}");
    let (status, body) = seam.ops_json("PUT", &path, &rows[1].body).await;
    assert_eq!(status, 200);
    assert_eq!(
        body["execution_aliases"],
        json!({"future_key": "kept", "openrouter": "anthropic/claude-opus-4-8"})
    );
    let changes = captured.lines("catalog_changes");
    assert_eq!(changes.len(), 1);
    assert_eq!(field(&changes[0], "change"), Some("execution_aliases"));
    assert_eq!(field(&changes[0], "column_id"), Some(id.as_str()));
    let (status, body) = seam.ops_json("PUT", &path, r#"{"status":"active"}"#).await;
    assert_eq!(status, 200);
    assert_eq!(body["execution_aliases"]["future_key"], "kept");
    let changes = captured.lines("catalog_changes");
    assert_eq!(changes.len(), 2);
    assert_eq!(field(&changes[1], "change"), Some("status"));
}

/// parity: api.TestInternalColumnRetireGuardBlocksThenAllowsAfterAmend
#[tokio::test]
async fn retire_guard_blocks_then_allows_after_amend() {
    let db = db!();
    let pool = db.pool().await;
    let seam = seam(&pool);
    seam.column("col_sole", "active").await;
    seam.column("col_replacement", "active").await;
    let id = seam.create_tenant("Solo").await;
    tenancy::delete_pools_by_tenant(&pool, &id).await.unwrap();
    tenancy::create_pool(
        &pool,
        "pool_solo",
        &id,
        "solo",
        &["col_sole".to_owned()],
        true,
    )
    .await
    .unwrap();
    let (status, body) = seam
        .ops(
            "PUT",
            "/internal/v1/columns/col_sole",
            r#"{"status":"retired"}"#,
        )
        .await;
    let (want_status, want) = expected(
        "TestInternalColumnRetireGuardBlocksThenAllowsAfterAmend",
        "PUT",
        "/internal/v1/columns/col_01M3KQRCKDKH0R403JGAVA1QJ6",
        r#"{"status":"retired"}"#,
    );
    let want = want
        .replace("col_01M3KQRCKDKH0R403JGAVA1QJ6", "col_sole")
        .replace("ten_01M3KQRCKBV3QYPB5Q733GC4P6", &id);
    assert_eq!((status, body.as_str()), (want_status, want.as_str()));
    assert_eq!(
        get_model_column(&pool, "col_sole").await.unwrap().status,
        "active"
    );
    let alerts = maverick_store::alerts::list_alerts_by_kind(&pool, "pool_would_be_empty")
        .await
        .unwrap();
    assert_eq!(alerts.len(), 1);
    assert_eq!(alerts[0].severity, "critical");
    let subject: Value = serde_json::from_str(alerts[0].subject.get()).unwrap();
    assert_eq!(subject["column_id"], "col_sole");
    let (status, _) = seam
        .ops(
            "PUT",
            &format!("/internal/v1/tenants/{id}/pools"),
            r#"{"pools":[{"name":"solo","model_column_ids":["col_sole","col_replacement"],"is_default":true}]}"#,
        )
        .await;
    assert_eq!(status, 200);
    let (status, body) = seam
        .ops_json(
            "PUT",
            "/internal/v1/columns/col_sole",
            r#"{"status":"retired"}"#,
        )
        .await;
    assert_eq!((status, &body["status"]), (200, &json!("retired")));
}

/// parity: api.TestInternalColumnRetireBlockDeliversAlert
#[tokio::test]
async fn retire_block_delivers_the_alert() {
    let db = db!();
    let pool = db.pool().await;
    let seam = seam(&pool);
    seam.column("col_sole", "active").await;
    let id = seam.create_tenant("Solo").await;
    tenancy::delete_pools_by_tenant(&pool, &id).await.unwrap();
    tenancy::create_pool(
        &pool,
        "pool_solo",
        &id,
        "solo",
        &["col_sole".to_owned()],
        true,
    )
    .await
    .unwrap();
    let (status, _) = seam
        .ops(
            "PUT",
            "/internal/v1/columns/col_sole",
            r#"{"status":"retired"}"#,
        )
        .await;
    assert_eq!(status, 422);
    let delivered = seam.notified.0.lock().unwrap();
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].kind, "pool_would_be_empty");
    assert!(delivered[0].id.starts_with("alr_"));
}

/// parity: api.TestInternalAlertsFeedFilters
#[tokio::test]
async fn alerts_feed_filters() {
    let db = db!();
    let pool = db.pool().await;
    let seam = seam(&pool);
    for (id, kind, severity, batch) in [
        ("alr_1", "spend_warning", "warning", "bat_feed_1"),
        ("alr_2", "job_failure", "critical", "bat_feed_2"),
    ] {
        let subject = JsonRawValue::from_string(json!({ "batch_id": batch }).to_string()).unwrap();
        maverick_store::alerts::insert_alert(&pool, id, kind, severity, &subject, "test alert")
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let (status, text) = seam.ops("GET", "/internal/v1/alerts", "").await;
    assert_eq!(status, 200);
    assert_key_order(
        &text,
        &[
            "id",
            "kind",
            "severity",
            "subject",
            "message",
            "created_at",
            "acked_at",
        ],
    );
    let all: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(all["alerts"].as_array().unwrap().len(), 2);
    let second_at = all["alerts"][1]["created_at"].as_str().unwrap().to_owned();
    let (_, spend) = seam
        .ops_json("GET", "/internal/v1/alerts?kind=spend_warning", "")
        .await;
    assert_eq!(spend["alerts"].as_array().unwrap().len(), 1);
    assert_eq!(spend["alerts"][0]["subject"]["batch_id"], "bat_feed_1");
    let (_, since) = seam
        .ops_json("GET", &format!("/internal/v1/alerts?since={second_at}"), "")
        .await;
    let ids: Vec<&str> = since["alerts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["alr_2"]);
    for path in [
        "/internal/v1/alerts?since=notatime",
        "/internal/v1/alerts?bogus=1",
    ] {
        seam.replay("TestInternalAlertsFeedFilters", "GET", path, "")
            .await;
    }
}

/// parity: api.TestInternalCorpusActivateFlipsActiveVersion
#[tokio::test]
async fn corpus_activate_flips_the_active_version() {
    let db = db!();
    let pool = db.pool().await;
    let seam = seam(&pool);
    for (version, description, count, active) in [
        ("int-corpus-a", "", 0, true),
        ("int-corpus-b", "next", 3, false),
    ] {
        sqlx::query(
            "insert into corpus_versions (version, description, task_count, active)
             values ($1, $2, $3, $4)",
        )
        .bind(version)
        .bind(description)
        .bind(count)
        .bind(active)
        .execute(&pool)
        .await
        .unwrap();
    }
    let captured = Captured::default();
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(captured.clone()));
    let (status, body) = seam
        .ops_json(
            "POST",
            "/internal/v1/corpus/activate",
            r#"{"version":"int-corpus-b"}"#,
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(body["version"], "int-corpus-b");
    assert_eq!(body["description"], "next");
    assert_eq!(body["task_count"], 3);
    assert_eq!(body["active"], true);
    let active: Vec<String> =
        sqlx::query_scalar("select version from corpus_versions where active")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(active, ["int-corpus-b"]);
    assert_eq!(
        field(&captured.audits("corpus.activate")[0], "corpus_version"),
        Some("int-corpus-b")
    );
    let test = "TestInternalCorpusActivateFlipsActiveVersion";
    seam.replay(
        test,
        "POST",
        "/internal/v1/corpus/activate",
        r#"{"version":"never-loaded"}"#,
    )
    .await;
    seam.replay(test, "POST", "/internal/v1/corpus/activate", "{}")
        .await;
}

/// parity: api.TestInternalV1UnknownPathAndMethodEnvelopes
#[tokio::test]
async fn unknown_paths_and_methods_are_404() {
    let db = db!();
    let pool = db.pool().await;
    let seam = seam(&pool);
    let test = "TestInternalV1UnknownPathAndMethodEnvelopes";
    seam.replay(test, "GET", "/internal/v1/nope", "").await;
    seam.replay(test, "DELETE", "/internal/v1/tenants", "")
        .await;
}

/// D4: the limits endpoint sets, keeps, and clears the classifier quota override and
/// echoes the effective value.
#[tokio::test]
async fn limits_set_keep_and_clear_the_quota_override() {
    let db = db!();
    let pool = db.pool().await;
    let seam = seam(&pool);
    let id = seam.create_tenant("Limits").await;
    let path = format!("/internal/v1/tenants/{id}/limits");
    let captured = Captured::default();
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(captured.clone()));
    let quota = |text: &str| -> Value { serde_json::from_str(text).unwrap() };
    let (status, body) = seam
        .ops("PUT", &path, r#"{"classification_daily_quota":25}"#)
        .await;
    assert_eq!(
        (status, quota(&body)),
        (200, json!({"classification_daily_quota": 25}))
    );
    let (_, body) = seam.ops("PUT", &path, "{}").await;
    assert_eq!(quota(&body), json!({"classification_daily_quota": 25}));
    assert_eq!(
        tenancy::get_tenant(&pool, &id)
            .await
            .unwrap()
            .classification_quota_override,
        Some(25)
    );
    let (_, body) = seam
        .ops("PUT", &path, r#"{"classification_daily_quota":null}"#)
        .await;
    assert_eq!(quota(&body), json!({"classification_daily_quota": 1000}));
    assert_eq!(
        tenancy::get_tenant(&pool, &id)
            .await
            .unwrap()
            .classification_quota_override,
        None
    );
    let (status, body) = seam
        .ops("PUT", &path, r#"{"classification_daily_quota":-1}"#)
        .await;
    assert_eq!(
        (status, body.as_str()),
        (
            422,
            "{\"error\":{\"code\":\"invalid_request\",\"message\":\"classification_daily_quota must be at least 0\",\"details\":{\"field\":\"classification_daily_quota\"}}}\n"
        )
    );
    let (status, _) = seam
        .ops("PUT", "/internal/v1/tenants/ten_missing/limits", "{}")
        .await;
    assert_eq!(status, 404);
    assert_eq!(captured.audits("tenant.limits.replace").len(), 3);
}

/// Spec §4.5: retention evidence is validated at write (90-day window, alias keys) and
/// stored; a downgrade that empties a zero-retention tenant's pool alerts, unblocked.
#[tokio::test]
async fn retention_evidence_is_validated_and_downgrades_alert() {
    let db = db!();
    let pool = db.pool().await;
    let seam = seam(&pool);
    let now = Utc::now();
    let verified = now - Duration::days(10);
    let too_far = verified + Duration::days(91);
    let (status, body) = seam
        .ops_json(
            "POST",
            "/internal/v1/columns",
            &json!({
                "provider": "anthropic", "model_id": "zr", "endpoint": "direct",
                "retention_mode": "zero",
                "retention_verified_at": verified.to_rfc3339(),
                "retention_valid_until": too_far.to_rfc3339(),
            })
            .to_string(),
        )
        .await;
    assert_eq!(status, 422);
    assert_eq!(
        body["error"]["message"],
        "retention_valid_until must be at most 90 days after retention_verified_at"
    );
    let (status, body) = seam
        .ops_json(
            "POST",
            "/internal/v1/columns",
            &json!({
                "provider": "anthropic", "model_id": "zr", "endpoint": "openrouter",
                "execution_aliases": {"openrouter": "anthropic/zr"},
                "execution_alias_retention": {"elsewhere": {"retention_mode": "zero"}},
            })
            .to_string(),
        )
        .await;
    assert_eq!(status, 422);
    assert_eq!(body["error"]["details"]["key"], "elsewhere");

    let valid_until = verified + Duration::days(80);
    let (status, body) = seam
        .ops_json(
            "POST",
            "/internal/v1/columns",
            &json!({
                "provider": "anthropic", "model_id": "zr", "endpoint": "direct",
                "retention_mode": "zero",
                "retention_verified_at": verified.to_rfc3339(),
                "retention_valid_until": valid_until.to_rfc3339(),
                "retention_source": "https://example.com/zdr",
            })
            .to_string(),
        )
        .await;
    assert_eq!(status, 201, "{body}");
    let id = body["column_id"].as_str().unwrap().to_owned();
    let stored = get_model_column(&pool, &id).await.unwrap();
    assert_eq!(stored.retention_mode, "zero");
    assert_eq!(
        stored.retention_source.as_deref(),
        Some("https://example.com/zdr")
    );
    sqlx::query("update model_columns set status = 'active' where id = $1")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    let tenant = seam.create_tenant("Zero").await;
    tenancy::upsert_tenant_constraints(&pool, &tenant, None, None, true, false)
        .await
        .unwrap();
    let (status, _) = seam
        .ops(
            "PUT",
            &format!("/internal/v1/columns/{id}"),
            r#"{"retention_mode":"standard"}"#,
        )
        .await;
    assert_eq!(status, 200, "a downgrade is not blocked");
    let alerts = maverick_store::alerts::list_alerts_by_kind(&pool, "pool_would_be_empty")
        .await
        .unwrap();
    assert_eq!(alerts.len(), 1);
    assert_eq!(alerts[0].severity, "critical");
    let subject: Value = serde_json::from_str(alerts[0].subject.get()).unwrap();
    assert_eq!(subject["tenant_id"], tenant.as_str());
    assert_eq!(subject["constraint"], "zero_retention");
}

/// parity: api.TestInternalUnknownPathsReturnEnvelope
#[tokio::test]
async fn unknown_paths_return_the_envelope() {
    let db = db!();
    let pool = db.pool().await;
    let seam = seam(&pool);
    let rows = recorded("TestInternalUnknownPathsReturnEnvelope");
    let (status, body) = send(&seam.internal, "GET", "/nope", Some(OPS), "").await;
    assert_eq!(
        (status, body.as_str()),
        (rows[0].status, rows[0].response.as_str())
    );
    let (status, body) = send(
        &seam.internal,
        "GET",
        "/internal/v1/tenants/ten_x",
        None,
        "",
    )
    .await;
    assert_eq!(
        (status, body.as_str()),
        (rows[1].status, rows[1].response.as_str())
    );
    let (status, body) = seam.ops("GET", "/internal/v1/tenants/ten_x", "").await;
    assert_eq!(
        (status, body.as_str()),
        (rows[2].status, rows[2].response.as_str())
    );
}

/// parity: api.TestInternalProvisionThenRecommendEndToEnd
#[tokio::test]
async fn provision_then_recommend_end_to_end() {
    let db = db!();
    let pool = db.pool().await;
    let seam = seam(&pool);
    const CORPUS: &str = "2026-07-01";
    maverick_store::corpus::upsert_corpus_version(&pool, CORPUS, "", 0, false)
        .await
        .unwrap();
    maverick_store::corpus::activate_corpus_version(&pool, CORPUS)
        .await
        .unwrap();
    let today = Utc::now().date_naive();
    for (column, quality, cost, latency) in [
        ("int-e2e-fast", 0.91_f32, 0.004_f32, 1800.0_f32),
        ("int-e2e-strong", 0.95, 0.02, 3600.0),
    ] {
        seam.column(column, "active").await;
        let mut rows: Vec<(i16, &str, f32)> = vec![
            (3, "quality", quality),
            (3, "cost_per_run_usd", cost),
            (3, "latency_p95_ms", latency),
        ];
        for (tier, rate) in (1..).zip([0.98_f32, 0.96, 0.94, 0.9, 0.88]) {
            rows.push((tier, "pass_rate", rate));
        }
        for (tier, dimension, ewma) in rows {
            upsert_aggregate(
                &pool,
                &AggregateUpsert {
                    model_column_id: column,
                    capability: "structured_extraction",
                    difficulty_tier: tier,
                    dimension,
                    ewma,
                    variance: 0.0,
                    n: 20,
                    measured: true,
                    matrix_version: CORPUS,
                    as_of: today,
                },
            )
            .await
            .unwrap();
        }
    }
    let tenant = seam.create_tenant("E2E Tenant").await;
    let (_, key) = seam.issue_key(&tenant).await;
    let (status, text) = seam
        .public(
            "POST",
            "/v1/recommend",
            &key,
            r#"{"workload":{"capability":"structured_extraction","difficulty":3},"mode":"balanced"}"#,
        )
        .await;
    assert_eq!(status, 200, "{text}");
    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["pool"], "default");
    assert_eq!(body["matrix_version"], CORPUS);
    assert!(
        body["recommendation_id"]
            .as_str()
            .is_some_and(|id| id.starts_with("rec_"))
    );
    let recommended = body["recommendations"].as_array().unwrap();
    assert!(!recommended.is_empty());
    for entry in recommended {
        let column = entry["column_id"].as_str().unwrap();
        assert!(
            ["int-e2e-fast", "int-e2e-strong"].contains(&column),
            "{column}"
        );
    }
}

/// D29: the seam accepts per-model parameter constraints, validates their shape, stores
/// them, and logs `catalog_changes` with `change=param_constraints` when they change.
#[tokio::test]
async fn param_constraints_are_validated_stored_and_logged() {
    let db = db!();
    let pool = db.pool().await;
    let seam = seam(&pool);
    let constraints = json!({"reasoning_effort": {"allowed": ["low", "high"], "order": ["minimal", "low", "medium", "high", "xhigh"]}});
    let (status, body) = seam
        .ops_json(
            "POST",
            "/internal/v1/columns",
            &json!({"provider": "openai", "model_id": "gpt-d29", "endpoint": "direct", "param_constraints": constraints}).to_string(),
        )
        .await;
    assert_eq!(status, 201, "{body}");
    let id = body["column_id"].as_str().unwrap().to_owned();
    let stored = get_model_column(&pool, &id).await.unwrap();
    let stored: Value = serde_json::from_str(stored.param_constraints.get()).unwrap();
    assert_eq!(stored, constraints);

    for (bad, key) in [
        (json!("nope"), None),
        (
            json!({"temperature": {"allowed": ["a"], "order": ["a"]}}),
            Some("temperature"),
        ),
        (
            json!({"reasoning_effort": {"allowed": [], "order": ["low"]}}),
            Some("reasoning_effort"),
        ),
        (
            json!({"reasoning_effort": {"allowed": ["max"], "order": ["low"]}}),
            Some("reasoning_effort"),
        ),
        (
            json!({"reasoning_effort": {"allowed": ["low"], "order": ["low", "low"]}}),
            Some("reasoning_effort"),
        ),
        (
            json!({"reasoning_effort": {"allowed": [1], "order": ["low"]}}),
            Some("reasoning_effort"),
        ),
        (
            json!({"reasoning_effort": {"allowed": ["low"]}}),
            Some("reasoning_effort"),
        ),
        (
            json!({"reasoning_effort": {"allowed": ["low"], "order": ["low"], "extra": 1}}),
            Some("reasoning_effort"),
        ),
    ] {
        let (status, body) = seam
            .ops_json(
                "PUT",
                &format!("/internal/v1/columns/{id}"),
                &json!({ "param_constraints": bad }).to_string(),
            )
            .await;
        assert_eq!(status, 422, "{bad}");
        assert_eq!(body["error"]["code"], "invalid_request");
        assert_eq!(body["error"]["details"]["field"], "param_constraints");
        assert_eq!(body["error"]["details"]["key"].as_str(), key, "{bad}");
    }

    let captured = Captured::default();
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(captured.clone()));
    let narrower = json!({"reasoning_effort": {"allowed": ["high"], "order": ["low", "high"]}});
    let (status, _) = seam
        .ops_json(
            "PUT",
            &format!("/internal/v1/columns/{id}"),
            &json!({ "param_constraints": narrower }).to_string(),
        )
        .await;
    assert_eq!(status, 200);
    let changes = captured.lines("catalog_changes");
    assert_eq!(changes.len(), 1);
    assert_eq!(field(&changes[0], "change"), Some("param_constraints"));
    assert_eq!(field(&changes[0], "column_id"), Some(id.as_str()));
    // An update that does not send the field keeps it and logs nothing for it.
    seam.ops_json(
        "PUT",
        &format!("/internal/v1/columns/{id}"),
        r#"{"status":"active"}"#,
    )
    .await;
    let stored = get_model_column(&pool, &id).await.unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(stored.param_constraints.get()).unwrap(),
        narrower
    );
    assert!(
        captured
            .lines("catalog_changes")
            .iter()
            .all(
                |fields| field(fields, "change") != Some("param_constraints")
                    || fields == &changes[0]
            )
    );
}
