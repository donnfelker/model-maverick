//! Package 2d proxy closeout: the proxy calls the real Rust engine, not a mock engine,
//! then relays the selected chain entry to a fake upstream over HTTP.

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::Request;
use maverick_api::{
    ApiState, AuthState, HealthState, Identity, RateLimiter, StaticAuthBackend, public_router_with,
};
use maverick_core::chain::ChainExecutor;
use maverick_core::recommend::{RecommendClient, RecommendClientConfig};
use maverick_domain::keys;
use maverick_proxy::config::load_config;
use maverick_proxy::server::{Proxy, ServeSettings, serve};
use maverick_proxy::surface::{ExecutorRunner, NoReports, Upstreams};
use maverick_store::catalog::{self, NewModelColumn};
use maverick_store::measurement::{self, AggregateUpsert};
use maverick_store::{JsonRawValue, MIGRATOR, corpus, tenancy};
use maverick_testdb::{Schema, TestDatabase};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::net::TcpListener;

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

const TENANT: &str = "ten_proxy_real_engine";
const KEY: &str = "key_proxy_real_engine";
const CORPUS: &str = "proxy-real-engine-corpus";
const COLUMN: &str = "col_proxy_real_engine";
const MODEL: &str = "gpt-proxy-real-engine";

#[derive(Debug, Clone)]
struct Seen {
    target: String,
    headers: Vec<(String, String)>,
    body: Value,
}

type SeenLog = Arc<Mutex<Vec<Seen>>>;

fn raw(text: &str) -> Box<JsonRawValue> {
    JsonRawValue::from_string(text.to_owned()).expect("json")
}

async fn seed_engine() -> Option<(TestDatabase, PgPool)> {
    let db = maverick_testdb::database(&SCHEMA).await?;
    let pool = db.pool().await;
    tenancy::create_tenant(&pool, TENANT, "Proxy Smoke", &[7; 32])
        .await
        .expect("tenant");
    tenancy::create_api_key(&pool, KEY, TENANT, b"hash", "abcd", "proxy smoke")
        .await
        .expect("key");
    corpus::upsert_corpus_version(&pool, CORPUS, "proxy smoke", 0, false)
        .await
        .expect("corpus");
    corpus::activate_corpus_version(&pool, CORPUS)
        .await
        .expect("activate corpus");

    let empty = raw("{}");
    catalog::create_model_column(
        &pool,
        &NewModelColumn {
            id: COLUMN,
            provider: "openai",
            model_id: MODEL,
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
    .expect("column");
    tenancy::create_pool(
        &pool,
        "pool_proxy_real_engine",
        TENANT,
        "default",
        &[COLUMN.to_owned()],
        true,
    )
    .await
    .expect("pool");

    for (tier, pass_rate) in [(1, 0.99), (2, 0.98)] {
        aggregate(&pool, tier, "pass_rate", pass_rate).await;
    }
    aggregate(&pool, 2, "quality", 0.97).await;
    aggregate(&pool, 2, "cost_per_run_usd", 0.002).await;
    aggregate(&pool, 2, "latency_p95_ms", 850.0).await;

    Some((db, pool))
}

async fn aggregate(pool: &PgPool, tier: i16, dimension: &str, ewma: f32) {
    measurement::upsert_aggregate(
        pool,
        &AggregateUpsert {
            model_column_id: COLUMN,
            capability: "factual_qa",
            difficulty_tier: tier,
            dimension,
            ewma,
            variance: 0.0,
            n: 20,
            measured: true,
            matrix_version: CORPUS,
            as_of: chrono::Utc::now().date_naive(),
        },
    )
    .await
    .expect("aggregate");
}

async fn start_engine(pool: &PgPool) -> String {
    let auth = AuthState::new(
        Arc::new(StaticAuthBackend::authenticated(Identity {
            tenant_id: TENANT.to_owned(),
            api_key_id: KEY.to_owned(),
        })),
        keys::parse_internal_tokens("ops:alpha").expect("tokens"),
        RateLimiter::new(10_000),
    );
    let health = HealthState::with_providers(pool.clone(), Vec::new());
    let router = public_router_with(auth, ApiState::with_health(pool.clone(), health));
    serve_router(router).await
}

async fn serve_router(router: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve");
    });
    url
}

async fn fake_upstream() -> (String, SeenLog) {
    let seen: SeenLog = Arc::default();
    let handler = {
        let seen = Arc::clone(&seen);
        move |request: Request| {
            let seen = Arc::clone(&seen);
            async move {
                let target = request
                    .uri()
                    .path_and_query()
                    .map(ToString::to_string)
                    .unwrap_or_default();
                let headers = request
                    .headers()
                    .iter()
                    .map(|(name, value)| {
                        (
                            name.as_str().to_owned(),
                            value.to_str().unwrap_or_default().to_owned(),
                        )
                    })
                    .collect();
                let bytes: Bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
                    .await
                    .expect("body");
                let body = serde_json::from_slice(&bytes).expect("json body");
                seen.lock().expect("seen").push(Seen {
                    target,
                    headers,
                    body,
                });
                (
                    axum::http::StatusCode::OK,
                    [("content-type", "application/json")],
                    json!({
                        "id": "chatcmpl_proxy_real_engine",
                        "choices": [{
                            "index": 0,
                            "message": {"role": "assistant", "content": "ok"},
                            "finish_reason": "stop"
                        }],
                        "usage": {"prompt_tokens": 7, "completion_tokens": 3}
                    })
                    .to_string(),
                )
            }
        }
    };
    (serve_router(Router::new().fallback(handler)).await, seen)
}

async fn start_proxy(engine_url: &str, openai_url: &str) -> String {
    let env: BTreeMap<&str, &str> = [
        ("MAVERICK_ENGINE_URL", engine_url),
        ("MAVERICK_API_KEY", "mk_live_proxy_real_engine"),
        ("MAVERICK_PROXY_TELEMETRY", "off"),
        ("MAVERICK_UPSTREAM_OPENAI", openai_url),
        ("MAVERICK_UPSTREAM_ANTHROPIC", "http://127.0.0.1:1"),
        ("MAVERICK_UPSTREAM_GOOGLE", "http://127.0.0.1:1"),
        ("MAVERICK_UPSTREAM_XAI", "http://127.0.0.1:1"),
        ("OPENAI_API_KEY", "sk-proxy-real-engine-openai"),
    ]
    .into_iter()
    .collect();
    let config =
        load_config(|name| env.get(name).map(|value| (*value).to_owned())).expect("proxy config");
    let http = reqwest::Client::builder().no_proxy().build().expect("http");
    let engine = RecommendClient::with_http(
        http.clone(),
        RecommendClientConfig {
            base_url: config.engine_url.clone(),
            timeout: Duration::from_secs(10),
            max_retries: 0,
        },
    );
    let runner = ExecutorRunner::with_executor(
        ChainExecutor::new(http),
        Upstreams::from_config(&config),
        Arc::new(NoReports),
    );
    let router = Proxy::with_engine(config, engine, Arc::new(runner)).router();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind proxy");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        serve(
            listener,
            router,
            ServeSettings::default(),
            std::future::pending::<()>(),
        )
        .await
        .expect("proxy");
    });
    format!("http://{addr}")
}

fn header<'a>(seen: &'a Seen, name: &str) -> Option<&'a str> {
    seen.headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

#[tokio::test]
async fn proxy_routes_through_the_real_engine_to_a_fake_upstream() {
    let Some((db, pool)) = seed_engine().await else {
        return;
    };
    let engine = start_engine(&pool).await;
    let (openai, seen) = fake_upstream().await;
    let proxy = start_proxy(&engine, &openai).await;

    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("client")
        .post(format!("{proxy}/v1/chat/completions"))
        .header("content-type", "application/json")
        .header("x-maverick-workload", "factual_qa:2")
        .body(r#"{"model":"maverick/auto","messages":[{"role":"user","content":"hello"}]}"#)
        .send()
        .await
        .expect("proxy response");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["id"], "chatcmpl_proxy_real_engine");

    let seen = seen.lock().expect("seen").clone();
    assert_eq!(seen.len(), 1, "{seen:#?}");
    let attempt = &seen[0];
    assert_eq!(attempt.target, "/v1/chat/completions");
    assert_eq!(attempt.body["model"], MODEL);
    assert_eq!(
        header(attempt, "authorization"),
        Some("Bearer sk-proxy-real-engine-openai")
    );
    assert_eq!(header(attempt, "x-maverick-api-key"), None);
    assert!(
        attempt
            .headers
            .iter()
            .all(|(_, value)| !value.contains("mk_live")),
        "{:?}",
        attempt.headers
    );

    pool.close().await;
    db.close().await;
}
