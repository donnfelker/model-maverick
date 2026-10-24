//! DIVERGENCE: D49. A replica ranks the signed public matrix exactly like its origin.
use axum::{
    Router,
    body::Body,
    http::{Method, Request, StatusCode},
};
use chrono::Utc;
use maverick_api::{
    ApiState, AuthState, HealthState, Identity, RateLimiter, RecommendConfig, StaticAuthBackend,
    public_router_with,
};
use maverick_bundle::{Manifest, SchemaRange, trust::test_keys};
use maverick_config::Role;
use maverick_store::{
    MIGRATOR, corpus,
    measurement::{self, AggregateUpsert},
    tenancy,
};
use maverick_testdb::Schema;
use serde_json::Value;
use sqlx::PgPool;
use std::sync::{Arc, LazyLock};
use tower::ServiceExt;
static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));
const VERSION: &str = "public-matrix-v1";

fn router(pool: &PgPool, role: Role) -> Router {
    let identity = Identity {
        tenant_id: "tenant".into(),
        api_key_id: "key".into(),
    };
    let auth = AuthState::new(
        Arc::new(StaticAuthBackend::authenticated(identity)),
        maverick_domain::keys::parse_internal_tokens("ops:s").unwrap(),
        RateLimiter::new(1000),
    );
    let api = ApiState::with_health(pool.clone(), HealthState::detached()).with_recommend_config(
        RecommendConfig {
            role,
            ..RecommendConfig::default()
        },
    );
    public_router_with(auth, api)
}
async fn call(router: &Router, path: &str, body: Option<&str>) -> Value {
    call_with_key(router, path, body, "mk_live_test").await
}

async fn call_with_key(router: &Router, path: &str, body: Option<&str>, key: &str) -> Value {
    let request = Request::builder()
        .method(if body.is_some() {
            Method::POST
        } else {
            Method::GET
        })
        .uri(path)
        .header("authorization", format!("Bearer {key}"))
        .body(Body::from(body.unwrap_or_default().to_owned()))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap()
}

/// xq7v.26: bootstrap can finish before the catalog arrives, with no manual pool PUT.
#[tokio::test]
async fn bootstrap_before_first_import_serves_with_the_bootstrap_key() {
    let Some(origin) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let Some(replica) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let op = origin.pool().await;
    let rp = replica.pool().await;
    catalog(&op).await;
    let boot = maverick_api::provision::bootstrap(&mut rp.acquire().await.unwrap(), "replica")
        .await
        .unwrap()
        .unwrap();
    assert!(
        tenancy::list_pools_by_tenant(&rp, &boot.tenant.id)
            .await
            .unwrap()
            .is_empty()
    );
    for (dimension, value) in [
        ("quality", 0.875),
        ("pass_rate", 0.875),
        ("cost_per_run_usd", 0.01),
        ("latency_p95_ms", 50.),
    ] {
        measurement::upsert_aggregate(
            &op,
            &AggregateUpsert {
                model_column_id: "high",
                capability: "factual_qa",
                difficulty_tier: 3,
                dimension,
                ewma: value,
                variance: 0.,
                n: 20,
                measured: true,
                matrix_version: VERSION,
                as_of: Utc::now().date_naive(),
            },
        )
        .await
        .unwrap();
    }
    let matrix = maverick_store::matrix::public_matrix(
        &mut op.acquire().await.unwrap(),
        VERSION,
        Utc::now().date_naive(),
    )
    .await
    .unwrap();
    let bytes = maverick_bundle::build(
        Manifest {
            format: maverick_bundle::FORMAT.into(),
            matrix_version: VERSION.into(),
            published_at: Utc::now(),
            origin_release: "test".into(),
            schema: SchemaRange {
                min: 11,
                max: i64::MAX,
            },
            files: Default::default(),
        },
        &matrix,
        &test_keys::signing(&[test_keys::CURRENT]),
    )
    .unwrap();
    let bundle =
        maverick_bundle::verify(&bytes, &test_keys::trust(&[test_keys::CURRENT]), 12).unwrap();
    maverick_store::matrix_import::import(&rp, &bundle, false)
        .await
        .unwrap();
    let auth = AuthState::new(
        Arc::new(maverick_api::PgAuthBackend::new(rp.clone())),
        maverick_domain::keys::parse_internal_tokens("ops:s").unwrap(),
        RateLimiter::new(1000),
    );
    let api = ApiState::with_health(rp.clone(), HealthState::detached()).with_recommend_config(
        RecommendConfig {
            role: Role::Replica,
            ..Default::default()
        },
    );
    let reply = call_with_key(
        &public_router_with(auth, api),
        "/v1/recommend",
        Some(r#"{"workload":{"capability":"factual_qa","difficulty":3}}"#),
        &boot.key.raw_key,
    )
    .await;
    assert_eq!(reply["recommendations"][0]["column_id"], "high");
    assert_eq!(reply["measured"], true);
    assert_eq!(reply["matrix_version"], VERSION);
    op.close().await;
    rp.close().await;
    origin.close().await;
    replica.close().await;
}
async fn tenant(pool: &PgPool) {
    tenancy::create_tenant(pool, "tenant", "tenant-private", &[1; 32])
        .await
        .unwrap();
    tenancy::create_api_key(pool, "key", "tenant", b"hash", "1234", "")
        .await
        .unwrap();
    tenancy::create_pool(
        pool,
        "pool",
        "tenant",
        "default",
        &["high".into(), "low".into()],
        true,
    )
    .await
    .unwrap();
}
async fn catalog(pool: &PgPool) {
    sqlx::raw_sql("insert into model_columns (id,provider,model_id,endpoint,status) values ('high','openai','model-high','direct','active'),('low','anthropic','model-low','direct','active')").execute(pool).await.unwrap();
}

#[tokio::test]
async fn cold_start_then_import_matches_the_origin_and_reports_the_bundle_on_every_surface() {
    let Some(origin) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let Some(replica) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let op = origin.pool().await;
    let rp = replica.pool().await;
    catalog(&op).await;
    catalog(&rp).await;
    tenant(&op).await;
    tenant(&rp).await;
    corpus::upsert_corpus_version(&op, VERSION, "", 0, true)
        .await
        .unwrap();
    let request = r#"{"workload":{"capability":"factual_qa","difficulty":3},"mode":"quality"}"#;
    let replica_api = router(&rp, Role::Replica);
    let cold = call(&replica_api, "/v1/recommend", Some(request)).await;
    assert_eq!(cold["measured"], false);
    assert_eq!(cold["matrix_version"], "");
    assert!(!cold["recommendations"].as_array().unwrap().is_empty());
    for (id, quality) in [("high", 0.875), ("low", 0.5)] {
        for (dimension, value) in [
            ("quality", quality),
            ("pass_rate", quality),
            ("cost_per_run_usd", 0.01),
            ("latency_p95_ms", 50.),
        ] {
            measurement::upsert_aggregate(
                &op,
                &AggregateUpsert {
                    model_column_id: id,
                    capability: "factual_qa",
                    difficulty_tier: 3,
                    dimension,
                    ewma: value,
                    variance: 0.,
                    n: 20,
                    measured: true,
                    matrix_version: VERSION,
                    as_of: Utc::now().date_naive(),
                },
            )
            .await
            .unwrap();
        }
    }
    let matrix = maverick_store::matrix::public_matrix(
        &mut op.acquire().await.unwrap(),
        VERSION,
        Utc::now().date_naive(),
    )
    .await
    .unwrap();
    let bytes = maverick_bundle::build(
        Manifest {
            format: maverick_bundle::FORMAT.into(),
            matrix_version: VERSION.into(),
            published_at: Utc::now(),
            origin_release: "test".into(),
            schema: SchemaRange {
                min: 11,
                max: i64::MAX,
            },
            files: Default::default(),
        },
        &matrix,
        &test_keys::signing(&[test_keys::CURRENT]),
    )
    .unwrap();
    let bundle =
        maverick_bundle::verify(&bytes, &test_keys::trust(&[test_keys::CURRENT]), 12).unwrap();
    maverick_store::matrix_import::import(&rp, &bundle, false)
        .await
        .unwrap();
    let mut expected = call(&router(&op, Role::Origin), "/v1/recommend", Some(request)).await;
    let mut actual = call(&replica_api, "/v1/recommend", Some(request)).await;
    // By hand: zero variance/ambiguity gives 100 * 0.875 = 87.5 quality points.
    assert_eq!(actual["recommendations"][0]["column_id"], "high");
    assert_eq!(actual["recommendations"][0]["score"], 87.5);
    assert_eq!(actual["measured"], true);
    actual.as_object_mut().unwrap().remove("recommendation_id");
    expected
        .as_object_mut()
        .unwrap()
        .remove("recommendation_id");
    assert_eq!(actual, expected);
    // A leftover corpus on a repurposed database never overrides a replica's matrix.
    corpus::upsert_corpus_version(&rp, "not-the-bundle", "", 0, true)
        .await
        .unwrap();
    for path in [
        "/v1/models",
        "/v1/policies?capability=factual_qa",
        "/v1/history/recommendations",
        "/v1/history/outcomes",
        "/v1/history/calibration",
        "/v1/history/models/high?capability=factual_qa&dimension=quality",
    ] {
        assert_eq!(
            call(&replica_api, path, None).await["matrix_version"],
            VERSION,
            "{path}"
        );
    }
    assert_eq!(
        call(&replica_api, "/v1/recommend", Some(request)).await["matrix_version"],
        VERSION
    );
    let traced: Vec<String> =
        sqlx::query_scalar("select matrix_version from recommendations order by created_at")
            .fetch_all(&rp)
            .await
            .unwrap();
    assert_eq!(traced, vec!["", VERSION, VERSION]);
    op.close().await;
    rp.close().await;
    origin.close().await;
    replica.close().await;
}
