//! The telemetry follow rule (spec §9.10, D44) on real Postgres: an accepted 2xx item
//! from a chain entry after the pick moves the conversation's row to that entry's column,
//! with a compare and set on the recommendation.

use std::sync::{Arc, LazyLock};

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use chrono::Utc;
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

const TENANT: &str = "ten_follow";
const OTHER: &str = "ten_other";
const CORPUS: &str = "2026-07-01";
const HMAC_KEY: [u8; 32] = [9; 32];
const CAP: &str = "factual_qa";
const CONVERSATION: &str = "conv_follow";

fn router_for(pool: &PgPool, tenant: &str, config: RecommendConfig) -> Router {
    let auth = AuthState::new(
        Arc::new(StaticAuthBackend::authenticated(Identity {
            tenant_id: tenant.to_owned(),
            api_key_id: format!("key_{tenant}"),
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

async fn call(router: Router, path: &str, body: &str) -> (StatusCode, String) {
    let request = Request::builder()
        .method(Method::POST)
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

/// One turn of the conversation: the recommendation id and its pick.
async fn turn(pool: &PgPool) -> (String, String) {
    let body = json!({"workload": {"capability": CAP}, "affinity": {"key": CONVERSATION}});
    let (status, text) = call(
        router_for(pool, TENANT, RecommendConfig::default()),
        "/v1/recommend",
        &body.to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let value: Value = serde_json::from_str(&text).unwrap();
    (
        value["recommendation_id"].as_str().unwrap().to_owned(),
        value["recommendations"][0]["column_id"]
            .as_str()
            .unwrap()
            .to_owned(),
    )
}

async fn report_as(pool: &PgPool, tenant: &str, config: RecommendConfig, items: Value) -> String {
    let (status, text) = call(
        router_for(pool, tenant, config),
        "/v1/telemetry",
        &json!({"items": items}).to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    text
}

async fn report(pool: &PgPool, items: Value) -> String {
    report_as(pool, TENANT, RecommendConfig::default(), items).await
}

fn item(recommendation_id: &str, model: &str, endpoint: &str, status: u16) -> Value {
    json!({"recommendation_id": recommendation_id, "model_used": model,
           "endpoint_used": endpoint, "status": status})
}

async fn row(pool: &PgPool) -> AffinityRow {
    affinity::get_affinity(
        pool,
        TENANT,
        &affinity_hash(&HMAC_KEY, CONVERSATION),
        Utc::now(),
    )
    .await
    .unwrap()
    .expect("the conversation's row")
}

fn raw(text: &str) -> Box<JsonRawValue> {
    JsonRawValue::from_string(text.to_owned()).unwrap()
}

/// The chain in quality mode: `col_best` (90), `col_close` (89), `col_weak` (50), whose
/// OpenRouter alias is `anthropic/weak`.
async fn database() -> Option<(TestDatabase, PgPool)> {
    let db = maverick_testdb::database(&SCHEMA).await?;
    let pool = db.pool().await;
    for (tenant, key) in [(TENANT, HMAC_KEY), (OTHER, [3; 32])] {
        tenancy::create_tenant(&pool, tenant, tenant, &key)
            .await
            .unwrap();
        tenancy::create_api_key(
            &pool,
            &format!("key_{tenant}"),
            tenant,
            tenant.as_bytes(),
            "abcd",
            "",
        )
        .await
        .unwrap();
    }
    corpus::upsert_corpus_version(&pool, CORPUS, "", 0, false)
        .await
        .unwrap();
    corpus::activate_corpus_version(&pool, CORPUS)
        .await
        .unwrap();
    let empty = raw("{}");
    let alias = raw(r#"{"openrouter": "anthropic/weak"}"#);
    for (id, q) in [
        ("col_best", 0.90f32),
        ("col_close", 0.89),
        ("col_weak", 0.50),
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
                execution_aliases: (id == "col_weak").then_some(&*alias),
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
    tenancy::create_pool(&pool, "pool_follow", TENANT, "default", &ids, true)
        .await
        .unwrap();
    Some((db, pool))
}

/// DIVERGENCE: D44. A 429 on the pick, then a 200 from the next entry: the row moves to
/// that entry, still owned by the same recommendation. The counts do not change.
#[tokio::test]
async fn a_later_entry_that_served_the_turn_takes_the_row() {
    let Some((db, pool)) = database().await else {
        return;
    };
    let (id, pick) = turn(&pool).await;
    assert_eq!(pick, "col_best");
    let counts = report(
        &pool,
        json!([
            item(&id, "col_best", "direct", 429),
            item(&id, "col_close", "direct", 200)
        ]),
    )
    .await;
    assert_eq!(counts, "{\"accepted\":2,\"dropped\":0}\n");
    let moved = row(&pool).await;
    assert_eq!(moved.column_id, "col_close");
    assert_eq!(moved.recommendation_id, id);
    db.close().await;
}

/// DIVERGENCE: D44. An OpenRouter item names the entry's alias.
#[tokio::test]
async fn an_alias_item_takes_the_row_to_its_entry() {
    let Some((db, pool)) = database().await else {
        return;
    };
    let (id, _) = turn(&pool).await;
    report(
        &pool,
        json!([item(&id, "anthropic/weak", "openrouter", 200)]),
    )
    .await;
    assert_eq!(row(&pool).await.column_id, "col_weak");
    db.close().await;
}

/// DIVERGENCE: D44. The pick serving the turn, a non-2xx item, an unknown model, and an
/// item without an endpoint (direct) for the pick all leave the row on the pick.
#[tokio::test]
async fn the_pick_or_a_failure_leaves_the_row() {
    let Some((db, pool)) = database().await else {
        return;
    };
    let (id, _) = turn(&pool).await;
    let counts = report(
        &pool,
        json!([
            item(&id, "col_best", "direct", 200),
            item(&id, "col_close", "direct", 500),
            item(&id, "col_close", "direct", 429),
            item(&id, "col_close", "vertex", 200),
            {"recommendation_id": id, "model_used": "col_best", "status": 200}
        ]),
    )
    .await;
    assert_eq!(counts, "{\"accepted\":5,\"dropped\":0}\n");
    assert_eq!(row(&pool).await.column_id, "col_best");
    db.close().await;
}

/// DIVERGENCE: D44. Late telemetry for an earlier turn never overwrites a later turn.
#[tokio::test]
async fn a_later_turn_is_never_overwritten() {
    let Some((db, pool)) = database().await else {
        return;
    };
    let (first, _) = turn(&pool).await;
    let (second, _) = turn(&pool).await;
    report(&pool, json!([item(&first, "col_close", "direct", 200)])).await;
    let kept = row(&pool).await;
    assert_eq!(kept.column_id, "col_best");
    assert_eq!(kept.recommendation_id, second);
    db.close().await;
}

/// DIVERGENCE: D44. Another tenant's item is dropped and moves nothing; an opted-out
/// tenant sends nothing that moves a row; with affinity off the item is accepted but the
/// row stays.
#[tokio::test]
async fn foreign_opted_out_and_disabled_items_move_nothing() {
    let Some((db, pool)) = database().await else {
        return;
    };
    let (id, _) = turn(&pool).await;
    let counts = report_as(
        &pool,
        OTHER,
        RecommendConfig::default(),
        json!([item(&id, "col_close", "direct", 200)]),
    )
    .await;
    assert_eq!(counts, "{\"accepted\":0,\"dropped\":1}\n");
    assert_eq!(row(&pool).await.column_id, "col_best");

    let off = RecommendConfig {
        affinity: AffinitySettings {
            enabled: false,
            ..AffinitySettings::default()
        },
        ..RecommendConfig::default()
    };
    let counts = report_as(
        &pool,
        TENANT,
        off,
        json!([item(&id, "col_close", "direct", 200)]),
    )
    .await;
    assert_eq!(counts, "{\"accepted\":1,\"dropped\":0}\n");
    assert_eq!(row(&pool).await.column_id, "col_best");

    tenancy::upsert_tenant_constraints(&pool, TENANT, None, None, false, true, false)
        .await
        .unwrap();
    let counts = report(&pool, json!([item(&id, "col_close", "direct", 200)])).await;
    assert_eq!(counts, "{\"accepted\":0,\"dropped\":1}\n");
    assert_eq!(row(&pool).await.column_id, "col_best");
    db.close().await;
}
