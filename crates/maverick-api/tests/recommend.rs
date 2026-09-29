//! `POST /v1/recommend` (declared path) and `GET /v1/checks/{ref...}` on real Postgres
//! (reference Part 5 §6.5, §6.6, §13.4; golden
//! `parity/fixtures/api/testdata/recommend_declared.golden.json`).
//!
//! The golden is the Go shape. The Rust response adds D8 and D9 fields, so the test
//! inserts them into the golden text before comparing bytes.

use std::sync::{Arc, LazyLock};

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use chrono::{Duration, NaiveDate, Utc};
use maverick_api::{
    ApiState, AuthState, HealthState, Identity, RateLimiter, StaticAuthBackend, public_router_with,
};
use maverick_domain::keys;
use maverick_store::alerts;
use maverick_store::catalog::{self, NewModelColumn};
use maverick_store::decisions;
use maverick_store::measurement::{self, AggregateUpsert};
use maverick_store::{Decimal, JsonRawValue, MIGRATOR, corpus, tenancy};
use maverick_testdb::{Schema, TestDatabase};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

const TENANT: &str = "ten_recommend";
const KEY: &str = "key_recommend";
const CORPUS: &str = "2026-07-01";

fn router_as(api: ApiState, backend: StaticAuthBackend) -> Router {
    let auth = AuthState::new(
        Arc::new(backend),
        keys::parse_internal_tokens("ops:alpha").unwrap(),
        RateLimiter::new(10_000),
    );
    public_router_with(auth, api)
}

fn tenant_key() -> StaticAuthBackend {
    StaticAuthBackend::authenticated(Identity {
        tenant_id: TENANT.to_owned(),
        api_key_id: KEY.to_owned(),
    })
}

fn router(pool: &PgPool) -> Router {
    let health = HealthState::with_providers(pool.clone(), Vec::new());
    router_as(ApiState::with_health(pool.clone(), health), tenant_key())
}

async fn call(router: Router, method: Method, path: &str, body: &str) -> (StatusCode, String) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, "Bearer mk_live_test")
        .body(Body::from(body.to_owned()))
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

async fn recommend(pool: &PgPool, body: &str) -> (StatusCode, String) {
    let (status, text) = call(router(pool), Method::POST, "/v1/recommend", body).await;
    eprintln!("POST /v1/recommend {body} -> {status} {text}");
    (status, text)
}

fn parse(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("not JSON ({e}): {text}"))
}

fn raw(text: &str) -> Box<JsonRawValue> {
    JsonRawValue::from_string(text.to_owned()).unwrap()
}

async fn database() -> Option<(TestDatabase, PgPool)> {
    let db = maverick_testdb::database(&SCHEMA).await?;
    let pool = db.pool().await;
    tenancy::create_tenant(&pool, TENANT, "Acme", &[1; 32])
        .await
        .unwrap();
    tenancy::create_api_key(&pool, KEY, TENANT, b"hash", "abcd", "")
        .await
        .unwrap();
    Some((db, pool))
}

/// One column. `aliases` is its `execution_aliases`; `retention` its stored retention.
async fn column(
    pool: &PgPool,
    id: &str,
    provider: &str,
    model: &str,
    aliases: Option<&str>,
    retention: Option<(&str, Option<chrono::DateTime<Utc>>)>,
) {
    let empty = raw("{}");
    let aliases = aliases.map(raw);
    catalog::create_model_column(
        pool,
        &NewModelColumn {
            id,
            provider,
            model_id: model,
            endpoint: "direct",
            settings: &empty,
            status: "active",
            pricing: &empty,
            quality_source_column_id: None,
            cold_start_until: None,
            deprecation_date: None,
            execution_aliases: aliases.as_deref(),
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

async fn pool_of(pool: &PgPool, name: &str, ids: &[&str], default: bool) {
    let ids: Vec<String> = ids.iter().map(|id| (*id).to_owned()).collect();
    tenancy::create_pool(pool, &format!("pool_{name}"), TENANT, name, &ids, default)
        .await
        .unwrap();
}

fn night() -> NaiveDate {
    Utc::now().date_naive()
}

async fn aggregate(
    pool: &PgPool,
    column: &str,
    capability: &str,
    tier: i16,
    dimension: &str,
    ewma: f32,
    as_of: NaiveDate,
) {
    measurement::upsert_aggregate(
        pool,
        &AggregateUpsert {
            model_column_id: column,
            capability,
            difficulty_tier: tier,
            dimension,
            ewma,
            variance: 0.0,
            n: 20,
            measured: true,
            matrix_version: CORPUS,
            as_of,
        },
    )
    .await
    .unwrap();
}

/// Pass rates for tiers 1.. of a column.
async fn pass_rates(pool: &PgPool, column: &str, capability: &str, rates: &[f32]) {
    for (tier, rate) in (1..).zip(rates) {
        aggregate(pool, column, capability, tier, "pass_rate", *rate, night()).await;
    }
}

/// Quality (0..1), cost, and p95 latency of a column at one tier.
async fn cell(pool: &PgPool, column: &str, capability: &str, tier: i16, q: f32, c: f32, l: f32) {
    aggregate(pool, column, capability, tier, "quality", q, night()).await;
    aggregate(
        pool,
        column,
        capability,
        tier,
        "cost_per_run_usd",
        c,
        night(),
    )
    .await;
    aggregate(pool, column, capability, tier, "latency_p95_ms", l, night()).await;
}

async fn active_corpus(pool: &PgPool) {
    corpus::upsert_corpus_version(pool, CORPUS, "", 0, false)
        .await
        .unwrap();
    corpus::activate_corpus_version(pool, CORPUS).await.unwrap();
}

async fn trace_rows(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM recommendations")
        .fetch_one(pool)
        .await
        .unwrap()
}

/// D8/D9: the Go golden text with the Rust fields inserted in their positions.
fn with_rust_fields(go: &str, retention: &str) -> String {
    go.replace(
        "\"execution_aliases\":{},",
        &format!(
            "\"execution_aliases\":{{}},\"settings\":{{}},\"retention_mode\":\"{retention}\","
        ),
    )
    .replace(
        "\"clears_difficulty\":true,",
        "\"clears_difficulty\":true,\"low_availability\":false,",
    )
    .replace(
        "\"measured\":true}",
        "\"measured\":true,\"zero_retention\":false}",
    )
}

// parity: api.TestRecommendRequiresAuth
// parity: api.TestChecksRequiresAuth
#[tokio::test]
async fn recommend_and_checks_require_auth() {
    let router = router_as(ApiState::detached(), StaticAuthBackend::missing());
    let unauthorized = "{\"error\":{\"code\":\"unauthorized\",\"message\":\"missing or invalid API key\",\"details\":{}}}\n";
    let (status, body) = call(
        router.clone(),
        Method::POST,
        "/v1/recommend",
        r#"{"workload":{"capability":"factual_qa"}}"#,
    )
    .await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::UNAUTHORIZED, unauthorized)
    );
    let (status, body) = call(router, Method::GET, "/v1/checks/extraction/v1", "").await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::UNAUTHORIZED, unauthorized)
    );
}

// parity: api.TestRecommendDeclaredResponseByteExact
#[tokio::test]
async fn declared_response_is_byte_exact() {
    let Some((db, pool)) = database().await else {
        return;
    };
    active_corpus(&pool).await;
    let cap = "structured_extraction";
    for (id, provider, model, rates, (q, c, l)) in [
        (
            "col_sonnet",
            "anthropic",
            "claude-sonnet-4-6",
            [0.97, 0.95, 0.91, 0.88, 0.86],
            (0.941, 0.0042, 2100.0),
        ),
        (
            "col_opus",
            "anthropic",
            "claude-opus-4-8",
            [0.99, 0.98, 0.96, 0.93, 0.90],
            (0.953, 0.021, 3900.0),
        ),
        (
            "col_gemini",
            "google",
            "gemini-3.1-pro",
            [0.95, 0.92, 0.89, 0.87, 0.86],
            (0.928, 0.0038, 1800.0),
        ),
    ] {
        column(&pool, id, provider, model, None, None).await;
        pass_rates(&pool, id, cap, &rates).await;
        cell(&pool, id, cap, 3, q, c, l).await;
    }
    pool_of(
        &pool,
        "vertex-only",
        &["col_sonnet", "col_opus", "col_gemini"],
        true,
    )
    .await;

    let (status, body) = recommend(
        &pool,
        r#"{"workload":{"capability":"structured_extraction","difficulty":3,"domain_tags":["legal"]},"pool":"vertex-only","mode":"balanced"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let id = parse(&body)["recommendation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(id.starts_with("rec_"), "{id}");
    let golden =
        include_str!("../../../parity/fixtures/api/testdata/recommend_declared.golden.json")
            // The checked-in file ends with a newline; the response is its JSON plus one.
            .trim_end()
            .replace("{{COL_SONNET}}", "col_sonnet")
            .replace("{{COL_OPUS}}", "col_opus")
            .replace("{{COL_GEMINI}}", "col_gemini")
            .replace("{{REC_ID}}", &id);
    assert_eq!(body, with_rust_fields(&golden, "unknown") + "\n");
    assert_eq!(trace_rows(&pool).await, 1);
    db.close().await;
}

// parity: api.TestRecommendDifficultyDefaultsOnColdStart
#[tokio::test]
async fn difficulty_defaults_on_cold_start() {
    let Some((db, pool)) = database().await else {
        return;
    };
    column(&pool, "col_a", "anthropic", "claude-a", None, None).await;
    pool_of(&pool, "default", &["col_a"], true).await;
    let (status, body) = recommend(
        &pool,
        r#"{"workload":{"capability":"summarization_synthesis"}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v = parse(&body);
    assert_eq!(
        v["classification"],
        json!({"capability":"summarization_synthesis","difficulty":3,"domain_tags":[],
               "high_stakes":false,"confidence":1,"method":"declared","difficulty_source":"default"})
    );
    let entries = v["recommendations"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["score"], Value::Null);
    assert_eq!(entries[0]["clears_difficulty"], false);
    assert_eq!(
        entries[0]["reason"],
        "below difficulty ceiling \u{2014} best available"
    );
    assert_eq!(v["measured"], false);
    assert_eq!(v["constraints_met"], true);
    assert_eq!(v["violations"], json!([]));
    assert_eq!(v["mode"], "balanced");
    assert_eq!(v["matrix_version"], "");
    assert!(v.get("prompt_hash").is_none());
    db.close().await;
}

// parity: api.TestRecommendEntriesCarryExecutionAliases
#[tokio::test]
async fn entries_carry_execution_aliases_and_settings_verbatim() {
    let Some((db, pool)) = database().await else {
        return;
    };
    column(
        &pool,
        "col_a",
        "anthropic",
        "claude-a",
        Some(r#"{"openrouter":"anthropic/claude-recommend-alias","vertex":"a"}"#),
        None,
    )
    .await;
    sqlx::query("UPDATE model_columns SET settings = '{\"thinking\": true, \"effort\": 3.0}'")
        .execute(&pool)
        .await
        .unwrap();
    pool_of(&pool, "default", &["col_a"], true).await;
    let (status, body) = recommend(&pool, r#"{"workload":{"capability":"factual_qa"}}"#).await;
    assert_eq!(status, StatusCode::OK);
    // jsonb order (shorter keys first) and number spelling, compacted (reference §3.9).
    assert!(
        body.contains(
            r#""execution_aliases":{"vertex":"a","openrouter":"anthropic/claude-recommend-alias"},"settings":{"effort":3.0,"thinking":true},"retention_mode":"unknown","#
        ),
        "{body}"
    );
    db.close().await;
}

// parity: api.TestRecommendHighStakesRequiresListedTag
#[tokio::test]
async fn high_stakes_requires_a_listed_tag() {
    let Some((db, pool)) = database().await else {
        return;
    };
    column(&pool, "col_a", "anthropic", "claude-a", None, None).await;
    pool_of(&pool, "default", &["col_a"], true).await;
    for (tags, want) in [
        (r#"["internal","support"]"#, false),
        (r#"["medical"]"#, true),
    ] {
        let body = format!(r#"{{"workload":{{"capability":"factual_qa","domain_tags":{tags}}}}}"#);
        let (status, text) = recommend(&pool, &body).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            parse(&text)["classification"]["high_stakes"],
            want,
            "{tags}"
        );
    }
    db.close().await;
}

// parity: api.TestRecommendRequestErrors
#[tokio::test]
async fn request_errors_answer_422_and_write_nothing() {
    let Some((db, pool)) = database().await else {
        return;
    };
    column(&pool, "col_in", "anthropic", "claude-in", None, None).await;
    column(&pool, "col_out", "openai", "gpt-out", None, None).await;
    pool_of(&pool, "default", &["col_in"], true).await;
    let cases: [(&str, &str, &str, Value); 21] = [
        (
            "{bad",
            "invalid_request",
            "malformed request body: ",
            json!({}),
        ),
        ("", "invalid_request", "request body is required", json!({})),
        (
            r#"{"workload":{"capability":"factual_qa"}}{"x":1}"#,
            "invalid_request",
            "request body has trailing content",
            json!({}),
        ),
        (
            r#"{"workload":{"capability":"factual_qa"},"model":"x"}"#,
            "invalid_request",
            "malformed request body: json: unknown field \"model\"",
            json!({}),
        ),
        (
            r#"{"workload":{"capability":"factual_qa","tier":2}}"#,
            "invalid_request",
            "malformed request body: json: unknown field \"tier\"",
            json!({}),
        ),
        (
            r#"{"workload":{"capability":"factual_qa"},"constraints":{"max_tokens":5}}"#,
            "invalid_request",
            "malformed request body: json: unknown field \"max_tokens\"",
            json!({}),
        ),
        (
            "{}",
            "invalid_request",
            "workload is required",
            json!({"field":"workload"}),
        ),
        (
            r#"{"workload":{}}"#,
            "invalid_request",
            "workload must carry exactly one of capability or prompt (spec §8.1)",
            json!({}),
        ),
        (
            r#"{"workload":{"capability":"factual_qa","prompt":"hi"}}"#,
            "invalid_request",
            "workload must carry exactly one of capability or prompt (spec §8.1)",
            json!({}),
        ),
        (
            r#"{"workload":{"prompt":""}}"#,
            "invalid_request",
            "workload.prompt must not be empty",
            json!({"field":"workload.prompt"}),
        ),
        (
            r#"{"workload":{"capability":"poetry"}}"#,
            "invalid_request",
            "workload.capability \"poetry\" is not a v1 capability",
            json!({"capability":"poetry"}),
        ),
        (
            r#"{"workload":{"capability":"factual_qa","difficulty":0}}"#,
            "invalid_request",
            "workload.difficulty 0 is out of range 1-5",
            json!({"difficulty":0}),
        ),
        (
            r#"{"workload":{"capability":"factual_qa","difficulty":6}}"#,
            "invalid_request",
            "workload.difficulty 6 is out of range 1-5",
            json!({"difficulty":6}),
        ),
        (
            r#"{"workload":{"capability":"factual_qa","difficulty":2.5}}"#,
            "invalid_request",
            "malformed request body: ",
            json!({}),
        ),
        (
            r#"{"workload":{"capability":"factual_qa"},"mode":"turbo"}"#,
            "invalid_request",
            "mode \"turbo\" is not one of quality, balanced, cost",
            json!({"mode":"turbo"}),
        ),
        (
            r#"{"workload":{"capability":"factual_qa"},"constraints":{"max_cost_per_run":0}}"#,
            "invalid_request",
            "constraints.max_cost_per_run must be greater than 0",
            json!({"field":"max_cost_per_run"}),
        ),
        (
            r#"{"workload":{"capability":"factual_qa"},"constraints":{"max_latency_ms":-5}}"#,
            "invalid_request",
            "constraints.max_latency_ms must be greater than 0",
            json!({"field":"max_latency_ms"}),
        ),
        (
            r#"{"workload":{"capability":"factual_qa"},"pool":"nope"}"#,
            "unknown_pool",
            "pool \"nope\" is not configured for this tenant",
            json!({"pool":"nope"}),
        ),
        (
            r#"{"workload":{"capability":"factual_qa"},"allowed_models":["definitely-not-a-model"]}"#,
            "unknown_model",
            "allowed_models entry \"definitely-not-a-model\" is not in the model catalog",
            json!({"entry":"definitely-not-a-model"}),
        ),
        (
            r#"{"workload":{"capability":"factual_qa"},"allowed_models":["gpt-out"]}"#,
            "model_not_in_pool",
            "allowed_models entry \"gpt-out\" is not in pool \"default\"",
            json!({"entry":"gpt-out","pool":"default"}),
        ),
        (
            r#"{"workload":{"capability":"factual_qa","prompt":"x","difficulty":2}}"#,
            "invalid_request",
            "workload must carry exactly one of capability or prompt (spec §8.1)",
            json!({}),
        ),
    ];
    for (body, code, message, details) in cases {
        let (status, text) = recommend(&pool, body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        let error = &parse(&text)["error"];
        assert_eq!(error["code"], code, "{body}");
        let got = error["message"].as_str().unwrap();
        // D25: after `malformed request body: ` the text is not a parity target.
        if message.ends_with(": ") {
            assert!(got.starts_with(message), "{body}: {got}");
        } else {
            assert_eq!(got, message, "{body}");
        }
        assert_eq!(error["details"], details, "{body}");
    }
    assert_eq!(
        trace_rows(&pool).await,
        0,
        "a failed call writes no trace row"
    );
    db.close().await;
}

// parity: api.TestRecommendTraceRowMixedSurvivorsAndExclusions
#[tokio::test]
async fn trace_row_records_survivors_then_exclusions() {
    let Some((db, pool)) = database().await else {
        return;
    };
    active_corpus(&pool).await;
    let cap = "factual_qa";
    for (id, rates, (q, c, l)) in [
        ("col_cheap", vec![0.96, 0.95], (0.90, 0.001, 900.0)),
        ("col_mid", vec![0.95, 0.94], (0.93, 0.005, 3000.0)),
        ("col_expensive", vec![0.99, 0.99], (0.97, 0.03, 2500.0)),
    ] {
        column(&pool, id, "anthropic", id, None, None).await;
        pass_rates(&pool, id, cap, &rates).await;
        cell(&pool, id, cap, 2, q, c, l).await;
    }
    column(&pool, "col_nodata", "anthropic", "nodata", None, None).await;
    column(&pool, "col_outside", "openai", "outside", None, None).await;
    pool_of(
        &pool,
        "default",
        &["col_cheap", "col_mid", "col_expensive", "col_nodata"],
        true,
    )
    .await;
    tenancy::upsert_tenant_constraints(&pool, TENANT, Some(Decimal::new(1, 2)), None, false, false)
        .await
        .unwrap();

    let body = r#"{"workload":{"capability":"factual_qa","difficulty":2},"constraints":{"max_latency_ms":2000}}"#;
    let (status, text) = recommend(&pool, body).await;
    assert_eq!(status, StatusCode::OK);
    let response = parse(&text);
    let ids: Vec<&str> = response["recommendations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["column_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["col_cheap", "col_mid"]);
    assert_eq!(response["constraints_met"], true);
    assert_eq!(response["violations"], json!([]));

    let id = response["recommendation_id"].as_str().unwrap();
    let row = decisions::get_recommendation(&pool, id).await.unwrap();
    assert_eq!(row.tenant_id, TENANT);
    assert_eq!(row.api_key_id, KEY);
    assert_eq!(row.prompt_hash, None);
    assert_eq!(row.matrix_version, CORPUS);
    let request = parse(row.request.get());
    assert_eq!(request, parse(body));
    assert_eq!(parse(row.response.get()), response);
    assert_eq!(parse(row.classification.get()), response["classification"]);
    let candidates = parse(row.candidates.get());
    eprintln!("candidates {candidates}");
    let candidates = candidates.as_array().unwrap();
    assert_eq!(candidates.len(), 5);
    let cheap = &candidates[0];
    assert_eq!(cheap["column_id"], "col_cheap");
    assert_eq!(cheap["status"], "survivor");
    assert_eq!(cheap["rank"], 0);
    assert!((cheap["quality"].as_f64().unwrap() - 90.0).abs() <= 0.05);
    assert!((cheap["pass_rate"].as_f64().unwrap() - 0.95).abs() <= 1e-4);
    assert_eq!(
        cheap["cost_per_run_usd"].as_f64().unwrap(),
        f64::from(0.001f32)
    );
    assert_eq!(cheap["latency_p95_ms"], 900);
    assert!(cheap["v"].is_number());
    for flag in ["leader", "measured", "clears_difficulty", "prefs_satisfied"] {
        assert_eq!(cheap[flag], true, "{flag}");
    }
    let mid = &candidates[1];
    assert_eq!(
        (mid["column_id"].as_str(), mid["rank"].as_u64()),
        (Some("col_mid"), Some(1))
    );
    assert_eq!(mid["prefs_satisfied"], false);
    let exclusions: Vec<(&str, &str)> = candidates[2..]
        .iter()
        .map(|c| {
            (
                c["column_id"].as_str().unwrap(),
                c["exclusion_reason"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        exclusions,
        [
            ("col_outside", "not_in_pool"),
            ("col_expensive", "standing_constraint:max_cost_per_run"),
            ("col_nodata", "no_data"),
        ]
    );
    assert!(candidates[3].get("rank").is_none() && candidates[3].get("quality").is_none());
    db.close().await;
}

// parity: api.TestRecommendPickViolatingPreferenceFlagsResponse
#[tokio::test]
async fn a_pick_that_breaks_a_preference_flags_the_response() {
    let Some((db, pool)) = database().await else {
        return;
    };
    for (id, q, l) in [("col_fast", 0.95, 3000.0), ("col_slow", 0.90, 4000.0)] {
        column(&pool, id, "anthropic", id, None, None).await;
        cell(&pool, id, "factual_qa", 3, q, 0.01, l).await;
    }
    pool_of(&pool, "default", &["col_fast", "col_slow"], true).await;
    let (status, text) = recommend(
        &pool,
        r#"{"workload":{"capability":"factual_qa"},"constraints":{"max_latency_ms":2000}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v = parse(&text);
    let ids: Vec<&str> = v["recommendations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["column_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["col_fast", "col_slow"]);
    assert_eq!(v["constraints_met"], false);
    assert!(
        text.contains(
            r#""violations":[{"constraint":"max_latency_ms","limit":2000,"estimated":3000}]"#
        ),
        "{text}"
    );
    db.close().await;
}

// parity: api.TestRecommendDriftFallbackPersistsAlert
#[tokio::test]
async fn drift_fallback_keeps_the_least_violating_column_and_alerts() {
    let Some((db, pool)) = database().await else {
        return;
    };
    for (id, cost) in [("col_cheap", 0.002f32), ("col_expensive", 0.05)] {
        column(&pool, id, "anthropic", id, None, None).await;
        cell(&pool, id, "code_generation", 3, 0.9, cost, 1000.0).await;
    }
    pool_of(&pool, "default", &["col_cheap", "col_expensive"], true).await;
    tenancy::upsert_tenant_constraints(&pool, TENANT, Some(Decimal::new(1, 3)), None, false, false)
        .await
        .unwrap();
    let (status, text) = recommend(&pool, r#"{"workload":{"capability":"code_generation"}}"#).await;
    assert_eq!(status, StatusCode::OK);
    let v = parse(&text);
    assert_eq!(v["constraints_met"], false);
    let entries = v["recommendations"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["column_id"], "col_cheap");
    let violation = &v["violations"][0];
    assert_eq!(violation["constraint"], "max_cost_per_run");
    assert_eq!(violation["limit"], 0.001);
    // The estimate is the stored `real` widened to f64 (reference §3.5, QUIRK Q5).
    assert_eq!(
        violation["estimated"].as_f64().unwrap(),
        f64::from(0.002f32)
    );
    let found = alerts::list_alerts_by_kind(&pool, "pool_would_be_empty")
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].severity, "critical");
    assert_eq!(parse(found[0].subject.get())["tenant_id"], TENANT);
    db.close().await;
}

/// D1: a zero-retention tenant never gets a column without valid zero-retention
/// evidence; an emptied pool answers 409 and raises an alert.
#[tokio::test]
async fn zero_retention_filters_columns_and_fails_closed() {
    let Some((db, pool)) = database().await else {
        return;
    };
    let later = Utc::now() + Duration::days(30);
    let earlier = Utc::now() - Duration::days(1);
    column(
        &pool,
        "col_zero",
        "anthropic",
        "zero",
        None,
        Some(("zero", Some(later))),
    )
    .await;
    column(
        &pool,
        "col_expired",
        "anthropic",
        "expired",
        None,
        Some(("zero", Some(earlier))),
    )
    .await;
    column(
        &pool,
        "col_standard",
        "openai",
        "standard",
        None,
        Some(("standard", None)),
    )
    .await;
    pool_of(
        &pool,
        "default",
        &["col_zero", "col_expired", "col_standard"],
        true,
    )
    .await;
    pool_of(&pool, "leaky", &["col_expired", "col_standard"], false).await;
    tenancy::upsert_tenant_constraints(&pool, TENANT, None, None, true, false)
        .await
        .unwrap();

    let (status, text) = recommend(&pool, r#"{"workload":{"capability":"factual_qa"}}"#).await;
    assert_eq!(status, StatusCode::OK);
    let v = parse(&text);
    let entries = v["recommendations"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["column_id"], "col_zero");
    assert_eq!(entries[0]["retention_mode"], "zero");
    assert_eq!(v["zero_retention"], true);

    let (status, text) = recommend(
        &pool,
        r#"{"workload":{"capability":"factual_qa"},"pool":"leaky"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let error = &parse(&text)["error"];
    assert_eq!(error["code"], "no_compliant_columns");
    assert_eq!(
        error["message"],
        "no column in pool \"leaky\" holds valid zero-retention evidence"
    );
    let found = alerts::list_alerts_by_kind(&pool, "pool_would_be_empty")
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(trace_rows(&pool).await, 1);
    db.close().await;
}

/// D8: a column under the availability floor over the trailing nights ranks after the
/// others and carries `low_availability`; older nights do not count.
#[tokio::test]
async fn low_availability_ranks_last() {
    let Some((db, pool)) = database().await else {
        return;
    };
    let cap = "factual_qa";
    for (id, q) in [("col_flaky", 0.97f32), ("col_steady", 0.90)] {
        column(&pool, id, "anthropic", id, None, None).await;
        cell(&pool, id, cap, 3, q, 0.01, 1000.0).await;
    }
    // Flaky: 0.80 and 0.90 in the window (mean 0.85 < 0.9). Steady: 0.95 in the window,
    // and an old 0.10 that must not count.
    aggregate(&pool, "col_flaky", cap, 3, "availability", 0.80, night()).await;
    aggregate(
        &pool,
        "col_flaky",
        cap,
        3,
        "availability",
        0.90,
        night() - Duration::days(6),
    )
    .await;
    aggregate(&pool, "col_steady", cap, 3, "availability", 0.95, night()).await;
    aggregate(
        &pool,
        "col_steady",
        cap,
        3,
        "availability",
        0.10,
        night() - Duration::days(7),
    )
    .await;
    pool_of(&pool, "default", &["col_flaky", "col_steady"], true).await;

    let (status, text) = recommend(
        &pool,
        r#"{"workload":{"capability":"factual_qa"},"mode":"quality"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v = parse(&text);
    let entries: Vec<(&str, bool)> = v["recommendations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["column_id"].as_str().unwrap(),
                e["low_availability"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(entries, [("col_steady", false), ("col_flaky", true)]);
    let row = decisions::get_recommendation(&pool, v["recommendation_id"].as_str().unwrap())
        .await
        .unwrap();
    let candidates = parse(row.candidates.get());
    let flaky = &candidates[1];
    assert!(
        (flaky["availability"].as_f64().unwrap() - 0.85).abs() < 1e-6,
        "{flaky}"
    );
    assert_eq!(flaky["low_availability"], true);
    db.close().await;
}

#[tokio::test]
async fn wrong_methods_are_not_found() {
    let router = router_as(ApiState::detached(), tenant_key());
    let not_found =
        "{\"error\":{\"code\":\"not_found\",\"message\":\"resource not found\",\"details\":{}}}\n";
    let (status, body) = call(router.clone(), Method::GET, "/v1/recommend", "").await;
    assert_eq!((status, body.as_str()), (StatusCode::NOT_FOUND, not_found));
    let (status, body) = call(router, Method::POST, "/v1/checks/extraction/v1", "").await;
    assert_eq!((status, body.as_str()), (StatusCode::NOT_FOUND, not_found));
}

// parity: api.TestChecksKnownRefShape
// parity: api.TestChecksUnknownRef404
// parity: api.TestChecksEveryCapabilityRefResolvable
#[tokio::test]
async fn checks_serve_every_registered_spec() {
    let router = router_as(ApiState::detached(), tenant_key());
    let (status, body) = call(router.clone(), Method::GET, "/v1/checks/extraction/v1", "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        "{\"ref\":\"checks/extraction/v1\",\"capability\":\"structured_extraction\",\
         \"type\":\"json_schema\",\"spec\":{\"schema\":{\"$schema\":\
         \"https://json-schema.org/draft/2020-12/schema\",\"type\":[\"object\",\"array\"]}}}\n"
    );
    let (status, body) = call(router.clone(), Method::GET, "/v1/checks/poetry/v1?x=1", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        body,
        "{\"error\":{\"code\":\"not_found\",\"message\":\"check spec \\\"checks/poetry/v1\\\" is \
         not registered\",\"details\":{\"ref\":\"checks/poetry/v1\"}}}\n"
    );
    for reference in [
        "code_generation/v1",
        "extraction/v1",
        "summarization/v1",
        "reasoning/v1",
        "schema_following/v1",
        "long_context_qa/v1",
        "factual_qa/v1",
        "rewriting/v1",
        "translation/v1",
        "critique/v1",
    ] {
        let (status, body) = call(
            router.clone(),
            Method::GET,
            &format!("/v1/checks/{reference}"),
            "",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{reference}");
        assert_eq!(parse(&body)["ref"], format!("checks/{reference}"));
    }
}
