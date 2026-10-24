//! The acceptance harness: the real engine and the real proxy in one process.
//!
//! [`Stack::start`] creates a migrated database (testcontainers), seeds the catalog and
//! the measurements every suite shares, and serves the engine's public and internal
//! routers the way `maverick serve` builds them: the [`Config`] from environment values,
//! `AuthState` and `ApiState` from that config, and the classifier resolver connected
//! through the catalog. The only substitutes are the provider clients: two [`Recorder`]s
//! answer the two classifier columns and keep every prompt they receive.
//!
//! [`Stack::proxy`] starts the proxy the way its binary does (hosted mode, from
//! environment values) against this engine and against [`Upstream`] mocks that keep every
//! request. [`Collector`] is a mock OTLP/HTTP collector that keeps every payload.
//! [`Stack::set_engine_down`] makes the engine's public listener answer `503`, so a suite
//! can show what the proxy does during an engine outage (D46).
//!
//! So a suite can show where a prompt went, and where it did not.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::Request;
use axum::middleware::{self, Next};
use axum::response::Response;
use chrono::{DateTime, Utc};
use maverick_api::classify::{Resolver, ResolverSettings};
use maverick_api::{
    ApiState, AuthState, RecommendConfig, internal_router_with, public_router_with,
};
use maverick_config::Config;
use maverick_domain::keys;
use maverick_providers::{
    CompletionRequest, CompletionResponse, ProviderClient, ProviderError, ProviderErrorKind,
    ProviderFuture,
};
use maverick_proxy::reports::ProxyReports;
use maverick_proxy::server::{Proxy, ServeSettings, serve};
use maverick_proxy::surface::ExecutorRunner;
use maverick_store::catalog::{self, NewModelColumn};
use maverick_store::measurement::{self, AggregateUpsert};
use maverick_store::{JsonRawValue, MIGRATOR, corpus, tenancy};
use maverick_testdb::{Schema, TestDatabase};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// The internal token `<label>:<secret>`.
pub const OPS_TOKEN: &str = "ops:alpha";
/// The capture master key (64 hex characters).
pub const MASTER_KEY_HEX: &str = "c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4";
/// The classifier column (`<model>/<endpoint>`); its provider is `anthropic`.
pub const CLASSIFIER_COLUMN: &str = "claude-haiku-4-5/direct";
/// The zero-retention classifier column; its provider is `google`, and its evidence is
/// valid.
pub const ZR_CLASSIFIER_COLUMN: &str = "gemini-2.5-flash/vertex";
/// The classifier's answer: `structured_extraction` at difficulty 3.
pub const LEGAL: &str = r#"{"capability":"structured_extraction","difficulty":3,"domain_tags":["legal"],"confidence":0.91}"#;
/// A declared recommend body the pool columns are measured for.
pub const DECLARED: &str = r#"{"workload":{"capability":"structured_extraction","difficulty":3}}"#;

/// The measured pool columns, best first (by quality).
///
/// | id | provider, model | retention | `openrouter` alias evidence |
/// |---|---|---|---|
/// | `col_opus` | anthropic `claude-opus-4-6` | zero | none (`unknown`) |
/// | `col_claude` | anthropic `claude-sonnet-4-6` | zero | zero |
/// | `col_gpt` | openai `gpt-5` | standard | none |
/// | `col_gemini` | google `gemini-2.5-pro` | none (`unknown`) | no alias |
pub const OPUS: &str = "col_opus";
pub const CLAUDE: &str = "col_claude";
pub const GPT: &str = "col_gpt";
pub const GEMINI: &str = "col_gemini";

const CORPUS: &str = "2026-07-01";

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---- fakes ----

/// A provider client that answers `reply` (`None` fails) and keeps every prompt.
#[derive(Clone)]
pub struct Recorder {
    name: &'static str,
    reply: Arc<Mutex<Option<String>>>,
    prompts: Arc<Mutex<Vec<String>>>,
}

impl Recorder {
    #[must_use]
    pub fn new(name: &'static str, reply: Option<&str>) -> Self {
        Self {
            name,
            reply: Arc::new(Mutex::new(reply.map(str::to_owned))),
            prompts: Arc::default(),
        }
    }

    /// Every prompt received, oldest first.
    #[must_use]
    pub fn prompts(&self) -> Vec<String> {
        lock(&self.prompts).clone()
    }
}

impl ProviderClient for Recorder {
    fn complete<'a>(&'a self, request: CompletionRequest) -> ProviderFuture<'a> {
        lock(&self.prompts).push(request.prompt);
        let reply = lock(&self.reply).clone();
        Box::pin(async move {
            match reply {
                Some(text) => Ok(CompletionResponse {
                    text,
                    tokens_in: 10,
                    tokens_out: 5,
                }),
                None => Err(ProviderError::new(
                    self.name,
                    ProviderErrorKind::ProviderError,
                )),
            }
        })
    }

    fn name(&self) -> &str {
        self.name
    }
}

/// One request a mock received.
#[derive(Debug, Clone)]
pub struct Seen {
    pub method: String,
    /// Path and query.
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Seen {
    /// The body as JSON (`Null` when it is not JSON).
    #[must_use]
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }

    /// The first value of a header (names are lowercase).
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

type SeenLog = Arc<Mutex<Vec<Seen>>>;

/// Serves `app` on a local port and returns its base URL.
async fn serve_local(app: Router) -> (String, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local address");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
    (format!("http://{addr}"), addr)
}

/// Keeps the request and gives it back with its body re-attached.
async fn keep(seen: &SeenLog, request: Request) -> Seen {
    let method = request.method().to_string();
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
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect();
    let bytes: Bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
        .await
        .unwrap_or_default();
    let seen_request = Seen {
        method,
        target,
        headers,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    };
    lock(seen).push(seen_request.clone());
    seen_request
}

/// A provider upstream: it keeps every request and answers a success in the wire format
/// of the path (`/messages` is Anthropic's, anything else OpenAI's chat completion), with
/// `completion` as the text. [`Upstream::fail`] scripts an error status for one model.
#[derive(Clone)]
pub struct Upstream {
    pub url: String,
    seen: SeenLog,
    failures: Arc<Mutex<HashMap<String, u16>>>,
}

impl Upstream {
    pub async fn start(completion: &str) -> Self {
        let seen: SeenLog = Arc::default();
        let failures: Arc<Mutex<HashMap<String, u16>>> = Arc::default();
        let completion = completion.to_owned();
        let log = seen.clone();
        let scripted = failures.clone();
        let handler = move |request: Request| {
            let log = log.clone();
            let completion = completion.clone();
            let scripted = scripted.clone();
            async move {
                let seen = keep(&log, request).await;
                let model = seen.json()["model"].as_str().unwrap_or_default().to_owned();
                if let Some(status) = lock(&scripted).get(&model).copied() {
                    let status = axum::http::StatusCode::from_u16(status).expect("a status");
                    let error =
                        json!({"error": {"type": "rate_limit_error", "message": "scripted"}});
                    return (
                        status,
                        [("content-type", "application/json")],
                        error.to_string(),
                    );
                }
                let body = if seen.target.contains("/messages") {
                    json!({
                        "id": "msg_1", "type": "message", "role": "assistant",
                        "model": seen.json()["model"],
                        "content": [{"type": "text", "text": completion}],
                        "stop_reason": "end_turn",
                        "usage": {"input_tokens": 12, "output_tokens": 7}
                    })
                } else {
                    json!({
                        "id": "chatcmpl-1", "object": "chat.completion",
                        "model": seen.json()["model"],
                        "choices": [{"index": 0, "finish_reason": "stop",
                                     "message": {"role": "assistant", "content": completion}}],
                        "usage": {"prompt_tokens": 12, "completion_tokens": 7, "total_tokens": 19}
                    })
                };
                (
                    axum::http::StatusCode::OK,
                    [("content-type", "application/json")],
                    body.to_string(),
                )
            }
        };
        let (url, _) = serve_local(Router::new().fallback(handler)).await;
        Self {
            url,
            seen,
            failures,
        }
    }

    /// From now on, a request for `model` gets `status` and an error body.
    pub fn fail(&self, model: &str, status: u16) {
        lock(&self.failures).insert(model.to_owned(), status);
    }

    /// Every model succeeds again.
    pub fn clear_failures(&self) {
        lock(&self.failures).clear();
    }

    /// Every request received, oldest first.
    #[must_use]
    pub fn seen(&self) -> Vec<Seen> {
        lock(&self.seen).clone()
    }
}

/// The five upstreams a hosted proxy can reach.
#[derive(Clone)]
pub struct Upstreams {
    pub anthropic: Upstream,
    pub openai: Upstream,
    pub google: Upstream,
    pub xai: Upstream,
    pub openrouter: Upstream,
}

impl Upstreams {
    pub async fn start(completion: &str) -> Self {
        Self {
            anthropic: Upstream::start(completion).await,
            openai: Upstream::start(completion).await,
            google: Upstream::start(completion).await,
            xai: Upstream::start(completion).await,
            openrouter: Upstream::start(completion).await,
        }
    }

    /// Every upstream with its name.
    #[must_use]
    pub fn all(&self) -> [(&'static str, &Upstream); 5] {
        [
            ("anthropic", &self.anthropic),
            ("openai", &self.openai),
            ("google", &self.google),
            ("xai", &self.xai),
            ("openrouter", &self.openrouter),
        ]
    }

    /// Every request any upstream received.
    #[must_use]
    pub fn seen(&self) -> Vec<Seen> {
        self.all()
            .iter()
            .flat_map(|(_, upstream)| upstream.seen())
            .collect()
    }
}

/// Every export a [`Collector`] received: (path, protobuf body).
type Exports = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

/// A mock OTLP/HTTP collector: it keeps every export's path and protobuf bytes.
#[derive(Clone)]
pub struct Collector {
    pub url: String,
    received: Exports,
}

impl Collector {
    pub async fn start() -> Self {
        let received: Exports = Arc::default();
        let log = received.clone();
        let handler = move |request: Request| {
            let log = log.clone();
            async move {
                let path = request.uri().path().to_owned();
                let bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
                    .await
                    .unwrap_or_default();
                lock(&log).push((path, bytes.to_vec()));
            }
        };
        let (url, _) = serve_local(Router::new().fallback(handler)).await;
        Self { url, received }
    }

    /// Every export received: (path, body).
    #[must_use]
    pub fn received(&self) -> Vec<(String, Vec<u8>)> {
        lock(&self.received).clone()
    }
}

// ---- the engine ----

/// Engine options.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Configure [`ZR_CLASSIFIER_COLUMN`] as `classifier.zero_retention_column`.
    pub zero_retention_classifier: bool,
}

/// One request the engine's public listener received.
#[derive(Debug, Clone)]
pub struct Hit {
    pub method: String,
    pub path: String,
    /// The bearer token, if any.
    pub bearer: Option<String>,
}

/// A tenant with its raw API key.
#[derive(Debug, Clone)]
pub struct Tenant {
    pub id: String,
    pub key: String,
}

/// An HTTP response.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub body: String,
    /// Every header, names lowercase.
    pub headers: Vec<(String, String)>,
}

impl Reply {
    /// The first value of a header (names are lowercase).
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// The body as JSON (panics with the body when it is not JSON).
    #[must_use]
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|error| panic!("not JSON ({error}): {}", self.body))
    }
}

/// The engine, its database, and its classifier fakes.
pub struct Stack {
    _db: TestDatabase,
    pub pool: PgPool,
    pub config: Config,
    /// The public listener's base URL.
    pub engine_url: String,
    /// The internal listener's base URL.
    pub internal_url: String,
    /// The provider of [`CLASSIFIER_COLUMN`].
    pub classifier: Recorder,
    /// The provider of [`ZR_CLASSIFIER_COLUMN`].
    pub zr_classifier: Recorder,
    hits: Arc<Mutex<Vec<Hit>>>,
    /// While set, the public listener answers every request with `503`.
    down: Arc<AtomicBool>,
    http: reqwest::Client,
}

impl Stack {
    /// A seeded, serving engine; `None` without Docker (as every store test).
    pub async fn start(options: Options) -> Option<Stack> {
        static SCHEMA: std::sync::LazyLock<Schema> =
            std::sync::LazyLock::new(|| Schema::sqlx(&MIGRATOR));
        let db = maverick_testdb::database(&SCHEMA).await?;
        let pool = db.pool().await;
        seed(&pool).await;

        let mut env: HashMap<&str, String> = HashMap::from([
            ("MAVERICK_ROLE", "origin".to_owned()),
            ("MAVERICK_DATABASE_URL", db.url().to_owned()),
            ("MAVERICK_INTERNAL_TOKENS", OPS_TOKEN.to_owned()),
            ("MAVERICK_CAPTURE_MASTER_KEY", MASTER_KEY_HEX.to_owned()),
            ("MAVERICK_CLASSIFIER_COLUMN", CLASSIFIER_COLUMN.to_owned()),
            ("MAVERICK_RATELIMIT_RPM", "100000".to_owned()),
        ]);
        if options.zero_retention_classifier {
            env.insert(
                "MAVERICK_CLASSIFIER_ZERO_RETENTION_COLUMN",
                ZR_CLASSIFIER_COLUMN.to_owned(),
            );
        }
        let config = maverick_config::load_with(|name| env.get(name).cloned())
            .unwrap_or_else(|error| panic!("engine config: {error}"));

        // `maverick serve` step 10, with recording clients for the two classifier columns.
        let classifier = Recorder::new("anthropic", Some(LEGAL));
        let zr_classifier = Recorder::new("google", Some(LEGAL));
        let clients: HashMap<String, Arc<dyn ProviderClient>> = HashMap::from([
            (
                "anthropic".to_owned(),
                Arc::new(classifier.clone()) as Arc<dyn ProviderClient>,
            ),
            (
                "google".to_owned(),
                Arc::new(zr_classifier.clone()) as Arc<dyn ProviderClient>,
            ),
        ]);
        let columns = catalog::list_model_columns(&pool).await.expect("catalog");
        let mut recommend = RecommendConfig::from_config(&config);
        recommend.classifier = Arc::new(
            Resolver::new(ResolverSettings::from_config(&config)).connect(&columns, &clients),
        );
        let auth = AuthState::from_config(pool.clone(), &config).expect("auth");
        let api = ApiState::new(pool.clone(), &config).with_recommend_config(recommend);

        let hits: Arc<Mutex<Vec<Hit>>> = Arc::default();
        let log = hits.clone();
        let down: Arc<AtomicBool> = Arc::default();
        let outage = down.clone();
        let public = public_router_with(auth.clone(), api.clone()).layer(middleware::from_fn(
            move |request: Request, next: Next| {
                let log = log.clone();
                let outage = outage.clone();
                async move {
                    let bearer = request
                        .headers()
                        .get("authorization")
                        .and_then(|value| value.to_str().ok())
                        .and_then(|value| value.strip_prefix("Bearer "))
                        .map(str::to_owned);
                    lock(&log).push(Hit {
                        method: request.method().to_string(),
                        path: request.uri().path().to_owned(),
                        bearer,
                    });
                    if outage.load(Ordering::SeqCst) {
                        return Response::builder()
                            .status(503)
                            .header("content-type", "application/json")
                            .body(axum::body::Body::from(
                                r#"{"error":{"code":"internal_error","message":"the engine is down"}}"#,
                            ))
                            .expect("outage response");
                    }
                    let response: Response = next.run(request).await;
                    response
                }
            },
        ));
        let (engine_url, _) = serve_local(public).await;
        let (internal_url, _) = serve_local(internal_router_with(auth, api)).await;
        Some(Stack {
            _db: db,
            pool,
            config,
            engine_url,
            internal_url,
            classifier,
            zr_classifier,
            hits,
            down,
            http: reqwest::Client::builder()
                .no_proxy()
                .build()
                .expect("http client"),
        })
    }

    /// A tenant whose default pool is `pool`, with its constraints and a minted key.
    pub async fn tenant(
        &self,
        name: &str,
        pool: &[&str],
        zero_retention: bool,
        telemetry_opt_out: bool,
    ) -> Tenant {
        let id = format!("ten_{name}");
        tenancy::create_tenant(&self.pool, &id, name, &[7; 32])
            .await
            .expect("tenant");
        let minted = keys::mint_key();
        tenancy::create_api_key(
            &self.pool,
            &format!("key_{name}"),
            &id,
            &minted.key_hash,
            &minted.last4,
            "",
        )
        .await
        .expect("key");
        tenancy::upsert_tenant_constraints(
            &self.pool,
            &id,
            None,
            None,
            zero_retention,
            telemetry_opt_out,
            false,
        )
        .await
        .expect("constraints");
        let columns: Vec<String> = pool.iter().map(|id| (*id).to_owned()).collect();
        tenancy::create_pool(
            &self.pool,
            &format!("pool_{name}"),
            &id,
            "default",
            &columns,
            true,
        )
        .await
        .expect("pool");
        Tenant {
            id,
            key: minted.raw_key,
        }
    }

    /// A request to the engine's public listener with `key` as the bearer.
    pub async fn engine(&self, method: &str, path: &str, key: &str, body: &str) -> Reply {
        let request = self
            .http
            .request(
                method.parse().expect("method"),
                format!("{}{path}", self.engine_url),
            )
            .header("authorization", format!("Bearer {key}"))
            .header("content-type", "application/json");
        let request = if body.is_empty() {
            request
        } else {
            request.body(body.to_owned())
        };
        reply(request).await
    }

    /// A prompt-form recommend.
    pub async fn recommend_prompt(&self, tenant: &Tenant, prompt: &str) -> Reply {
        let body = json!({"workload": {"prompt": prompt}}).to_string();
        self.engine("POST", "/v1/recommend", &tenant.key, &body)
            .await
    }

    /// While `down`, the public listener answers every request with `503`, as an engine
    /// that is failing would. Requests are still counted in [`Stack::hits`].
    pub fn set_engine_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
    }

    /// Every request the public listener received.
    #[must_use]
    pub fn hits(&self) -> Vec<Hit> {
        lock(&self.hits).clone()
    }

    /// Rows in `table`.
    pub async fn count(&self, table: &str) -> i64 {
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!("select count(*) from {table}")))
            .fetch_one(&self.pool)
            .await
            .expect("count")
    }

    /// The proxy in hosted mode against this engine and `upstreams`, built the way
    /// its binary builds it. `extra` adds or overrides environment values.
    pub async fn proxy(&self, upstreams: &Upstreams, extra: &[(&str, &str)]) -> RunningProxy {
        let mut env: Vec<(String, String)> = vec![
            ("MAVERICK_PROXY_MODE".into(), "hosted".into()),
            ("MAVERICK_ENGINE_URL".into(), self.engine_url.clone()),
            ("MAVERICK_PROXY_RATELIMIT_RPM".into(), "100000".into()),
            (
                "MAVERICK_UPSTREAM_ANTHROPIC".into(),
                upstreams.anthropic.url.clone(),
            ),
            (
                "MAVERICK_UPSTREAM_OPENAI".into(),
                upstreams.openai.url.clone(),
            ),
            (
                "MAVERICK_UPSTREAM_GOOGLE".into(),
                upstreams.google.url.clone(),
            ),
            ("MAVERICK_UPSTREAM_XAI".into(), upstreams.xai.url.clone()),
            (
                "MAVERICK_UPSTREAM_OPENROUTER".into(),
                upstreams.openrouter.url.clone(),
            ),
        ];
        for (name, value) in extra {
            env.retain(|(key, _)| key != name);
            env.push(((*name).to_owned(), (*value).to_owned()));
        }
        let looked_up: Arc<Mutex<Vec<String>>> = Arc::default();
        let config = maverick_proxy::config::load_config(|name| {
            lock(&looked_up).push(name.to_owned());
            env.iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        })
        .unwrap_or_else(|error| panic!("proxy config: {error}"));
        let looked_up = lock(&looked_up).clone();

        // The binary's `run`, with a test listener and a test shutdown signal.
        let reports = Arc::new(ProxyReports::from_config(&config).expect("reports"));
        let runner = ExecutorRunner::new(&config, reports.clone()).expect("runner");
        let proxy = Proxy::new(config, Arc::new(runner)).expect("proxy");
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind proxy");
        let addr = listener.local_addr().expect("proxy address");
        let (stop, stopped) = oneshot::channel::<()>();
        let server = tokio::spawn(serve(
            listener,
            proxy.router(),
            ServeSettings::default(),
            async move {
                let _ = stopped.await;
            },
        ));
        RunningProxy {
            url: format!("http://{addr}"),
            looked_up,
            stop: Some(stop),
            server: Some(server),
            reports,
            http: self.http.clone(),
        }
    }
}

/// Sends a request and reads the reply.
async fn reply(request: reqwest::RequestBuilder) -> Reply {
    let response = request.send().await.expect("response");
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect();
    let body = response.text().await.unwrap_or_default();
    Reply {
        status,
        body,
        headers,
    }
}

/// A running proxy.
pub struct RunningProxy {
    pub url: String,
    /// Every environment name the configuration looked up.
    pub looked_up: Vec<String>,
    stop: Option<oneshot::Sender<()>>,
    server: Option<JoinHandle<io::Result<()>>>,
    reports: Arc<ProxyReports>,
    http: reqwest::Client,
}

impl RunningProxy {
    /// `POST /v1/chat/completions` for the tenant, with `bearer` as the caller's provider
    /// key.
    pub async fn chat(&self, tenant: &Tenant, bearer: Option<&str>, body: &Value) -> Reply {
        let mut request = self
            .http
            .post(format!("{}/v1/chat/completions", self.url))
            .header("x-maverick-api-key", &tenant.key)
            .header("content-type", "application/json")
            .body(body.to_string());
        if let Some(bearer) = bearer {
            request = request.header("authorization", format!("Bearer {bearer}"));
        }
        reply(request).await
    }

    /// `POST /v1/messages` for the tenant, with `api_key` as the caller's Anthropic key.
    pub async fn messages(&self, tenant: &Tenant, api_key: Option<&str>, body: &Value) -> Reply {
        let mut request = self
            .http
            .post(format!("{}/v1/messages", self.url))
            .header("x-maverick-api-key", &tenant.key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .body(body.to_string());
        if let Some(api_key) = api_key {
            request = request.header("x-api-key", api_key);
        }
        reply(request).await
    }

    /// `POST {path}` for the tenant with extra headers (the caller's provider key, the
    /// declared workload, `X-Maverick-Settings`).
    pub async fn post(
        &self,
        path: &str,
        tenant: &Tenant,
        headers: &[(&str, &str)],
        body: &Value,
    ) -> Reply {
        let mut request = self
            .http
            .post(format!("{}{path}", self.url))
            .header("x-maverick-api-key", &tenant.key)
            .header("content-type", "application/json")
            .body(body.to_string());
        if path == "/v1/messages" {
            request = request.header("anthropic-version", "2023-06-01");
        }
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        reply(request).await
    }

    /// Stops the proxy the way its binary does: drain, then flush telemetry and OTel.
    pub async fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(server) = self.server.take() {
            server
                .await
                .expect("proxy task")
                .expect("proxy shut down cleanly");
        }
        self.reports.shutdown().await;
    }
}

// ---- seed ----

fn raw(text: &str) -> Box<JsonRawValue> {
    JsonRawValue::from_string(text.to_owned()).expect("raw JSON")
}

/// One catalog column. `retention` is the stored mode (valid for 30 days); `None` stores
/// no evidence (`unknown`). `alias_zero` gives the `openrouter` alias zero evidence.
struct Column<'a> {
    id: &'a str,
    provider: &'a str,
    model: &'a str,
    endpoint: &'a str,
    retention: Option<&'a str>,
    alias: Option<&'a str>,
    alias_zero: bool,
}

async fn column(pool: &PgPool, column: &Column<'_>, until: DateTime<Utc>) {
    let empty = raw("{}");
    let aliases = column
        .alias
        .map(|alias| raw(&json!({"openrouter": alias}).to_string()));
    let alias_retention = column.alias_zero.then(|| {
        raw(&json!({"openrouter": {
            "retention_mode": "zero",
            "retention_verified_at": Utc::now().to_rfc3339(),
            "retention_valid_until": until.to_rfc3339(),
            "retention_source": "acceptance"
        }})
        .to_string())
    });
    let evidence = column.retention.is_some();
    catalog::create_model_column(
        pool,
        &NewModelColumn {
            id: column.id,
            provider: column.provider,
            model_id: column.model,
            endpoint: column.endpoint,
            settings: &empty,
            status: "active",
            pricing: &empty,
            quality_source_column_id: None,
            cold_start_until: None,
            deprecation_date: None,
            execution_aliases: aliases.as_deref(),
            retention_mode: column.retention,
            retention_verified_at: evidence.then(Utc::now),
            retention_valid_until: evidence.then_some(until),
            retention_source: evidence.then_some("acceptance"),
            execution_alias_retention: alias_retention.as_deref(),
            param_constraints: None,
        },
    )
    .await
    .unwrap_or_else(|error| panic!("column {}: {error}", column.id));
}

async fn aggregate(pool: &PgPool, id: &str, capability: &str, tier: i16, dim: &str, ewma: f32) {
    measurement::upsert_aggregate(
        pool,
        &AggregateUpsert {
            model_column_id: id,
            capability,
            difficulty_tier: tier,
            dimension: dim,
            ewma,
            variance: 0.0,
            n: 20,
            measured: true,
            matrix_version: CORPUS,
            as_of: Utc::now().date_naive(),
        },
    )
    .await
    .expect("aggregate");
}

/// The catalog, the classifier columns, and measurements for the pool columns.
async fn seed(pool: &PgPool) {
    corpus::upsert_corpus_version(pool, CORPUS, "", 0, false)
        .await
        .expect("corpus");
    corpus::activate_corpus_version(pool, CORPUS)
        .await
        .expect("activate");
    let until = Utc::now() + chrono::Duration::days(30);
    let columns = [
        Column {
            id: OPUS,
            provider: "anthropic",
            model: "claude-opus-4-6",
            endpoint: "direct",
            retention: Some("zero"),
            alias: Some("anthropic/claude-opus-4.6"),
            alias_zero: false,
        },
        Column {
            id: CLAUDE,
            provider: "anthropic",
            model: "claude-sonnet-4-6",
            endpoint: "direct",
            retention: Some("zero"),
            alias: Some("anthropic/claude-sonnet-4.6"),
            alias_zero: true,
        },
        Column {
            id: GPT,
            provider: "openai",
            model: "gpt-5",
            endpoint: "direct",
            retention: Some("standard"),
            alias: Some("openai/gpt-5"),
            alias_zero: false,
        },
        Column {
            id: GEMINI,
            provider: "google",
            model: "gemini-2.5-pro",
            endpoint: "direct",
            retention: None,
            alias: None,
            alias_zero: false,
        },
        Column {
            id: "col_haiku",
            provider: "anthropic",
            model: "claude-haiku-4-5",
            endpoint: "direct",
            retention: Some("standard"),
            alias: None,
            alias_zero: false,
        },
        Column {
            id: "col_flash_zr",
            provider: "google",
            model: "gemini-2.5-flash",
            endpoint: "vertex",
            retention: Some("zero"),
            alias: None,
            alias_zero: false,
        },
    ];
    for spec in &columns {
        column(pool, spec, until).await;
    }
    for (id, quality) in [(OPUS, 0.97), (CLAUDE, 0.95), (GPT, 0.93), (GEMINI, 0.91)] {
        for capability in ["structured_extraction", "multi_step_reasoning"] {
            for tier in 1..=5 {
                aggregate(pool, id, capability, tier, "pass_rate", 0.95).await;
            }
            aggregate(pool, id, capability, 3, "quality", quality).await;
            aggregate(pool, id, capability, 3, "cost_per_run_usd", 0.01).await;
            aggregate(pool, id, capability, 3, "latency_p95_ms", 900.0).await;
        }
    }
}

/// Waits up to 5 s for `ready`.
pub async fn eventually(mut ready: impl FnMut() -> bool) -> bool {
    for _ in 0..500 {
        if ready() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    ready()
}
