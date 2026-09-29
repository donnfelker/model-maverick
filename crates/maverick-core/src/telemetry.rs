//! The telemetry producer (spec §9.10 and §8.1 "Trial proxy behavior", D27, D16, D14).
//!
//! One item per dispatched attempt goes to a batcher that sends every 10 s or at 20
//! queued items, one batch stream per tenant key, and flushes at shutdown within a bound.
//! Delivery is fire-and-forget: a failed send is dropped (debug log only) and never
//! reaches the caller. Off means zero bytes: an off producer has no task and opens no
//! connection.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::{Map, Value};
use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};

use crate::gojson::{members, quote};

/// Send at least this often while items are queued.
pub const FLUSH_INTERVAL: Duration = Duration::from_secs(10);
/// Send as soon as a tenant has this many queued items; also the largest batch.
pub const MAX_BATCH: usize = 20;
/// Each telemetry POST's timeout.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(5);
/// How long shutdown waits for the final sends before dropping them.
pub const SHUTDOWN_FLUSH: Duration = Duration::from_secs(5);

/// True when a telemetry switch (`MAVERICK_PROXY_TELEMETRY`, `MODELMAVERICK_TELEMETRY`)
/// turns telemetry off: exactly `off` in any case, with no trimming.
pub fn is_off(value: Option<&str>) -> bool {
    value.is_some_and(|v| v.eq_ignore_ascii_case("off"))
}

/// A transport failure's class; HTTP answers have none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    Timeout,
    ConnectError,
}

impl ErrorClass {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorClass::Timeout => "timeout",
            ErrorClass::ConnectError => "connect_error",
        }
    }
}

/// One dispatched attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelemetryItem {
    pub recommendation_id: String,
    pub model_used: String,
    /// The upstream status, or 0 for a transport error.
    pub status: u16,
    pub latency_ms: i64,
    pub tokens_in: Option<i64>,
    pub tokens_out: Option<i64>,
    pub error_class: Option<ErrorClass>,
    /// `direct` or `openrouter`.
    pub endpoint_used: String,
    /// Three-valued (D16): `None` when no check ran, never defaulted.
    pub check_pass: Option<bool>,
    /// Whether the entry's settings were applied (D14).
    pub settings_applied: Option<bool>,
}

impl TelemetryItem {
    /// The item as Go's `json.Marshal` writes it, fields in order, absent optional
    /// fields omitted.
    pub fn write_json(&self, out: &mut String) {
        out.push_str("{\"recommendation_id\":");
        out.push_str(&quote(&self.recommendation_id));
        out.push_str(",\"model_used\":");
        out.push_str(&quote(&self.model_used));
        out.push_str(&format!(
            ",\"status\":{},\"latency_ms\":{}",
            self.status, self.latency_ms
        ));
        if let Some(n) = self.tokens_in {
            out.push_str(&format!(",\"tokens_in\":{n}"));
        }
        if let Some(n) = self.tokens_out {
            out.push_str(&format!(",\"tokens_out\":{n}"));
        }
        if let Some(class) = self.error_class {
            out.push_str(",\"error_class\":");
            out.push_str(&quote(class.as_str()));
        }
        out.push_str(",\"endpoint_used\":");
        out.push_str(&quote(&self.endpoint_used));
        if let Some(pass) = self.check_pass {
            out.push_str(&format!(",\"check_pass\":{pass}"));
        }
        if let Some(applied) = self.settings_applied {
            out.push_str(&format!(",\"settings_applied\":{applied}"));
        }
        out.push('}');
    }
}

/// The `POST /v1/telemetry` body: `{"items":[...]}`.
pub fn encode_batch(items: &[TelemetryItem]) -> String {
    let mut out = String::from("{\"items\":[");
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        item.write_json(&mut out);
    }
    out.push_str("]}");
    out
}

/// Which usage field names a response carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageShape {
    /// Chat completions: `usage.prompt_tokens`, `usage.completion_tokens`.
    Chat,
    /// Messages: `usage.input_tokens`, `usage.output_tokens`.
    Messages,
}

/// `parseUsage`: the token counts of a buffered 2xx body. The body must be a JSON
/// object whose `usage` fields are JSON integers; `null` is absent and `0` is kept. A
/// non-integer number, a wrong type, or a non-JSON body makes both absent (Go's decode
/// into `*int` fields failed as a whole).
pub fn parse_usage(shape: UsageShape, body: &[u8]) -> (Option<i64>, Option<i64>) {
    let (in_field, out_field) = match shape {
        UsageShape::Chat => ("prompt_tokens", "completion_tokens"),
        UsageShape::Messages => ("input_tokens", "output_tokens"),
    };
    let decode = || -> Option<(Option<i64>, Option<i64>)> {
        let Value::Object(root) = serde_json::from_slice::<Value>(body).ok()? else {
            return None;
        };
        let mut usage: Option<Map<String, Value>> = None;
        for value in members(&root, "usage") {
            match value {
                Value::Object(map) => usage = Some(map.clone()),
                Value::Null => usage = None,
                _ => return None,
            }
        }
        let Some(usage) = usage else {
            return Some((None, None));
        };
        let field = |name: &str| -> Option<Option<i64>> {
            let mut out = None;
            for value in members(&usage, name) {
                out = match value {
                    Value::Null => None,
                    Value::Number(n) => Some(n.as_i64()?),
                    _ => return None,
                };
            }
            Some(out)
        };
        Some((field(in_field)?, field(out_field)?))
    };
    decode().unwrap_or((None, None))
}

/// Where batches go.
pub trait TelemetrySink: Send + Sync + 'static {
    /// Sends one batch body for one tenant key. Failures are the sink's to swallow.
    fn send<'a>(
        &'a self,
        tenant_key: &'a str,
        body: String,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
}

/// `POST {engine}/v1/telemetry` with `Authorization: Bearer <tenant key>`, a 5 s
/// timeout, and the response drained and ignored.
#[derive(Debug, Clone)]
pub struct HttpSink {
    client: reqwest::Client,
    url: String,
}

impl HttpSink {
    pub fn new(engine_url: &str) -> Result<Self, reqwest::Error> {
        Ok(HttpSink {
            client: reqwest::Client::builder().timeout(SEND_TIMEOUT).build()?,
            url: format!("{}/v1/telemetry", engine_url.trim_end_matches('/')),
        })
    }
}

impl TelemetrySink for HttpSink {
    fn send<'a>(
        &'a self,
        tenant_key: &'a str,
        body: String,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let sent = self
                .client
                .post(&self.url)
                .bearer_auth(tenant_key)
                .header("content-type", "application/json")
                .body(body)
                .send()
                .await;
            match sent {
                Ok(response) => {
                    let _ = response.bytes().await;
                }
                Err(error) => {
                    tracing::debug!(error = %error, "telemetry batch dropped");
                }
            }
        })
    }
}

/// The batcher's timing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatcherConfig {
    pub interval: Duration,
    pub max_batch: usize,
    pub shutdown_flush: Duration,
}

impl Default for BatcherConfig {
    fn default() -> Self {
        BatcherConfig {
            interval: FLUSH_INTERVAL,
            max_batch: MAX_BATCH,
            shutdown_flush: SHUTDOWN_FLUSH,
        }
    }
}

enum Message {
    Items(String, Vec<TelemetryItem>),
    Shutdown(oneshot::Sender<()>),
}

/// The telemetry producer. Cloning shares one batcher.
#[derive(Clone, Default)]
pub struct Telemetry {
    inner: Option<Arc<Inner>>,
}

struct Inner {
    tx: mpsc::UnboundedSender<Message>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for Telemetry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Telemetry")
            .field("on", &self.inner.is_some())
            .finish()
    }
}

impl Telemetry {
    /// Telemetry off: no task, no connection, no bytes.
    pub fn off() -> Self {
        Telemetry { inner: None }
    }

    pub fn is_on(&self) -> bool {
        self.inner.is_some()
    }

    /// Starts the batcher task on the current runtime.
    pub fn start(sink: Arc<dyn TelemetrySink>, config: BatcherConfig) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(run(rx, sink, config));
        Telemetry {
            inner: Some(Arc::new(Inner {
                tx,
                task: Mutex::new(Some(task)),
            })),
        }
    }

    /// Queues one request's items under its tenant key. A request that dispatched no
    /// attempt has no items and sends nothing. Never blocks and never fails.
    pub fn record(&self, tenant_key: &str, items: Vec<TelemetryItem>) {
        if items.is_empty() {
            return;
        }
        if let Some(inner) = &self.inner {
            let _ = inner.tx.send(Message::Items(tenant_key.to_owned(), items));
        }
    }

    /// Flushes every queue and waits, at most the configured bound, for the sends.
    pub async fn shutdown(&self) {
        let Some(inner) = &self.inner else { return };
        let (done, wait) = oneshot::channel();
        if inner.tx.send(Message::Shutdown(done)).is_ok() {
            let _ = wait.await;
        }
        let task = inner
            .task
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }
}

fn dispatch(
    inflight: &mut JoinSet<()>,
    sink: &Arc<dyn TelemetrySink>,
    tenant_key: &str,
    items: Vec<TelemetryItem>,
) {
    let sink = Arc::clone(sink);
    let key = tenant_key.to_owned();
    inflight.spawn(async move {
        sink.send(&key, encode_batch(&items)).await;
    });
}

async fn run(
    mut rx: mpsc::UnboundedReceiver<Message>,
    sink: Arc<dyn TelemetrySink>,
    config: BatcherConfig,
) {
    let max = config.max_batch.max(1);
    let mut queues: BTreeMap<String, Vec<TelemetryItem>> = BTreeMap::new();
    let mut inflight = JoinSet::new();
    let mut ticker = tokio::time::interval(config.interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await;
    let done = loop {
        tokio::select! {
            message = rx.recv() => match message {
                Some(Message::Items(key, items)) => {
                    let queue = queues.entry(key.clone()).or_default();
                    queue.extend(items);
                    while queue.len() >= max {
                        let batch: Vec<TelemetryItem> = queue.drain(..max).collect();
                        dispatch(&mut inflight, &sink, &key, batch);
                    }
                }
                Some(Message::Shutdown(done)) => break Some(done),
                None => break None,
            },
            _ = ticker.tick() => {
                for (key, queue) in &mut queues {
                    while !queue.is_empty() {
                        let take = queue.len().min(max);
                        let batch: Vec<TelemetryItem> = queue.drain(..take).collect();
                        dispatch(&mut inflight, &sink, key, batch);
                    }
                }
            }
            Some(_) = inflight.join_next(), if !inflight.is_empty() => {}
        }
    };
    for (key, queue) in &mut queues {
        while !queue.is_empty() {
            let take = queue.len().min(max);
            let batch: Vec<TelemetryItem> = queue.drain(..take).collect();
            dispatch(&mut inflight, &sink, key, batch);
        }
    }
    let drained = tokio::time::timeout(config.shutdown_flush, async {
        while inflight.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        tracing::debug!("telemetry shutdown flush timed out; remaining batches dropped");
        inflight.abort_all();
    }
    if let Some(done) = done {
        let _ = done.send(());
    }
}
