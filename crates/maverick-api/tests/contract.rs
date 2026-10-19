//! The per-endpoint error-code contract matrix (bead 5.17; AGENTS.md "Each endpoint has a
//! contract test for each error code"; spec §15, Appendix A and B; reference Part 5 §5).
//!
//! Every (endpoint, status, code) pair the API can return has a named test, one of:
//! - a recorded Go exchange that `tests/replay.rs` replays and compares byte for byte (the
//!   first exchange for the pair in `parity/blackbox/api-recorded.jsonl`);
//! - a shared-middleware test in this file, run against every route: 401, 403, 413, 429,
//!   and 500 when the store fails;
//! - a named Rust test ([`RUST`]).
//!
//! [`REQUIRED`] lists the pairs spec Appendix A and B and reference Part 5 §5.2 name for each
//! endpoint. The matrix test fails when a required or recorded pair has no test, or a named
//! test does not exist. `tests/CONTRACT.md` renders the matrix;
//! `MAVERICK_UPDATE_CONTRACT=1 cargo test -p maverick-api --test contract` rewrites it.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use maverick_api::{
    ApiState, AuthState, Clock, HealthState, Identity, RateLimiter, StaticAuthBackend,
    internal_router_with, public_router_with,
};
use maverick_domain::keys;
use maverick_store::catalog::{NewModelColumn, create_model_column};
use maverick_store::{JsonRawValue, MIGRATOR, tenancy};
use maverick_testdb::Schema;
use serde_json::Value;
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;

/// One route and a request that reaches its handler.
#[derive(Debug, Clone, Copy)]
struct Endpoint {
    internal: bool,
    method: &'static str,
    template: &'static str,
    path: &'static str,
    body: &'static str,
    /// The handler reads the store for this request, so a store failure is its 500.
    store: bool,
}

const fn public(
    method: &'static str,
    template: &'static str,
    path: &'static str,
    body: &'static str,
    store: bool,
) -> Endpoint {
    Endpoint {
        internal: false,
        method,
        template,
        path,
        body,
        store,
    }
}

const fn internal(
    method: &'static str,
    template: &'static str,
    path: &'static str,
    body: &'static str,
) -> Endpoint {
    Endpoint {
        internal: true,
        method,
        template,
        path,
        body,
        store: true,
    }
}

const MCP_INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"contract","version":"v1"},"protocolVersion":"2025-11-25","capabilities":{}}}"#;

/// Every route (spec Appendix A, §13), with a request that passes validation.
const ENDPOINTS: &[Endpoint] = &[
    public("GET", "/v1/config/pools", "/v1/config/pools", "", true),
    public(
        "PUT",
        "/v1/config/pools",
        "/v1/config/pools",
        r#"{"pools":[{"name":"default","model_column_ids":["col_a"],"is_default":true}]}"#,
        true,
    ),
    public("GET", "/v1/config/weights", "/v1/config/weights", "", true),
    public(
        "PUT",
        "/v1/config/weights",
        "/v1/config/weights",
        r#"{"weights":[{"capability":"factual_qa","w_quality":0.5,"w_cost":0.3,"w_latency":0.2}]}"#,
        true,
    ),
    public(
        "GET",
        "/v1/config/constraints",
        "/v1/config/constraints",
        "",
        true,
    ),
    public(
        "PUT",
        "/v1/config/constraints",
        "/v1/config/constraints",
        r#"{"constraints":{"max_cost_per_run":null,"max_latency_ms":null,"zero_retention":false}}"#,
        true,
    ),
    public("GET", "/v1/policies", "/v1/policies", "", true),
    public(
        "POST",
        "/v1/recommend",
        "/v1/recommend",
        r#"{"workload":{"capability":"factual_qa"}}"#,
        true,
    ),
    public(
        "GET",
        "/v1/checks/{ref}",
        "/v1/checks/checks/extraction/v1",
        "",
        false,
    ),
    public("GET", "/v1/models", "/v1/models", "", true),
    public(
        "POST",
        "/v1/telemetry",
        "/v1/telemetry",
        r#"{"items":[{"recommendation_id":"rec_x","model_used":"m","status":200}]}"#,
        true,
    ),
    public(
        "GET",
        "/v1/history/models/{column_id}",
        "/v1/history/models/col_x",
        "",
        true,
    ),
    public(
        "GET",
        "/v1/history/recommendations",
        "/v1/history/recommendations",
        "",
        true,
    ),
    public(
        "GET",
        "/v1/history/recommendations/{id}",
        "/v1/history/recommendations/rec_x",
        "",
        true,
    ),
    public(
        "GET",
        "/v1/history/outcomes",
        "/v1/history/outcomes",
        "",
        true,
    ),
    public(
        "GET",
        "/v1/history/calibration",
        "/v1/history/calibration",
        "",
        true,
    ),
    public(
        "POST",
        "/v1/debug/capture-windows",
        "/v1/debug/capture-windows",
        r#"{"duration_minutes":60}"#,
        true,
    ),
    public(
        "DELETE",
        "/v1/debug/capture-windows/{id}",
        "/v1/debug/capture-windows/cw_x",
        "",
        true,
    ),
    public(
        "POST",
        "/v1/debug/replay/{capture_id}",
        "/v1/debug/replay/cap_x",
        "{}",
        true,
    ),
    public("GET", "/v1/debug/captures", "/v1/debug/captures", "", true),
    public(
        "GET",
        "/v1/debug/captures/{id}/prompt",
        "/v1/debug/captures/cap_x/prompt",
        "",
        true,
    ),
    public("POST", "/mcp", "/mcp", MCP_INITIALIZE, false),
    public("DELETE", "/mcp", "/mcp", "", false),
    public("GET", "/mcp", "/mcp", "", false),
    internal(
        "POST",
        "/internal/v1/tenants",
        "/internal/v1/tenants",
        r#"{"name":"contract"}"#,
    ),
    internal(
        "GET",
        "/internal/v1/tenants/{id}",
        "/internal/v1/tenants/ten_x",
        "",
    ),
    internal(
        "POST",
        "/internal/v1/tenants/{id}/suspend",
        "/internal/v1/tenants/ten_x/suspend",
        "",
    ),
    internal(
        "POST",
        "/internal/v1/tenants/{id}/activate",
        "/internal/v1/tenants/ten_x/activate",
        "",
    ),
    internal(
        "POST",
        "/internal/v1/tenants/{id}/api-keys",
        "/internal/v1/tenants/ten_x/api-keys",
        r#"{"name":"primary"}"#,
    ),
    internal(
        "DELETE",
        "/internal/v1/api-keys/{id}",
        "/internal/v1/api-keys/key_x",
        "",
    ),
    internal(
        "PUT",
        "/internal/v1/tenants/{id}/pools",
        "/internal/v1/tenants/ten_x/pools",
        r#"{"pools":[{"name":"default","model_column_ids":["col_a"],"is_default":true}]}"#,
    ),
    internal(
        "PUT",
        "/internal/v1/tenants/{id}/weights",
        "/internal/v1/tenants/ten_x/weights",
        r#"{"weights":[{"capability":"factual_qa","w_quality":0.5,"w_cost":0.3,"w_latency":0.2}]}"#,
    ),
    internal(
        "PUT",
        "/internal/v1/tenants/{id}/constraints",
        "/internal/v1/tenants/ten_x/constraints",
        r#"{"constraints":{"max_cost_per_run":null,"max_latency_ms":null,"zero_retention":false}}"#,
    ),
    internal(
        "PUT",
        "/internal/v1/tenants/{id}/limits",
        "/internal/v1/tenants/ten_x/limits",
        r#"{"classification_daily_quota":10}"#,
    ),
    internal("GET", "/internal/v1/alerts", "/internal/v1/alerts", ""),
    internal(
        "POST",
        "/internal/v1/columns",
        "/internal/v1/columns",
        r#"{"provider":"anthropic","model_id":"claude-contract","endpoint":"direct","settings":{},"pricing":{"input_per_mtok":3,"output_per_mtok":15,"currency":"USD","as_of":"2026-07-01","source":"list"}}"#,
    ),
    internal(
        "PUT",
        "/internal/v1/columns/{id}",
        "/internal/v1/columns/col_x",
        "{}",
    ),
    internal(
        "POST",
        "/internal/v1/models/refresh",
        "/internal/v1/models/refresh",
        "{}",
    ),
    internal(
        "POST",
        "/internal/v1/corpus/activate",
        "/internal/v1/corpus/activate",
        r#"{"version":"2026-07-01"}"#,
    ),
    internal("GET", "/internal/v1/captures", "/internal/v1/captures", ""),
    internal(
        "GET",
        "/internal/v1/captures/{id}/prompt",
        "/internal/v1/captures/cap_x/prompt",
        "",
    ),
    Endpoint {
        internal: true,
        method: "GET",
        template: "/health/detail",
        path: "/health/detail",
        body: "",
        store: false,
    },
];

const KEY: &str = "Bearer mk_live_0000000000000000000000000000000000000000000";
const OPS: &str = "Bearer ops:alpha";

fn api_on_unreachable_store() -> ApiState {
    let pool = PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(300))
        .connect_lazy("postgres://contract@127.0.0.1:1/unreachable")
        .expect("lazy pool");
    ApiState::with_health(pool, HealthState::detached())
}

/// A clock that never moves. The limiter's windows are aligned to the minute, so two
/// calls on the system clock could fall in two windows and both pass.
struct FixedClock;

impl Clock for FixedClock {
    fn unix_seconds(&self) -> i64 {
        1_783_000_000
    }
}

fn routers(backend: StaticAuthBackend, rpm: u32, api: ApiState) -> (Router, Router) {
    let auth = AuthState::new(
        Arc::new(backend),
        keys::parse_internal_tokens("ops:alpha").unwrap(),
        RateLimiter::with_clock(rpm, Arc::new(FixedClock)),
    );
    (
        public_router_with(auth.clone(), api.clone()),
        internal_router_with(auth, api),
    )
}

fn authenticated() -> StaticAuthBackend {
    StaticAuthBackend::authenticated(Identity {
        tenant_id: "ten_contract".to_owned(),
        api_key_id: "key_contract".to_owned(),
    })
}

async fn call(
    routers: &(Router, Router),
    endpoint: &Endpoint,
    auth: Option<&str>,
    body: Vec<u8>,
) -> (u16, String) {
    let mut request = Request::builder()
        .method(endpoint.method)
        .uri(endpoint.path)
        .header("content-type", "application/json")
        .header("content-length", body.len());
    if let Some(auth) = auth {
        request = request.header("authorization", auth);
    }
    let router = if endpoint.internal {
        &routers.1
    } else {
        &routers.0
    };
    let response = router
        .clone()
        .oneshot(request.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn code(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["error"]["code"].as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("(no envelope: {body})"))
}

fn credential(endpoint: &Endpoint) -> &'static str {
    if endpoint.internal { OPS } else { KEY }
}

#[tokio::test]
async fn every_endpoint_answers_401_without_a_credential() {
    let routers = routers(StaticAuthBackend::missing(), 1000, ApiState::detached());
    for endpoint in ENDPOINTS {
        let (status, body) =
            call(&routers, endpoint, None, endpoint.body.as_bytes().to_vec()).await;
        assert_eq!(
            (status, code(&body).as_str()),
            (401, "unauthorized"),
            "{endpoint:?}"
        );
    }
}

#[tokio::test]
async fn every_public_endpoint_answers_403_for_a_suspended_tenant() {
    let routers = routers(StaticAuthBackend::suspended(), 1000, ApiState::detached());
    for endpoint in ENDPOINTS.iter().filter(|e| !e.internal) {
        let (status, body) = call(
            &routers,
            endpoint,
            Some(KEY),
            endpoint.body.as_bytes().to_vec(),
        )
        .await;
        assert_eq!(
            (status, code(&body).as_str()),
            (403, "tenant_suspended"),
            "{endpoint:?}"
        );
    }
}

#[tokio::test]
async fn every_public_endpoint_answers_429_past_the_rate_limit() {
    for endpoint in ENDPOINTS.iter().filter(|e| !e.internal) {
        let routers = routers(authenticated(), 1, ApiState::detached());
        let _ = call(
            &routers,
            endpoint,
            Some(KEY),
            endpoint.body.as_bytes().to_vec(),
        )
        .await;
        let (status, body) = call(
            &routers,
            endpoint,
            Some(KEY),
            endpoint.body.as_bytes().to_vec(),
        )
        .await;
        assert_eq!(
            (status, code(&body).as_str()),
            (429, "rate_limited"),
            "{endpoint:?}"
        );
    }
}

#[tokio::test]
async fn every_endpoint_answers_413_over_the_body_cap() {
    let routers = routers(authenticated(), 1000, ApiState::detached());
    let oversized = vec![b'a'; 2 * 1024 * 1024 + 1];
    for endpoint in ENDPOINTS.iter().filter(|e| e.template != "/health/detail") {
        let (status, body) = call(
            &routers,
            endpoint,
            Some(credential(endpoint)),
            oversized.clone(),
        )
        .await;
        assert_eq!(
            (status, code(&body).as_str()),
            (413, "payload_too_large"),
            "{endpoint:?}"
        );
    }
}

#[tokio::test]
async fn every_store_backed_endpoint_answers_500_when_the_store_fails() {
    let routers = routers(authenticated(), 1000, api_on_unreachable_store());
    for endpoint in ENDPOINTS.iter().filter(|e| e.store) {
        let (status, body) = call(
            &routers,
            endpoint,
            Some(credential(endpoint)),
            endpoint.body.as_bytes().to_vec(),
        )
        .await;
        assert_eq!(
            (status, body.as_str()),
            (
                500,
                "{\"error\":{\"code\":\"internal_error\",\"message\":\"internal error\",\"details\":{}}}\n"
            ),
            "{endpoint:?}"
        );
    }
}

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

/// The seam's config writes run the public validation; the recording holds these two
/// codes only on the public side.
#[tokio::test]
async fn the_seam_config_writes_answer_the_policy_codes() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    tenancy::create_tenant(&pool, "ten_seam", "Seam", &[0; 32])
        .await
        .unwrap();
    let empty = JsonRawValue::from_string("{}".to_owned()).unwrap();
    create_model_column(
        &pool,
        &NewModelColumn {
            id: "col_retired",
            provider: "anthropic",
            model_id: "model-col_retired",
            endpoint: "direct",
            settings: &empty,
            status: "retired",
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
    let api = ApiState::with_health(pool.clone(), HealthState::detached());
    let routers = routers(authenticated(), 1000, api);
    for (resource, body, status, want) in [
        (
            "pools",
            r#"{"pools":[{"name":"graveyard","model_column_ids":["col_retired"],"is_default":true}]}"#,
            422,
            "pool_would_be_empty",
        ),
        (
            "constraints",
            r#"{"constraints":{"max_cost_per_run":0}}"#,
            422,
            "invalid_request",
        ),
    ] {
        let endpoint = ENDPOINTS
            .iter()
            .find(|e| e.template == format!("/internal/v1/tenants/{{id}}/{resource}"))
            .unwrap();
        let endpoint = Endpoint {
            path: if resource == "pools" {
                "/internal/v1/tenants/ten_seam/pools"
            } else {
                "/internal/v1/tenants/ten_seam/constraints"
            },
            ..*endpoint
        };
        let (got, text) = call(&routers, &endpoint, Some(OPS), body.as_bytes().to_vec()).await;
        assert_eq!(
            (got, code(&text).as_str()),
            (status, want),
            "{resource}: {text}"
        );
    }
}

// ---- the matrix -------------------------------------------------------------------------

/// A pair's test.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Evidence {
    /// A recorded Go exchange, replayed byte for byte by `tests/replay.rs`.
    Replay(String, u64),
    /// A test function in a file under `crates/maverick-api/tests/`.
    Rust(&'static str, &'static str),
}

/// (endpoint, status, code). An endpoint is `METHOD /template`; MCP tool errors are
/// `TOOL <name>` with status 200 (a tool result with `isError: true`).
type Pair = (String, u16, String);

/// The pairs spec Appendix A and B and reference Part 5 §5.2 name, beyond the shared
/// middleware codes every route has.
const REQUIRED: &[(&str, u16, &str)] = &[
    ("POST /v1/recommend", 422, "invalid_request"),
    ("POST /v1/recommend", 422, "unknown_model"),
    ("POST /v1/recommend", 422, "model_not_in_pool"),
    ("POST /v1/recommend", 422, "unknown_pool"),
    (
        "POST /v1/recommend",
        422,
        "zero_retention_requires_declared",
    ),
    ("POST /v1/recommend", 409, "no_compliant_columns"),
    ("POST /v1/telemetry", 422, "invalid_request"),
    ("GET /v1/policies", 422, "invalid_request"),
    ("GET /v1/policies", 422, "unknown_pool"),
    ("GET /v1/checks/{ref}", 404, "not_found"),
    ("GET /v1/history/models/{column_id}", 404, "not_found"),
    ("GET /v1/history/models/{column_id}", 422, "invalid_request"),
    ("GET /v1/history/recommendations", 422, "invalid_request"),
    ("GET /v1/history/recommendations/{id}", 404, "not_found"),
    ("GET /v1/history/outcomes", 422, "invalid_request"),
    ("GET /v1/history/calibration", 422, "invalid_request"),
    ("PUT /v1/config/pools", 422, "invalid_request"),
    ("PUT /v1/config/pools", 422, "unknown_model"),
    ("PUT /v1/config/pools", 422, "pool_would_be_empty"),
    ("PUT /v1/config/weights", 422, "invalid_request"),
    ("PUT /v1/config/constraints", 422, "invalid_request"),
    ("PUT /v1/config/constraints", 422, "pool_would_be_empty"),
    ("POST /v1/debug/capture-windows", 422, "invalid_request"),
    (
        "POST /v1/debug/capture-windows",
        409,
        "zero_retention_conflict",
    ),
    ("DELETE /v1/debug/capture-windows/{id}", 404, "not_found"),
    ("POST /v1/debug/replay/{capture_id}", 404, "not_found"),
    ("POST /v1/debug/replay/{capture_id}", 410, "capture_expired"),
    ("POST /v1/debug/replay/{capture_id}", 422, "invalid_request"),
    ("GET /v1/debug/captures", 422, "invalid_request"),
    ("GET /v1/debug/captures/{id}/prompt", 404, "not_found"),
    ("GET /v1/debug/captures/{id}/prompt", 410, "capture_expired"),
    ("GET /up", 503, "(no envelope)"),
    ("POST /mcp", 400, "(no envelope)"),
    ("POST /mcp", 404, "(no envelope)"),
    ("GET /mcp", 405, "(no envelope)"),
    ("TOOL recommend_model", 200, "invalid_request"),
    ("TOOL recommend_model", 200, "unknown_pool"),
    (
        "TOOL recommend_model",
        200,
        "zero_retention_requires_declared",
    ),
    ("TOOL recommend_model", 200, "no_compliant_columns"),
    ("POST /internal/v1/tenants", 422, "invalid_request"),
    ("GET /internal/v1/tenants/{id}", 404, "not_found"),
    ("POST /internal/v1/tenants/{id}/suspend", 404, "not_found"),
    ("POST /internal/v1/tenants/{id}/activate", 404, "not_found"),
    ("POST /internal/v1/tenants/{id}/api-keys", 404, "not_found"),
    (
        "POST /internal/v1/tenants/{id}/api-keys",
        422,
        "invalid_request",
    ),
    ("DELETE /internal/v1/api-keys/{id}", 404, "not_found"),
    ("PUT /internal/v1/tenants/{id}/pools", 404, "not_found"),
    (
        "PUT /internal/v1/tenants/{id}/pools",
        422,
        "invalid_request",
    ),
    ("PUT /internal/v1/tenants/{id}/pools", 422, "unknown_model"),
    (
        "PUT /internal/v1/tenants/{id}/pools",
        422,
        "pool_would_be_empty",
    ),
    ("PUT /internal/v1/tenants/{id}/weights", 404, "not_found"),
    (
        "PUT /internal/v1/tenants/{id}/weights",
        422,
        "invalid_request",
    ),
    (
        "PUT /internal/v1/tenants/{id}/constraints",
        404,
        "not_found",
    ),
    (
        "PUT /internal/v1/tenants/{id}/constraints",
        422,
        "invalid_request",
    ),
    (
        "PUT /internal/v1/tenants/{id}/constraints",
        422,
        "pool_would_be_empty",
    ),
    ("PUT /internal/v1/tenants/{id}/limits", 404, "not_found"),
    (
        "PUT /internal/v1/tenants/{id}/limits",
        422,
        "invalid_request",
    ),
    ("GET /internal/v1/alerts", 422, "invalid_request"),
    ("POST /internal/v1/columns", 422, "invalid_request"),
    ("PUT /internal/v1/columns/{id}", 404, "not_found"),
    ("PUT /internal/v1/columns/{id}", 422, "invalid_request"),
    ("PUT /internal/v1/columns/{id}", 422, "pool_would_be_empty"),
    ("POST /internal/v1/models/refresh", 422, "invalid_request"),
    ("POST /internal/v1/corpus/activate", 404, "not_found"),
    ("POST /internal/v1/corpus/activate", 422, "invalid_request"),
    ("GET /internal/v1/captures", 422, "invalid_request"),
    ("GET /internal/v1/captures/{id}/prompt", 404, "not_found"),
    (
        "GET /internal/v1/captures/{id}/prompt",
        410,
        "capture_expired",
    ),
    ("* public (unrouted)", 404, "not_found"),
    ("* internal (unrouted)", 404, "not_found"),
];

/// Pairs the recording does not hold, each with its Rust test.
const RUST: &[(&str, u16, &str, &str, &str)] = &[
    (
        "POST /internal/v1/models/refresh",
        422,
        "invalid_request",
        "models_refresh.rs",
        "a_bad_body_is_422",
    ),
    (
        "POST /internal/v1/models/refresh",
        422,
        "invalid_request",
        "models_refresh.rs",
        "an_entry_needs_pricing_and_a_valid_column",
    ),
    (
        "POST /internal/v1/models/refresh",
        422,
        "invalid_request",
        "models_refresh.rs",
        "an_entry_for_a_provider_that_did_not_answer_is_422",
    ),
    (
        "POST /internal/v1/models/refresh",
        422,
        "invalid_request",
        "models_refresh.rs",
        "an_entry_that_is_not_new_is_422",
    ),
    (
        "POST /internal/v1/models/refresh",
        422,
        "invalid_request",
        "models_refresh.rs",
        "enrollment_is_all_or_nothing",
    ),
    (
        "POST /v1/recommend",
        422,
        "zero_retention_requires_declared",
        "recommend_prompt.rs",
        "a_zero_retention_tenant_without_a_zero_retention_classifier_is_refused",
    ),
    (
        "POST /v1/recommend",
        409,
        "no_compliant_columns",
        "recommend.rs",
        "zero_retention_filters_columns_and_fails_closed",
    ),
    (
        "POST /mcp",
        400,
        "(no envelope)",
        "mcp.rs",
        "requests_need_a_live_session",
    ),
    (
        "POST /mcp",
        404,
        "(no envelope)",
        "mcp.rs",
        "requests_need_a_live_session",
    ),
    (
        "GET /mcp",
        405,
        "(no envelope)",
        "mcp.rs",
        "requests_need_a_live_session",
    ),
    (
        "TOOL recommend_model",
        200,
        "invalid_request",
        "mcp.rs",
        "recommend_model_bad_arguments_are_tool_errors",
    ),
    (
        "TOOL recommend_model",
        200,
        "unknown_pool",
        "mcp.rs",
        "recommend_model_bad_arguments_are_tool_errors",
    ),
    (
        "TOOL recommend_model",
        200,
        "zero_retention_requires_declared",
        "mcp.rs",
        "recommend_model_surfaces_the_privacy_codes",
    ),
    (
        "TOOL recommend_model",
        200,
        "no_compliant_columns",
        "mcp.rs",
        "recommend_model_surfaces_the_privacy_codes",
    ),
    (
        "PUT /internal/v1/tenants/{id}/limits",
        404,
        "not_found",
        "internal.rs",
        "limits_set_keep_and_clear_the_quota_override",
    ),
    (
        "PUT /internal/v1/tenants/{id}/limits",
        422,
        "invalid_request",
        "internal.rs",
        "limits_set_keep_and_clear_the_quota_override",
    ),
    (
        "PUT /internal/v1/tenants/{id}/pools",
        422,
        "pool_would_be_empty",
        "contract.rs",
        "the_seam_config_writes_answer_the_policy_codes",
    ),
    (
        "PUT /internal/v1/tenants/{id}/constraints",
        422,
        "invalid_request",
        "contract.rs",
        "the_seam_config_writes_answer_the_policy_codes",
    ),
];

fn tests_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests")
}

/// The endpoint a recorded request hit.
fn endpoint_of(listener: &str, method: &str, path: &str) -> String {
    let path = path.split('?').next().unwrap_or_default();
    if path == "/up" && method == "GET" {
        return "GET /up".to_owned();
    }
    let segments: Vec<&str> = path.split('/').collect();
    let found = ENDPOINTS.iter().find(|e| {
        let internal = listener == "internal";
        if e.internal != internal || e.method != method {
            return false;
        }
        let template: Vec<&str> = e.template.split('/').collect();
        if e.template == "/v1/checks/{ref}" {
            return path.starts_with("/v1/checks/") && path.len() > "/v1/checks/".len();
        }
        template.len() == segments.len()
            && template
                .iter()
                .zip(&segments)
                .all(|(t, s)| (t.starts_with('{') && !s.is_empty()) || t == s)
    });
    match found {
        Some(endpoint) => format!("{} {}", endpoint.method, endpoint.template),
        None => format!("* {listener} (unrouted)"),
    }
}

/// Every error pair in the recording, with its first exchange.
fn recorded() -> BTreeMap<Pair, Evidence> {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../parity/blackbox/api-recorded.jsonl");
    let mut out = BTreeMap::new();
    for line in std::fs::read_to_string(path).unwrap().lines() {
        let row: Value = serde_json::from_str(line).unwrap();
        let status = u16::try_from(row["response"]["status"].as_u64().unwrap()).unwrap();
        let test = row["test"].as_str().unwrap();
        if status < 400 && !(status == 200 && row["request"]["path"] == "/mcp") {
            continue;
        }
        if test == "TestPhase2ExitEndToEndRecommend" {
            continue;
        }
        let body = row["response"]["body"].as_str().unwrap_or_default();
        let endpoint = endpoint_of(
            row["listener"].as_str().unwrap(),
            row["request"]["method"].as_str().unwrap(),
            row["request"]["path"].as_str().unwrap(),
        );
        let pair = if status == 200 {
            // An MCP tool error: `isError` with a `<code>: <message>` text.
            if !body.contains("\"isError\":true") {
                continue;
            }
            let request: Value =
                serde_json::from_str(row["request"]["body"].as_str().unwrap()).unwrap();
            let tool = request["params"]["name"].as_str().unwrap().to_owned();
            let data: Value =
                serde_json::from_str(body.split("data: ").nth(1).unwrap().trim()).unwrap();
            let text = data["result"]["content"][0]["text"].as_str().unwrap();
            let code = if text.starts_with("validating \"arguments\"") {
                "invalid_request".to_owned()
            } else {
                text.split(':').next().unwrap().to_owned()
            };
            (format!("TOOL {tool}"), 200, code)
        } else {
            let code = serde_json::from_str::<Value>(body)
                .ok()
                .and_then(|v| v["error"]["code"].as_str().map(str::to_owned))
                .unwrap_or_else(|| "(no envelope)".to_owned());
            (endpoint, status, code)
        };
        out.entry(pair)
            .or_insert_with(|| Evidence::Replay(test.to_owned(), row["seq"].as_u64().unwrap()));
    }
    out
}

/// The shared-middleware pairs, from the tests above.
fn middleware() -> BTreeMap<Pair, Evidence> {
    let mut out = BTreeMap::new();
    let mut add = |endpoint: &Endpoint, status: u16, code: &str, test: &'static str| {
        out.insert(
            (
                format!("{} {}", endpoint.method, endpoint.template),
                status,
                code.to_owned(),
            ),
            Evidence::Rust("contract.rs", test),
        );
    };
    for endpoint in ENDPOINTS {
        add(
            endpoint,
            401,
            "unauthorized",
            "every_endpoint_answers_401_without_a_credential",
        );
        if !endpoint.internal {
            add(
                endpoint,
                403,
                "tenant_suspended",
                "every_public_endpoint_answers_403_for_a_suspended_tenant",
            );
            add(
                endpoint,
                429,
                "rate_limited",
                "every_public_endpoint_answers_429_past_the_rate_limit",
            );
        }
        if endpoint.template != "/health/detail" {
            add(
                endpoint,
                413,
                "payload_too_large",
                "every_endpoint_answers_413_over_the_body_cap",
            );
        }
        if endpoint.store {
            add(
                endpoint,
                500,
                "internal_error",
                "every_store_backed_endpoint_answers_500_when_the_store_fails",
            );
        }
    }
    out
}

fn matrix() -> BTreeMap<Pair, Vec<Evidence>> {
    let mut out: BTreeMap<Pair, Vec<Evidence>> = BTreeMap::new();
    for (pair, evidence) in recorded().into_iter().chain(middleware()) {
        out.entry(pair).or_default().push(evidence);
    }
    for (endpoint, status, code, file, test) in RUST {
        out.entry(((*endpoint).to_owned(), *status, (*code).to_owned()))
            .or_default()
            .push(Evidence::Rust(file, test));
    }
    for evidence in out.values_mut() {
        evidence.sort();
        evidence.dedup();
    }
    out
}

fn render(matrix: &BTreeMap<Pair, Vec<Evidence>>) -> String {
    let mut out = String::from(
        "# API error-code contract matrix\n\n\
         Generated by `crates/maverick-api/tests/contract.rs` (bead 5.17). Every (endpoint, status, code) pair \
         the API returns, and the test that pins it. `replay` rows are recorded Go exchanges that \
         `tests/replay.rs` compares byte for byte. `TOOL` rows are MCP tool results with `isError: true`. \
         Regenerate with `MAVERICK_UPDATE_CONTRACT=1 cargo test -p maverick-api --test contract`.\n\n\
         | Endpoint | Status | Code | Test |\n|---|---|---|---|\n",
    );
    for ((endpoint, status, code), evidence) in matrix {
        let tests: Vec<String> = evidence
            .iter()
            .map(|e| match e {
                Evidence::Replay(test, seq) => format!("replay `{test}` #{seq}"),
                Evidence::Rust(file, test) => format!("`{file}` `{test}`"),
            })
            .collect();
        out.push_str(&format!(
            "| `{endpoint}` | {status} | `{code}` | {} |\n",
            tests.join("; ")
        ));
    }
    out
}

#[test]
fn every_endpoint_code_pair_has_a_named_test() {
    let matrix = matrix();
    let missing: Vec<&(&str, u16, &str)> = REQUIRED
        .iter()
        .filter(|(endpoint, status, code)| {
            !matrix.contains_key(&((*endpoint).to_owned(), *status, (*code).to_owned()))
        })
        .collect();
    assert!(
        missing.is_empty(),
        "required pairs with no test: {missing:#?}"
    );
    // Every named Rust test exists.
    for evidence in matrix.values().flatten() {
        if let Evidence::Rust(file, test) = evidence {
            let source = std::fs::read_to_string(tests_dir().join(file)).unwrap_or_default();
            assert!(
                source.contains(&format!("fn {test}(")),
                "{file} has no test {test}"
            );
        }
    }
    // The rendered table is current.
    let path = tests_dir().join("CONTRACT.md");
    let want = render(&matrix);
    if std::env::var_os("MAVERICK_UPDATE_CONTRACT").is_some() {
        std::fs::write(&path, &want).unwrap();
        return;
    }
    assert!(
        std::fs::read_to_string(&path).unwrap_or_default() == want,
        "tests/CONTRACT.md is stale: MAVERICK_UPDATE_CONTRACT=1 cargo test -p maverick-api --test contract"
    );
}
