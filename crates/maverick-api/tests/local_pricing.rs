//! Local price overrides on a replica (spec §14.4, D49): a pricing snapshot written
//! through `PUT /internal/v1/columns/{id}` is stored with `"source": "local"`, so a
//! bundle import keeps it. An origin stores the snapshot as sent.

use std::sync::{Arc, LazyLock};

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use maverick_api::{
    ApiState, AuthState, HealthState, InternalSettings, PgAuthBackend, RateLimiter,
    internal_router_with,
};
use maverick_config::Role;
use maverick_domain::keys;
use maverick_domain::pricing::{is_local, parse_pricing};
use maverick_store::catalog::{self, NewModelColumn, create_model_column};
use maverick_store::{JsonRawValue, MIGRATOR};
use maverick_testdb::Schema;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

const OPS: &str = "ops:alpha";
const BUNDLE_PRICING: &str = r#"{"input_per_mtok":1,"output_per_mtok":2,"source":"bundle"}"#;

fn internal(pool: &PgPool, role: Role) -> Router {
    let auth = AuthState::new(
        Arc::new(PgAuthBackend::new(pool.clone())),
        keys::parse_internal_tokens(OPS).unwrap(),
        RateLimiter::new(1000),
    );
    let api = ApiState::with_health(pool.clone(), HealthState::detached()).with_internal(
        InternalSettings {
            role,
            ..InternalSettings::default()
        },
    );
    internal_router_with(auth, api)
}

async fn put(router: &Router, id: &str, body: &str) -> (u16, Value) {
    let request = Request::builder()
        .method("PUT")
        .uri(format!("/internal/v1/columns/{id}"))
        .header("authorization", format!("Bearer {OPS}"))
        .body(Body::from(body.to_owned()))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// An active column whose pricing came from a bundle.
async fn column(pool: &PgPool, id: &str) {
    let empty = JsonRawValue::from_string("{}".to_owned()).unwrap();
    let pricing = JsonRawValue::from_string(BUNDLE_PRICING.to_owned()).unwrap();
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
            pricing: &pricing,
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

async fn stored_is_local(pool: &PgPool, id: &str) -> bool {
    let column = catalog::get_model_column(pool, id).await.unwrap();
    is_local(&parse_pricing(column.pricing.get()).unwrap())
}

/// DIVERGENCE: D49. On a replica a PUT snapshot is stored as local, whatever `source`
/// the caller sent. A PUT without pricing leaves the stored snapshot as it was, and an
/// invalid snapshot is refused as on an origin.
#[tokio::test]
async fn a_replica_stores_put_pricing_as_local() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    column(&pool, "col_a").await;
    let router = internal(&pool, Role::Replica);

    let (status, body) = put(&router, "col_a", r#"{"status":"active"}"#).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["pricing"],
        serde_json::from_str::<Value>(BUNDLE_PRICING).unwrap()
    );
    assert!(!stored_is_local(&pool, "col_a").await);

    let (status, body) = put(
        &router,
        "col_a",
        r#"{"pricing":{"input_per_mtok":2.5,"Source":"models.dev","output_per_mtok":10}}"#,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["pricing"],
        json!({"input_per_mtok": 2.5, "output_per_mtok": 10, "source": "local"})
    );
    assert!(stored_is_local(&pool, "col_a").await);

    let (status, body) = put(
        &router,
        "col_a",
        r#"{"pricing":{"input_per_mtok":-1,"output_per_mtok":10}}"#,
    )
    .await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(
        body["error"]["message"],
        "pricing is not a valid snapshot: pricing snapshot has a negative price"
    );
    assert!(stored_is_local(&pool, "col_a").await);
    pool.close().await;
    db.close().await;
}

/// An origin stores a PUT snapshot exactly as sent, as Go did.
#[tokio::test]
async fn an_origin_stores_put_pricing_as_sent() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    column(&pool, "col_o").await;
    let router = internal(&pool, Role::Origin);
    let (status, body) = put(
        &router,
        "col_o",
        r#"{"pricing":{"input_per_mtok":2.5,"output_per_mtok":10,"source":"models.dev"}}"#,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["pricing"],
        json!({"input_per_mtok": 2.5, "output_per_mtok": 10, "source": "models.dev"})
    );
    assert!(!stored_is_local(&pool, "col_o").await);
    pool.close().await;
    db.close().await;
}
