//! `GET /up` and `GET /health/detail` on real Postgres (reference Part 5 §8; D35).

use std::sync::{Arc, LazyLock};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::{Router, response::Response};
use maverick_api::{
    ApiState, AuthState, HealthState, Identity, RateLimiter, StaticAuthBackend,
    internal_router_with, public_router_with,
};
use maverick_domain::keys;
use maverick_store::health::GRAPHILE_WORKER_SCHEMA;
use maverick_testdb::Schema;
use serde_json::{Value, json};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;

/// The engine migrations and graphile_worker's, as a deployment has them.
static SCHEMA: LazyLock<Schema> = LazyLock::new(|| {
    Schema::sqlx(&maverick_store::MIGRATOR).then(Schema::custom(
        "graphile_worker 0.13",
        |url| async move {
            let pool = PgPoolOptions::new()
                .max_connections(2)
                .connect(&url)
                .await?;
            graphile_worker::WorkerUtils::new(pool.clone(), GRAPHILE_WORKER_SCHEMA)
                .migrate()
                .await?;
            pool.close().await;
            Ok(())
        },
    ))
});

/// Engine migrations only: no graphile_worker schema, so the queue read fails.
static NO_QUEUE: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&maverick_store::MIGRATOR));

fn auth() -> AuthState {
    AuthState::new(
        Arc::new(StaticAuthBackend::authenticated(Identity {
            tenant_id: "ten_test".to_owned(),
            api_key_id: "key_test".to_owned(),
        })),
        keys::parse_internal_tokens("ops:alpha,rails:beta").expect("tokens"),
        RateLimiter::new(300),
    )
}

fn internal(pool: PgPool, providers: Vec<&'static str>) -> Router {
    let health = HealthState::with_providers(pool.clone(), providers);
    internal_router_with(auth(), ApiState::with_health(pool, health))
}

fn public(pool: PgPool) -> Router {
    let health = HealthState::with_providers(pool.clone(), Vec::new());
    public_router_with(auth(), ApiState::with_health(pool, health))
}

async fn get(router: Router, path: &str, token: Option<&str>) -> (StatusCode, String, Response) {
    let mut request = Request::builder().uri(path);
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let response = router
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let (parts, body) = response.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    (status, text, Response::from_parts(parts, Body::empty()))
}

fn parse(body: &str) -> Value {
    serde_json::from_str(body).unwrap_or_else(|err| panic!("not JSON ({err}): {body}"))
}

async fn activate_corpus(pool: &PgPool, version: &str) {
    maverick_store::corpus::upsert_corpus_version(pool, version, "", 0, false)
        .await
        .expect("version");
    maverick_store::corpus::activate_corpus_version(pool, version)
        .await
        .expect("activate");
}

// parity: api.TestUpOKWithLiveDatabase
#[tokio::test]
async fn up_is_ok_with_a_live_database() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let (status, body, response) = get(public(pool.clone()), "/up", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "{\"status\":\"ok\"}\n");
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    pool.close().await;
    db.close().await;
}

// parity: api.TestUpReturns503WhenDatabaseUnreachable
#[tokio::test]
async fn up_is_503_when_the_database_is_unreachable() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    pool.close().await; // a server built on a closed pool
    let (status, body, _) = get(public(pool), "/up", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body, "{\"status\":\"unavailable\"}\n");
    db.close().await;
}

// parity: api.TestInternalHealthDetailRequiresBearer
#[tokio::test]
async fn health_detail_requires_the_internal_token() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    for token in [None, Some("ops:wrong")] {
        let (status, body, _) =
            get(internal(pool.clone(), Vec::new()), "/health/detail", token).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{token:?}: {body}");
        assert_eq!(parse(&body)["error"]["code"], "unauthorized", "{body}");
    }
    pool.close().await;
    db.close().await;
}

// parity: api.TestInternalHealthDetailAcceptsEachRotationToken
#[tokio::test]
async fn health_detail_accepts_each_rotation_token() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    for token in ["ops:alpha", "rails:beta"] {
        let (status, body, _) = get(
            internal(pool.clone(), Vec::new()),
            "/health/detail",
            Some(token),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{token}: {body}");
    }
    pool.close().await;
    db.close().await;
}

// parity: api.TestInternalHealthDetailShape
// D35: `job_queue` replaces Go's `river_queues`; the rest of the recorded body is Go's.
#[tokio::test]
async fn health_detail_shape_matches_the_recorded_body() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    activate_corpus(&pool, "2026-07-01").await;
    let (status, body, _) = get(
        internal(pool.clone(), Vec::new()),
        "/health/detail",
        Some("ops:alpha"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        concat!(
            r#"{"status":"ok","components":{"db":{"status":"ok"},"#,
            r#""job_queue":{"status":"no_workers","detail":"no graphile_worker worker is registered"},"#,
            r#""provider_client_config":{"status":"none_configured","detail":"no provider API keys set"}},"#,
            r#""active_matrix_version":"2026-07-01","last_batch_times":{}}"#,
            "\n"
        )
    );
    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn health_detail_reports_workers_providers_and_batch_times() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    sqlx::query("SELECT graphile_worker.worker_heartbeat('graphile_worker_1')")
        .execute(&pool)
        .await
        .expect("heartbeat");
    sqlx::raw_sql(
        "INSERT INTO batches (id, kind, batch_window, status, completed_at) VALUES
           ('bat_1', 'nightly_smoke', '2026-07-07', 'completed', '2026-07-07T03:40:00Z'),
           ('bat_2', 'nightly_smoke', '2026-07-08', 'completed', '2026-07-08T03:41:12Z'),
           ('bat_3', 'aggregate_rebuild', 'bat_2', 'completed', '2026-07-08T03:45:00.25Z');
         INSERT INTO batches (id, kind, batch_window, status) VALUES
           ('bat_4', 'weekly_sweep', '2026-W28', 'running');",
    )
    .execute(&pool)
    .await
    .expect("batches");

    let router = || internal(pool.clone(), vec!["anthropic", "openai"]);
    let (status, body, _) = get(router(), "/health/detail", Some("ops:alpha")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let detail = parse(&body);
    assert_eq!(
        detail["components"]["job_queue"],
        json!({"status": "ok", "detail": "graphile_worker queues: default"}),
        "{body}"
    );
    assert_eq!(
        detail["components"]["provider_client_config"],
        json!({"status": "ok", "detail": "configured: anthropic, openai"}),
        "{body}"
    );
    assert_eq!(
        detail["active_matrix_version"],
        Value::Null,
        "cold stack: {body}"
    );
    assert_eq!(
        detail["last_batch_times"],
        json!({"aggregate_rebuild": "2026-07-08T03:45:00.25Z", "nightly_smoke": "2026-07-08T03:41:12Z"}),
        "{body}"
    );

    sqlx::query("UPDATE graphile_worker._private_workers SET last_heartbeat_at = now() - interval '10 minutes'")
        .execute(&pool)
        .await
        .expect("stale");
    let (_, body, _) = get(router(), "/health/detail", Some("ops:alpha")).await;
    let job_queue = &parse(&body)["components"]["job_queue"];
    assert_eq!(job_queue["status"], "stale", "{body}");
    let detail = job_queue["detail"].as_str().unwrap();
    assert!(
        detail.starts_with("no worker heartbeat for 60")
            && detail.ends_with("s (graphile_worker queues: default)"),
        "{body}"
    );
    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn health_detail_is_degraded_when_the_database_is_down() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    pool.close().await;
    let (status, body, _) = get(
        internal(pool, vec!["xai"]),
        "/health/detail",
        Some("ops:alpha"),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    let detail = parse(&body);
    assert_eq!(detail["status"], "degraded", "{body}");
    assert_eq!(detail["components"]["db"]["status"], "error", "{body}");
    assert!(
        !detail["components"]["db"]["detail"]
            .as_str()
            .unwrap()
            .is_empty(),
        "{body}"
    );
    assert_eq!(
        detail["components"]["job_queue"],
        json!({"status": "unknown", "detail": "database unreachable"}),
        "{body}"
    );
    assert_eq!(
        detail["components"]["provider_client_config"],
        json!({"status": "ok", "detail": "configured: xai"}),
        "{body}"
    );
    assert_eq!(detail["active_matrix_version"], Value::Null, "{body}");
    assert_eq!(detail["last_batch_times"], Value::Null, "{body}");
    let keys: Vec<&String> = detail.as_object().unwrap().keys().collect();
    assert_eq!(keys.len(), 4, "{body}");
    db.close().await;
}

#[tokio::test]
async fn a_failed_queue_read_is_unknown_but_the_service_stays_ok() {
    let Some(db) = maverick_testdb::database(&NO_QUEUE).await else {
        return;
    };
    let pool = db.pool().await;
    let (status, body, _) = get(
        internal(pool.clone(), Vec::new()),
        "/health/detail",
        Some("ops:alpha"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let detail = parse(&body);
    assert_eq!(detail["status"], "ok", "{body}");
    assert_eq!(
        detail["components"]["job_queue"],
        json!({"status": "unknown", "detail": "querying job queue state failed"}),
        "{body}"
    );
    assert_eq!(detail["last_batch_times"], Value::Null, "{body}");
    pool.close().await;
    db.close().await;
}
