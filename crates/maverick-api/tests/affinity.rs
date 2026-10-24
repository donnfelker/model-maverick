//! Conversation affinity on `POST /v1/recommend` (spec §7.6, §9.2, §9.7, §10.2, D44) on
//! real Postgres: validation, the tenant setting and the request override, each status,
//! the stored row and trace block, and turning affinity off.

use std::sync::{Arc, LazyLock};

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use chrono::{DateTime, Duration, Utc};
use maverick_api::{
    AffinitySettings, ApiState, AuthState, HealthState, Identity, RateLimiter, RecommendConfig,
    StaticAuthBackend, public_router_with,
};
use maverick_domain::capture::affinity_hash;
use maverick_domain::keys;
use maverick_store::affinity::{self, AffinityRow};
use maverick_store::catalog::{self, NewModelColumn};
use maverick_store::measurement::{self, AggregateUpsert};
use maverick_store::{JsonRawValue, MIGRATOR, corpus, tenancy};
use maverick_testdb::{Schema, TestDatabase};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

const TENANT: &str = "ten_affinity";
const KEY: &str = "key_affinity";
const CORPUS: &str = "2026-07-01";
const HMAC_KEY: [u8; 32] = [7; 32];
const CAP: &str = "factual_qa";
const CONVERSATION: &str = "conv_8f2a41";

fn router_with(pool: &PgPool, config: RecommendConfig) -> Router {
    let auth = AuthState::new(
        Arc::new(StaticAuthBackend::authenticated(Identity {
            tenant_id: TENANT.to_owned(),
            api_key_id: KEY.to_owned(),
        })),
        keys::parse_internal_tokens("ops:alpha").unwrap(),
        RateLimiter::new(10_000),
    );
    let health = HealthState::with_providers(pool.clone(), Vec::new());
    public_router_with(
        auth,
        ApiState::with_health(pool.clone(), health).with_recommend_config(config),
    )
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

async fn recommend_with(
    pool: &PgPool,
    config: RecommendConfig,
    body: &str,
) -> (StatusCode, Value, String) {
    let (status, text) = call(
        router_with(pool, config),
        Method::POST,
        "/v1/recommend",
        body,
    )
    .await;
    let value = serde_json::from_str(&text).unwrap_or(Value::Null);
    (status, value, text)
}

async fn recommend(pool: &PgPool, body: &str) -> (StatusCode, Value, String) {
    recommend_with(pool, RecommendConfig::default(), body).await
}

fn raw(text: &str) -> Box<JsonRawValue> {
    JsonRawValue::from_string(text.to_owned()).unwrap()
}

/// A tenant whose default pool is three measured `factual_qa` columns. With no mode
/// (quality mode, band 0) `col_best` (90) leads `col_close` (89) by 1 point, outside the
/// 0.75 noise band; `col_weak` (50) is last.
async fn database() -> Option<(TestDatabase, PgPool)> {
    let db = maverick_testdb::database(&SCHEMA).await?;
    let pool = db.pool().await;
    tenancy::create_tenant(&pool, TENANT, "Acme", &HMAC_KEY)
        .await
        .unwrap();
    tenancy::create_api_key(&pool, KEY, TENANT, b"hash", "abcd", "")
        .await
        .unwrap();
    corpus::upsert_corpus_version(&pool, CORPUS, "", 0, false)
        .await
        .unwrap();
    corpus::activate_corpus_version(&pool, CORPUS)
        .await
        .unwrap();
    let empty = raw("{}");
    let later = Utc::now() + Duration::days(30);
    for (id, q, retention) in [
        ("col_best", 0.90f32, Some(("zero", Some(later)))),
        ("col_close", 0.89, None),
        ("col_weak", 0.50, Some(("zero", Some(later)))),
    ] {
        catalog::create_model_column(
            &pool,
            &NewModelColumn {
                id,
                provider: "anthropic",
                model_id: id,
                endpoint: "direct",
                settings: &empty,
                status: "active",
                pricing: &empty,
                quality_source_column_id: None,
                cold_start_until: None,
                deprecation_date: None,
                execution_aliases: None,
                retention_mode: retention.map(|(mode, _)| mode),
                retention_verified_at: retention.map(|_| Utc::now()),
                retention_valid_until: retention.and_then(|(_, until)| until),
                retention_source: None,
                execution_alias_retention: None,
                param_constraints: None,
            },
        )
        .await
        .unwrap();
        for (dimension, value) in [
            ("quality", q),
            ("cost_per_run_usd", 0.01),
            ("latency_p95_ms", 1000.0),
            ("availability", 1.0),
        ] {
            measurement::upsert_aggregate(
                &pool,
                &AggregateUpsert {
                    model_column_id: id,
                    capability: CAP,
                    difficulty_tier: 3,
                    dimension,
                    ewma: value,
                    variance: 0.0,
                    n: 20,
                    measured: true,
                    matrix_version: CORPUS,
                    as_of: Utc::now().date_naive(),
                },
            )
            .await
            .unwrap();
        }
    }
    let ids: Vec<String> = ["col_best", "col_close", "col_weak"]
        .iter()
        .map(|id| (*id).to_owned())
        .collect();
    tenancy::create_pool(&pool, "pool_default", TENANT, "default", &ids, true)
        .await
        .unwrap();
    Some((db, pool))
}

fn hash() -> String {
    affinity_hash(&HMAC_KEY, CONVERSATION)
}

async fn pin(pool: &PgPool, column_id: &str, used: DateTime<Utc>) {
    affinity::upsert_affinity(
        pool,
        &AffinityRow {
            tenant_id: TENANT.to_owned(),
            affinity_hash: hash(),
            column_id: column_id.to_owned(),
            recommendation_id: "rec_earlier".to_owned(),
            last_used_at: used,
            expires_at: used + Duration::hours(24),
        },
    )
    .await
    .unwrap();
}

async fn row(pool: &PgPool) -> Option<AffinityRow> {
    affinity::get_affinity(pool, TENANT, &hash(), Utc::now())
        .await
        .unwrap()
}

fn body(affinity: Value) -> String {
    json!({"workload": {"capability": CAP}, "affinity": affinity}).to_string()
}

async fn trace(pool: &PgPool, id: &str) -> (Value, Option<Value>) {
    let (request, block): (String, Option<String>) =
        sqlx::query_as("SELECT request::text, affinity::text FROM recommendations WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap();
    (
        serde_json::from_str(&request).unwrap(),
        block.map(|block| serde_json::from_str(&block).unwrap()),
    )
}

async fn trace_rows(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM recommendations")
        .fetch_one(pool)
        .await
        .unwrap()
}

fn pick(value: &Value) -> &str {
    value["recommendations"][0]["column_id"].as_str().unwrap()
}

/// DIVERGENCE: D44. Each bad `affinity` value is `422 invalid_request` naming its field,
/// an unknown member fails strict decoding, and nothing is written.
#[tokio::test]
async fn affinity_errors_answer_422_and_write_nothing() {
    let Some((db, pool)) = database().await else {
        return;
    };
    let long = "k".repeat(257);
    for (affinity, field) in [
        (json!({}), "affinity.key"),
        (json!({"key": null}), "affinity.key"),
        (json!({"key": ""}), "affinity.key"),
        (json!({"key": long}), "affinity.key"),
        (json!({"key": 5}), "affinity.key"),
        (
            json!({"key": "c", "context_tokens": -1}),
            "affinity.context_tokens",
        ),
        (
            json!({"key": "c", "context_tokens": 1.5}),
            "affinity.context_tokens",
        ),
        (
            json!({"key": "c", "context_tokens": 10_000_001}),
            "affinity.context_tokens",
        ),
        (
            json!({"key": "c", "context_tokens": "9"}),
            "affinity.context_tokens",
        ),
        (
            json!({"key": "c", "cost_aware": "yes"}),
            "affinity.cost_aware",
        ),
    ] {
        let (status, value, text) = recommend(&pool, &body(affinity.clone())).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{affinity}: {text}"
        );
        assert_eq!(value["error"]["code"], "invalid_request", "{affinity}");
        assert_eq!(value["error"]["details"]["field"], field, "{affinity}");
    }
    let (status, value, _) = recommend(&pool, &body(json!({"key": "c", "ttl": 5}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        value["error"]["message"],
        "malformed request body: json: unknown field \"ttl\""
    );
    let (status, value, _) = recommend(&pool, &body(json!("conv"))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(value["error"]["code"], "invalid_request");
    // The limits themselves are accepted.
    let edge = json!({"key": "k".repeat(256), "context_tokens": 10_000_000, "cost_aware": false});
    let (status, _, text) = recommend(&pool, &body(edge)).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(trace_rows(&pool).await, 1);
    db.close().await;
}

/// DIVERGENCE: D44. The first turn is `new` and writes the row; the next turn is `kept`
/// with a warm cache. The block sits after `telemetry`, the stored request has no key,
/// and the trace and history carry the decision. `null` affinity is no affinity.
#[tokio::test]
async fn a_conversation_starts_new_and_is_kept() {
    let Some((db, pool)) = database().await else {
        return;
    };
    let turn = body(json!({"key": CONVERSATION, "context_tokens": 1200}));
    let (status, first, text) = recommend(&pool, &turn).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(
        first["affinity"],
        json!({"status": "new", "previous_column_id": null, "reason": null,
               "cache_warm": false, "cost_aware": false})
    );
    assert!(
        text.ends_with(
            r#""telemetry":true,"affinity":{"status":"new","previous_column_id":null,"reason":null,"cache_warm":false,"cost_aware":false}}
"#
        ),
        "{text}"
    );
    assert_eq!(pick(&first), "col_best");
    let id = first["recommendation_id"].as_str().unwrap().to_owned();
    let stored = row(&pool).await.expect("the row");
    assert_eq!(stored.column_id, "col_best");
    assert_eq!(stored.recommendation_id, id);
    assert!(stored.expires_at - stored.last_used_at == Duration::hours(24));

    let (request, block) = trace(&pool, &id).await;
    assert_eq!(request["affinity"], json!({"context_tokens": 1200}));
    assert!(!text.contains(CONVERSATION));
    let block = block.expect("the trace block");
    assert_eq!(block["affinity_hash"], hash());
    assert_eq!(block["status"], "new");
    assert_eq!(block["context_tokens"], 1200);
    assert_eq!(block["cost_aware"], false);
    assert_eq!(block["cost_aware_source"], "tenant");
    assert_eq!(block["turn_cost_skipped"], "cost_aware_off");
    assert_eq!(block["bonus"], 0);
    assert_eq!(block["ranking_costs"], Value::Null);

    let (status, second, _) = recommend(&pool, &turn).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        second["affinity"],
        json!({"status": "kept", "previous_column_id": "col_best", "reason": null,
               "cache_warm": true, "cost_aware": false})
    );
    let second_id = second["recommendation_id"].as_str().unwrap();
    assert_eq!(row(&pool).await.unwrap().recommendation_id, second_id);

    // History returns the block; a request without affinity has no block and no key.
    let (status, history) = call(
        router_with(&pool, RecommendConfig::default()),
        Method::GET,
        &format!("/v1/history/recommendations/{second_id}"),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{history}");
    let history: Value = serde_json::from_str(&history).unwrap();
    assert_eq!(history["affinity"]["status"], "kept");
    let plain = json!({"workload": {"capability": CAP}, "affinity": null}).to_string();
    let (status, value, text) = recommend(&pool, &plain).await;
    assert_eq!(status, StatusCode::OK);
    assert!(value.get("affinity").is_none(), "{text}");
    let (_, block) = trace(&pool, value["recommendation_id"].as_str().unwrap()).await;
    assert_eq!(block, None);
    db.close().await;
}

/// DIVERGENCE: D44. Cost-aware off, a pinned column that is not the best is `switched`
/// with `best_model`, and the row moves to the pick. Cost-aware on (the tenant setting),
/// the same pin is kept by the bonus: 89 + 1.5 > 90. The request override wins both ways.
#[tokio::test]
async fn the_setting_and_the_override_decide_cost_aware() {
    let Some((db, pool)) = database().await else {
        return;
    };
    pin(&pool, "col_close", Utc::now()).await;
    let (_, off, _) = recommend(&pool, &body(json!({"key": CONVERSATION}))).await;
    assert_eq!(pick(&off), "col_best");
    assert_eq!(off["affinity"]["status"], "switched");
    assert_eq!(off["affinity"]["reason"], "best_model");
    assert_eq!(off["affinity"]["previous_column_id"], "col_close");
    assert_eq!(row(&pool).await.unwrap().column_id, "col_best");

    // The tenant setting, written and read back through the constraints payload.
    let router = router_with(&pool, RecommendConfig::default());
    let (status, text) = call(
        router.clone(),
        Method::PUT,
        "/v1/config/constraints",
        r#"{"constraints":{"affinity_cost_aware":true}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(
        text,
        "{\"constraints\":{\"max_cost_per_run\":null,\"max_latency_ms\":null,\
         \"zero_retention\":false,\"telemetry_opt_out\":false,\"affinity_cost_aware\":true}}\n"
    );
    let (_, read) = call(router, Method::GET, "/v1/config/constraints", "").await;
    assert_eq!(read, text);

    pin(&pool, "col_close", Utc::now()).await;
    let (_, on, _) = recommend(&pool, &body(json!({"key": CONVERSATION}))).await;
    assert_eq!(pick(&on), "col_close");
    assert_eq!(on["affinity"]["status"], "kept");
    assert_eq!(on["affinity"]["cost_aware"], true);
    assert_eq!(
        on["recommendations"][0]["reason"],
        "kept for conversation continuity"
    );
    let (_, block) = trace(&pool, on["recommendation_id"].as_str().unwrap()).await;
    let block = block.unwrap();
    assert_eq!(block["bonus"], 1.5);
    assert_eq!(block["cost_aware_source"], "tenant");
    assert_eq!(block["turn_cost_skipped"], "no_context_tokens");

    // The request turns it off for one call.
    let (_, overridden, _) = recommend(
        &pool,
        &body(json!({"key": CONVERSATION, "cost_aware": false})),
    )
    .await;
    assert_eq!(pick(&overridden), "col_best");
    assert_eq!(overridden["affinity"]["reason"], "best_model");
    assert_eq!(overridden["affinity"]["cost_aware"], false);
    let (_, block) = trace(&pool, overridden["recommendation_id"].as_str().unwrap()).await;
    assert_eq!(block.unwrap()["cost_aware_source"], "request");
    db.close().await;
}

/// DIVERGENCE: D44. A pin outside the allowlist is `broken` with `not_in_allowlist`; a
/// zero-retention tenant's pin without evidence is `broken` with
/// `standing_constraint:zero_retention`. A stale row reads as missing: `new`.
#[tokio::test]
async fn a_pin_that_did_not_survive_is_broken() {
    let Some((db, pool)) = database().await else {
        return;
    };
    pin(&pool, "col_best", Utc::now()).await;
    let narrowed = json!({"workload": {"capability": CAP}, "allowed_models": ["col_close"],
                          "affinity": {"key": CONVERSATION}})
    .to_string();
    let (status, value, text) = recommend(&pool, &narrowed).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(value["affinity"]["status"], "broken");
    assert_eq!(value["affinity"]["reason"], "not_in_allowlist");

    tenancy::upsert_tenant_constraints(&pool, TENANT, None, None, true, false, false)
        .await
        .unwrap();
    pin(&pool, "col_close", Utc::now()).await;
    let (status, value, text) = recommend(&pool, &body(json!({"key": CONVERSATION}))).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(pick(&value), "col_best");
    assert_eq!(value["affinity"]["status"], "broken");
    assert_eq!(
        value["affinity"]["reason"],
        "standing_constraint:zero_retention"
    );
    assert_eq!(value["zero_retention"], true);

    // An expired row is no row.
    sqlx::query("UPDATE conversation_affinity SET expires_at = now() - interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    let (_, value, _) = recommend(&pool, &body(json!({"key": CONVERSATION}))).await;
    assert_eq!(value["affinity"]["status"], "new");
    db.close().await;
}

/// DIVERGENCE: D44. With `MAVERICK_AFFINITY_ENABLED=false` the request is still
/// validated, no row is read or written, and the answer is `new` with the normal
/// ranking, cost-aware or not.
#[tokio::test]
async fn disabled_affinity_validates_and_writes_no_row() {
    let Some((db, pool)) = database().await else {
        return;
    };
    let off = || RecommendConfig {
        affinity: AffinitySettings {
            enabled: false,
            ..AffinitySettings::default()
        },
        ..RecommendConfig::default()
    };
    pin(&pool, "col_close", Utc::now()).await;
    let before = row(&pool).await.unwrap();
    let (status, value, text) = recommend_with(
        &pool,
        off(),
        &body(json!({"key": CONVERSATION, "cost_aware": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(pick(&value), "col_best");
    assert_eq!(
        value["affinity"],
        json!({"status": "new", "previous_column_id": null, "reason": null,
               "cache_warm": false, "cost_aware": false})
    );
    assert_eq!(row(&pool).await.unwrap(), before);
    let (_, block) = trace(&pool, value["recommendation_id"].as_str().unwrap()).await;
    assert_eq!(block.unwrap()["affinity_hash"], Value::Null);

    let (status, value, _) = recommend_with(&pool, off(), &body(json!({"key": ""}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(value["error"]["details"]["field"], "affinity.key");
    db.close().await;
}
