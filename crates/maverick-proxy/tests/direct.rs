//! D45: explicit provider models cross neither the engine nor the reporting seam.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Bytes, to_bytes};
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use maverick_core::chain::ChainExecutor;
use maverick_core::recommend::{RecommendClient, RecommendClientConfig};
use maverick_proxy::chain::Surface;
use maverick_proxy::config::load_config;
use maverick_proxy::ratelimit::{Clock, RateLimiter};
use maverick_proxy::server::Proxy;
use maverick_proxy::surface::{ChainReport, ExecutorRunner, ReportSink, Upstreams};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

const TENANT: &str = "mk_live_direct_test";
const PROVIDERS: [&str; 5] = ["openai", "anthropic", "google", "xai", "openrouter"];

#[derive(Clone)]
struct Seen {
    target: String,
    headers: HeaderMap,
    body: Bytes,
}

struct Mock {
    url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
    task: JoinHandle<()>,
}

impl Mock {
    async fn start(status: u16, content_type: &str, reply: &str, delay: Duration) -> Self {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let reply = reply.to_owned();
        let content_type = content_type.to_owned();
        let router = Router::new().fallback(move |request: Request| {
            let log = log.clone();
            let reply = reply.clone();
            let content_type = content_type.clone();
            async move {
                let (parts, body) = request.into_parts();
                let body = to_bytes(body, 1024 * 1024).await.unwrap();
                log.lock().unwrap().push(Seen {
                    target: parts.uri.to_string(),
                    headers: parts.headers,
                    body,
                });
                tokio::time::sleep(delay).await;
                (
                    StatusCode::from_u16(status).unwrap(),
                    [
                        ("content-type", content_type),
                        ("x-provider-header", "unchanged".to_owned()),
                    ],
                    reply,
                )
            }
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self { url, seen, task }
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Default)]
struct Reports(Mutex<Vec<ChainReport>>);

impl ReportSink for Reports {
    fn report(&self, report: ChainReport) {
        self.0.lock().unwrap().push(report);
    }
}

struct Env {
    http: reqwest::Client,
    url: String,
    engine: Mock,
    providers: BTreeMap<&'static str, Mock>,
    reports: Arc<Reports>,
    task: JoinHandle<()>,
}

impl Drop for Env {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct FixedClock;
impl Clock for FixedClock {
    fn unix_seconds(&self) -> i64 {
        1_700_000_000
    }
}

impl Env {
    async fn new(
        hosted: bool,
        status: u16,
        content_type: &str,
        reply: &str,
        delay: Duration,
    ) -> Self {
        Self::with_limit(hosted, status, content_type, reply, delay, 60).await
    }

    async fn with_limit(
        hosted: bool,
        status: u16,
        content_type: &str,
        reply: &str,
        delay: Duration,
        limit: u32,
    ) -> Self {
        let engine = Mock::start(
            500,
            "application/json",
            "engine must not be called",
            Duration::ZERO,
        )
        .await;
        let mut providers = BTreeMap::new();
        for provider in PROVIDERS {
            providers.insert(
                provider,
                Mock::start(status, content_type, reply, delay).await,
            );
        }
        let mut vars = BTreeMap::from([
            (
                "MAVERICK_PROXY_MODE",
                if hosted { "hosted" } else { "self-hosted" }.to_owned(),
            ),
            ("MAVERICK_API_KEY", TENANT.to_owned()),
            ("MAVERICK_ENGINE_URL", engine.url.clone()),
            ("OPENAI_API_KEY", "env-openai".to_owned()),
            ("ANTHROPIC_API_KEY", "env-anthropic".to_owned()),
        ]);
        for (name, provider) in [
            ("MAVERICK_UPSTREAM_OPENAI", "openai"),
            ("MAVERICK_UPSTREAM_ANTHROPIC", "anthropic"),
            ("MAVERICK_UPSTREAM_GOOGLE", "google"),
            ("MAVERICK_UPSTREAM_XAI", "xai"),
            ("MAVERICK_UPSTREAM_OPENROUTER", "openrouter"),
        ] {
            vars.insert(name, providers[provider].url.clone());
        }
        let config = load_config(|name| vars.get(name).cloned()).unwrap();
        let http = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let reports = Arc::new(Reports::default());
        let executor = ChainExecutor::new(http.clone()).with_header_timeout(if delay.is_zero() {
            Duration::from_secs(5)
        } else {
            Duration::from_millis(100)
        });
        let runner = ExecutorRunner::with_executor(
            executor,
            Upstreams::from_config(&config),
            reports.clone(),
        );
        let engine_client = RecommendClient::with_http(
            http.clone(),
            RecommendClientConfig {
                base_url: engine.url.clone(),
                timeout: Duration::from_millis(100),
                max_retries: 0,
            },
        );
        let proxy = Proxy::with_engine(config, engine_client, Arc::new(runner))
            .with_limiter(RateLimiter::with_clock(limit, Arc::new(FixedClock)));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task =
            tokio::spawn(async move { axum::serve(listener, proxy.router()).await.unwrap() });
        Self {
            http,
            url,
            engine,
            providers,
            reports,
            task,
        }
    }

    fn request(&self, surface: Surface, body: String, key: &str) -> reqwest::RequestBuilder {
        let (path, header, credential) = match surface {
            Surface::Chat => (
                "/v1/chat/completions",
                "authorization",
                format!("Bearer {key}"),
            ),
            Surface::Messages => ("/v1/messages", "x-api-key", key.to_owned()),
        };
        self.http
            .post(format!("{}{path}?beta=1&raw=%2F", self.url))
            .header("x-maverick-api-key", TENANT)
            .header(header, credential)
            .header("content-type", "application/json")
            .body(body)
    }

    fn assert_calls(&self, provider: Option<&str>, count: usize) {
        assert!(
            self.engine.seen.lock().unwrap().is_empty(),
            "no recommend or telemetry call"
        );
        assert!(
            self.reports.0.lock().unwrap().is_empty(),
            "no report or telemetry item"
        );
        for (name, mock) in &self.providers {
            assert_eq!(
                mock.seen.lock().unwrap().len(),
                if Some(*name) == provider { count } else { 0 },
                "{name}"
            );
        }
    }
}

fn body(model: &str) -> String {
    format!(
        r#"{{ "model":{model}, "messages":[{{"role":"user","content":"synthetic direct request"}}], "temperature":1e-2, "reasoning_effort":"xhigh", "output_config":{{"effort":"xhigh"}}, "thinking":{{"type":"disabled"}} }}"#,
        model = serde_json::to_string(model).unwrap()
    )
}

#[tokio::test]
async fn native_models_and_author_prefixes_preserve_caller_parameters() {
    for hosted in [true, false] {
        for (surface, native, slug, path, credential_header) in [
            (
                Surface::Chat,
                "openai",
                "gpt-direct",
                "/v1/chat/completions",
                "authorization",
            ),
            (
                Surface::Messages,
                "anthropic",
                "claude-direct",
                "/v1/messages",
                "x-api-key",
            ),
        ] {
            for prefixed in [false, true] {
                let env = Env::new(
                    hosted,
                    200,
                    "application/json",
                    r#"{"provider":"ok"}"#,
                    Duration::ZERO,
                )
                .await;
                let model = if prefixed {
                    format!("{native}/{slug}")
                } else {
                    slug.to_owned()
                };
                let original = body(&model);
                let response = env
                    .request(surface, original.clone(), "sk-caller")
                    .header("x-maverick-settings", "caller")
                    .header("x-maverick-conversation", "opaque-private-conversation")
                    .header("x-maverick-workload", "ignored-on-direct")
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                assert!(
                    !response
                        .headers()
                        .contains_key("x-maverick-recommendation-id")
                );
                assert!(!response.headers().contains_key("x-maverick-model-used"));
                assert_eq!(response.headers()["x-provider-header"], "unchanged");
                assert_eq!(response.text().await.unwrap(), r#"{"provider":"ok"}"#);
                env.assert_calls(Some(native), 1);
                let seen = env.providers[native].seen.lock().unwrap()[0].clone();
                assert_eq!(seen.target, format!("{path}?beta=1&raw=%2F"));
                assert_eq!(
                    seen.headers[credential_header],
                    if surface == Surface::Chat {
                        "Bearer sk-caller"
                    } else {
                        "sk-caller"
                    }
                );
                assert!(!seen.headers.contains_key("x-maverick-api-key"));
                assert!(!seen.headers.contains_key("x-maverick-settings"));
                assert!(!seen.headers.contains_key("x-maverick-conversation"));
                let mut expected: Value = serde_json::from_str(&original).unwrap();
                expected["model"] = slug.into();
                assert_eq!(
                    serde_json::from_slice::<Value>(&seen.body).unwrap(),
                    expected
                );
                assert!(std::str::from_utf8(&seen.body).unwrap().contains("1e-2"));
                if !prefixed {
                    assert_eq!(seen.body.as_ref(), original.as_bytes());
                }
            }
        }
    }
}

#[tokio::test]
async fn foreign_authors_are_refused_before_any_call() {
    let env = Env::new(true, 200, "application/json", "ok", Duration::ZERO).await;
    for (surface, model) in [
        (Surface::Chat, "anthropic/claude-direct"),
        (Surface::Messages, "openai/gpt-direct"),
        (Surface::Messages, "Anthropic/claude-direct"),
    ] {
        let response = env
            .request(surface, body(model), "sk-native")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let reply: Value = response.json().await.unwrap();
        assert_eq!(reply["error"]["code"], "invalid_request");
        assert_eq!(
            reply["error"]["message"],
            format!("model {model} is not served by this surface")
        );
    }
    env.assert_calls(None, 0);
}

#[tokio::test]
async fn openrouter_direct_models_keep_the_exact_body_and_caller_key() {
    let env = Env::new(true, 200, "application/json", "ok", Duration::ZERO).await;
    for model in [
        "anthropic/claude-direct:variant",
        "gpt-direct",
        "openai/gpt-direct",
    ] {
        let original = body(model);
        let response = env
            .request(Surface::Chat, original.clone(), "sk-or-v1-caller")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            !response
                .headers()
                .contains_key("x-maverick-recommendation-id")
        );
        assert_eq!(response.text().await.unwrap(), "ok");
        let seen = env.providers["openrouter"]
            .seen
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        assert_eq!(seen.target, "/chat/completions?beta=1&raw=%2F");
        assert_eq!(seen.headers["authorization"], "Bearer sk-or-v1-caller");
        assert_eq!(seen.body.as_ref(), original.as_bytes());
    }
    env.assert_calls(Some("openrouter"), 3);
}

#[tokio::test]
async fn provider_errors_and_sse_are_relayed_without_retry_or_rewriting() {
    for (status, content_type, reply) in [
        (
            429,
            "application/json",
            r#"{"error":"provider rate limit"}"#,
        ),
        (
            503,
            "application/json",
            r#"{"error":"provider unavailable"}"#,
        ),
        (
            200,
            "text/event-stream",
            "data: {\"delta\":\"unchanged\"}\n\ndata: [DONE]\n\n",
        ),
    ] {
        let env = Env::new(true, status, content_type, reply, Duration::ZERO).await;
        let response = env
            .request(Surface::Chat, body("gpt-direct"), "sk-caller")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), status);
        assert_eq!(response.headers()["content-type"], content_type);
        assert_eq!(response.text().await.unwrap(), reply);
        env.assert_calls(Some("openai"), 1);
    }
}

#[tokio::test]
async fn invalid_settings_and_empty_models_never_reach_a_provider() {
    let env = Env::new(true, 200, "application/json", "ok", Duration::ZERO).await;
    for surface in [Surface::Chat, Surface::Messages] {
        for value in ["", "apply", "CALLER", "caller,apply"] {
            let response = env
                .request(surface, body("direct"), "sk-caller")
                .header("x-maverick-settings", value)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert_eq!(
                response.json::<Value>().await.unwrap()["error"]["message"],
                "X-Maverick-Settings must be caller"
            );
        }
        let opaque = HeaderValue::from_bytes(b"c\xff").unwrap();
        let response = env
            .request(surface, body("direct"), "sk-caller")
            .header("x-maverick-settings", opaque)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let response = env
            .request(surface, body("direct"), "sk-caller")
            .header("x-maverick-settings", "caller")
            .header("x-maverick-settings", "apply")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        for body in [body(""), "{\"messages\":[]}".to_owned()] {
            let response = env
                .request(surface, body, "sk-caller")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert_eq!(
                response.json::<Value>().await.unwrap()["error"]["code"],
                "invalid_model_alias"
            );
        }
    }
    env.assert_calls(None, 0);
}

#[tokio::test]
async fn header_timeout_has_no_chain_or_telemetry_side_effects() {
    let env = Env::new(
        true,
        200,
        "application/json",
        "late",
        Duration::from_millis(500),
    )
    .await;
    let response = env
        .request(Surface::Chat, body("gpt-direct"), "sk-caller")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(
        !response
            .headers()
            .contains_key("x-maverick-recommendation-id")
    );
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["code"],
        "no_executable_candidates"
    );
    env.assert_calls(Some("openai"), 1);
}

#[tokio::test]
async fn direct_calls_obey_the_same_tenant_rate_limit() {
    let env = Env::with_limit(true, 200, "application/json", "ok", Duration::ZERO, 1).await;
    let first = env
        .request(Surface::Chat, body("gpt-direct"), "sk-caller")
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let _ = first.text().await.unwrap();
    let second = env
        .request(Surface::Messages, body("claude-direct"), "sk-caller")
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(second.headers()["x-ratelimit-limit"], "1");
    assert!(second.headers().contains_key("retry-after"));
    assert_eq!(
        second.json::<Value>().await.unwrap()["error"]["code"],
        "rate_limited"
    );
    env.assert_calls(Some("openai"), 1);
}

#[tokio::test]
async fn hosted_direct_calls_still_require_a_tenant_key_and_caller_credential() {
    let env = Env::new(true, 200, "application/json", "ok", Duration::ZERO).await;
    let response = env
        .http
        .post(format!("{}/v1/chat/completions", env.url))
        .header("authorization", "Bearer sk-caller")
        .body(body("gpt-direct"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    for surface in [Surface::Chat, Surface::Messages] {
        let response = env
            .request(surface, body("direct"), "")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }
    env.assert_calls(None, 0);
}

#[tokio::test]
async fn self_hosted_native_env_keys_remain_available_without_a_caller_key() {
    let env = Env::new(false, 200, "application/json", "ok", Duration::ZERO).await;
    for (surface, provider, header, expected) in [
        (
            Surface::Chat,
            "openai",
            "authorization",
            "Bearer env-openai",
        ),
        (Surface::Messages, "anthropic", "x-api-key", "env-anthropic"),
    ] {
        let response = env
            .request(surface, body("direct"), "")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let _ = response.text().await.unwrap();
        assert_eq!(
            env.providers[provider].seen.lock().unwrap()[0].headers[header],
            expected
        );
    }
    assert!(env.engine.seen.lock().unwrap().is_empty());
    assert!(env.reports.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_connection_failure_never_contacts_the_engine_or_reports() {
    let env = Env::new(true, 200, "application/json", "ok", Duration::ZERO).await;
    env.providers["openai"].task.abort();
    tokio::task::yield_now().await;
    let response = env
        .request(Surface::Chat, body("direct"), "sk-caller")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    env.assert_calls(None, 0);
}
