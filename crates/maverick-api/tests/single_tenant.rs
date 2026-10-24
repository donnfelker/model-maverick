//! A single-tenant replica on real Postgres (spec §14.4, D49): the bootstrap creates one
//! tenant and one key exactly once, and the internal API refuses tenant administration
//! with `409 single_tenant_install` while every other route works.

use std::sync::{Arc, LazyLock};

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use maverick_api::{
    ApiState, AuthState, HealthState, InternalSettings, PgAuthBackend, RateLimiter,
    internal_router_with, provision,
};
use maverick_config::Role;
use maverick_domain::keys;
use maverick_store::catalog::{NewModelColumn, create_model_column};
use maverick_store::{JsonRawValue, MIGRATOR, tenancy};
use maverick_testdb::Schema;
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

const OPS: &str = "ops:alpha";
const REFUSED: &str = "{\"error\":{\"code\":\"single_tenant_install\",\"message\":\"this install has a single tenant (MAVERICK_ROLE=replica)\",\"details\":{}}}\n";

/// Once per binary, a global subscriber, so the scoped capture below never misses a
/// callsite that another thread registered first (bead 3bgl).
fn every_callsite_enabled() {
    static GLOBAL: std::sync::Once = std::sync::Once::new();
    GLOBAL.call_once(|| {
        let _ = tracing::subscriber::set_global_default(tracing_subscriber::registry());
    });
}

macro_rules! db {
    () => {{
        every_callsite_enabled();
        let Some(db) = maverick_testdb::database(&SCHEMA).await else {
            return;
        };
        db
    }};
}

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

async fn ops(router: &Router, method: &str, path: &str, body: &str) -> (u16, String) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {OPS}"))
        .body(Body::from(body.to_owned()))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

async fn active_column(pool: &PgPool, id: &str) {
    let empty = JsonRawValue::from_string("{}".to_owned()).unwrap();
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

async fn bootstrap(pool: &PgPool, name: &str) -> Option<provision::Bootstrapped> {
    let mut conn = pool.acquire().await.unwrap();
    provision::bootstrap(&mut conn, name).await.unwrap()
}

/// DIVERGENCE: D49. The first start creates one tenant with the default pool and one
/// API key, with the internal API's audit lines; a later start creates nothing.
#[tokio::test]
async fn bootstrap_creates_one_tenant_and_one_key_once() {
    let db = db!();
    let pool = db.pool().await;
    active_column(&pool, "col_a").await;
    let capture = test_support::init_test_tracing();

    let created = bootstrap(&pool, "Acme Corp")
        .await
        .expect("first start creates");
    // G5: the raw key reaches no log line.
    let lines = capture.captured_lines();
    assert!(
        lines
            .iter()
            .all(|line| !line.contains(&created.key.raw_key)),
        "{lines:?}"
    );
    assert_eq!(created.tenant.name, "Acme Corp");
    assert_eq!(created.key.name, "bootstrap");
    assert!(created.key.raw_key.starts_with("mk_live_"));
    let stored = tenancy::get_api_key_by_hash(&pool, &keys::key_hash(&created.key.raw_key))
        .await
        .unwrap();
    assert_eq!(
        (
            stored.id.as_str(),
            stored.tenant_id.as_str(),
            stored.status.as_str()
        ),
        (
            created.key.id.as_str(),
            created.tenant.id.as_str(),
            "active"
        )
    );
    let pools = tenancy::list_pools_by_tenant(&pool, &created.tenant.id)
        .await
        .unwrap();
    assert_eq!(pools.len(), 1);
    assert_eq!(
        (pools[0].name.as_str(), pools[0].is_default),
        ("default", true)
    );
    assert_eq!(pools[0].model_column_ids, ["col_a"]);

    assert!(bootstrap(&pool, "Acme Corp").await.is_none());
    assert!(bootstrap(&pool, "Other").await.is_none());
    let tenants = tenancy::list_tenants(&pool).await.unwrap();
    assert_eq!(tenants.len(), 1);
    let keys = tenancy::list_api_keys_by_tenant(&pool, &created.tenant.id)
        .await
        .unwrap();
    assert_eq!(keys.len(), 1);

    let audits: Vec<(String, String)> = capture
        .captured_json()
        .into_iter()
        .filter(|line| line["msg"] == "internal provisioning write")
        .map(|line| {
            assert_eq!(line["actor"], "bootstrap", "{line}");
            (
                line["action"].as_str().unwrap().to_owned(),
                line["tenant_id"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        audits,
        [
            ("tenant.create".to_owned(), created.tenant.id.clone()),
            ("api_key.issue".to_owned(), created.tenant.id.clone()),
        ]
    );
    db.close().await;
}

/// D49: concurrent first starts create one tenant and one key (an advisory lock).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_first_starts_create_one_tenant() {
    let db = db!();
    let pool = db.pool().await;
    let starts: Vec<_> = (0..8)
        .map(|_| {
            let pool = pool.clone();
            tokio::spawn(async move { bootstrap(&pool, "default").await.is_some() })
        })
        .collect();
    let mut created = 0;
    for start in starts {
        created += usize::from(start.await.unwrap());
    }
    assert_eq!(created, 1);
    let tenants = tenancy::list_tenants(&pool).await.unwrap();
    assert_eq!(tenants.len(), 1);
    assert_eq!(tenants[0].name, "default");
    let keys = tenancy::list_api_keys_by_tenant(&pool, &tenants[0].id)
        .await
        .unwrap();
    assert_eq!(keys.len(), 1);
    db.close().await;
}

/// DIVERGENCE: D49. On a replica, tenant create, suspend, activate, and limit overrides
/// answer 409 before any lookup or body read; a wrong method is still 404. Every other
/// internal route works.
#[tokio::test]
async fn replica_refuses_tenant_administration() {
    let db = db!();
    let pool = db.pool().await;
    let tenant = bootstrap(&pool, "default").await.unwrap().tenant.id;
    let replica = internal(&pool, Role::Replica);

    for (method, path, body) in [
        (
            "POST",
            "/internal/v1/tenants".to_owned(),
            r#"{"name":"second"}"#,
        ),
        ("POST", "/internal/v1/tenants".to_owned(), "not json"),
        ("POST", format!("/internal/v1/tenants/{tenant}/suspend"), ""),
        (
            "POST",
            format!("/internal/v1/tenants/{tenant}/activate"),
            "",
        ),
        (
            "POST",
            "/internal/v1/tenants/ten_missing/suspend".to_owned(),
            "",
        ),
        (
            "PUT",
            format!("/internal/v1/tenants/{tenant}/limits"),
            r#"{"classification_daily_quota":5}"#,
        ),
        (
            "PUT",
            "/internal/v1/tenants/ten_missing/limits".to_owned(),
            "{",
        ),
    ] {
        assert_eq!(
            ops(&replica, method, &path, body).await,
            (409, REFUSED.to_owned()),
            "{method} {path}"
        );
    }
    let (status, body) = ops(&replica, "GET", "/internal/v1/tenants", "").await;
    assert_eq!(status, 404, "{body}");
    let tenants = tenancy::list_tenants(&pool).await.unwrap();
    assert_eq!(tenants.len(), 1);
    assert_eq!(tenants[0].status, "active");
    assert_eq!(tenants[0].classification_quota_override, None);

    // Everything else still works on a replica.
    let (status, body) = ops(
        &replica,
        "GET",
        &format!("/internal/v1/tenants/{tenant}"),
        "",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = ops(
        &replica,
        "POST",
        &format!("/internal/v1/tenants/{tenant}/api-keys"),
        r#"{"name":"app"}"#,
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let issued: Value = serde_json::from_str(&body).unwrap();
    let (status, body) = ops(
        &replica,
        "DELETE",
        &format!("/internal/v1/api-keys/{}", issued["id"].as_str().unwrap()),
        "",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["status"],
        "revoked"
    );

    // The same routes work on an origin.
    let origin = internal(&pool, Role::Origin);
    let (status, body) = ops(
        &origin,
        "POST",
        "/internal/v1/tenants",
        r#"{"name":"second"}"#,
    )
    .await;
    assert_eq!(status, 201, "{body}");
    for (method, path, body) in [
        ("POST", format!("/internal/v1/tenants/{tenant}/suspend"), ""),
        (
            "POST",
            format!("/internal/v1/tenants/{tenant}/activate"),
            "",
        ),
        (
            "PUT",
            format!("/internal/v1/tenants/{tenant}/limits"),
            r#"{"classification_daily_quota":5}"#,
        ),
    ] {
        let (status, response) = ops(&origin, method, &path, body).await;
        assert_eq!(status, 200, "{method} {path}: {response}");
    }
    db.close().await;
}
