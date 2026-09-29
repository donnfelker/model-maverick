//! The chain executor (bead 6.2) against scripted upstreams: the proxy chain-walk scenarios
//! in `parity/fixtures/proxy/scenarios.json`, sdk-v0 conformance scenarios 2 to 5, D26,
//! skipped entries, cancellation (QUIRK 12), latency (QUIRK 13), and the attempt log.
//!
//! Time is manual. Every test runs on a paused clock, and a parked blocking task holds off
//! tokio's auto-advance, so the clock moves only when a test advances it after an observed
//! event: an upstream that delays its headers, or a D26 wait in the attempt log. Real socket
//! I/O can never let a timer fire early, and every latency is exact. A failing test prints
//! the attempt log and the upstream requests.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::future::Future;
use std::io;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use maverick_core::chain::{
    AttemptRecord, ChainExecutor, ChainOutcome, ChainResult, ErrorClass, LastFailure,
    PreparedAttempt, RESPONSE_HEADER_TIMEOUT,
};
use maverick_core::recommend::ChainEntry;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// The prompt text and the credential every test request carries. Neither may reach the log.
const PROMPT: &str = "PROMPT-TEXT-never-logged";
const CREDENTIAL: &str = "Bearer sk-credential-never-logged";

/// Real-time polls (1 ms each) before a wait gives up.
const REAL_POLLS: usize = 10_000;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Sleep 1 ms of real time. The paused clock does not move.
async fn real_pause() {
    tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_millis(1)))
        .await
        .expect("real pause");
}

fn entry(provider: &str, model: &str) -> ChainEntry {
    ChainEntry {
        provider: provider.to_owned(),
        model: model.to_owned(),
        ..ChainEntry::default()
    }
}

// ---------------------------------------------------------------------------------------
// Scripted upstreams (the fixture's `provider_scripts` replay rules)
// ---------------------------------------------------------------------------------------

/// One scripted upstream answer.
#[derive(Debug, Clone, Default)]
struct Step {
    status: u16,
    headers: Vec<(String, String)>,
    body_chunks: Vec<String>,
    delay_before_headers: Option<Duration>,
    delay_before_body: Option<Duration>,
    abort_after_chunks: bool,
    /// Never answer; read until the client goes away.
    hang: bool,
}

impl Step {
    fn json(status: u16, body: &str) -> Self {
        Self {
            status,
            headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
            body_chunks: vec![body.to_owned()],
            ..Self::default()
        }
    }

    fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    fn hang() -> Self {
        Self {
            hang: true,
            ..Self::default()
        }
    }

    fn from_fixture(value: &Value) -> Self {
        let text = |value: &Value| value.as_str().expect("string").to_owned();
        Self {
            status: u16::try_from(value["status"].as_u64().expect("status")).expect("status"),
            headers: value["headers"]
                .as_array()
                .expect("headers")
                .iter()
                .map(|pair| (text(&pair[0]), text(&pair[1])))
                .collect(),
            body_chunks: value["body_chunks"]
                .as_array()
                .expect("body_chunks")
                .iter()
                .map(text)
                .collect(),
            delay_before_headers: value
                .get("delay_before_headers_ms")
                .and_then(Value::as_u64)
                .map(Duration::from_millis),
            abort_after_chunks: value
                .get("abort_connection_after_chunks")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            ..Self::default()
        }
    }

    fn body(&self) -> String {
        self.body_chunks.concat()
    }
}

/// One request an upstream received: which upstream, its per-upstream index, the model in
/// its body, and the (paused) time it arrived. Bodies and headers are not kept.
#[derive(Debug, Clone)]
struct Received {
    upstream: String,
    index: usize,
    model: String,
    delay_before_headers: Option<Duration>,
    at: Instant,
}

/// Answer request N with `script[N]`; past the end, `500 {"error":"unscripted request"}`.
/// Every response closes its connection, so each attempt is one connection.
async fn serve(
    upstream: String,
    listener: TcpListener,
    script: Vec<Step>,
    events: mpsc::UnboundedSender<Received>,
) {
    let mut next = 0;
    while let Ok((stream, _)) = listener.accept().await {
        let step = script
            .get(next)
            .cloned()
            .unwrap_or_else(|| Step::json(500, r#"{"error":"unscripted request"}"#));
        tokio::spawn(answer(stream, upstream.clone(), next, step, events.clone()));
        next += 1;
    }
}

async fn answer(
    mut stream: TcpStream,
    upstream: String,
    index: usize,
    step: Step,
    events: mpsc::UnboundedSender<Received>,
) {
    let Some(body) = read_request(&mut stream).await else {
        return;
    };
    let model = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|body| body["model"].as_str().map(str::to_owned))
        .unwrap_or_default();
    let _ = events.send(Received {
        upstream,
        index,
        model,
        delay_before_headers: step.delay_before_headers,
        at: Instant::now(),
    });
    if step.hang {
        let mut sink = [0_u8; 256];
        while matches!(stream.read(&mut sink).await, Ok(read) if read > 0) {}
        return;
    }
    if let Some(delay) = step.delay_before_headers {
        tokio::time::sleep(delay).await;
    }
    let mut head = format!("HTTP/1.1 {} Scripted\r\n", step.status);
    for (name, value) in &step.headers {
        let _ = write!(head, "{name}: {value}\r\n");
    }
    head.push_str("Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n");
    if stream.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    if let Some(delay) = step.delay_before_body {
        tokio::time::sleep(delay).await;
    }
    for chunk in &step.body_chunks {
        let framed = format!("{:x}\r\n{chunk}\r\n", chunk.len());
        if stream.write_all(framed.as_bytes()).await.is_err() {
            return;
        }
    }
    if !step.abort_after_chunks {
        let _ = stream.write_all(b"0\r\n\r\n").await;
        let _ = stream.shutdown().await;
    }
}

/// Read one request and return its body.
async fn read_request(stream: &mut TcpStream) -> Option<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    let head_end = loop {
        if let Some(at) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break at + 4;
        }
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    while buffer.len() < head_end + length {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    Some(String::from_utf8_lossy(&buffer[head_end..head_end + length]).into_owned())
}

// ---------------------------------------------------------------------------------------
// The attempt log
// ---------------------------------------------------------------------------------------

/// Captures this test thread's tracing output.
#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl LogBuffer {
    fn text(&self) -> String {
        String::from_utf8_lossy(&lock(&self.0)).into_owned()
    }

    fn attempt_lines(&self) -> Vec<String> {
        self.text()
            .lines()
            .filter(|line| line.contains("maverick-core: chain attempt"))
            .map(str::to_owned)
            .collect()
    }

    /// Every D26 wait the attempt log reports, in order.
    fn waits(&self) -> Vec<Duration> {
        self.attempt_lines()
            .iter()
            .filter_map(|line| field(line, "wait_ms"))
            .map(|millis| Duration::from_millis(millis.parse().expect("wait_ms")))
            .collect()
    }
}

impl io::Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        lock(&self.0).extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The value of ` name=value` in a formatted log line, quotes removed.
fn field(line: &str, name: &str) -> Option<String> {
    let start = line.find(&format!(" {name}="))? + name.len() + 2;
    let value = line[start..].split(' ').next()?;
    Some(value.trim_matches('"').to_owned())
}

// ---------------------------------------------------------------------------------------
// The rig: manual clock, log capture, upstreams, and the surface's `prepare`
// ---------------------------------------------------------------------------------------

/// The surface side of a walk: builds each attempt's request and records `prepare` calls.
/// An entry is skipped when its index is in `skip` or its provider has no upstream.
#[derive(Clone)]
struct Surface {
    http: reqwest::Client,
    bases: BTreeMap<String, String>,
    skip: Vec<usize>,
    prepared: Arc<Mutex<Vec<usize>>>,
}

impl Surface {
    fn prepare(&self, index: usize, entry: &ChainEntry) -> Option<PreparedAttempt> {
        lock(&self.prepared).push(index);
        if self.skip.contains(&index) {
            return None;
        }
        let base = self.bases.get(&entry.provider)?;
        let body = serde_json::json!({
            "model": entry.model,
            "messages": [{"role": "user", "content": PROMPT}],
        });
        let request = self
            .http
            .post(format!("{base}/v1/chat/completions"))
            .header("authorization", CREDENTIAL)
            .header("content-type", "application/json")
            .body(body.to_string())
            .build()
            .expect("request");
        Some(PreparedAttempt {
            request,
            settings_applied: !entry.settings.is_empty(),
            endpoint_used: if entry.endpoint.is_empty() {
                "direct".to_owned()
            } else {
                entry.endpoint.clone()
            },
        })
    }

    fn prepared(&self) -> Vec<usize> {
        lock(&self.prepared).clone()
    }
}

struct Rig {
    log: LogBuffer,
    _subscriber: tracing::subscriber::DefaultGuard,
    /// Dropping it ends the blocking task that holds off auto-advance.
    _manual_clock: std::sync::mpsc::Sender<()>,
    http: reqwest::Client,
    bases: BTreeMap<String, String>,
    events_tx: mpsc::UnboundedSender<Received>,
    events: mpsc::UnboundedReceiver<Received>,
    received: Vec<Received>,
    waits_applied: usize,
}

impl Rig {
    fn new() -> Self {
        // On a paused current-thread runtime, a running blocking task inhibits auto-advance.
        let (release, parked) = std::sync::mpsc::channel::<()>();
        drop(tokio::task::spawn_blocking(move || {
            let _ = parked.recv();
        }));
        let log = LogBuffer::default();
        let writer = log.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .without_time()
            .with_target(false)
            .with_writer(move || writer.clone())
            .finish();
        let (events_tx, events) = mpsc::unbounded_channel();
        Self {
            log,
            _subscriber: tracing::subscriber::set_default(subscriber),
            _manual_clock: release,
            http: reqwest::Client::builder()
                .no_proxy()
                .build()
                .expect("client"),
            bases: BTreeMap::new(),
            events_tx,
            events,
            received: Vec::new(),
            waits_applied: 0,
        }
    }

    async fn upstream(&mut self, name: &str, script: Vec<Step>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        self.bases
            .insert(name.to_owned(), format!("http://{address}"));
        tokio::spawn(serve(
            name.to_owned(),
            listener,
            script,
            self.events_tx.clone(),
        ));
    }

    /// An upstream whose base URL is a closed port (the fixture's
    /// `dead_upstreams_connection_refused`).
    fn dead(&mut self, name: &str) {
        self.bases
            .insert(name.to_owned(), "http://127.0.0.1:1".to_owned());
    }

    fn surface(&self) -> Surface {
        Surface {
            http: self.http.clone(),
            bases: self.bases.clone(),
            skip: Vec::new(),
            prepared: Arc::default(),
        }
    }

    fn executor(&self) -> ChainExecutor {
        ChainExecutor::new(self.http.clone())
    }

    fn walk(
        &self,
        executor: ChainExecutor,
        chain: Vec<ChainEntry>,
        surface: &Surface,
        deadline: Option<Instant>,
        cancelled: impl Future<Output = ()> + Send + 'static,
    ) -> JoinHandle<ChainOutcome> {
        let surface = surface.clone();
        tokio::spawn(async move {
            executor
                .execute(
                    &chain,
                    |index, entry| surface.prepare(index, entry),
                    deadline,
                    cancelled,
                )
                .await
        })
    }

    fn collect_events(&mut self) -> Vec<Received> {
        let mut fresh = Vec::new();
        while let Ok(received) = self.events.try_recv() {
            self.received.push(received.clone());
            fresh.push(received);
        }
        fresh
    }

    /// Poll in real time until `ready` holds. The clock does not move.
    async fn until(&mut self, what: &str, ready: impl Fn(&Self) -> bool) {
        for _ in 0..REAL_POLLS {
            self.collect_events();
            if ready(self) {
                return;
            }
            real_pause().await;
        }
        panic!("gave up waiting for {what}");
    }

    /// Run the walk to its end. The clock advances only on observed events: an arrival
    /// whose step delays its headers (by the delay, capped at the header timeout), and a
    /// D26 wait in the attempt log (by the wait, which the executor has already started).
    async fn drive(
        &mut self,
        walk: JoinHandle<ChainOutcome>,
        header_timeout: Duration,
    ) -> ChainOutcome {
        for _ in 0..REAL_POLLS {
            if walk.is_finished() {
                break;
            }
            let mut advances: Vec<Duration> = self
                .collect_events()
                .iter()
                .filter_map(|received| received.delay_before_headers)
                .map(|delay| delay.min(header_timeout))
                .collect();
            let waits = self.log.waits();
            advances.extend(&waits[self.waits_applied..]);
            self.waits_applied = waits.len();
            for advance in advances {
                tokio::time::advance(advance).await;
            }
            real_pause().await;
        }
        self.finish(walk).await
    }

    /// Wait for the walk to end without moving the clock.
    async fn settle(&mut self, walk: JoinHandle<ChainOutcome>) -> ChainOutcome {
        for _ in 0..REAL_POLLS {
            if walk.is_finished() {
                break;
            }
            real_pause().await;
        }
        self.finish(walk).await
    }

    async fn finish(&mut self, walk: JoinHandle<ChainOutcome>) -> ChainOutcome {
        assert!(walk.is_finished(), "the walk did not finish");
        let outcome = walk.await.expect("walk task");
        self.collect_events();
        self.check_log(&outcome.attempts);
        outcome
    }

    /// One `chain attempt` line per record, with the record's fields, and no prompt text or
    /// credential anywhere in the log.
    fn check_log(&self, records: &[AttemptRecord]) {
        let text = self.log.text();
        assert!(!text.contains(PROMPT), "the prompt reached the log");
        assert!(
            !text.contains("sk-credential"),
            "the credential reached the log"
        );
        let lines: Vec<String> = self
            .log
            .attempt_lines()
            .into_iter()
            .filter(|line| field(line, "outcome").as_deref() != Some("skip"))
            .collect();
        assert_eq!(lines.len(), records.len(), "one log line per attempt");
        for (line, record) in lines.iter().zip(records) {
            let want = [
                ("entry", record.index.to_string()),
                ("provider", record.provider.clone()),
                ("model", record.model.clone()),
                (
                    "outcome",
                    if record.relayed { "relay" } else { "advance" }.to_owned(),
                ),
                ("status", record.status.to_string()),
                ("latency_ms", record.latency_ms.to_string()),
            ];
            for (name, value) in want {
                assert_eq!(field(line, name), Some(value), "{name} in {line}");
            }
            assert_eq!(
                field(line, "error_class"),
                record.error_class.map(|class| class.as_str().to_owned()),
                "error_class in {line}"
            );
            assert_eq!(
                field(line, "wait_ms"),
                record
                    .retry_after_wait
                    .map(|wait| wait.as_millis().to_string()),
                "wait_ms in {line}"
            );
        }
    }

    /// `(upstream, per-upstream index)` of every request, in arrival order.
    fn attempt_order(&self) -> Vec<(String, usize)> {
        self.received
            .iter()
            .map(|received| (received.upstream.clone(), received.index))
            .collect()
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!(
                "--- attempt log ---\n{}--- upstream requests ---\n{:#?}",
                self.log.text(),
                self.received
            );
        }
    }
}

/// Read a response body to its end, or to the first transport error.
async fn body_of(mut response: reqwest::Response) -> String {
    let mut body = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        body.extend_from_slice(&chunk);
    }
    String::from_utf8(body).expect("utf-8 body")
}

/// `(index, model, status, error_class, relayed)` of each record.
fn summary(records: &[AttemptRecord]) -> Vec<(usize, &str, u16, Option<ErrorClass>, bool)> {
    records
        .iter()
        .map(|record| {
            (
                record.index,
                record.model.as_str(),
                record.status,
                record.error_class,
                record.relayed,
            )
        })
        .collect()
}

fn relayed(outcome: ChainOutcome) -> (usize, reqwest::Response) {
    match outcome.result {
        ChainResult::Relayed { index, response } => (index, response),
        ChainResult::Exhausted { .. } => panic!("expected a relay, got an exhausted chain"),
    }
}

fn exhausted(outcome: ChainOutcome) -> Option<LastFailure> {
    match outcome.result {
        ChainResult::Exhausted { last_failure } => last_failure,
        ChainResult::Relayed { index, .. } => panic!("expected exhaustion, entry {index} relayed"),
    }
}

// ---------------------------------------------------------------------------------------
// Fixture replay
// ---------------------------------------------------------------------------------------

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

/// Replay one proxy scenario at the executor level and check everything the executor
/// decides: the attempt order and models, one record per telemetry item (model, status,
/// error class, endpoint), exact latencies, and the relayed or exhausted result.
async fn replay(id: &str) -> Rig {
    let scenario = scenario(id);
    let engine: Value = serde_json::from_str(
        scenario["engine_recommend_response"]["body"]
            .as_str()
            .expect("engine body"),
    )
    .expect("engine json");
    let chain: Vec<ChainEntry> = engine["recommendations"]
        .as_array()
        .expect("recommendations")
        .iter()
        .map(|item| {
            entry(
                item["provider"].as_str().expect("provider"),
                item["model"].as_str().expect("model"),
            )
        })
        .collect();

    let mut rig = Rig::new();
    let scripts = scenario["provider_scripts"].as_object().expect("scripts");
    for (name, steps) in scripts {
        let steps = steps.as_array().expect("steps");
        rig.upstream(name, steps.iter().map(Step::from_fixture).collect())
            .await;
    }
    for name in scenario
        .get("dead_upstreams_connection_refused")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        rig.dead(name.as_str().expect("dead upstream"));
    }
    let header_timeout = scenario
        .get("response_header_timeout_ms_override")
        .and_then(Value::as_u64)
        .map_or(RESPONSE_HEADER_TIMEOUT, Duration::from_millis);

    let executor = rig.executor().with_header_timeout(header_timeout);
    let surface = rig.surface();
    let walk = rig.walk(executor, chain, &surface, None, std::future::pending());
    let outcome = rig.drive(walk, header_timeout).await;
    let expected = &scenario["expected"];

    // Attempt order, and the model each upstream request carried.
    let want_order: Vec<(String, usize)> = expected["upstream_attempt_order"]
        .as_array()
        .expect("upstream_attempt_order")
        .iter()
        .map(|item| {
            let index = item["index"].as_u64().expect("index");
            (
                item["upstream"].as_str().expect("upstream").to_owned(),
                usize::try_from(index).expect("index"),
            )
        })
        .collect();
    assert_eq!(rig.attempt_order(), want_order, "{id}: attempt order");
    for (upstream, requests) in expected["upstream_requests"]
        .as_object()
        .expect("upstream_requests")
    {
        let want: Vec<String> = requests
            .as_array()
            .expect("requests")
            .iter()
            .map(|request| {
                let body: Value =
                    serde_json::from_str(request["body"].as_str().expect("body")).expect("json");
                body["model"].as_str().expect("model").to_owned()
            })
            .collect();
        let got: Vec<String> = rig
            .received
            .iter()
            .filter(|received| &received.upstream == upstream)
            .map(|received| received.model.clone())
            .collect();
        assert_eq!(got, want, "{id}: models sent to {upstream}");
    }

    // One record per telemetry item.
    let telemetry = expected["telemetry_requests"]
        .as_array()
        .expect("telemetry_requests");
    assert_eq!(telemetry.len(), 1, "{id}: one telemetry batch");
    let batch: Value = serde_json::from_str(
        &telemetry[0]["body_template"]
            .as_str()
            .expect("body_template")
            .replace("<LATENCY_MS>", "0"),
    )
    .expect("telemetry json");
    let items = batch["items"].as_array().expect("items");
    assert_eq!(outcome.attempts.len(), items.len(), "{id}: records");
    for (record, item) in outcome.attempts.iter().zip(items) {
        assert_eq!(record.model, item["model_used"], "{id}: model_used");
        assert_eq!(u64::from(record.status), item["status"], "{id}: status");
        assert_eq!(
            record.error_class.map(ErrorClass::as_str),
            item.get("error_class").and_then(Value::as_str),
            "{id}: error_class"
        );
        assert_eq!(
            record.endpoint_used, item["endpoint_used"],
            "{id}: endpoint"
        );
        let want_latency = if record.error_class == Some(ErrorClass::Timeout) {
            header_timeout
        } else {
            Duration::ZERO
        };
        assert_eq!(
            u128::from(record.latency_ms),
            want_latency.as_millis(),
            "{id}: latency is exact on the manual clock"
        );
    }

    // The result.
    let response = &expected["response"];
    let last = outcome.attempts.last().cloned();
    if response["status"] == 502
        && response["body"]
            .as_str()
            .is_some_and(|body| body.contains("no_executable_candidates"))
    {
        // Conformance 5: the last provider error is surfaced unchanged.
        match (exhausted(outcome), last) {
            (Some(LastFailure::Response(failure)), Some(last)) => {
                assert_eq!(failure.status.as_u16(), last.status, "{id}");
                let arrival = rig.received.last().expect("a request");
                assert_eq!(arrival.model, last.model, "{id}");
                let steps = scripts[&arrival.upstream].as_array().expect("steps");
                let step = Step::from_fixture(&steps[arrival.index]);
                assert_eq!(
                    failure.body,
                    step.body().as_bytes(),
                    "{id}: last error body"
                );
                for (name, value) in &step.headers {
                    assert_eq!(
                        failure.headers[name.as_str()],
                        value.as_str(),
                        "{id}: {name}"
                    );
                }
            }
            (Some(LastFailure::Transport(class)), Some(last)) => {
                assert_eq!(Some(class), last.error_class, "{id}");
            }
            (None, None) => {}
            (failure, last) => panic!("{id}: last failure {failure:?} for record {last:?}"),
        }
    } else {
        let last = last.expect("a relayed record");
        assert!(last.relayed, "{id}: the last record is the relay");
        assert_eq!(
            response["headers"]["X-Maverick-Model-Used"][0], last.model,
            "{id}: relayed model"
        );
        let (index, relayed) = relayed(outcome);
        assert_eq!(index, last.index, "{id}");
        assert_eq!(
            relayed.status().as_u16(),
            response["status"],
            "{id}: status"
        );
        assert_eq!(
            body_of(relayed).await,
            response["body"].as_str().expect("body"),
            "{id}: relayed body"
        );
    }
    rig
}

// ---------------------------------------------------------------------------------------
// Proxy scenarios and conformance scenarios 2 to 5
// ---------------------------------------------------------------------------------------

/// Conformance 3: a 503 on entry 0 advances, and entry 1 is relayed.
// parity: proxy.TestChatCompletions_FirstEntry503SecondEntryUsed
#[tokio::test(start_paused = true)]
async fn chat_first_503_second_used() {
    replay("chat_first_503_second_used").await;
}

/// Conformance 4: a semantic 400 is relayed verbatim, and the chain does not advance.
// parity: proxy.TestChatCompletions_Semantic400NoFallback
#[tokio::test(start_paused = true)]
async fn chat_semantic_400_no_fallback() {
    replay("chat_semantic_400_no_fallback").await;
}

/// Conformance 4: a 404 is relayed; no second attempt.
#[tokio::test(start_paused = true)]
async fn x_chat_404_relayed() {
    replay("x_chat_404_relayed").await;
}

/// DIVERGENCE: D26. Conformance 2: the 429 carries `Retry-After: 30`. Go advanced at once
/// (QUIRK 11); the executor waits exactly 30 s, then advances. The telemetry items match Go.
#[tokio::test(start_paused = true)]
async fn x_chat_429_advances_after_retry_after() {
    let d26 = parity_support::divergence!("D26");
    assert_eq!(d26.id(), "D26");
    let rig = replay("x_chat_429_advances").await;
    assert_eq!(
        rig.received[1].at - rig.received[0].at,
        Duration::from_secs(30)
    );
    assert_eq!(rig.log.waits(), [Duration::from_secs(30)]);
}

/// Conformance 3: connection refused is `connect_error` with status 0, and the chain
/// advances to the next provider.
#[tokio::test(start_paused = true)]
async fn x_chat_connect_error_advances() {
    replay("x_chat_connect_error_advances").await;
}

/// Conformance 3: no response headers within the header timeout (lowered to 100 ms, as in
/// the capture) is `timeout` with status 0 and a 100 ms latency; the chain advances.
#[tokio::test(start_paused = true)]
async fn x_chat_header_timeout_advances() {
    replay("x_chat_header_timeout_advances").await;
}

/// Conformance 5: every attempt fails; one record per attempt, and the last provider error
/// (the 500 and its body) is surfaced.
#[tokio::test(start_paused = true)]
async fn x_chat_all_fail_exhausted_502() {
    replay("x_chat_all_fail_exhausted_502").await;
}

/// Two providers, both 500: exhausted, the last error from the second upstream.
#[tokio::test(start_paused = true)]
async fn x_chat_google_xai_paths() {
    replay("x_chat_google_xai_paths").await;
}

/// Fallback happens before the first byte; the 2xx stream is then relayed chunk for chunk.
#[tokio::test(start_paused = true)]
async fn x_chat_stream_after_503() {
    replay("x_chat_stream_after_503").await;
}

/// An upstream that aborts after its first chunk is still the relay: the head settled the
/// attempt, and the body just ends. No fallback.
#[tokio::test(start_paused = true)]
async fn x_messages_mid_stream_abort() {
    replay("x_messages_mid_stream_abort").await;
}

// ---------------------------------------------------------------------------------------
// The advance rule, D26 bounds, latency, skips, and cancellation
// ---------------------------------------------------------------------------------------

/// Conformance 3 and 4 as a table: 429 and every 5xx advance; every other status is relayed.
#[tokio::test(start_paused = true)]
async fn only_429_and_5xx_advance() {
    for status in [429, 500, 502, 503, 504, 599] {
        let mut rig = Rig::new();
        rig.upstream(
            "openai",
            vec![Step::json(status, "{}"), Step::json(200, r#"{"ok":true}"#)],
        )
        .await;
        let chain = vec![entry("openai", "a"), entry("openai", "b")];
        let walk = rig.walk(
            rig.executor(),
            chain,
            &rig.surface(),
            None,
            std::future::pending(),
        );
        let outcome = rig.drive(walk, RESPONSE_HEADER_TIMEOUT).await;
        assert_eq!(
            summary(&outcome.attempts),
            [(0, "a", status, None, false), (1, "b", 200, None, true)],
            "{status} advances"
        );
        assert_eq!(relayed(outcome).0, 1);
    }
    for status in [200, 201, 300, 400, 401, 403, 404, 409, 422] {
        let mut rig = Rig::new();
        rig.upstream(
            "openai",
            vec![Step::json(status, r#"{"s":1}"#), Step::json(200, "{}")],
        )
        .await;
        let chain = vec![entry("openai", "a"), entry("openai", "b")];
        let walk = rig.walk(
            rig.executor(),
            chain,
            &rig.surface(),
            None,
            std::future::pending(),
        );
        let outcome = rig.drive(walk, RESPONSE_HEADER_TIMEOUT).await;
        assert_eq!(
            summary(&outcome.attempts),
            [(0, "a", status, None, true)],
            "{status} is relayed"
        );
        assert_eq!(rig.received.len(), 1, "{status}: no second request");
        let (index, response) = relayed(outcome);
        assert_eq!((index, response.status().as_u16()), (0, status));
        assert_eq!(
            body_of(response).await,
            r#"{"s":1}"#,
            "{status}: body verbatim"
        );
    }
}

/// DIVERGENCE: D26. The wait is `Retry-After` when it fits before the deadline (a wait that
/// ends exactly at the deadline fits). A wait that would pass the deadline advances at once
/// and logs why. No header, or a header that is neither seconds nor a date, means no wait.
#[tokio::test(start_paused = true)]
async fn retry_after_waits_only_within_the_deadline() {
    let _d26 = parity_support::divergence!("D26");
    let secs = Duration::from_secs;
    let cases: [(Option<&str>, Option<u64>, Option<Duration>); 7] = [
        (Some("30"), None, Some(secs(30))),
        (Some("30"), Some(60), Some(secs(30))),
        (Some("30"), Some(30), Some(secs(30))),
        (Some("30"), Some(29), None),
        (Some("0"), None, Some(Duration::ZERO)),
        (None, None, None),
        (Some("soon"), None, None),
    ];
    for (retry_after, deadline, want) in cases {
        let mut rig = Rig::new();
        let mut limited = Step::json(429, r#"{"error":"rate_limited"}"#);
        if let Some(value) = retry_after {
            limited = limited.with_header("Retry-After", value);
        }
        rig.upstream("openai", vec![limited, Step::json(200, "{}")])
            .await;
        let chain = vec![entry("openai", "a"), entry("openai", "b")];
        let deadline = deadline.map(|deadline| Instant::now() + secs(deadline));
        let walk = rig.walk(
            rig.executor(),
            chain,
            &rig.surface(),
            deadline,
            std::future::pending(),
        );
        let outcome = rig.drive(walk, RESPONSE_HEADER_TIMEOUT).await;
        let case = format!("Retry-After {retry_after:?}, deadline {deadline:?}");
        assert_eq!(outcome.attempts[0].retry_after_wait, want, "{case}");
        assert_eq!(
            rig.received[1].at - rig.received[0].at,
            want.unwrap_or_default(),
            "{case}: time between the attempts"
        );
        assert_eq!(
            rig.log.text().contains("Retry-After passes the deadline"),
            retry_after == Some("30") && want.is_none(),
            "{case}: the skipped wait is logged"
        );
        assert_eq!(relayed(outcome).0, 1, "{case}");
    }
}

/// D26 does not wait after the last entry, because nothing follows. The walk ends at once
/// with the 429 as the last provider error.
#[tokio::test(start_paused = true)]
async fn a_429_on_the_last_entry_ends_the_walk_at_once() {
    let mut rig = Rig::new();
    rig.upstream(
        "openai",
        vec![Step::json(429, r#"{"error":"slow down"}"#).with_header("Retry-After", "30")],
    )
    .await;
    let started = Instant::now();
    let walk = rig.walk(
        rig.executor(),
        vec![entry("openai", "a")],
        &rig.surface(),
        None,
        std::future::pending(),
    );
    let outcome = rig.settle(walk).await;
    assert_eq!(Instant::now(), started, "no wait");
    assert_eq!(outcome.attempts[0].retry_after_wait, None);
    let Some(LastFailure::Response(failure)) = exhausted(outcome) else {
        panic!("the 429 is the last failure");
    };
    assert_eq!(failure.status.as_u16(), 429);
    assert_eq!(failure.headers["retry-after"], "30");
    assert_eq!(failure.body, br#"{"error":"slow down"}"#);
}

/// Reference part 6 §8.4: a 429 or 5xx body is drained before the next attempt. Here the
/// 503 body arrives 5 s after its head, and the second attempt starts only then. The last
/// failure keeps a body that ends early at the first read error.
#[tokio::test(start_paused = true)]
async fn failed_bodies_are_drained_before_the_next_attempt() {
    let mut rig = Rig::new();
    let slow_error = Step {
        delay_before_body: Some(Duration::from_secs(5)),
        ..Step::json(503, r#"{"error":"busy"}"#)
    };
    let aborted_error = Step {
        abort_after_chunks: true,
        ..Step::json(500, r#"{"partial":"#)
    };
    rig.upstream("openai", vec![slow_error, aborted_error])
        .await;
    let chain = vec![entry("openai", "a"), entry("openai", "b")];
    let walk = rig.walk(
        rig.executor(),
        chain,
        &rig.surface(),
        None,
        std::future::pending(),
    );
    rig.until("the first request", |rig| rig.received.len() == 1)
        .await;
    for _ in 0..20 {
        real_pause().await;
    }
    rig.collect_events();
    assert_eq!(
        rig.received.len(),
        1,
        "no second attempt while the body is unread"
    );
    tokio::time::advance(Duration::from_secs(5)).await;
    let outcome = rig.drive(walk, RESPONSE_HEADER_TIMEOUT).await;
    assert_eq!(
        rig.received[1].at - rig.received[0].at,
        Duration::from_secs(5)
    );
    assert_eq!(
        summary(&outcome.attempts),
        [(0, "a", 503, None, false), (1, "b", 500, None, false)]
    );
    assert_eq!(
        outcome.attempts[0].latency_ms, 0,
        "latency ends at the head"
    );
    let Some(LastFailure::Response(failure)) = exhausted(outcome) else {
        panic!("the 500 is the last failure");
    };
    assert_eq!(failure.status.as_u16(), 500);
    assert_eq!(failure.body, br#"{"partial":"#);
}

/// QUIRK 13: `latency_ms` is the time to response headers. The 2xx is handed over when its
/// head arrives, and its body has no deadline: here it arrives 60 s later, past the 30 s
/// header timeout, and still reads in full.
#[tokio::test(start_paused = true)]
async fn latency_is_time_to_headers_and_the_body_has_no_deadline() {
    let mut rig = Rig::new();
    let slow = Step {
        delay_before_headers: Some(Duration::from_millis(250)),
        delay_before_body: Some(Duration::from_secs(60)),
        ..Step::json(200, r#"{"late":"body"}"#)
    };
    rig.upstream("openai", vec![slow]).await;
    let walk = rig.walk(
        rig.executor(),
        vec![entry("openai", "a")],
        &rig.surface(),
        None,
        std::future::pending(),
    );
    let outcome = rig.drive(walk, RESPONSE_HEADER_TIMEOUT).await;
    assert_eq!(outcome.attempts[0].latency_ms, 250);
    let (_, response) = relayed(outcome);
    tokio::time::advance(Duration::from_secs(60)).await;
    assert_eq!(body_of(response).await, r#"{"late":"body"}"#);
}

/// The production response-header timeout is 30 s: nothing has happened at 29.999 s, and
/// the attempt is a `timeout` with a 30000 ms latency at 30 s.
#[tokio::test(start_paused = true)]
async fn the_header_timeout_is_thirty_seconds() {
    assert_eq!(RESPONSE_HEADER_TIMEOUT, Duration::from_secs(30));
    let mut rig = Rig::new();
    rig.upstream("openai", vec![Step::hang(), Step::json(200, "{}")])
        .await;
    let chain = vec![entry("openai", "a"), entry("openai", "b")];
    let walk = rig.walk(
        rig.executor(),
        chain,
        &rig.surface(),
        None,
        std::future::pending(),
    );
    rig.until("the first request", |rig| rig.received.len() == 1)
        .await;
    tokio::time::advance(Duration::from_millis(29_999)).await;
    for _ in 0..20 {
        real_pause().await;
    }
    rig.collect_events();
    assert_eq!(rig.received.len(), 1, "no second attempt before 30 s");
    assert!(!walk.is_finished());
    tokio::time::advance(Duration::from_millis(1)).await;
    let outcome = rig.drive(walk, RESPONSE_HEADER_TIMEOUT).await;
    assert_eq!(
        summary(&outcome.attempts),
        [
            (0, "a", 0, Some(ErrorClass::Timeout), false),
            (1, "b", 200, None, true)
        ]
    );
    assert_eq!(outcome.attempts[0].latency_ms, 30_000);
}

/// Skipped entries (the surface returns no request) make no request and no record, and log
/// one `skip` line each. The records keep the chain indices.
#[tokio::test(start_paused = true)]
async fn skipped_entries_make_no_request_and_no_record() {
    let mut rig = Rig::new();
    rig.upstream("openai", vec![Step::json(503, "{}"), Step::json(200, "{}")])
        .await;
    let mut surface = rig.surface();
    surface.skip = vec![0];
    let chain = vec![
        entry("openai", "a"),
        entry("openai", "b"),
        entry("anthropic", "c"),
        entry("openai", "d"),
    ];
    let walk = rig.walk(
        rig.executor(),
        chain,
        &surface,
        None,
        std::future::pending(),
    );
    let outcome = rig.drive(walk, RESPONSE_HEADER_TIMEOUT).await;
    assert_eq!(surface.prepared(), [0, 1, 2, 3]);
    assert_eq!(
        summary(&outcome.attempts),
        [(1, "b", 503, None, false), (3, "d", 200, None, true)]
    );
    assert_eq!(
        rig.attempt_order(),
        [("openai".to_owned(), 0), ("openai".to_owned(), 1)]
    );
    let skips: Vec<(Option<String>, Option<String>)> = rig
        .log
        .attempt_lines()
        .iter()
        .filter(|line| field(line, "outcome").as_deref() == Some("skip"))
        .map(|line| (field(line, "entry"), field(line, "model")))
        .collect();
    assert_eq!(
        skips,
        [
            (Some("0".to_owned()), Some("a".to_owned())),
            (Some("2".to_owned()), Some("c".to_owned()))
        ]
    );
    assert_eq!(relayed(outcome).0, 3);
}

/// Nothing to attempt: an empty chain, or a chain whose entries are all skipped, is
/// exhausted with no last failure and no request.
#[tokio::test(start_paused = true)]
async fn nothing_to_attempt_is_exhausted_without_a_failure() {
    let mut rig = Rig::new();
    let walk = rig.walk(
        rig.executor(),
        Vec::new(),
        &rig.surface(),
        None,
        std::future::pending(),
    );
    let outcome = rig.settle(walk).await;
    assert!(outcome.attempts.is_empty());
    assert!(exhausted(outcome).is_none());

    let chain = vec![entry("anthropic", "a"), entry("google", "b")];
    let surface = rig.surface();
    let walk = rig.walk(
        rig.executor(),
        chain,
        &surface,
        None,
        std::future::pending(),
    );
    let outcome = rig.settle(walk).await;
    assert_eq!(surface.prepared(), [0, 1]);
    assert!(outcome.attempts.is_empty());
    assert!(exhausted(outcome).is_none());
    assert!(rig.received.is_empty());
}

/// QUIRK 12: a client that goes away ends the attempt in flight as `connect_error`. Every
/// later entry that is not skipped is also `connect_error`, without a request.
#[tokio::test(start_paused = true)]
async fn a_client_that_goes_away_ends_the_walk_as_connect_errors() {
    let mut rig = Rig::new();
    rig.upstream(
        "openai",
        vec![Step::hang(), Step::json(200, "{}"), Step::json(200, "{}")],
    )
    .await;
    let mut surface = rig.surface();
    surface.skip = vec![2];
    let chain = vec![
        entry("openai", "a"),
        entry("openai", "b"),
        entry("openai", "c"),
        entry("openai", "d"),
    ];
    let (cancel, cancelled) = oneshot::channel::<()>();
    let started = Instant::now();
    let walk = rig.walk(rig.executor(), chain, &surface, None, async move {
        let _ = cancelled.await;
    });
    rig.until("the first request", |rig| rig.received.len() == 1)
        .await;
    cancel.send(()).expect("cancel");
    let outcome = rig.settle(walk).await;
    let connect_error = Some(ErrorClass::ConnectError);
    assert_eq!(
        summary(&outcome.attempts),
        [
            (0, "a", 0, connect_error, false),
            (1, "b", 0, connect_error, false),
            (3, "d", 0, connect_error, false)
        ]
    );
    assert_eq!(surface.prepared(), [0, 1, 2, 3]);
    assert_eq!(
        rig.received.len(),
        1,
        "no request after the client went away"
    );
    assert_eq!(Instant::now(), started);
    assert!(matches!(
        exhausted(outcome),
        Some(LastFailure::Transport(ErrorClass::ConnectError))
    ));
}

/// A client that goes away during a D26 wait ends the wait at once. The later entries are
/// `connect_error` without a request.
#[tokio::test(start_paused = true)]
async fn a_client_that_goes_away_ends_a_retry_after_wait() {
    let _d26 = parity_support::divergence!("D26");
    let mut rig = Rig::new();
    rig.upstream(
        "openai",
        vec![
            Step::json(429, "{}").with_header("Retry-After", "30"),
            Step::json(200, "{}"),
        ],
    )
    .await;
    let chain = vec![
        entry("openai", "a"),
        entry("openai", "b"),
        entry("openai", "c"),
    ];
    let (cancel, cancelled) = oneshot::channel::<()>();
    let started = Instant::now();
    let walk = rig.walk(rig.executor(), chain, &rig.surface(), None, async move {
        let _ = cancelled.await;
    });
    rig.until("the D26 wait", |rig| !rig.log.waits().is_empty())
        .await;
    cancel.send(()).expect("cancel");
    let outcome = rig.settle(walk).await;
    let connect_error = Some(ErrorClass::ConnectError);
    assert_eq!(
        summary(&outcome.attempts),
        [
            (0, "a", 429, None, false),
            (1, "b", 0, connect_error, false),
            (2, "c", 0, connect_error, false)
        ]
    );
    assert_eq!(
        outcome.attempts[0].retry_after_wait,
        Some(Duration::from_secs(30))
    );
    assert_eq!(rig.received.len(), 1);
    assert_eq!(Instant::now(), started, "the wait ended at once");
}

/// Records carry the surface's endpoint and the D14 `settings_applied` flag.
#[tokio::test(start_paused = true)]
async fn records_carry_the_endpoint_and_settings_applied() {
    let mut rig = Rig::new();
    rig.upstream("openai", vec![Step::json(500, "{}"), Step::json(200, "{}")])
        .await;
    let mut with_settings = entry("openai", "a");
    with_settings.endpoint = "openrouter".to_owned();
    with_settings
        .settings
        .insert("reasoning_effort".to_owned(), Value::from("high"));
    let chain = vec![with_settings, entry("openai", "b")];
    let walk = rig.walk(
        rig.executor(),
        chain,
        &rig.surface(),
        None,
        std::future::pending(),
    );
    let outcome = rig.drive(walk, RESPONSE_HEADER_TIMEOUT).await;
    let flags: Vec<(&str, bool)> = outcome
        .attempts
        .iter()
        .map(|record| (record.endpoint_used.as_str(), record.settings_applied))
        .collect();
    assert_eq!(flags, [("openrouter", true), ("direct", false)]);
    assert!(
        outcome
            .attempts
            .iter()
            .all(|record| record.provider == "openai")
    );
}
