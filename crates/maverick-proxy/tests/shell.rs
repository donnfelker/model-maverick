//! The proxy shell (bead 6.5): configuration, routing, the pipeline up to the chain walk,
//! the D38 body cap, hosted mode, and the server settings.
//!
//! Requests go over real TCP to the real server with raw HTTP/1.1, so status lines,
//! headers, and body bytes are checked exactly as a client sees them. Every mismatch prints
//! the request and the full response.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use maverick_core::recommend::{RecommendClient, RecommendClientConfig};
use maverick_core::settings::SettingsMode;
use maverick_proxy::chain::{BoxFuture, ChainRequest, ChainRunner, NoExecutor, Surface};
use maverick_proxy::config::{
    self, Config, ConfigError, DEFAULT_MAX_BODY_BYTES, DeploymentMode, load_config,
};
use maverick_proxy::envelope::envelope_body;
use maverick_proxy::server::{Proxy, ServeSettings, serve};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

// ---------------------------------------------------------------------------------------
// Harness

/// Each engine request: lowercase header names to values, and the body.
type EngineLog = Arc<Mutex<Vec<(HashMap<String, String>, String)>>>;

/// A mock engine: scripted replies in order, then 500 for anything unscripted. Every
/// request is recorded.
struct MockEngine {
    url: String,
    requests: EngineLog,
}

impl MockEngine {
    async fn start(replies: Vec<(u16, &str)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind engine");
        let url = format!("http://{}", listener.local_addr().expect("engine address"));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let mut replies: Vec<(u16, String)> = replies
            .into_iter()
            .map(|(status, body)| (status, body.to_owned()))
            .collect();
        replies.reverse();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let raw = read_http_message(&mut socket).await;
                let headers = raw
                    .headers
                    .iter()
                    .map(|(name, value)| (name.to_ascii_lowercase(), value.clone()))
                    .collect();
                recorded
                    .lock()
                    .expect("engine lock")
                    .push((headers, String::from_utf8_lossy(&raw.body).into_owned()));
                let (status, body) = replies
                    .pop()
                    .unwrap_or((500, "{\"error\":\"unscripted\"}".to_owned()));
                let response = format!(
                    "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        Self { url, requests }
    }

    fn requests(&self) -> Vec<(HashMap<String, String>, String)> {
        self.requests.lock().expect("engine lock").clone()
    }
}

struct TestProxy {
    addr: String,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl TestProxy {
    async fn start(config: Config, chain: Arc<dyn ChainRunner>, settings: ServeSettings) -> Self {
        let http = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("engine http client");
        let engine = RecommendClient::with_http(
            http,
            RecommendClientConfig {
                base_url: config.engine_url.clone(),
                timeout: Duration::from_secs(5),
                max_retries: 0,
            },
        );
        let app = Proxy::with_engine(config, engine, chain).router();
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind proxy");
        let addr = listener.local_addr().expect("proxy address").to_string();
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(serve(listener, app, settings, async move {
            let _ = stopped.await;
        }));
        Self {
            addr,
            stop: Some(stop),
            task,
        }
    }

    async fn stop(mut self) -> std::io::Result<()> {
        let _ = self.stop.take().expect("stop once").send(());
        self.task.await.expect("server task")
    }
}

fn config_from(vars: &[(&str, &str)]) -> Result<Config, ConfigError> {
    let vars: HashMap<String, String> = vars
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
    load_config(|name| vars.get(name).cloned())
}

fn self_hosted(engine_url: &str) -> Config {
    config_from(&[
        ("MAVERICK_API_KEY", "mk_live_test"),
        ("MAVERICK_ENGINE_URL", engine_url),
    ])
    .expect("config")
}

async fn start(config: Config) -> TestProxy {
    TestProxy::start(config, Arc::new(NoExecutor), ServeSettings::default()).await
}

#[derive(Debug)]
struct RawMessage {
    start_line: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl RawMessage {
    fn status(&self) -> u16 {
        self.start_line
            .split(' ')
            .nth(1)
            .and_then(|status| status.parse().ok())
            .expect("status code")
    }

    fn header_values(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
            .collect()
    }

    fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Read one HTTP/1.1 message: the head, then `Content-Length` bytes of body.
async fn read_http_message(socket: &mut TcpStream) -> RawMessage {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 8192];
    let head_end = loop {
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
        let read = socket.read(&mut chunk).await.expect("read head");
        assert!(
            read > 0,
            "connection closed inside the head: {:?}",
            String::from_utf8_lossy(&buffer)
        );
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8(buffer[..head_end].to_vec()).expect("utf-8 head");
    let mut lines = head.split("\r\n");
    let start_line = lines.next().expect("start line").to_owned();
    let headers: Vec<(String, String)> = lines
        .filter(|line| !line.is_empty())
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_owned(), value.trim().to_owned()))
        .collect();
    let length: usize = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map_or(0, |(_, value)| value.parse().expect("content-length"));
    let mut body = buffer[head_end..].to_vec();
    while body.len() < length {
        let read = socket.read(&mut chunk).await.expect("read body");
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    RawMessage {
        start_line,
        headers,
        body,
    }
}

/// A raw request. `Content-Length` is added for a body or a POST, as Go's client did.
fn raw_request(method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut out = format!("{method} {path} HTTP/1.1\r\nHost: proxy.test\r\n");
    for (name, value) in headers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    if !body.is_empty() || method == "POST" {
        out.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    out.push_str("Connection: close\r\n\r\n");
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

async fn exchange(addr: &str, request: &[u8]) -> RawMessage {
    let mut socket = TcpStream::connect(addr).await.expect("connect proxy");
    socket.write_all(request).await.expect("send request");
    read_http_message(&mut socket).await
}

async fn post(addr: &str, path: &str, headers: &[(&str, &str)], body: &str) -> RawMessage {
    exchange(addr, &raw_request("POST", path, headers, body.as_bytes())).await
}

fn assert_response(context: &str, response: &RawMessage, status: u16, body: &str) {
    assert!(
        response.status() == status && response.body == body.as_bytes(),
        "{context}\n== expected ==\n{status} {body:?}\n== actual ==\n{} {:?}\nheaders: {:?}",
        response.start_line,
        response.body_text(),
        response.headers
    );
}

// ---------------------------------------------------------------------------------------
// Configuration

fn field(config: &Config, name: &str) -> Value {
    match name {
        "EngineURL" => config.engine_url.clone().into(),
        "APIKey" => config.api_key.clone().into(),
        "Addr" => config.addr.clone().into(),
        "Telemetry" => config.telemetry.into(),
        "OTLPEndpoint" => config.otlp_endpoint.clone().into(),
        "OTLPServiceName" => config.otlp_service_name.clone().into(),
        "UpstreamOpenAI" => config.upstream_openai.clone().into(),
        "UpstreamAnthropic" => config.upstream_anthropic.clone().into(),
        "UpstreamGoogle" => config.upstream_google.clone().into(),
        "UpstreamXAI" => config.upstream_xai.clone().into(),
        "AnthropicAPIKey" => config.anthropic_api_key.clone().into(),
        "OpenAIAPIKey" => config.openai_api_key.clone().into(),
        "GeminiAPIKey" => config.gemini_api_key.clone().into(),
        "XAIAPIKey" => config.xai_api_key.clone().into(),
        other => panic!("unknown Go config field {other}"),
    }
}

/// `unit.json#load_config-*`: the Go loader's rows, self-hosted mode.
#[test]
fn load_config_rows() {
    let rows =
        parity_support::cases("fixtures/proxy/unit.json#load_config").expect("load_config rows");
    assert_eq!(rows.len(), 6);
    for row in rows {
        let id = row["id"].as_str().expect("id");
        let env: Vec<(String, String)> = row["env"]
            .as_object()
            .expect("env")
            .iter()
            .map(|(name, value)| (name.clone(), value.as_str().expect("env value").to_owned()))
            .collect();
        let env: Vec<(&str, &str)> = env
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        let loaded = config_from(&env);
        if let Some(error) = row.get("error").and_then(Value::as_str) {
            assert_eq!(
                loaded.map_err(|error| error.to_string()),
                Err(error.to_owned()),
                "{id}"
            );
            continue;
        }
        let loaded = loaded.unwrap_or_else(|error| panic!("{id}: {error}"));
        for (name, want) in row["config"].as_object().expect("config") {
            assert_eq!(&field(&loaded, name), want, "{id}: {name}");
        }
        assert_eq!(loaded.mode, DeploymentMode::SelfHosted, "{id}");
        assert_eq!(loaded.max_body_bytes, DEFAULT_MAX_BODY_BYTES, "{id}");
    }
}

/// D12, D13: a hosted instance never looks up a provider key or `MAVERICK_API_KEY`.
#[test]
fn hosted_mode_reads_no_provider_or_tenant_keys() {
    let asked = Mutex::new(Vec::new());
    let loaded = load_config(|name| {
        asked.lock().expect("asked").push(name.to_owned());
        match name {
            config::MODE_VAR => Some("hosted".to_owned()),
            config::MAX_BODY_BYTES_VAR => Some("1024".to_owned()),
            config::RATELIMIT_RPM_VAR => Some("5".to_owned()),
            _ => Some(format!("value-of-{name}")),
        }
    })
    .expect("hosted config");
    let asked = asked.into_inner().expect("asked");
    for secret in [
        "MAVERICK_API_KEY",
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "GEMINI_API_KEY",
        "XAI_API_KEY",
    ] {
        assert!(
            !asked.iter().any(|name| name == secret),
            "hosted mode read {secret}: {asked:?}"
        );
    }
    assert_eq!(loaded.mode, DeploymentMode::Hosted);
    assert!(
        loaded.api_key.is_empty()
            && loaded.openai_api_key.is_empty()
            && loaded.xai_api_key.is_empty()
    );
    assert_eq!(loaded.engine_url, "value-of-MAVERICK_ENGINE_URL");
    assert_eq!(loaded.ratelimit_rpm, 5);
}

#[test]
fn mode_and_body_cap_settings() {
    assert_eq!(
        config_from(&[("MAVERICK_PROXY_MODE", "HOSTED")]),
        Err(ConfigError::InvalidMode("HOSTED".to_owned()))
    );
    let explicit = config_from(&[
        ("MAVERICK_PROXY_MODE", "self-hosted"),
        ("MAVERICK_API_KEY", "mk"),
    ])
    .expect("config");
    assert_eq!(explicit.mode, DeploymentMode::SelfHosted);
    assert_eq!(
        config_from(&[("MAVERICK_PROXY_MODE", "self-hosted")]).map_err(|error| error.to_string()),
        Err("MAVERICK_API_KEY: required but not set".to_owned())
    );
    let capped = config_from(&[
        ("MAVERICK_API_KEY", "mk"),
        ("MAVERICK_PROXY_MAX_BODY_BYTES", "1024"),
    ])
    .expect("config");
    assert_eq!(capped.max_body_bytes, 1024);
    let empty_is_default = config_from(&[
        ("MAVERICK_API_KEY", "mk"),
        ("MAVERICK_PROXY_MAX_BODY_BYTES", ""),
    ])
    .expect("config");
    assert_eq!(empty_is_default.max_body_bytes, 33_554_432);
    for bad in ["0", "-1", "32MiB", " 5"] {
        assert_eq!(
            config_from(&[
                ("MAVERICK_API_KEY", "mk"),
                ("MAVERICK_PROXY_MAX_BODY_BYTES", bad)
            ]),
            Err(ConfigError::InvalidMaxBodyBytes(bad.to_owned())),
            "{bad:?}"
        );
    }
}

// ---------------------------------------------------------------------------------------
// scenarios.json rows owned by the shell

const SHELL_SCENARIOS: [&str; 8] = [
    "up",
    "x_method_not_allowed",
    "x_unknown_path_404",
    "x_post_up_405",
    "x_chat_invalid_json_400",
    "x_chat_model_not_string_400",
    "x_chat_missing_model_400_alias",
    "x_messages_concrete_model_400",
];

/// Routing and pipeline rows: exact status, listed headers, and body bytes; the engine is
/// never called.
// parity: proxy.TestUp
#[tokio::test]
async fn scenario_rows() {
    let doc = parity_support::fixture("proxy", "scenarios.json").expect("scenarios.json");
    let scenarios = doc["scenarios"].as_array().expect("scenarios");
    let engine = MockEngine::start(Vec::new()).await;
    let proxy = start(self_hosted(&engine.url)).await;
    for id in SHELL_SCENARIOS {
        let scenario = scenarios
            .iter()
            .find(|scenario| scenario["id"] == id)
            .unwrap_or_else(|| panic!("scenario {id}"));
        let inbound = &scenario["inbound_request"];
        let headers: Vec<(String, String)> = inbound["headers"]
            .as_array()
            .expect("headers")
            .iter()
            .map(|pair| {
                (
                    pair[0].as_str().expect("name").to_owned(),
                    pair[1].as_str().expect("value").to_owned(),
                )
            })
            .collect();
        let headers: Vec<(&str, &str)> = headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        let request = raw_request(
            inbound["method"].as_str().expect("method"),
            inbound["path_and_query"].as_str().expect("path"),
            &headers,
            inbound["body"].as_str().expect("body").as_bytes(),
        );
        let response = exchange(&proxy.addr, &request).await;

        let mut expected = scenario["expected"]["response"].clone();
        // D45 replaces the grammar message while preserving the status and code.
        if expected["body"]
            .as_str()
            .unwrap()
            .contains("invalid_model_alias")
        {
            let body = envelope_body(
                "invalid_model_alias",
                "model must be a provider model id or maverick/<route>[:<variant>] with variant quality, balanced, or cost",
            );
            expected["headers"]["Content-Length"] = serde_json::json!([body.len().to_string()]);
            expected["body"] = body.into();
        }
        let context = format!("{id}\n== request ==\n{}", String::from_utf8_lossy(&request));
        assert_response(
            &context,
            &response,
            u16::try_from(expected["status"].as_u64().expect("status")).expect("u16"),
            expected["body"].as_str().expect("body"),
        );
        for (name, values) in expected["headers"].as_object().expect("expected headers") {
            let want: Vec<&str> = values
                .as_array()
                .expect("values")
                .iter()
                .map(|value| value.as_str().expect("header value"))
                .collect();
            assert_eq!(
                response.header_values(name),
                want,
                "{context}\nheader {name}"
            );
        }
    }
    assert!(
        engine.requests().is_empty(),
        "no shell row calls the engine"
    );
    proxy.stop().await.expect("clean shutdown");
}

#[tokio::test]
async fn head_up_has_the_get_headers_and_no_body() {
    let proxy = start(self_hosted("http://engine.invalid")).await;
    let mut socket = TcpStream::connect(&proxy.addr).await.expect("connect");
    socket
        .write_all(&raw_request("HEAD", "/up", &[], b""))
        .await
        .expect("send");
    let mut raw = Vec::new();
    socket.read_to_end(&mut raw).await.expect("read");
    let text = String::from_utf8_lossy(&raw);
    assert!(text.starts_with("HTTP/1.1 200"), "{text}");
    assert!(
        text.to_ascii_lowercase()
            .contains("content-type: application/json"),
        "{text}"
    );
    assert!(text.ends_with("\r\n\r\n"), "HEAD has no body: {text:?}");
    proxy.stop().await.expect("clean shutdown");
}

// ---------------------------------------------------------------------------------------
// D38 body cap

fn capped(engine_url: &str, cap: &str) -> Config {
    config_from(&[
        ("MAVERICK_API_KEY", "mk_live_test"),
        ("MAVERICK_ENGINE_URL", engine_url),
        ("MAVERICK_PROXY_MAX_BODY_BYTES", cap),
    ])
    .expect("config")
}

#[tokio::test]
async fn body_cap_rejects_declared_and_chunked_bodies() {
    let engine = MockEngine::start(Vec::new()).await;
    let proxy = start(capped(&engine.url, "64")).await;
    let too_large = envelope_body("payload_too_large", "request body is larger than 64 bytes");

    // Declared length over the cap: answered before any body byte is sent.
    let mut socket = TcpStream::connect(&proxy.addr).await.expect("connect");
    socket
        .write_all(b"POST /v1/chat/completions HTTP/1.1\r\nHost: p\r\nContent-Type: application/json\r\nContent-Length: 65\r\n\r\n")
        .await
        .expect("send head");
    let response = tokio::time::timeout(Duration::from_secs(5), read_http_message(&mut socket))
        .await
        .expect("answered without waiting for the body");
    assert_response("declared 65 > 64", &response, 413, &too_large);

    // Chunked body cut off at the cap.
    let chunk = format!(
        "{{\"model\":\"maverick/auto\",\"pad\":\"{}\"}}",
        "x".repeat(80)
    );
    let mut request = b"POST /v1/messages HTTP/1.1\r\nHost: p\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
    request.extend_from_slice(format!("{:x}\r\n{chunk}\r\n0\r\n\r\n", chunk.len()).as_bytes());
    let response = exchange(&proxy.addr, &request).await;
    assert_response("chunked over the cap", &response, 413, &too_large);

    // Exactly the cap passes the cap (and then fails the alias check).
    let exact = format!("{{\"model\":\"gpt-4o\"{}}}", " ".repeat(64 - 18));
    assert_eq!(exact.len(), 64);
    let response = post(&proxy.addr, "/v1/chat/completions", &[], &exact).await;
    assert_eq!(response.status(), 400, "{}", response.body_text());
    assert!(response.body_text().contains("invalid_model_alias"));

    assert!(engine.requests().is_empty(), "the engine is never called");
    proxy.stop().await.expect("clean shutdown");
}

// ---------------------------------------------------------------------------------------
// Pipeline steps 4 to 6

const CHAT_HI: &str = r#"{"model":"maverick/auto","messages":[{"role":"user","content":"hi"}]}"#;

#[tokio::test(flavor = "current_thread")]
async fn engine_failure_is_502_engine_error_and_one_log_line() {
    let logs =
        test_support::init_test_tracing_with_secrets(&["secret-prompt-text", "mk_live_test"]);
    let engine = MockEngine::start(vec![(500, "{\"error\":{\"code\":\"internal_error\"}}")]).await;
    let proxy = start(self_hosted(&engine.url)).await;
    let body =
        r#"{"model":"maverick/auto","messages":[{"role":"user","content":"secret-prompt-text"}]}"#;
    let response = post(
        &proxy.addr,
        "/v1/chat/completions",
        &[("Content-Type", "application/json")],
        body,
    )
    .await;
    assert_response(
        "engine 500",
        &response,
        502,
        "{\"error\":{\"code\":\"engine_error\",\"message\":\"the engine's POST /v1/recommend call failed\"}}\n",
    );
    let sent = engine.requests();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].0.get("authorization").map(String::as_str),
        Some("Bearer mk_live_test")
    );

    let lines = logs.captured_json();
    let failures: Vec<&Value> = lines
        .iter()
        .filter(|line| {
            line.to_string()
                .contains("maverick-proxy: engine recommend failed")
        })
        .collect();
    assert_eq!(failures.len(), 1, "{lines:?}");
    assert!(
        failures[0].to_string().contains("status 500"),
        "{:?}",
        failures[0]
    );
    proxy.stop().await.expect("clean shutdown");
    // Dropping `logs` fails the test if the prompt or the key reached any log line.
}

/// D46: the terminal privacy codes are never fail-open triggers; the proxy relays them.
#[tokio::test]
async fn terminal_privacy_errors_are_relayed_with_their_code() {
    let engine = MockEngine::start(vec![
        (422, r#"{"error":{"code":"zero_retention_requires_declared","message":"send the declared form"}}"#),
        (409, r#"{"error":{"code":"no_compliant_columns","message":"no column holds valid zero-retention evidence"}}"#),
        (409, r#"{"error":{"code":"no_compliant_columns"}}"#),
    ])
    .await;
    let proxy = start(self_hosted(&engine.url)).await;
    let response = post(&proxy.addr, "/v1/chat/completions", &[], CHAT_HI).await;
    assert_response(
        "422 relayed",
        &response,
        422,
        &envelope_body("zero_retention_requires_declared", "send the declared form"),
    );
    let response = post(&proxy.addr, "/v1/messages", &[], CHAT_HI).await;
    assert_response(
        "409 relayed",
        &response,
        409,
        &envelope_body(
            "no_compliant_columns",
            "no column holds valid zero-retention evidence",
        ),
    );
    let response = post(&proxy.addr, "/v1/messages", &[], CHAT_HI).await;
    assert_response(
        "409 without a message",
        &response,
        409,
        &envelope_body("no_compliant_columns", "no_compliant_columns"),
    );
    proxy.stop().await.expect("clean shutdown");
}

/// What the chain walk received: surface, recommendation id, settings mode, tenant key,
/// and body.
type Handoff = (Surface, String, SettingsMode, String, Vec<u8>);

/// Records what the pipeline hands to the chain walk.
#[derive(Clone, Default)]
struct Recorder {
    seen: Arc<Mutex<Vec<Handoff>>>,
}

impl ChainRunner for Recorder {
    fn run(&self, request: ChainRequest) -> BoxFuture<'_, axum::response::Response> {
        self.seen.lock().expect("recorder").push((
            request.surface,
            request.recommendation.recommendation_id.clone(),
            request.settings_mode,
            request.tenant_key.expose().to_owned(),
            request.body.to_vec(),
        ));
        Box::pin(async { axum::response::Response::new(axum::body::Body::from("chain ran")) })
    }
}

#[tokio::test]
async fn the_chain_runner_gets_the_request_and_the_recommendation() {
    let reply = r#"{"recommendation_id":"rec_1","recommendations":[{"provider":"openai","model":"gpt-a"}]}"#;
    let engine = MockEngine::start(vec![(200, reply), (200, reply)]).await;
    let recorder = Recorder::default();
    let proxy = TestProxy::start(
        self_hosted(&engine.url),
        Arc::new(recorder.clone()),
        ServeSettings::default(),
    )
    .await;

    let response = post(&proxy.addr, "/v1/chat/completions", &[], CHAT_HI).await;
    assert_response("chain ran (chat)", &response, 200, "chain ran");
    let messages = r#"{"model":"maverick/p:quality","max_tokens":8,"messages":[{"role":"user","content":"hi"}]}"#;
    let response = post(
        &proxy.addr,
        "/v1/messages",
        &[("X-Maverick-Settings", "caller")],
        messages,
    )
    .await;
    assert_response("chain ran (messages)", &response, 200, "chain ran");

    let sent: Vec<String> = engine
        .requests()
        .into_iter()
        .map(|(_, body)| body)
        .collect();
    assert_eq!(
        sent,
        [
            r#"{"mode":"quality","workload":{"projection_version":"p1","prompt":"[conversation]\nuser: hi"}}"#,
            r#"{"mode":"quality","pool":"p","workload":{"projection_version":"p1","prompt":"[conversation]\nuser: hi"}}"#,
        ]
    );
    let seen = recorder.seen.lock().expect("recorder").clone();
    assert_eq!(seen.len(), 2);
    assert_eq!(
        (
            seen[0].0,
            seen[0].1.as_str(),
            seen[0].2,
            seen[0].3.as_str(),
            seen[0].4.as_slice()
        ),
        (
            Surface::Chat,
            "rec_1",
            SettingsMode::Apply,
            "mk_live_test",
            CHAT_HI.as_bytes()
        )
    );
    assert_eq!(
        (seen[1].0, seen[1].2),
        (Surface::Messages, SettingsMode::Caller)
    );
    proxy.stop().await.expect("clean shutdown");
}

/// D14, D45: settings are an exact request header; the retired suffix is malformed.
#[tokio::test]
async fn invalid_settings_and_retired_suffix_never_call_the_engine() {
    let engine = MockEngine::start(Vec::new()).await;
    let proxy = start(self_hosted(&engine.url)).await;
    for path in ["/v1/chat/completions", "/v1/messages"] {
        for value in ["", "apply", "CALLER", "caller,caller"] {
            let response = post(
                &proxy.addr,
                path,
                &[("X-Maverick-Settings", value)],
                CHAT_HI,
            )
            .await;
            assert_response(
                "invalid settings",
                &response,
                400,
                &envelope_body("invalid_request", "X-Maverick-Settings must be caller"),
            );
        }
        let body = CHAT_HI.replace("maverick/auto", "maverick/auto?settings=caller");
        let response = post(&proxy.addr, path, &[], &body).await;
        assert_response(
            "retired suffix",
            &response,
            400,
            &envelope_body(
                "invalid_model_alias",
                "model must be a provider model id or maverick/<route>[:<variant>] with variant quality, balanced, or cost",
            ),
        );
    }
    assert!(engine.requests().is_empty());
    proxy.stop().await.expect("clean shutdown");
}

#[tokio::test]
async fn the_default_runner_answers_no_executable_candidates() {
    let engine = MockEngine::start(vec![
        (200, r#"{"recommendation_id":"rec_9","recommendations":[{"provider":"openai","model":"gpt-a"}]}"#),
        (200, r#"{"recommendation_id":"","recommendations":[]}"#),
    ])
    .await;
    let proxy = start(self_hosted(&engine.url)).await;
    let response = post(&proxy.addr, "/v1/chat/completions", &[], CHAT_HI).await;
    assert_response(
        "chat exhausted",
        &response,
        502,
        "{\"error\":{\"code\":\"no_executable_candidates\",\"message\":\"no chain entry could be executed (no credential for any candidate, or every attempt failed)\"}}\n",
    );
    assert_eq!(
        response.header_values("x-maverick-recommendation-id"),
        ["rec_9"]
    );
    let response = post(&proxy.addr, "/v1/messages", &[], CHAT_HI).await;
    assert_response(
        "messages exhausted",
        &response,
        502,
        "{\"error\":{\"code\":\"no_executable_candidates\",\"message\":\"no anthropic chain entry could be executed (no anthropic candidate, no credential, or every attempt failed)\"}}\n",
    );
    assert!(
        response
            .header_values("x-maverick-recommendation-id")
            .is_empty(),
        "empty id: no header"
    );
    proxy.stop().await.expect("clean shutdown");
}

#[tokio::test]
async fn workload_header_selects_the_declared_form() {
    let engine = MockEngine::start(vec![(200, r#"{"recommendations":[]}"#)]).await;
    let proxy = start(self_hosted(&engine.url)).await;
    let response = post(
        &proxy.addr,
        "/v1/chat/completions",
        &[("X-Maverick-Workload", "code_generation:4")],
        CHAT_HI,
    )
    .await;
    assert_eq!(response.status(), 502, "{}", response.body_text());
    let response = post(
        &proxy.addr,
        "/v1/chat/completions",
        &[("X-Maverick-Workload", "code_generation:hard")],
        CHAT_HI,
    )
    .await;
    assert_response(
        "malformed workload header",
        &response,
        400,
        &envelope_body(
            "invalid_request",
            "X-Maverick-Workload must be <capability>[:<tier>]",
        ),
    );
    let sent: Vec<String> = engine
        .requests()
        .into_iter()
        .map(|(_, body)| body)
        .collect();
    assert_eq!(
        sent,
        [r#"{"mode":"quality","workload":{"capability":"code_generation","difficulty":4}}"#],
        "the malformed header never reached the engine"
    );
    proxy.stop().await.expect("clean shutdown");
}

#[tokio::test]
async fn hosted_mode_takes_the_tenant_key_per_request() {
    let engine = MockEngine::start(vec![(200, r#"{"recommendations":[]}"#)]).await;
    let hosted = config_from(&[
        ("MAVERICK_PROXY_MODE", "hosted"),
        ("MAVERICK_ENGINE_URL", &engine.url),
    ])
    .expect("config");
    let proxy = start(hosted).await;
    let unauthorized = envelope_body("unauthorized", "missing or invalid X-Maverick-API-Key");

    let response = post(&proxy.addr, "/v1/chat/completions", &[], CHAT_HI).await;
    assert_response("no tenant key", &response, 401, &unauthorized);
    let response = post(
        &proxy.addr,
        "/v1/chat/completions",
        &[("X-Maverick-API-Key", "")],
        CHAT_HI,
    )
    .await;
    assert_response("empty tenant key", &response, 401, &unauthorized);

    let response = post(
        &proxy.addr,
        "/v1/chat/completions",
        &[
            ("X-Maverick-API-Key", "mk_live_tenant_a"),
            ("Authorization", "Bearer sk-provider"),
        ],
        CHAT_HI,
    )
    .await;
    assert_eq!(response.status(), 502, "{}", response.body_text());
    let sent = engine.requests();
    assert_eq!(
        sent.len(),
        1,
        "unauthenticated requests never reach the engine"
    );
    assert_eq!(
        sent[0].0.get("authorization").map(String::as_str),
        Some("Bearer mk_live_tenant_a")
    );
    proxy.stop().await.expect("clean shutdown");
}

// ---------------------------------------------------------------------------------------
// Server settings

#[tokio::test]
async fn slow_request_heads_are_cut_off() {
    let settings = ServeSettings {
        header_read_timeout: Duration::from_millis(200),
        ..ServeSettings::default()
    };
    let proxy = TestProxy::start(
        self_hosted("http://engine.invalid"),
        Arc::new(NoExecutor),
        settings,
    )
    .await;
    let mut socket = TcpStream::connect(&proxy.addr).await.expect("connect");
    socket
        .write_all(b"GET /up HTTP/1.1\r\nHost: p\r\n")
        .await
        .expect("partial head");
    let started = Instant::now();
    let mut rest = Vec::new();
    let closed = tokio::time::timeout(Duration::from_secs(3), socket.read_to_end(&mut rest)).await;
    assert!(closed.is_ok(), "the server closed the connection");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert!(
        !String::from_utf8_lossy(&rest).contains("200"),
        "no response to a partial head"
    );
    proxy.stop().await.expect("clean shutdown");
}

#[tokio::test]
async fn shutdown_drains_and_returns() {
    let proxy = start(self_hosted("http://engine.invalid")).await;
    let response = exchange(&proxy.addr, &raw_request("GET", "/up", &[], b"")).await;
    assert_eq!(response.status(), 200);
    let started = Instant::now();
    proxy.stop().await.expect("clean shutdown");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
}

// ---------------------------------------------------------------------------------------
// Tenant auth and credentials through the pipeline (bead 6.6; D12, D13, D30)

fn hosted(engine_url: &str) -> Config {
    config_from(&[
        ("MAVERICK_PROXY_MODE", "hosted"),
        ("MAVERICK_ENGINE_URL", engine_url),
        ("OPENAI_API_KEY", "operator-openai-secret"),
        ("ANTHROPIC_API_KEY", "operator-anthropic-secret"),
    ])
    .expect("hosted config")
}

/// What the chain walk received about credentials.
#[derive(Debug, Clone, PartialEq)]
struct CredentialHandoff {
    tenant_key: String,
    credential: maverick_proxy::credentials::CallerCredential,
    openai_key: Option<String>,
    anthropic_key: Option<String>,
    upstream_header_names: Vec<String>,
}

#[derive(Clone, Default)]
struct CredentialRecorder {
    seen: Arc<Mutex<Vec<CredentialHandoff>>>,
}

impl ChainRunner for CredentialRecorder {
    fn run(&self, request: ChainRequest) -> BoxFuture<'_, axum::response::Response> {
        use maverick_proxy::credentials::resolve_key;
        let key = |provider: &str| {
            resolve_key(
                provider,
                request.surface,
                &request.credential,
                &request.env_keys,
            )
            .map(str::to_owned)
        };
        let handoff = CredentialHandoff {
            tenant_key: request.tenant_key.expose().to_owned(),
            credential: request.credential.clone(),
            openai_key: key("openai"),
            anthropic_key: key("anthropic"),
            upstream_header_names: request
                .upstream_headers
                .keys()
                .map(|name| name.as_str().to_owned())
                .collect(),
        };
        self.seen.lock().expect("recorder").push(handoff);
        Box::pin(async { axum::response::Response::new(axum::body::Body::from("chain ran")) })
    }
}

#[tokio::test]
async fn hosted_tenant_keys_are_checked_and_isolated() {
    let reply = r#"{"recommendation_id":"rec_t","recommendations":[{"provider":"openai","model":"gpt-a"}]}"#;
    let engine = MockEngine::start(vec![(200, reply), (200, reply)]).await;
    let recorder = CredentialRecorder::default();
    let proxy = TestProxy::start(
        hosted(&engine.url),
        Arc::new(recorder.clone()),
        ServeSettings::default(),
    )
    .await;
    let unauthorized = envelope_body("unauthorized", "missing or invalid X-Maverick-API-Key");

    // Malformed keys never reach the engine (the engine requires the `mk_live_` prefix).
    for bad in ["sk-not-maverick", "Bearer mk_live_x", "MK_LIVE_x"] {
        let response = post(
            &proxy.addr,
            "/v1/chat/completions",
            &[("X-Maverick-API-Key", bad)],
            CHAT_HI,
        )
        .await;
        assert_response(bad, &response, 401, &unauthorized);
    }
    assert!(engine.requests().is_empty());

    // Two tenants: each engine call and each chain walk carries its own key.
    for (tenant, credential) in [
        ("mk_live_tenant_a", "sk-a-openai"),
        ("mk_live_tenant_b", "sk-b-openai"),
    ] {
        let response = post(
            &proxy.addr,
            "/v1/chat/completions",
            &[
                ("X-Maverick-API-Key", tenant),
                ("Authorization", &format!("Bearer {credential}")),
            ],
            CHAT_HI,
        )
        .await;
        assert_response(tenant, &response, 200, "chain ran");
    }
    let engine_keys: Vec<String> = engine
        .requests()
        .into_iter()
        .map(|(headers, _)| headers["authorization"].clone())
        .collect();
    assert_eq!(
        engine_keys,
        ["Bearer mk_live_tenant_a", "Bearer mk_live_tenant_b"]
    );

    let seen = recorder.seen.lock().expect("recorder").clone();
    assert_eq!(seen.len(), 2);
    for (handoff, (tenant, credential)) in seen.iter().zip([
        ("mk_live_tenant_a", "sk-a-openai"),
        ("mk_live_tenant_b", "sk-b-openai"),
    ]) {
        assert_eq!(handoff.tenant_key, tenant);
        assert_eq!(
            handoff.openai_key.as_deref(),
            Some(credential),
            "the caller's own key"
        );
        assert_eq!(
            handoff.anthropic_key, None,
            "D12: no operator key in hosted mode"
        );
        assert!(
            !handoff
                .upstream_header_names
                .iter()
                .any(|name| name == "x-maverick-api-key" || name == "authorization"),
            "D30: {:?}",
            handoff.upstream_header_names
        );
    }
    proxy.stop().await.expect("clean shutdown");
}

/// D13, D46: an engine 401 or 403 keeps this mapping and never triggers fail-open.
#[tokio::test]
async fn hosted_engine_401_and_403_map_to_the_tenant() {
    let engine = MockEngine::start(vec![
        (401, r#"{"error":{"code":"unauthorized","message":"missing or invalid API key","details":{}}}"#),
        (403, r#"{"error":{"code":"tenant_suspended","message":"tenant is suspended","details":{}}}"#),
    ])
    .await;
    let proxy = start(hosted(&engine.url)).await;
    let tenant = [("X-Maverick-API-Key", "mk_live_revoked")];
    let response = post(&proxy.addr, "/v1/chat/completions", &tenant, CHAT_HI).await;
    assert_response(
        "engine 401",
        &response,
        401,
        &envelope_body("unauthorized", "missing or invalid X-Maverick-API-Key"),
    );
    let response = post(&proxy.addr, "/v1/messages", &tenant, CHAT_HI).await;
    assert_response(
        "engine 403",
        &response,
        403,
        "{\"error\":{\"code\":\"tenant_suspended\",\"message\":\"tenant is suspended\"}}\n",
    );
    proxy.stop().await.expect("clean shutdown");
}

#[tokio::test]
async fn self_hosted_engine_401_stays_engine_error() {
    let engine = MockEngine::start(vec![(401, r#"{"error":{"code":"unauthorized"}}"#)]).await;
    let proxy = start(self_hosted(&engine.url)).await;
    let response = post(&proxy.addr, "/v1/chat/completions", &[], CHAT_HI).await;
    assert_eq!(response.status(), 502, "{}", response.body_text());
    assert!(response.body_text().contains("engine_error"));
    proxy.stop().await.expect("clean shutdown");
}

/// G5: provider keys and tenant keys stay out of every log line.
#[tokio::test(flavor = "current_thread")]
async fn keys_never_reach_the_logs() {
    let logs = test_support::init_test_tracing_with_secrets(&[
        "mk_live_tenant_logcheck",
        "sk-caller-logcheck",
        "operator-openai-secret",
    ]);
    let engine = MockEngine::start(vec![(500, "{}")]).await;
    let proxy = TestProxy::start(
        hosted(&engine.url),
        Arc::new(CredentialRecorder::default()),
        ServeSettings::default(),
    )
    .await;
    let response = post(
        &proxy.addr,
        "/v1/chat/completions",
        &[
            ("X-Maverick-API-Key", "mk_live_tenant_logcheck"),
            ("Authorization", "Bearer sk-caller-logcheck"),
        ],
        CHAT_HI,
    )
    .await;
    assert_eq!(response.status(), 502);
    assert!(
        !logs.captured_lines().is_empty(),
        "the engine failure was logged"
    );
    proxy.stop().await.expect("clean shutdown");
    // Dropping `logs` fails the test if any registered secret reached a log line.
}
