//! The MCP server on real Postgres (reference Part 5 §9 and §13.13; D34). Wire bytes
//! are compared with `parity/fixtures/api/mcp.json`; tool results are compared with the
//! REST twins.

use std::sync::{Arc, LazyLock};

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request};
use chrono::Utc;
use http_body_util::BodyExt;
use maverick_api::{
    ApiState, AuthState, HealthState, Identity, PgAuthBackend, RateLimiter, mcp, public_router_with,
};
use maverick_domain::keys;
use maverick_store::catalog::{NewModelColumn, create_model_column};
use maverick_store::measurement::{AggregateUpsert, upsert_aggregate};
use maverick_store::{JsonRawValue, MIGRATOR, corpus, tenancy};
use maverick_testdb::Schema;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));
static FIXTURE: LazyLock<Value> =
    LazyLock::new(|| parity_support::fixture("api", "mcp.json").expect("mcp fixture"));

const CORPUS: &str = "2026-07-01";
const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"contract-test","version":"v1"},"protocolVersion":"2025-11-25","capabilities":{"roots":{"listChanged":true}}}}"#;
const INITIALIZED: &str = r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#;

macro_rules! db {
    () => {{
        let Some(db) = maverick_testdb::database(&SCHEMA).await else {
            return;
        };
        db
    }};
}

fn raw(text: &str) -> Box<JsonRawValue> {
    JsonRawValue::from_string(text.to_owned()).unwrap()
}

async fn column(pool: &PgPool, id: &str) {
    let empty = raw("{}");
    let model = format!("model-{id}");
    create_model_column(
        pool,
        &NewModelColumn {
            id,
            provider: "anthropic",
            model_id: &model,
            endpoint: "direct",
            settings: &empty,
            status: "active",
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

/// An active corpus and two measured columns for factual_qa and structured_extraction.
async fn matrix(pool: &PgPool) {
    corpus::upsert_corpus_version(pool, CORPUS, "", 0, false)
        .await
        .unwrap();
    corpus::activate_corpus_version(pool, CORPUS).await.unwrap();
    let today = Utc::now().date_naive();
    for (id, quality, cost) in [("col_a", 0.91_f32, 0.004_f32), ("col_b", 0.95, 0.02)] {
        column(pool, id).await;
        for capability in ["factual_qa", "structured_extraction"] {
            let mut rows = vec![
                (3_i16, "quality", quality),
                (3, "cost_per_run_usd", cost),
                (3, "latency_p95_ms", 1800.0),
            ];
            for (tier, rate) in (1..).zip([0.98_f32, 0.96, 0.94, 0.9, 0.88]) {
                rows.push((tier, "pass_rate", rate));
            }
            for (tier, dimension, ewma) in rows {
                upsert_aggregate(
                    pool,
                    &AggregateUpsert {
                        model_column_id: id,
                        capability,
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
    }
}

/// A tenant whose only (default) pool is `pool_name`, and a key for it.
async fn tenant(pool: &PgPool, id: &str, pool_name: &str) -> String {
    tenancy::create_tenant(pool, id, id, &[7; 32])
        .await
        .unwrap();
    tenancy::create_pool(
        pool,
        &format!("pool_{id}"),
        id,
        pool_name,
        &["col_a".to_owned(), "col_b".to_owned()],
        true,
    )
    .await
    .unwrap();
    let minted = keys::mint_key();
    tenancy::create_api_key(
        pool,
        &format!("key_{id}"),
        id,
        &minted.key_hash,
        &minted.last4,
        "k",
    )
    .await
    .unwrap();
    minted.raw_key
}

fn router(pool: &PgPool, rpm: u32) -> Router {
    let auth = AuthState::new(
        Arc::new(PgAuthBackend::new(pool.clone())),
        keys::parse_internal_tokens("ops:alpha").unwrap(),
        RateLimiter::new(rpm),
    );
    public_router_with(
        auth,
        ApiState::with_health(pool.clone(), HealthState::detached()),
    )
}

struct Reply {
    status: u16,
    session: Option<String>,
    headers: HeaderMap,
    body: String,
}

async fn send(
    router: &Router,
    method: &str,
    path: &str,
    key: Option<&str>,
    session: Option<&str>,
    body: &str,
) -> Reply {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(key) = key {
        request = request.header("authorization", format!("Bearer {key}"));
    }
    if let Some(session) = session {
        request = request.header("mcp-session-id", session);
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::from(body.to_owned())).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let session = headers
        .get("mcp-session-id")
        .map(|v| v.to_str().unwrap().to_owned());
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
        session,
        headers,
        body: String::from_utf8(bytes.to_vec()).unwrap(),
    }
}

/// The JSON-RPC message inside one SSE frame.
fn frame(reply: &Reply) -> Value {
    let data = reply
        .body
        .strip_prefix("event: message\ndata: ")
        .and_then(|rest| rest.strip_suffix("\n\n"))
        .unwrap_or_else(|| panic!("not one SSE frame: {:?}", reply.body));
    serde_json::from_str(data).unwrap()
}

/// Opens a session with `key` and sends the initialized notification.
async fn open(router: &Router, key: &str) -> String {
    let reply = send(router, "POST", "/mcp", Some(key), None, INITIALIZE).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let session = reply.session.expect("session id");
    let ack = send(
        router,
        "POST",
        "/mcp",
        Some(key),
        Some(&session),
        INITIALIZED,
    )
    .await;
    assert_eq!((ack.status, ack.body.as_str()), (202, ""));
    session
}

fn call(id: u64, tool: &str, arguments: &Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": tool, "arguments": arguments}})
        .to_string()
}

async fn tool(router: &Router, key: &str, session: &str, name: &str, arguments: &Value) -> Value {
    let reply = send(
        router,
        "POST",
        "/mcp",
        Some(key),
        Some(session),
        &call(9, name, arguments),
    )
    .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    frame(&reply)["result"].clone()
}

fn text(result: &Value) -> &str {
    result["content"][0]["text"].as_str().unwrap()
}

/// parity: api.TestMCPListsExactlyTheSpecTools
#[tokio::test]
async fn lists_exactly_the_spec_tools() {
    let db = db!();
    let pool = db.pool().await;
    let key = tenant(&pool, "ten_list", "default").await;
    let router = router(&pool, 1000);
    let initialize = &FIXTURE["initialize"];
    let request = initialize["request_body_exact"].as_str().unwrap();
    assert_eq!(request, INITIALIZE);
    let reply = send(&router, "POST", "/mcp", Some(&key), None, request).await;
    assert_eq!(reply.status, 200);
    assert_eq!(
        reply.body,
        initialize["response_body_exact"].as_str().unwrap()
    );
    for (name, value) in initialize["response_headers_subset"].as_object().unwrap() {
        if name != "Mcp-Session-Id" {
            assert_eq!(
                reply.headers[name.as_str()],
                value.as_str().unwrap(),
                "{name}"
            );
        }
    }
    let session = reply.session.expect("session id");
    assert_eq!(session.len(), 26);
    let ack = send(
        &router,
        "POST",
        "/mcp",
        Some(&key),
        Some(&session),
        INITIALIZED,
    )
    .await;
    let notified = &FIXTURE["initialized_notification"];
    assert_eq!(
        u64::from(ack.status),
        notified["response_status"].as_u64().unwrap()
    );
    assert_eq!(ack.body, notified["response_body"].as_str().unwrap());
    let list = &FIXTURE["tools_list"];
    let reply = send(
        &router,
        "POST",
        "/mcp",
        Some(&key),
        Some(&session),
        list["request_body_exact"].as_str().unwrap(),
    )
    .await;
    assert_eq!(reply.body, list["response_body_exact"].as_str().unwrap());
    let names: Vec<String> = frame(&reply)["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names, ["get_policy", "list_models", "recommend_model"]);
    let reply = send(&router, "DELETE", "/mcp", Some(&key), Some(&session), "").await;
    let delete = &FIXTURE["session_delete"];
    assert_eq!(
        u64::from(reply.status),
        delete["response_status"].as_u64().unwrap()
    );
    // The closed session is gone.
    let reply = send(
        &router,
        "POST",
        "/mcp",
        Some(&key),
        Some(&session),
        list["request_body_exact"].as_str().unwrap(),
    )
    .await;
    assert_eq!(reply.status, 404);
}

/// parity: api.TestMCPEndpointRequiresAuth
#[tokio::test]
async fn the_endpoint_requires_auth() {
    let db = db!();
    let pool = db.pool().await;
    let router = router(&pool, 1000);
    let unauthorized = "{\"error\":{\"code\":\"unauthorized\",\"message\":\"missing or invalid API key\",\"details\":{}}}\n";
    for key in [None, Some("garbage")] {
        for body in ["{}", INITIALIZE] {
            let reply = send(&router, "POST", "/mcp", key, None, body).await;
            assert_eq!((reply.status, reply.body.as_str()), (401, unauthorized));
        }
    }
}

/// parity: api.TestMCPRateLimitAppliesToMCPRequests
#[tokio::test]
async fn the_rate_limit_applies_to_mcp_requests() {
    let db = db!();
    let pool = db.pool().await;
    let key = tenant(&pool, "ten_rl", "default").await;
    let router = router(&pool, 2);
    let session = open(&router, &key).await;
    let reply = send(
        &router,
        "POST",
        "/mcp",
        Some(&key),
        Some(&session),
        &call(2, "list_models", &json!({})),
    )
    .await;
    assert_eq!(reply.status, 429);
    assert!(reply.body.contains("\"rate_limited\""));
}

/// parity: api.TestMCPToolCallsResolveTenantPerRequest
#[tokio::test]
async fn tool_calls_resolve_the_tenant_per_request() {
    let db = db!();
    let pool = db.pool().await;
    matrix(&pool).await;
    let alpha = tenant(&pool, "ten_alpha", "alpha-pool").await;
    let beta = tenant(&pool, "ten_beta", "beta-pool").await;
    let router = router(&pool, 1000);
    let session = open(&router, &alpha).await;
    let arguments = json!({"workload": {"capability": "factual_qa"}});
    let result = tool(&router, &alpha, &session, "recommend_model", &arguments).await;
    assert_eq!(result["structuredContent"]["pool"], "alpha-pool");
    let result = tool(&router, &beta, &session, "recommend_model", &arguments).await;
    assert_eq!(result["structuredContent"]["pool"], "beta-pool");
    let rows: Vec<(String, String)> =
        sqlx::query_as("select tenant_id, api_key_id from recommendations order by created_at, id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        rows,
        [
            ("ten_alpha".to_owned(), "key_ten_alpha".to_owned()),
            ("ten_beta".to_owned(), "key_ten_beta".to_owned()),
        ]
    );
}

/// parity: api.TestMCPRecommendModelMatchesRESTAndWritesTraceRow
#[tokio::test]
async fn recommend_model_matches_rest_and_writes_the_trace_row() {
    let db = db!();
    let pool = db.pool().await;
    matrix(&pool).await;
    let key = tenant(&pool, "ten_rec", "default").await;
    let router = router(&pool, 1000);
    let request = json!({"workload": {"capability": "structured_extraction", "difficulty": 3, "domain_tags": ["legal"]}, "mode": "balanced"});
    let rest = send(
        &router,
        "POST",
        "/v1/recommend",
        Some(&key),
        None,
        &request.to_string(),
    )
    .await;
    assert_eq!(rest.status, 200, "{}", rest.body);
    let rest_id = serde_json::from_str::<Value>(&rest.body).unwrap()["recommendation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let session = open(&router, &key).await;
    let result = tool(&router, &key, &session, "recommend_model", &request).await;
    let mcp_id = result["structuredContent"]["recommendation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(mcp_id, rest_id);
    assert!(mcp_id.starts_with("rec_"));
    assert_eq!(
        format!("{}\n", text(&result)).replace(&mcp_id, &rest_id),
        rest.body
    );
    assert_eq!(
        result["structuredContent"],
        serde_json::from_str::<Value>(text(&result)).unwrap()
    );
    let rows: Vec<(String, String, Option<String>, String, String)> = sqlx::query_as(
        "select tenant_id, api_key_id, prompt_hash, matrix_version, candidates::text
         from recommendations where id = any($1) order by id",
    )
    .bind([rest_id.clone(), mcp_id.clone()])
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], rows[1]);
    assert_eq!(rows[0].2, None);
}

/// parity: api.TestMCPGetPolicyMatchesREST
#[tokio::test]
async fn get_policy_matches_rest() {
    let db = db!();
    let pool = db.pool().await;
    matrix(&pool).await;
    let key = tenant(&pool, "ten_pol", "default").await;
    let router = router(&pool, 1000);
    let session = open(&router, &key).await;
    for (query, arguments) in [
        (
            "?capability=structured_extraction",
            json!({"capability": "structured_extraction"}),
        ),
        ("", json!({})),
        ("?pool=default", json!({"pool": "default"})),
    ] {
        let rest = send(
            &router,
            "GET",
            &format!("/v1/policies{query}"),
            Some(&key),
            None,
            "",
        )
        .await;
        assert_eq!(rest.status, 200, "{}", rest.body);
        let result = tool(&router, &key, &session, "get_policy", &arguments).await;
        assert_eq!(format!("{}\n", text(&result)), rest.body, "{arguments}");
    }
}

/// parity: api.TestMCPListModelsMatchesREST
#[tokio::test]
async fn list_models_matches_rest() {
    let db = db!();
    let pool = db.pool().await;
    matrix(&pool).await;
    let key = tenant(&pool, "ten_models", "default").await;
    let router = router(&pool, 1000);
    let session = open(&router, &key).await;
    let rest = send(&router, "GET", "/v1/models", Some(&key), None, "").await;
    assert_eq!(rest.status, 200);
    let result = tool(&router, &key, &session, "list_models", &json!({})).await;
    assert_eq!(format!("{}\n", text(&result)), rest.body);
}

/// parity: api.TestMCPRecommendModelBadArgs
#[tokio::test]
async fn recommend_model_bad_arguments_are_tool_errors() {
    let db = db!();
    let pool = db.pool().await;
    matrix(&pool).await;
    let key = tenant(&pool, "ten_bad", "default").await;
    let router = router(&pool, 1000);
    let session = open(&router, &key).await;
    let _ = parity_support::divergence!("D34");
    for case in FIXTURE["tool_error_cases"].as_array().unwrap() {
        let request = case["request_body_exact"].as_str().unwrap();
        let reply = send(&router, "POST", "/mcp", Some(&key), Some(&session), request).await;
        assert_eq!(reply.status, 200);
        let result = frame(&reply)["result"].clone();
        assert_eq!(result["isError"], true, "{request}");
        assert_eq!(result["content"].as_array().unwrap().len(), 1);
        let want = case["response_result"]["content"][0]["text"]
            .as_str()
            .unwrap();
        let got = text(&result);
        if want.starts_with("validating \"arguments\": ") {
            // D34: the shape and the prefix are Go's; the rest names the property.
            assert!(got.starts_with("validating \"arguments\": "), "{got}");
            let named = if want.contains("\"model\"") {
                "model"
            } else if want.contains("difficulty") {
                "difficulty"
            } else {
                "workload"
            };
            assert!(got.contains(named), "{got} should name {named}");
        } else {
            assert_eq!(got, want, "{request}");
        }
    }
}

/// Spec Appendix B: the two privacy codes reach an MCP caller as tool errors, with the
/// code and message of the REST answer (the contract matrix, bead 5.17).
#[tokio::test]
async fn recommend_model_surfaces_the_privacy_codes() {
    let db = db!();
    let pool = db.pool().await;
    matrix(&pool).await;
    let key = tenant(&pool, "ten_zr", "default").await;
    tenancy::upsert_tenant_constraints(&pool, "ten_zr", None, None, true, false)
        .await
        .unwrap();
    let router = router(&pool, 1000);
    let session = open(&router, &key).await;
    for (arguments, status, code) in [
        (
            json!({"workload": {"prompt": "Summarize the contract."}}),
            422,
            "zero_retention_requires_declared",
        ),
        (
            json!({"workload": {"capability": "factual_qa"}}),
            409,
            "no_compliant_columns",
        ),
    ] {
        let rest = send(
            &router,
            "POST",
            "/v1/recommend",
            Some(&key),
            None,
            &arguments.to_string(),
        )
        .await;
        assert_eq!(rest.status, status, "{}", rest.body);
        let envelope: Value = serde_json::from_str(&rest.body).unwrap();
        assert_eq!(envelope["error"]["code"], code);
        let result = tool(&router, &key, &session, "recommend_model", &arguments).await;
        assert_eq!(result["isError"], true);
        assert_eq!(
            text(&result),
            format!("{code}: {}", envelope["error"]["message"].as_str().unwrap())
        );
    }
}

/// parity: api.TestMCPStdioStyleTransportBindsRunContextIdentity
#[tokio::test]
async fn the_stdio_transport_binds_the_run_identity() {
    let db = db!();
    let pool = db.pool().await;
    matrix(&pool).await;
    tenant(&pool, "ten_stdio", "stdio-pool").await;
    let api = ApiState::with_health(pool.clone(), HealthState::detached());
    let identity = Identity {
        tenant_id: "ten_stdio".to_owned(),
        api_key_id: "key_ten_stdio".to_owned(),
    };
    let input = format!(
        "{INITIALIZE}\n{INITIALIZED}\n\n{}\nnot json\n",
        call(
            2,
            "recommend_model",
            &json!({"workload": {"capability": "factual_qa"}})
        )
    );
    let mut output = Vec::new();
    mcp::serve_lines(&api, &identity, input.as_bytes(), &mut output)
        .await
        .unwrap();
    let lines: Vec<Value> = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 3, "initialize, the call, and the parse error");
    assert_eq!(lines[0]["result"]["serverInfo"]["name"], "model-maverick");
    assert_eq!(
        lines[1]["result"]["structuredContent"]["pool"],
        "stdio-pool"
    );
    assert_eq!(lines[2]["error"]["code"], -32700);
    let tenant: String = sqlx::query_scalar("select tenant_id from recommendations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(tenant, "ten_stdio");
}

/// A request without a session id, or with an unknown one, is refused before dispatch.
#[tokio::test]
async fn requests_need_a_live_session() {
    let db = db!();
    let pool = db.pool().await;
    let key = tenant(&pool, "ten_sess", "default").await;
    let router = router(&pool, 1000);
    let list = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#;
    let reply = send(&router, "POST", "/mcp", Some(&key), None, list).await;
    assert_eq!(reply.status, 400);
    let reply = send(&router, "POST", "/mcp", Some(&key), Some("NOPE"), list).await;
    assert_eq!(reply.status, 404);
    let reply = send(&router, "GET", "/mcp", Some(&key), None, "").await;
    assert_eq!(reply.status, 405);
}
