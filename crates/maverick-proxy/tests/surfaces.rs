//! The proxy (beads 6.7 and 6.9): every scenario in `parity/fixtures/proxy/scenarios.json`,
//! replayed through the real proxy with a scripted mock engine and scripted mock upstreams
//! over real TCP.
//!
//! Each scenario checks:
//! - the client response: status, headers (`Date` and framing aside), and body bytes;
//! - the upstream attempts: their order, and each request's method, path, query, headers,
//!   and body bytes;
//! - every recommend request: method, path, headers, and body bytes (D5);
//! - the telemetry POSTs the engine receives through the production sink, after a flush:
//!   none when telemetry is off or nothing was attempted, else one per request with Go's
//!   items. D14 adds `settings_applied`, which is left out of the comparison; D27 batches
//!   per tenant key, which for one replayed request is one POST.
//!
//! D32: the transport headers Go's client added (`Accept-Encoding: gzip`,
//! `User-Agent: Go-http-client/1.1`) and the one reqwest adds (`Accept: */*`) are ignored,
//! unless the client sent that header; then it must reach the upstream unchanged.
//!
//! OTel (D31): a mock collector receives the production sink's OTLP. After the flush, the
//! `otel_*` scenarios' decoded payloads must match `otlp_requests_after_forced_flush`,
//! and every other scenario must send the collector nothing.
//!
//! No proxy or tenant key reaches an upstream header value (Go's `mk_live_test` check,
//! and D30 for the hosted key).
//!
//! The reviewed allowlist ([`ALLOWLIST`]): each scenario records the D-rows whose
//! tolerances it actually used, and they must be among the rows reviewed for it. An
//! unreviewed difference fails the scenario. Each scenario writes its outcome to
//! `target/ci/parity/proxy-replay/<id>.txt` for the CI parity tier. Set
//! `PROXY_REPLAY_PRINT_TOLERANCES=1` to print the used rows (after a fixture recapture)
//! instead of checking them.

mod otlp;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use maverick_core::alias::parse_alias;
use maverick_core::chain::{ChainExecutor, RESPONSE_HEADER_TIMEOUT};
use maverick_core::recommend::{
    RecommendClient, RecommendClientConfig, RecommendRequest, Workload,
};
use maverick_core::telemetry::{BatcherConfig, HttpSink, Telemetry};
use maverick_proxy::config::{Config, load_config};
use maverick_proxy::otel::ProxyOtel;
use maverick_proxy::reports::ProxyReports;
use maverick_proxy::server::{Proxy, ServeSettings, serve};
use maverick_proxy::surface::{ChainReport, ExecutorRunner, ReportSink, Upstreams};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

/// Headers that an HTTP client adds on its own (D32).
const TRANSPORT_HEADERS: [&str; 3] = ["Accept", "Accept-Encoding", "User-Agent"];

/// The upstream base URL variables and the mock each one points at.
const UPSTREAM_VARS: [(&str, &str); 5] = [
    ("MAVERICK_UPSTREAM_OPENAI", "openai"),
    ("MAVERICK_UPSTREAM_ANTHROPIC", "anthropic"),
    ("MAVERICK_UPSTREAM_GOOGLE", "google"),
    ("MAVERICK_UPSTREAM_XAI", "xai"),
    ("MAVERICK_UPSTREAM_OPENROUTER", "openrouter"),
];

/// A closed local port (the fixture's `dead_upstreams_connection_refused`).
const CLOSED_PORT: &str = "http://127.0.0.1:1";

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn text(value: &Value) -> String {
    value.as_str().expect("string").to_owned()
}

/// Go's `CanonicalMIMEHeaderKey` for ASCII names: upper case after each `-` and at start.
fn canonical(name: &str) -> String {
    let mut upper = true;
    name.chars()
        .map(|ch| {
            let out = if upper {
                ch.to_ascii_uppercase()
            } else {
                ch.to_ascii_lowercase()
            };
            upper = ch == '-';
            out
        })
        .collect()
}

type HeaderValues = BTreeMap<String, Vec<String>>;

fn header_map(headers: &[(String, String)]) -> HeaderValues {
    let mut map = HeaderValues::new();
    for (name, value) in headers {
        map.entry(canonical(name)).or_default().push(value.clone());
    }
    map
}

fn expected_headers(value: &Value) -> HeaderValues {
    value
        .as_object()
        .expect("headers")
        .iter()
        .map(|(name, values)| {
            let values = values
                .as_array()
                .expect("values")
                .iter()
                .map(text)
                .collect();
            (name.clone(), values)
        })
        .collect()
}

// ---------------------------------------------------------------------------------------
// HTTP/1.1 messages over raw TCP
// ---------------------------------------------------------------------------------------

/// One HTTP/1.1 message as it was received.
#[derive(Debug, Clone)]
struct Message {
    start_line: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    /// False when the body ended early (connection closed before its end).
    complete: bool,
}

impl Message {
    fn target(&self) -> (&str, &str, &str) {
        let mut parts = self.start_line.split(' ');
        let method = parts.next().unwrap_or_default();
        let target = parts.next().unwrap_or_default();
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        (method, path, query)
    }

    fn status(&self) -> u16 {
        self.start_line
            .split(' ')
            .nth(1)
            .and_then(|status| status.parse().ok())
            .expect("status")
    }

    fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Read more bytes; false at end of stream.
async fn fill(socket: &mut TcpStream, buffer: &mut Vec<u8>) -> bool {
    let mut chunk = [0_u8; 8192];
    match socket.read(&mut chunk).await {
        Ok(0) | Err(_) => false,
        Ok(read) => {
            buffer.extend_from_slice(&chunk[..read]);
            true
        }
    }
}

fn find(buffer: &[u8], needle: &[u8]) -> Option<usize> {
    buffer
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Read one message: the head, then a chunked or `Content-Length` body. `None` when the
/// connection ends before a complete head. Bytes past the message stay in `buffer`.
async fn read_message(socket: &mut TcpStream, buffer: &mut Vec<u8>) -> Option<Message> {
    let head_end = loop {
        if let Some(position) = find(buffer, b"\r\n\r\n") {
            break position + 4;
        }
        if !fill(socket, buffer).await {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    buffer.drain(..head_end);
    let mut lines = head.split("\r\n");
    let start_line = lines.next()?.to_owned();
    let headers: Vec<(String, String)> = lines
        .filter(|line| !line.is_empty())
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_owned(), value.trim().to_owned()))
        .collect();
    let mut message = Message {
        start_line,
        headers,
        body: Vec::new(),
        complete: true,
    };
    let chunked = message.headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("transfer-encoding") && value.eq_ignore_ascii_case("chunked")
    });
    if chunked {
        loop {
            let line_end = loop {
                if let Some(position) = find(buffer, b"\r\n") {
                    break position;
                }
                if !fill(socket, buffer).await {
                    message.complete = false;
                    return Some(message);
                }
            };
            let size_text = String::from_utf8_lossy(&buffer[..line_end]).into_owned();
            let size = usize::from_str_radix(size_text.split(';').next()?.trim(), 16).ok()?;
            buffer.drain(..line_end + 2);
            while buffer.len() < size + 2 {
                if !fill(socket, buffer).await {
                    message.body.append(buffer);
                    message.complete = false;
                    return Some(message);
                }
            }
            if size == 0 {
                buffer.drain(..2);
                return Some(message);
            }
            message.body.extend_from_slice(&buffer[..size]);
            buffer.drain(..size + 2);
        }
    }
    let length = message
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    while buffer.len() < length {
        if !fill(socket, buffer).await {
            message.body.append(buffer);
            message.complete = false;
            return Some(message);
        }
    }
    message.body = buffer.drain(..length).collect();
    Some(message)
}

// ---------------------------------------------------------------------------------------
// Mock engine and mock upstreams
// ---------------------------------------------------------------------------------------

type Log = Arc<Mutex<Vec<Message>>>;

/// The mock engine: `POST /v1/recommend` answers the scenario's scripted response, and
/// `POST /v1/telemetry` answers `200 {"accepted":1,"dropped":0}`.
async fn start_engine(recommend: (u16, String, String), log: Log) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind engine");
    let url = format!("http://{}", listener.local_addr().expect("address"));
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let recommend = recommend.clone();
            let log = Arc::clone(&log);
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                while let Some(request) = read_message(&mut socket, &mut buffer).await {
                    let (status, content_type, body) = if request.target().1 == "/v1/recommend" {
                        recommend.clone()
                    } else {
                        (
                            200,
                            "application/json".to_owned(),
                            r#"{"accepted":1,"dropped":0}"#.to_owned(),
                        )
                    };
                    lock(&log).push(request);
                    let response = format!(
                        "HTTP/1.1 {status} Mock\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    if socket.write_all(response.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    url
}

/// Every upstream request in arrival order: upstream name, per-upstream index, request.
type AttemptLog = Arc<Mutex<Vec<(String, usize, Message)>>>;

/// A mock upstream. Request N (per upstream) is answered by `script[N]`: optional delay,
/// status and headers, then each body chunk flushed as its own HTTP chunk, then the end
/// (or an abort). Past the script: `500 {"error":"unscripted request"}`.
async fn start_upstream(name: &str, script: Vec<Value>, log: AttemptLog) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let url = format!("http://{}", listener.local_addr().expect("address"));
    let name = name.to_owned();
    let next = Arc::new(Mutex::new(0_usize));
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let (name, script, log, next) = (
                name.clone(),
                script.clone(),
                Arc::clone(&log),
                Arc::clone(&next),
            );
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                while let Some(request) = read_message(&mut socket, &mut buffer).await {
                    let index = {
                        let mut next = lock(&next);
                        *next += 1;
                        *next - 1
                    };
                    lock(&log).push((name.clone(), index, request));
                    let step = script.get(index).cloned().unwrap_or_else(|| {
                        serde_json::json!({
                            "status": 500,
                            "headers": [["Content-Type", "application/json"]],
                            "body_chunks": ["{\"error\":\"unscripted request\"}"],
                        })
                    });
                    if !answer(&mut socket, &step).await {
                        return;
                    }
                }
            });
        }
    });
    url
}

/// Write one scripted response; false when the connection must end.
async fn answer(socket: &mut TcpStream, step: &Value) -> bool {
    if step.get("hang").and_then(Value::as_bool) == Some(true) {
        // Never answer; wait until the client goes away.
        let mut sink = [0_u8; 256];
        while matches!(socket.read(&mut sink).await, Ok(read) if read > 0) {}
        return false;
    }
    if let Some(delay) = step.get("delay_before_headers_ms").and_then(Value::as_u64) {
        tokio::time::sleep(Duration::from_millis(delay)).await;
    }
    let mut head = format!("HTTP/1.1 {} Scripted\r\n", step["status"]);
    for pair in step["headers"].as_array().expect("headers") {
        head.push_str(&format!("{}: {}\r\n", text(&pair[0]), text(&pair[1])));
    }
    head.push_str("Transfer-Encoding: chunked\r\n\r\n");
    if socket.write_all(head.as_bytes()).await.is_err() {
        return false;
    }
    let pause = step
        .get("pause_after_first_chunk_ms")
        .and_then(Value::as_u64)
        .map(Duration::from_millis);
    for (index, chunk) in step["body_chunks"]
        .as_array()
        .expect("body_chunks")
        .iter()
        .enumerate()
    {
        let chunk = text(chunk);
        let framed = format!("{:x}\r\n{chunk}\r\n", chunk.len());
        if socket.write_all(framed.as_bytes()).await.is_err() || socket.flush().await.is_err() {
            return false;
        }
        if let (0, Some(pause)) = (index, pause) {
            tokio::time::sleep(pause).await;
        }
    }
    if step
        .get("abort_connection_after_chunks")
        .and_then(Value::as_bool)
        == Some(true)
    {
        return false;
    }
    socket.write_all(b"0\r\n\r\n").await.is_ok()
}

// ---------------------------------------------------------------------------------------
// Replay
// ---------------------------------------------------------------------------------------

/// Keeps every report for the test and hands it to the proxy's real sink (telemetry).
struct Recorder {
    reports: Arc<Mutex<Vec<ChainReport>>>,
    sink: ProxyReports,
}

impl ReportSink for Recorder {
    fn report(&self, report: ChainReport) {
        lock(&self.reports).push(report.clone());
        self.sink.report(report);
    }
}

fn scenario(id: &str) -> Value {
    let doc = parity_support::fixture("proxy", "scenarios.json").expect("scenarios.json");
    doc["scenarios"]
        .as_array()
        .expect("scenarios")
        .iter()
        .find(|scenario| scenario["id"] == id)
        .unwrap_or_else(|| panic!("no scenario {id}"))
        .clone()
}

/// Drop the D32 transport headers from both sides unless the client sent them, and note
/// D32 when one was there.
fn without_transport(
    mut headers: HeaderValues,
    client_sent: &HeaderValues,
    used: &mut BTreeSet<&'static str>,
) -> HeaderValues {
    for name in TRANSPORT_HEADERS {
        if !client_sent.contains_key(name) && headers.remove(name).is_some() {
            used.insert("D32");
        }
    }
    headers
}

/// DIVERGENCE: D5. The recommend body carries the versioned workload projection where Go
/// sent the last user message (or the declared fallback). The body must be exactly the
/// Rust projection of the inbound body, and every field besides `workload` must match Go.
fn check_d5_recommend_body(
    context: &str,
    got: &str,
    want: &str,
    inbound_body: &str,
    client_sent: &HeaderValues,
) {
    let d5 = parity_support::divergence!("D5");
    assert_eq!(d5.id(), "D5");
    let inbound: Value = serde_json::from_str(inbound_body).expect("inbound json");
    let alias = parse_alias(inbound["model"].as_str().expect("model")).expect("alias");
    let header = client_sent
        .get("X-Maverick-Workload")
        .and_then(|values| values.first())
        .map(String::as_str);
    let workload = Workload::for_chat_request(inbound_body.as_bytes(), header).expect("workload");
    assert_eq!(
        got,
        RecommendRequest::for_alias(&alias, workload).to_body(),
        "{context}: D5 recommend body"
    );
    let without_workload = |body: &str| {
        let mut body: Value = serde_json::from_str(body).expect("recommend json");
        body.as_object_mut().expect("object").remove("workload");
        body
    };
    assert_eq!(
        without_workload(got),
        without_workload(want),
        "{context}: recommend fields besides the workload"
    );
}

/// A proxy under test, with the reports its runner produced.
struct RunningProxy {
    addr: std::net::SocketAddr,
    reports: Arc<Mutex<Vec<ChainReport>>>,
    sink: ProxyReports,
    stop: oneshot::Sender<()>,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl RunningProxy {
    /// Send every queued telemetry item now, as shutdown does (D27).
    async fn flush_telemetry(&self) {
        self.sink.shutdown().await;
    }

    async fn stop(self) {
        let _ = self.stop.send(());
        self.server
            .await
            .expect("server task")
            .expect("clean shutdown");
    }
}

/// The real proxy on `config`, with the executor's response-header timeout set.
async fn start_proxy(config: Config, header_timeout: Duration) -> RunningProxy {
    let reports = Arc::new(Mutex::new(Vec::new()));
    // The production sink, with an interval long enough that batches go out only at the
    // test's flush: one POST per replayed request, as Go sent.
    let sink = ProxyReports::new(if config.telemetry {
        Telemetry::start(
            Arc::new(HttpSink::new(&config.engine_url).expect("telemetry client")),
            BatcherConfig {
                interval: Duration::from_secs(3600),
                ..BatcherConfig::default()
            },
        )
    } else {
        Telemetry::off()
    })
    .with_otel(ProxyOtel::init(&config).map(Arc::new));
    let upstream_http = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("upstream client");
    let runner = ExecutorRunner::with_executor(
        ChainExecutor::new(upstream_http).with_header_timeout(header_timeout),
        Upstreams::from_config(&config),
        Arc::new(Recorder {
            reports: Arc::clone(&reports),
            sink: sink.clone(),
        }),
    );
    let engine = RecommendClient::with_http(
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("engine client"),
        RecommendClientConfig {
            base_url: config.engine_url.clone(),
            timeout: Duration::from_secs(5),
            max_retries: 0,
        },
    );
    let app = Proxy::with_engine(config, engine, Arc::new(runner)).router();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind proxy");
    let addr = listener.local_addr().expect("proxy address");
    let (stop, stopped) = oneshot::channel::<()>();
    let server = tokio::spawn(serve(listener, app, ServeSettings::default(), async move {
        let _ = stopped.await;
    }));

    RunningProxy {
        addr,
        reports,
        sink,
        stop,
        server,
    }
}

/// A telemetry body with every `latency_ms` set to 0 and the D14 `settings_applied` field
/// removed, in its original key order.
fn normalize_telemetry(body: &str) -> String {
    let body = body
        .replace(",\"settings_applied\":false", "")
        .replace(",\"settings_applied\":true", "");
    let mut out = String::new();
    let mut rest = body.as_str();
    let key = "\"latency_ms\":";
    while let Some(at) = rest.find(key) {
        let (head, tail) = rest.split_at(at + key.len());
        out.push_str(head);
        out.push('0');
        let digits = tail.bytes().take_while(u8::is_ascii_digit).count();
        rest = &tail[digits..];
    }
    out.push_str(rest);
    out
}

/// Replay one scenario and check it. Returns the reports for extra checks.
async fn replay(id: &str) -> Vec<ChainReport> {
    replay_noting(id, &[]).await
}

/// [`replay`], adding the D-rows a caller's own checks rely on (such as D26).
async fn replay_noting(id: &str, extra: &[&'static str]) -> Vec<ChainReport> {
    let scenario = scenario(id);
    let expected = &scenario["expected"];
    let mut used: BTreeSet<&'static str> = extra.iter().copied().collect();
    let (collector_url, collected) = otlp::collector().await;

    // Mocks.
    let engine_log: Log = Arc::default();
    let engine_url = match scenario["engine_recommend_response"].as_object() {
        None => "http://engine.invalid".to_owned(),
        Some(response) => {
            let status = response["status"].as_u64().expect("status");
            start_engine(
                (
                    u16::try_from(status).expect("status"),
                    text(&response["content_type"]),
                    text(&response["body"]),
                ),
                Arc::clone(&engine_log),
            )
            .await
        }
    };
    let attempts: AttemptLog = Arc::default();
    let mut upstream_urls = BTreeMap::new();
    if let Some(scripts) = scenario["provider_scripts"].as_object() {
        for (name, script) in scripts {
            let script = script.as_array().expect("script").clone();
            let url = start_upstream(name, script, Arc::clone(&attempts)).await;
            upstream_urls.insert(name.clone(), url);
        }
    }
    for name in scenario
        .get("dead_upstreams_connection_refused")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        upstream_urls.insert(text(name), CLOSED_PORT.to_owned());
    }

    // The proxy, configured from `proxy_env` with the mock URLs.
    let mut env: BTreeMap<String, String> = scenario["proxy_env"]
        .as_object()
        .expect("proxy_env")
        .iter()
        .map(|(name, value)| {
            let mut value = text(value).replace("<mock engine base URL>", &engine_url);
            for (upstream, url) in &upstream_urls {
                value = value.replace(&format!("<mock {upstream} base URL>"), url);
            }
            (
                name.clone(),
                value.replace("<mock collector base URL>", &collector_url),
            )
        })
        .collect();
    // No real provider calls: every upstream is a mock or a closed local port, never the
    // production default that an unset variable would give.
    for (var, upstream) in UPSTREAM_VARS {
        let url = upstream_urls
            .get(upstream)
            .cloned()
            .unwrap_or_else(|| CLOSED_PORT.to_owned());
        env.insert(var.to_owned(), url);
    }
    let config = load_config(|name| env.get(name).cloned()).expect("config");
    for url in [
        &config.upstream_openai,
        &config.upstream_anthropic,
        &config.upstream_google,
        &config.upstream_xai,
        &config.upstream_openrouter,
    ] {
        assert!(url.starts_with("http://127.0.0.1:"), "{id}: upstream {url}");
    }
    let header_timeout = scenario
        .get("response_header_timeout_ms_override")
        .and_then(Value::as_u64)
        .map_or(RESPONSE_HEADER_TIMEOUT, Duration::from_millis);
    let running = start_proxy(config, header_timeout).await;
    let (addr, reports) = (running.addr, Arc::clone(&running.reports));

    // The inbound request, exactly as captured.
    let inbound = &scenario["inbound_request"];
    let client_headers: Vec<(String, String)> = inbound["headers"]
        .as_array()
        .expect("headers")
        .iter()
        .map(|pair| (text(&pair[0]), text(&pair[1])))
        .collect();
    let method = text(&inbound["method"]);
    let body = text(&inbound["body"]);
    let mut request = format!(
        "{method} {} HTTP/1.1\r\nHost: proxy.test\r\n",
        text(&inbound["path_and_query"])
    );
    for (name, value) in &client_headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    if !body.is_empty() || method == "POST" {
        request.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    request.push_str("\r\n");
    request.push_str(&body);
    let mut socket = TcpStream::connect(addr).await.expect("connect proxy");
    socket
        .write_all(request.as_bytes())
        .await
        .expect("send request");
    let mut buffer = Vec::new();
    let response = read_message(&mut socket, &mut buffer)
        .await
        .expect("a response");
    drop(socket);
    let client_sent = header_map(&client_headers);
    let context = format!(
        "{id}\n== response ==\n{}\n{:?}\n{:?}",
        response.start_line,
        response.headers,
        response.body_text()
    );

    // The client response.
    let want = &expected["response"];
    assert_eq!(u64::from(response.status()), want["status"], "{context}");
    assert!(response.complete, "{context}: the response ended normally");
    assert_eq!(response.body_text(), text(&want["body"]), "{context}");
    let mut got_headers = header_map(&response.headers);
    got_headers.remove("Date");
    got_headers.remove("Transfer-Encoding");
    assert_eq!(
        got_headers,
        expected_headers(&want["headers"]),
        "{context}: response headers"
    );

    // Upstream attempts.
    let attempts = lock(&attempts).clone();
    let order: Vec<Value> = attempts
        .iter()
        .map(|(upstream, index, _)| serde_json::json!({"index": index, "upstream": upstream}))
        .collect();
    assert_eq!(
        Value::Array(order),
        expected["upstream_attempt_order"],
        "{context}: attempt order"
    );
    for (upstream, requests) in expected["upstream_requests"]
        .as_object()
        .expect("upstream_requests")
    {
        let got: Vec<&Message> = attempts
            .iter()
            .filter(|(name, _, _)| name == upstream)
            .map(|(_, _, request)| request)
            .collect();
        let want = requests.as_array().expect("requests");
        assert_eq!(got.len(), want.len(), "{context}: requests to {upstream}");
        for (index, (got, want)) in got.iter().zip(want).enumerate() {
            let at = format!("{context}\n{upstream} request {index}");
            let (method, path, query) = got.target();
            assert_eq!(method, want["method"], "{at}");
            assert_eq!(path, want["path"], "{at}");
            assert_eq!(query, want["query"], "{at}");
            assert_eq!(got.body_text(), text(&want["body"]), "{at}: body");
            let mut got_headers = header_map(&got.headers);
            got_headers.remove("Host");
            assert_eq!(
                without_transport(got_headers, &client_sent, &mut used),
                without_transport(expected_headers(&want["headers"]), &client_sent, &mut used),
                "{at}: headers"
            );
        }
    }

    // No proxy key (Go's `mk_live_test` check) and no hosted tenant key (D30) in any
    // upstream header value.
    let mut keys: Vec<String> = env.get("MAVERICK_API_KEY").cloned().into_iter().collect();
    keys.extend(
        client_sent
            .get("X-Maverick-Api-Key")
            .into_iter()
            .flatten()
            .cloned(),
    );
    for (upstream, _, request) in &attempts {
        for (name, value) in &request.headers {
            for key in keys.iter().filter(|key| !key.is_empty()) {
                assert!(
                    !value.contains(key.as_str()),
                    "{context}: a key reached {upstream} in {name}"
                );
            }
        }
    }

    // Recommend requests.
    let engine_requests: Vec<Message> = lock(&engine_log)
        .iter()
        .filter(|request| request.target().1 == "/v1/recommend")
        .cloned()
        .collect();
    let want_recommend = expected["recommend_requests"]
        .as_array()
        .expect("recommend_requests");
    assert_eq!(
        engine_requests.len(),
        want_recommend.len(),
        "{context}: recommend calls"
    );
    for (got, want) in engine_requests.iter().zip(want_recommend) {
        let (method, path, query) = got.target();
        assert_eq!(
            (method, path, query),
            (
                want["method"].as_str().expect("method"),
                want["path"].as_str().expect("path"),
                want["query"].as_str().expect("query"),
            ),
            "{context}"
        );
        let mut got_headers = header_map(&got.headers);
        got_headers.remove("Host");
        let mut want_headers = expected_headers(&want["headers"]);
        let want_body = text(&want["body"]);
        if got.body_text() != want_body {
            check_d5_recommend_body(&context, &got.body_text(), &want_body, &body, &client_sent);
            used.insert("D5");
            // The body length differs with the body.
            got_headers.remove("Content-Length");
            want_headers.remove("Content-Length");
        }
        let none = HeaderValues::new();
        assert_eq!(
            without_transport(got_headers, &none, &mut used),
            without_transport(want_headers, &none, &mut used),
            "{context}: recommend headers"
        );
    }

    // Telemetry: the reported items, then the POSTs the engine received after a flush.
    // D14 adds `settings_applied` to each item; D27 batches per tenant key, which for one
    // replayed request is one POST, as Go sent.
    let d14 = parity_support::divergence!("D14");
    let d27 = parity_support::divergence!("D27");
    assert_eq!((d14.id(), d27.id()), ("D14", "D27"));
    let reports = lock(&reports).clone();
    let no_items = reports.iter().all(|report| report.items.is_empty());
    running.flush_telemetry().await;
    let telemetry: Vec<Message> = lock(&engine_log)
        .iter()
        .filter(|request| request.target().1 == "/v1/telemetry")
        .cloned()
        .collect();
    let want_telemetry = expected["telemetry_requests"]
        .as_array()
        .expect("telemetry_requests");
    if want_telemetry.is_empty() && !env.contains_key("MAVERICK_PROXY_TELEMETRY") {
        assert!(no_items, "{context}: no attempt, no telemetry item");
    }
    assert_eq!(
        telemetry.len(),
        want_telemetry.len(),
        "{context}: telemetry POSTs {:?}",
        telemetry.iter().map(Message::body_text).collect::<Vec<_>>()
    );
    if !want_telemetry.is_empty() {
        // Delivery goes through the D27 batcher; the harness observes it at the flush.
        used.insert("D27");
    }
    for (got, want) in telemetry.iter().zip(want_telemetry) {
        if got.body_text().contains("\"settings_applied\"") {
            used.insert("D14");
        }
        let (method, path, _) = got.target();
        assert_eq!(method, want["method"], "{context}");
        assert_eq!(path, want["path"], "{context}");
        assert_eq!(
            normalize_telemetry(&got.body_text()),
            text(&want["body_template"]).replace("<LATENCY_MS>", "0"),
            "{context}: telemetry body"
        );
        let mut got_headers = header_map(&got.headers);
        got_headers.remove("Host");
        got_headers.remove("Content-Length");
        let mut want_headers = expected_headers(&want["headers"]);
        want_headers.remove("Content-Length");
        let none = HeaderValues::new();
        assert_eq!(
            without_transport(got_headers, &none, &mut used),
            without_transport(want_headers, &none, &mut used),
            "{context}: telemetry headers"
        );
    }

    // OTel (D31): the telemetry flush above shut the sink down, which flushed OTLP.
    if expected.get("otlp_requests_after_forced_flush").is_some() {
        otlp::assert_matches_fixture(&context, &collected, &scenario);
        used.insert("D31");
    } else {
        assert!(
            lock(&collected).is_empty(),
            "{context}: OTLP without an endpoint"
        );
    }

    running.stop().await;
    check_allowlist(id, &used);
    reports
}

/// Refused before the engine is called: nothing differs.
const REJECTED_BEFORE_ENGINE: &[&str] = &[];
/// The recommend call (D5 body, D32 transport headers), any upstream attempts (D32), and
/// no telemetry POST.
const ENGINE_NO_TELEMETRY: &[&str] = &["D5", "D32"];
/// A settled chain with telemetry: D14 `settings_applied` in each item and D27 delivery.
const FULL_CHAIN: &[&str] = &["D5", "D14", "D27", "D32"];
/// A settled chain with OTel export (D31).
const OTEL: &[&str] = &["D5", "D14", "D27", "D31", "D32"];
/// The 429 whose `Retry-After` the executor obeys (D26).
const RETRY_AFTER: &[&str] = &["D5", "D14", "D26", "D27", "D32"];

/// The reviewed allowlist: for each scenario, the DIVERGENCES rows whose tolerances its
/// comparison may use. D32 (transport headers) is on every scenario that reaches an
/// upstream, the engine, or telemetry; D5 where the recommend body carries the Rust
/// workload projection; D14 and D27 where telemetry items are compared; D26 on the
/// `Retry-After` scenario; D31 on the OTel scenarios.
const ALLOWLIST: &[(&str, &[&str])] = &[
    ("up", REJECTED_BEFORE_ENGINE),
    ("chat_engine_error_502", ENGINE_NO_TELEMETRY),
    ("chat_concrete_model_rejected", REJECTED_BEFORE_ENGINE),
    ("chat_malformed_alias_rejected", REJECTED_BEFORE_ENGINE),
    ("chat_happy_path", FULL_CHAIN),
    ("chat_first_503_second_used", FULL_CHAIN),
    ("chat_semantic_400_no_fallback", FULL_CHAIN),
    ("chat_byok_native_provider", FULL_CHAIN),
    ("chat_cross_provider_env_key", FULL_CHAIN),
    ("chat_pool_mode_alias", FULL_CHAIN),
    ("chat_prompt_last_user", FULL_CHAIN),
    ("chat_declared_fallback", FULL_CHAIN),
    ("chat_telemetry_off", ENGINE_NO_TELEMETRY),
    ("chat_streaming_sse", FULL_CHAIN),
    ("messages_anthropic_only_skip_others", FULL_CHAIN),
    ("messages_default_anthropic_version", FULL_CHAIN),
    ("messages_byok_x_api_key", FULL_CHAIN),
    ("messages_zero_anthropic_502", ENGINE_NO_TELEMETRY),
    ("messages_beta_body_and_header_passthrough", FULL_CHAIN),
    ("messages_client_authorization_stripped", FULL_CHAIN),
    ("chat_client_x_api_key_stripped", FULL_CHAIN),
    ("chat_hop_by_hop_stripped", FULL_CHAIN),
    ("chat_custom_header_forwarded", FULL_CHAIN),
    ("messages_custom_header_forwarded", FULL_CHAIN),
    ("messages_query_string_forwarded", FULL_CHAIN),
    ("chat_query_string_forwarded", FULL_CHAIN),
    ("messages_effort_xhigh_to_max", FULL_CHAIN),
    ("chat_reasoning_effort_xhigh_to_high", FULL_CHAIN),
    ("otel_happy_path_log_record", OTEL),
    ("otel_measured_false_fallback_counted", OTEL),
    ("otel_disabled_emits_nothing", FULL_CHAIN),
    ("x_chat_connect_error_advances", FULL_CHAIN),
    ("x_chat_header_timeout_advances", FULL_CHAIN),
    ("x_chat_all_fail_exhausted_502", FULL_CHAIN),
    ("x_chat_no_credential_502", ENGINE_NO_TELEMETRY),
    ("x_chat_google_xai_paths", FULL_CHAIN),
    ("x_chat_stream_after_503", FULL_CHAIN),
    ("x_messages_streaming_sse", FULL_CHAIN),
    ("x_messages_mid_stream_abort", FULL_CHAIN),
    ("x_chat_404_relayed", FULL_CHAIN),
    ("x_chat_invalid_json_400", REJECTED_BEFORE_ENGINE),
    ("x_chat_model_not_string_400", REJECTED_BEFORE_ENGINE),
    ("x_chat_missing_model_400_alias", REJECTED_BEFORE_ENGINE),
    ("x_messages_concrete_model_400", REJECTED_BEFORE_ENGINE),
    ("x_engine_malformed_json_502", ENGINE_NO_TELEMETRY),
    ("x_engine_empty_chain_502", ENGINE_NO_TELEMETRY),
    ("x_chat_prompt_escaping_and_array", FULL_CHAIN),
    ("x_chat_body_reencoding", FULL_CHAIN),
    ("x_chat_upstream_sets_maverick_header", FULL_CHAIN),
    ("x_chat_sse_content_type_case", FULL_CHAIN),
    ("x_method_not_allowed", REJECTED_BEFORE_ENGINE),
    ("x_unknown_path_404", REJECTED_BEFORE_ENGINE),
    ("x_post_up_405", REJECTED_BEFORE_ENGINE),
    ("x_chat_429_advances", RETRY_AFTER),
];

/// Checks the rows a scenario used against its reviewed entry and writes its outcome for
/// the CI parity tier.
fn check_allowlist(id: &str, used: &BTreeSet<&'static str>) {
    let rows: Vec<&str> = used.iter().copied().collect();
    if std::env::var_os("PROXY_REPLAY_PRINT_TOLERANCES").is_some() {
        eprintln!("TOLERANCES {id} {rows:?}");
        return;
    }
    let allowed = ALLOWLIST
        .iter()
        .find(|(scenario, _)| *scenario == id)
        .map_or(&[][..], |(_, rows)| *rows);
    let unreviewed: Vec<&str> = rows
        .iter()
        .copied()
        .filter(|row| !allowed.contains(row))
        .collect();
    assert!(
        unreviewed.is_empty(),
        "{id}: differences under {unreviewed:?} are not in the reviewed allowlist"
    );
    let outcome = if rows.is_empty() {
        format!("{id}: passed\n")
    } else {
        format!("{id}: allowlisted ({})\n", rows.join(", "))
    };
    let dir = parity_support::repo_root()
        .expect("repo root")
        .join("target/ci/parity/proxy-replay");
    std::fs::create_dir_all(&dir).expect("report directory");
    std::fs::write(dir.join(format!("{id}.txt")), outcome).expect("report");
}

macro_rules! scenarios {
    ($($id:ident),* $(,)?) => {
        $(
            #[tokio::test]
            async fn $id() {
                replay(stringify!($id)).await;
            }
        )*
    };
}

// Every Go test below is one of these scenarios, checked in full: the client response, the
// upstream requests, the recommend request (D5), and the telemetry POSTs (D14, D27). The
// three OTel tests are tagged with bead model-maverick-7sa, which emits OTel.
// parity: proxy.TestUp
// parity: proxy.TestChatCompletions_EngineErrorIs502
// parity: proxy.TestChatCompletions_ConcreteModelRejected
// parity: proxy.TestChatCompletions_MalformedAliasRejected
// parity: proxy.TestChatCompletions_HappyPath
// parity: proxy.TestChatCompletions_TelemetryOffMeansZeroTelemetryCalls
// parity: proxy.TestChatCompletions_FirstEntry503SecondEntryUsed
// parity: proxy.TestChatCompletions_Semantic400NoFallback
// parity: proxy.TestChatCompletions_BYOKVsEnvKey
// parity: proxy.TestChatCompletions_BYOKVsEnvKey/native_provider_uses_client_credential
// parity: proxy.TestChatCompletions_BYOKVsEnvKey/cross-provider_pick_uses_env_key,_not_client_credential
// parity: proxy.TestChatCompletions_PoolModeAliasParsedIntoRecommend
// parity: proxy.TestChatCompletions_PromptFormExtractedFromLastUserMessage
// parity: proxy.TestChatCompletions_DeclaredFallbackWhenNoUserText
// parity: proxy.TestChatCompletions_StreamingSSEPassThroughByteIdentical
// parity: proxy.TestMessages_RoutesAnthropicOnlySkipsOthers
// parity: proxy.TestMessages_DefaultAnthropicVersionWhenClientOmitsIt
// parity: proxy.TestMessages_BYOKXAPIKeyForwardedForNativeProvider
// parity: proxy.TestMessages_ZeroAnthropicEntriesNoExecutableCandidates
// parity: proxy.TestMessages_ClaudeCodeBetaBodyAndHeaderPassThrough
// parity: proxy.TestMessages_ClientAuthorizationNeverReachesUpstream
// parity: proxy.TestChatCompletions_ClientXAPIKeyNeverReachesUpstream
// parity: proxy.TestChatCompletions_HopByHopHeadersStripped
// parity: proxy.TestCustomHeaderForwarded
// parity: proxy.TestCustomHeaderForwarded/chat_surface
// parity: proxy.TestCustomHeaderForwarded/messages_surface
// parity: proxy.TestQueryStringForwarded
// parity: proxy.TestQueryStringForwarded/messages_surface
// parity: proxy.TestQueryStringForwarded/chat_surface
// parity: proxy.TestMessages_EffortXHighNormalizedToMax
// parity: proxy.TestChatCompletions_ReasoningEffortXHighNormalizedToHigh
scenarios!(
    up,
    chat_engine_error_502,
    chat_concrete_model_rejected,
    chat_malformed_alias_rejected,
    chat_happy_path,
    chat_first_503_second_used,
    chat_semantic_400_no_fallback,
    chat_byok_native_provider,
    chat_cross_provider_env_key,
    chat_pool_mode_alias,
    chat_prompt_last_user,
    chat_declared_fallback,
    chat_telemetry_off,
    chat_streaming_sse,
    messages_anthropic_only_skip_others,
    messages_default_anthropic_version,
    messages_byok_x_api_key,
    messages_zero_anthropic_502,
    messages_beta_body_and_header_passthrough,
    messages_client_authorization_stripped,
    chat_client_x_api_key_stripped,
    chat_hop_by_hop_stripped,
    chat_custom_header_forwarded,
    messages_custom_header_forwarded,
    messages_query_string_forwarded,
    chat_query_string_forwarded,
    messages_effort_xhigh_to_max,
    chat_reasoning_effort_xhigh_to_high,
    otel_happy_path_log_record,
    otel_measured_false_fallback_counted,
    otel_disabled_emits_nothing,
    x_chat_connect_error_advances,
    x_chat_header_timeout_advances,
    x_chat_all_fail_exhausted_502,
    x_chat_no_credential_502,
    x_chat_google_xai_paths,
    x_chat_stream_after_503,
    x_messages_streaming_sse,
    x_messages_mid_stream_abort,
    x_chat_404_relayed,
    x_chat_invalid_json_400,
    x_chat_model_not_string_400,
    x_chat_missing_model_400_alias,
    x_messages_concrete_model_400,
    x_engine_malformed_json_502,
    x_engine_empty_chain_502,
    x_chat_prompt_escaping_and_array,
    x_chat_body_reencoding,
    x_chat_upstream_sets_maverick_header,
    x_chat_sse_content_type_case,
    x_method_not_allowed,
    x_unknown_path_404,
    x_post_up_405,
);

/// A self-hosted proxy whose engine answers `recommendations` and whose openai upstream
/// runs `script`.
async fn custom_proxy(
    recommendations: Value,
    script: Vec<Value>,
) -> (RunningProxy, AttemptLog, Log) {
    let engine_log: Log = Arc::default();
    let engine_url = start_engine(
        (
            200,
            "application/json".to_owned(),
            serde_json::json!({
                "classification": {"method": "declared"},
                "measured": true,
                "recommendation_id": "rec_custom",
                "recommendations": recommendations,
            })
            .to_string(),
        ),
        Arc::clone(&engine_log),
    )
    .await;
    let attempts: AttemptLog = Arc::default();
    let openai = start_upstream("openai", script, Arc::clone(&attempts)).await;
    let env: BTreeMap<&str, String> = [
        ("MAVERICK_API_KEY", "mk_live_test".to_owned()),
        ("MAVERICK_ENGINE_URL", engine_url),
        ("MAVERICK_UPSTREAM_OPENAI", openai),
        ("MAVERICK_UPSTREAM_ANTHROPIC", CLOSED_PORT.to_owned()),
        ("MAVERICK_UPSTREAM_GOOGLE", CLOSED_PORT.to_owned()),
        ("MAVERICK_UPSTREAM_XAI", CLOSED_PORT.to_owned()),
        ("OPENAI_API_KEY", "sk-env".to_owned()),
    ]
    .into_iter()
    .collect();
    let config = load_config(|name| env.get(name).cloned()).expect("config");
    (
        start_proxy(config, RESPONSE_HEADER_TIMEOUT).await,
        attempts,
        engine_log,
    )
}

fn chat_request(body: &str) -> String {
    format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: proxy.test\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

/// Wait in real time until `ready` holds.
async fn eventually(what: &str, ready: impl Fn() -> bool) {
    for _ in 0..5_000 {
        if ready() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("gave up waiting for {what}");
}

/// QUIRK 12: a client that goes away while an attempt is in flight does not end the walk.
/// The attempt in flight and every later entry are `connect_error` (status 0), the later
/// entries send no request, and the request is still reported for telemetry.
#[tokio::test]
async fn a_client_that_goes_away_mid_chain_still_reports_every_entry() {
    let (proxy, attempts, _) = custom_proxy(
        serde_json::json!([
            {"provider": "openai", "model": "gpt-a"},
            {"provider": "openai", "model": "gpt-b"},
            {"provider": "openai", "model": "gpt-c"},
        ]),
        vec![
            serde_json::json!({"hang": true}),
            serde_json::json!({"status": 200, "headers": [], "body_chunks": ["{}"]}),
        ],
    )
    .await;
    let mut socket = TcpStream::connect(proxy.addr).await.expect("connect");
    let body = r#"{"model":"maverick/auto","messages":[{"role":"user","content":"hi"}]}"#;
    socket
        .write_all(chat_request(body).as_bytes())
        .await
        .expect("send");
    eventually("the first upstream request", || lock(&attempts).len() == 1).await;
    drop(socket);
    let reports = Arc::clone(&proxy.reports);
    eventually("the report", || !lock(&reports).is_empty()).await;

    let report = lock(&reports)[0].clone();
    let items: Vec<(&str, u16, Option<&str>)> = report
        .items
        .iter()
        .map(|item| {
            (
                item.model_used.as_str(),
                item.status,
                item.error_class.map(|class| class.as_str()),
            )
        })
        .collect();
    assert_eq!(
        items,
        [
            ("gpt-a", 0, Some("connect_error")),
            ("gpt-b", 0, Some("connect_error")),
            ("gpt-c", 0, Some("connect_error")),
        ]
    );
    assert_eq!(lock(&attempts).len(), 1, "no request after the client left");
    assert_eq!(report.recommendation_id, "rec_custom");
    proxy.stop().await;
}

/// QUIRK 10: a buffered (non-SSE) body that fails mid-read is relayed as an empty body with
/// the upstream status and headers, and it carries no token counts.
#[tokio::test]
async fn a_buffered_read_error_relays_an_empty_body() {
    let (proxy, _, _) = custom_proxy(
        serde_json::json!([{"provider": "openai", "model": "gpt-a"}]),
        vec![serde_json::json!({
            "status": 200,
            "headers": [["Content-Type", "application/json"], ["X-Up", "1"]],
            "body_chunks": ["{\"usage\":{\"prompt_tokens\":1,"],
            "abort_connection_after_chunks": true,
        })],
    )
    .await;
    let mut socket = TcpStream::connect(proxy.addr).await.expect("connect");
    let body = r#"{"model":"maverick/auto","messages":[{"role":"user","content":"hi"}]}"#;
    socket
        .write_all(chat_request(body).as_bytes())
        .await
        .expect("send");
    let mut buffer = Vec::new();
    let response = read_message(&mut socket, &mut buffer)
        .await
        .expect("response");
    assert_eq!(response.status(), 200);
    assert!(response.body.is_empty(), "{:?}", response.body_text());
    let headers = header_map(&response.headers);
    assert_eq!(headers["X-Up"], ["1"]);
    assert_eq!(headers["X-Maverick-Model-Used"], ["gpt-a"]);
    assert_eq!(headers["X-Maverick-Recommendation-Id"], ["rec_custom"]);
    let reports = Arc::clone(&proxy.reports);
    eventually("the report", || !lock(&reports).is_empty()).await;
    let item = lock(&reports)[0].items[0].clone();
    assert_eq!(
        (item.status, item.tokens_in, item.tokens_out),
        (200, None, None)
    );
    proxy.stop().await;
}

/// Telemetry for a streamed relay is reported only when the stream has ended (reference
/// part 6 §11): mid-stream there is no report; after the last chunk there is one, with no
/// token counts (the stream is not parsed).
#[tokio::test]
async fn a_streamed_relay_reports_after_the_stream_ends() {
    let (proxy, _, _) = custom_proxy(
        serde_json::json!([{"provider": "openai", "model": "gpt-a"}]),
        vec![serde_json::json!({
            "status": 200,
            "headers": [["Content-Type", "text/event-stream"]],
            "body_chunks": ["data: 1\n\n", "data: [DONE]\n\n"],
            "pause_after_first_chunk_ms": 300,
        })],
    )
    .await;
    let mut socket = TcpStream::connect(proxy.addr).await.expect("connect");
    let body = r#"{"model":"maverick/auto","messages":[{"role":"user","content":"hi"}]}"#;
    socket
        .write_all(chat_request(body).as_bytes())
        .await
        .expect("send");
    let mut buffer = Vec::new();
    while find(&buffer, b"data: 1").is_none() {
        assert!(fill(&mut socket, &mut buffer).await, "the first chunk");
    }
    assert!(
        lock(&proxy.reports).is_empty(),
        "no report while the stream is open"
    );
    while find(&buffer, b"0\r\n\r\n").is_none() {
        assert!(
            fill(&mut socket, &mut buffer).await,
            "the end of the stream"
        );
    }
    let reports = Arc::clone(&proxy.reports);
    eventually("the report", || !lock(&reports).is_empty()).await;
    let report = lock(&reports)[0].clone();
    assert_eq!(report.items.len(), 1);
    assert_eq!(
        (
            report.items[0].status,
            report.items[0].tokens_in,
            report.items[0].tokens_out
        ),
        (200, None, None)
    );
    assert_eq!(
        (report.model_used.as_str(), report.final_status),
        ("gpt-a", 200)
    );
    proxy.stop().await;
}

/// Captures this test thread's log lines.
#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl LogBuffer {
    fn text(&self) -> String {
        String::from_utf8_lossy(&lock(&self.0)).into_owned()
    }
}

impl std::io::Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        lock(&self.0).extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// DIVERGENCE: D26. The 429 carries `Retry-After: 30`. Go advanced at once (QUIRK 11); the
/// proxy waits 30 s before the second attempt. Everything the client and the upstream see
/// matches Go.
///
/// The clock is paused and moves only when this test advances it: a parked blocking task
/// holds off tokio's auto-advance, so socket I/O never fires a timer early. The test
/// advances exactly 30 s once the executor logs the wait.
#[tokio::test(start_paused = true)]
async fn x_chat_429_advances() {
    let d26 = parity_support::divergence!("D26");
    assert_eq!(d26.id(), "D26");
    let log = LogBuffer::default();
    let writer = log.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let _subscriber = tracing::subscriber::set_default(subscriber);
    tracing::callsite::rebuild_interest_cache();
    let (release, parked) = std::sync::mpsc::channel::<()>();
    drop(tokio::task::spawn_blocking(move || {
        let _ = parked.recv();
    }));

    let started = tokio::time::Instant::now();
    let driver = tokio::spawn({
        let log = log.clone();
        async move {
            for _ in 0..10_000 {
                // Another test thread can cache a callsite's interest before this
                // subscriber exists; rebuilding keeps the executor's line visible here.
                tracing::callsite::rebuild_interest_cache();
                if log.text().contains("wait_ms=30000") {
                    tokio::time::advance(Duration::from_secs(30)).await;
                    return true;
                }
                tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_millis(1)))
                    .await
                    .expect("real pause");
            }
            false
        }
    });
    replay_noting("x_chat_429_advances", &["D26"]).await;
    assert!(driver.await.expect("driver"), "the D26 wait was logged");
    assert_eq!(started.elapsed(), Duration::from_secs(30));
    drop(release);
}

/// The reviewed allowlist has one entry per scenario, and every row it names exists in
/// `parity/DIVERGENCES.md`.
#[test]
fn the_allowlist_covers_every_scenario_with_known_rows() {
    let doc = parity_support::fixture("proxy", "scenarios.json").expect("scenarios.json");
    let ids: BTreeSet<String> = doc["scenarios"]
        .as_array()
        .expect("scenarios")
        .iter()
        .map(|scenario| text(&scenario["id"]))
        .collect();
    let listed: BTreeSet<String> = ALLOWLIST.iter().map(|(id, _)| (*id).to_owned()).collect();
    assert_eq!(listed, ids, "one reviewed entry per scenario");
    for (id, rows) in ALLOWLIST {
        for row in *rows {
            assert!(
                parity_support::divergence(row).is_ok(),
                "{id}: {row} is not a DIVERGENCES.md row"
            );
        }
    }
}

/// The fixture has exactly these 54 scenarios, so a new capture fails here until it is
/// replayed.
#[test]
fn every_scenario_is_replayed() {
    let doc = parity_support::fixture("proxy", "scenarios.json").expect("scenarios.json");
    let ids: Vec<String> = doc["scenarios"]
        .as_array()
        .expect("scenarios")
        .iter()
        .map(|scenario| text(&scenario["id"]))
        .collect();
    assert_eq!(ids.len(), 54);
}
