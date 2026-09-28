//! Provider clients and the fake provider.
//!
//! This crate owns the common provider contract, error taxonomy, retry policy,
//! settings helpers, and test HTTP server. Concrete clients are added by later
//! package 2c beads.

use rand::RngExt as _;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::error::Error as StdError;
use std::fmt;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;

pub type ProviderResult<T> = Result<T, ProviderError>;
pub type ProviderFuture<'a> =
    Pin<Box<dyn Future<Output = ProviderResult<CompletionResponse>> + Send + 'a>>;
pub type BoxError = Box<dyn StdError + Send + Sync + 'static>;

pub const SETTING_RESPONSE_SCHEMA: &str = "response_schema";

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct CompletionRequest {
    pub model: String,
    pub system: String,
    pub prompt: String,
    pub temperature: f32,
    pub max_tokens: usize,
    #[serde(default)]
    pub settings: Map<String, Value>,
}

impl fmt::Debug for CompletionRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompletionRequest")
            .field("model", &self.model)
            .field("system_len", &self.system.len())
            .field("prompt_len", &self.prompt.len())
            .field("temperature", &self.temperature)
            .field("max_tokens", &self.max_tokens)
            .field("settings_keys", &self.settings.keys().collect::<Vec<_>>())
            .finish()
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompletionResponse {
    pub text: String,
    pub tokens_in: usize,
    pub tokens_out: usize,
}

impl fmt::Debug for CompletionResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompletionResponse")
            .field("text_len", &self.text.len())
            .field("tokens_in", &self.tokens_in)
            .field("tokens_out", &self.tokens_out)
            .finish()
    }
}

pub trait ProviderClient: Send + Sync {
    fn complete<'a>(&'a self, request: CompletionRequest) -> ProviderFuture<'a>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderErrorKind {
    Unknown,
    InvalidRequest,
    RateLimited,
    ProviderError,
    Timeout,
}

impl ProviderErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::InvalidRequest => "invalid_request",
            Self::RateLimited => "rate_limited",
            Self::ProviderError => "provider_error",
            Self::Timeout => "timeout",
        }
    }

    pub fn is_retryable(self) -> bool {
        matches!(self, Self::RateLimited | Self::ProviderError)
    }
}

impl fmt::Display for ProviderErrorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ProviderErrorKind {
    type Err = ParseProviderErrorKind;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "unknown" => Ok(Self::Unknown),
            "invalid_request" => Ok(Self::InvalidRequest),
            "rate_limited" => Ok(Self::RateLimited),
            "provider_error" => Ok(Self::ProviderError),
            "timeout" => Ok(Self::Timeout),
            other => Err(ParseProviderErrorKind {
                value: other.to_string(),
            }),
        }
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
#[error("unknown provider error kind {value}")]
pub struct ParseProviderErrorKind {
    value: String,
}

#[derive(Debug)]
pub struct ProviderError {
    provider: String,
    kind: ProviderErrorKind,
    status: i32,
    attempts: usize,
    inner: Option<BoxError>,
}

impl ProviderError {
    pub fn new(provider: impl Into<String>, kind: ProviderErrorKind) -> Self {
        Self {
            provider: provider.into(),
            kind,
            status: 0,
            attempts: 0,
            inner: None,
        }
    }

    pub fn with_status(mut self, status: i32) -> Self {
        self.status = status;
        self
    }

    pub fn with_attempts(mut self, attempts: usize) -> Self {
        self.attempts = attempts;
        self
    }

    pub fn with_source(mut self, source: impl IntoSourceError) -> Self {
        self.inner = Some(source.into_source_error());
        self
    }

    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub fn kind(&self) -> ProviderErrorKind {
        self.kind
    }

    pub fn status(&self) -> i32 {
        self.status
    }

    pub fn attempts(&self) -> usize {
        self.attempts
    }

    pub fn is_retryable(&self) -> bool {
        self.kind.is_retryable()
    }

    pub fn set_attempts(&mut self, attempts: usize) {
        self.attempts = attempts;
    }
}

impl fmt::Display for ProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.provider, self.kind)?;
        if self.status != 0 {
            write!(formatter, " (status {})", self.status)?;
        }
        if self.attempts > 0 {
            write!(formatter, " after {} attempt(s)", self.attempts)?;
        }
        if let Some(inner) = &self.inner {
            write!(formatter, ": {inner}")?;
        }
        Ok(())
    }
}

impl StdError for ProviderError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.inner
            .as_ref()
            .map(|inner| inner.as_ref() as &(dyn StdError + 'static))
    }
}

pub trait IntoSourceError {
    fn into_source_error(self) -> BoxError;
}

impl IntoSourceError for String {
    fn into_source_error(self) -> BoxError {
        Box::new(MessageError(self))
    }
}

impl IntoSourceError for &str {
    fn into_source_error(self) -> BoxError {
        Box::new(MessageError((*self).to_string()))
    }
}

impl IntoSourceError for BoxError {
    fn into_source_error(self) -> BoxError {
        self
    }
}

impl IntoSourceError for MessageError {
    fn into_source_error(self) -> BoxError {
        Box::new(self)
    }
}

impl IntoSourceError for DeadlineExceeded {
    fn into_source_error(self) -> BoxError {
        Box::new(self)
    }
}

impl IntoSourceError for ContextCanceled {
    fn into_source_error(self) -> BoxError {
        Box::new(self)
    }
}

impl IntoSourceError for io::Error {
    fn into_source_error(self) -> BoxError {
        Box::new(self)
    }
}

impl IntoSourceError for tokio::time::error::Elapsed {
    fn into_source_error(self) -> BoxError {
        Box::new(self)
    }
}

impl IntoSourceError for reqwest::Error {
    fn into_source_error(self) -> BoxError {
        Box::new(self)
    }
}

pub fn boxed_source(error: impl StdError + Send + Sync + 'static) -> BoxError {
    Box::new(error)
}

#[derive(Debug, Error)]
#[error("{0}")]
pub struct MessageError(String);

#[derive(Debug, Error)]
#[error("context deadline exceeded")]
pub struct DeadlineExceeded;

#[derive(Debug, Error)]
#[error("context canceled")]
pub struct ContextCanceled;

pub fn wrap_error(
    provider: impl Into<String>,
    status: i32,
    source: impl IntoSourceError,
) -> ProviderError {
    let source = source.into_source_error();
    let kind = if error_chain_is_timeout(source.as_ref()) {
        ProviderErrorKind::Timeout
    } else if status == 429 {
        ProviderErrorKind::RateLimited
    } else if status >= 500 {
        ProviderErrorKind::ProviderError
    } else if status >= 400 {
        ProviderErrorKind::InvalidRequest
    } else {
        ProviderErrorKind::ProviderError
    };

    ProviderError {
        provider: provider.into(),
        kind,
        status,
        attempts: 0,
        inner: Some(source),
    }
}

pub fn kind_of(error: &(dyn StdError + 'static)) -> ProviderErrorKind {
    let mut current = Some(error);
    while let Some(error) = current {
        if let Some(provider_error) = error.downcast_ref::<ProviderError>() {
            return provider_error.kind();
        }
        if let Some(retry_error) = error.downcast_ref::<RetryError>()
            && let RetryError::Provider(provider_error) = retry_error
        {
            return provider_error.kind();
        }
        current = error.source();
    }
    ProviderErrorKind::Unknown
}

fn error_chain_is_timeout(error: &(dyn StdError + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(error) = current {
        if error.is::<DeadlineExceeded>() || error.is::<ContextCanceled>() {
            return true;
        }

        if let Some(error) = error.downcast_ref::<io::Error>()
            && matches!(
                error.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
            )
        {
            return true;
        }

        if error
            .downcast_ref::<tokio::time::error::Elapsed>()
            .is_some()
        {
            return true;
        }

        if let Some(error) = error.downcast_ref::<reqwest::Error>()
            && error.is_timeout()
        {
            return true;
        }

        current = error.source();
    }
    false
}

pub fn setting_bool(settings: &Map<String, Value>, key: &str) -> Option<bool> {
    settings.get(key).and_then(Value::as_bool)
}

pub fn setting_string<'a>(settings: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    settings.get(key).and_then(Value::as_str)
}

pub fn setting_int(settings: &Map<String, Value>, key: &str) -> Option<i64> {
    let number = settings.get(key)?.as_number()?;
    if let Some(value) = number.as_i64() {
        Some(value)
    } else if let Some(value) = number.as_u64() {
        i64::try_from(value).ok()
    } else {
        number.as_f64().map(|value| value.trunc() as i64)
    }
}

#[derive(Clone)]
pub struct RetryPolicy {
    pub max_attempts: usize,
    pub base_delay: Duration,
    pub max_delay: Duration,
    jitter: Option<Arc<dyn Fn() -> f64 + Send + Sync + 'static>>,
}

impl fmt::Debug for RetryPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RetryPolicy")
            .field("max_attempts", &self.max_attempts)
            .field("base_delay", &self.base_delay)
            .field("max_delay", &self.max_delay)
            .field("jitter", &self.jitter.as_ref().map(|_| "<custom>"))
            .finish()
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_millis(250),
            max_delay: Duration::from_secs(4),
            jitter: None,
        }
    }
}

impl RetryPolicy {
    pub fn new(max_attempts: usize, base_delay: Duration, max_delay: Duration) -> Self {
        Self {
            max_attempts,
            base_delay,
            max_delay,
            jitter: None,
        }
    }

    pub fn with_jitter(mut self, jitter: impl Fn() -> f64 + Send + Sync + 'static) -> Self {
        self.jitter = Some(Arc::new(jitter));
        self
    }

    pub fn with_fixed_jitter(self, jitter: f64) -> Self {
        self.with_jitter(move || jitter)
    }

    pub fn effective_max_attempts(&self) -> usize {
        if self.max_attempts == 0 {
            Self::default().max_attempts
        } else {
            self.max_attempts
        }
    }

    pub fn effective_base_delay(&self) -> Duration {
        if self.base_delay.is_zero() {
            Self::default().base_delay
        } else {
            self.base_delay
        }
    }

    pub fn effective_max_delay(&self) -> Duration {
        if self.max_delay.is_zero() {
            Self::default().max_delay
        } else {
            self.max_delay
        }
    }

    pub fn delay(&self, attempt: i32) -> Duration {
        self.delay_with_jitter(attempt, self.next_jitter())
    }

    pub fn delay_with_jitter(&self, attempt: i32, jitter: f64) -> Duration {
        let base_ns = duration_ns_i64(self.effective_base_delay());
        let max_ns = duration_ns_i64(self.effective_max_delay());
        let mut full_ns = max_ns;
        let shift = attempt - 1;

        if (0..32).contains(&shift)
            && let Some(exp_ns) = base_ns.checked_shl(shift as u32)
        {
            full_ns = exp_ns.min(max_ns);
        }

        let half = full_ns / 2;
        let jitter = if jitter.is_finite() { jitter } else { 0.0 };
        let extra = (jitter * half as f64) as i64;
        Duration::from_nanos((half + extra).max(0) as u64)
    }

    fn next_jitter(&self) -> f64 {
        self.jitter
            .as_ref()
            .map(|jitter| jitter())
            .unwrap_or_else(|| rand::rng().random::<f64>())
    }
}

fn duration_ns_i64(duration: Duration) -> i64 {
    duration.as_nanos().min(i64::MAX as u128) as i64
}

#[derive(Debug, Error)]
pub enum RetryError {
    #[error("{0}")]
    Provider(#[source] ProviderError),
    #[error("{0}")]
    Other(#[source] BoxError),
}

impl RetryError {
    pub fn provider_error(&self) -> Option<&ProviderError> {
        match self {
            Self::Provider(error) => Some(error),
            Self::Other(_) => None,
        }
    }

    pub fn into_provider_error(self) -> Option<ProviderError> {
        match self {
            Self::Provider(error) => Some(error),
            Self::Other(_) => None,
        }
    }
}

impl From<ProviderError> for RetryError {
    fn from(error: ProviderError) -> Self {
        Self::Provider(error)
    }
}

pub fn other_retry_error(error: impl IntoSourceError) -> RetryError {
    RetryError::Other(error.into_source_error())
}

pub async fn retry<T, F, Fut>(policy: RetryPolicy, operation: F) -> Result<T, RetryError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, RetryError>>,
{
    retry_until(policy, None, operation).await
}

pub async fn retry_until<T, F, Fut>(
    policy: RetryPolicy,
    deadline: Option<Instant>,
    mut operation: F,
) -> Result<T, RetryError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, RetryError>>,
{
    for attempt in 1.. {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(mut error) => {
                if let RetryError::Provider(provider_error) = &mut error {
                    provider_error.set_attempts(attempt);
                }

                if !retryable_error(&error) || attempt >= policy.effective_max_attempts() {
                    return Err(error);
                }

                let delay = policy.delay(attempt as i32);
                if sleep_or_deadline(delay, deadline).await.is_err() {
                    let provider = error
                        .provider_error()
                        .map(ProviderError::provider)
                        .unwrap_or_default()
                        .to_string();
                    return Err(ProviderError::new(provider, ProviderErrorKind::Timeout)
                        .with_attempts(attempt)
                        .with_source(DeadlineExceeded)
                        .into());
                }
            }
        }
    }

    unreachable!("retry loop returns from every branch")
}

pub async fn retry_provider<T, F, Fut>(
    policy: RetryPolicy,
    operation: F,
) -> Result<T, ProviderError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, ProviderError>>,
{
    retry_provider_until(policy, None, operation).await
}

pub async fn retry_provider_until<T, F, Fut>(
    policy: RetryPolicy,
    deadline: Option<Instant>,
    mut operation: F,
) -> Result<T, ProviderError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, ProviderError>>,
{
    for attempt in 1.. {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(mut error) => {
                error.set_attempts(attempt);

                if !error.is_retryable() || attempt >= policy.effective_max_attempts() {
                    return Err(error);
                }

                let delay = policy.delay(attempt as i32);
                if sleep_or_deadline(delay, deadline).await.is_err() {
                    return Err(
                        ProviderError::new(error.provider(), ProviderErrorKind::Timeout)
                            .with_attempts(attempt)
                            .with_source(DeadlineExceeded),
                    );
                }
            }
        }
    }

    unreachable!("retry loop returns from every branch")
}

fn retryable_error(error: &RetryError) -> bool {
    match error {
        RetryError::Provider(error) => error.is_retryable(),
        RetryError::Other(_) => false,
    }
}

async fn sleep_or_deadline(delay: Duration, deadline: Option<Instant>) -> Result<(), ()> {
    if let Some(deadline) = deadline {
        tokio::select! {
            () = tokio::time::sleep(delay) => Ok(()),
            () = tokio::time::sleep_until(deadline) => Err(()),
        }
    } else {
        tokio::time::sleep(delay).await;
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScriptedResponse {
    pub status: u16,
    pub body: Vec<u8>,
    pub delay: Option<Duration>,
}

impl ScriptedResponse {
    pub fn json(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            body: body.into(),
            delay: None,
        }
    }

    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = Some(delay);
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    pub body: Vec<u8>,
}

#[derive(Debug)]
pub struct ScriptedTestServer {
    addr: SocketAddr,
    state: Arc<Mutex<ScriptedState>>,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

#[derive(Debug)]
struct ScriptedState {
    script: Vec<ScriptedResponse>,
    calls: Vec<RecordedRequest>,
}

impl ScriptedTestServer {
    pub async fn start(script: Vec<ScriptedResponse>) -> io::Result<Self> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let addr = listener.local_addr()?;
        let state = Arc::new(Mutex::new(ScriptedState {
            script,
            calls: Vec::new(),
        }));
        let (shutdown, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(run_scripted_server(
            listener,
            Arc::clone(&state),
            shutdown_rx,
        ));

        Ok(Self {
            addr,
            state,
            shutdown: Some(shutdown),
            task,
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub async fn calls(&self) -> Vec<RecordedRequest> {
        self.state.lock().await.calls.clone()
    }

    pub async fn call_count(&self) -> usize {
        self.state.lock().await.calls.len()
    }

    pub async fn last_request(&self) -> Option<RecordedRequest> {
        self.state.lock().await.calls.last().cloned()
    }
}

impl Drop for ScriptedTestServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.task.abort();
    }
}

async fn run_scripted_server(
    listener: TcpListener,
    state: Arc<Mutex<ScriptedState>>,
    mut shutdown: oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            result = listener.accept() => {
                let Ok((stream, _peer)) = result else {
                    break;
                };
                tokio::spawn(handle_scripted_connection(stream, Arc::clone(&state)));
            }
            _ = &mut shutdown => break,
        }
    }
}

async fn handle_scripted_connection(mut stream: TcpStream, state: Arc<Mutex<ScriptedState>>) {
    let Ok(request) = read_http_request(&mut stream).await else {
        return;
    };

    let response = {
        let mut state = state.lock().await;
        let index = state.calls.len();
        state.calls.push(request);
        state
            .script
            .get(index)
            .or_else(|| state.script.last())
            .cloned()
            .unwrap_or_else(|| ScriptedResponse::json(500, r#"{"error":"unscripted request"}"#))
    };

    if let Some(delay) = response.delay {
        tokio::time::sleep(delay).await;
    }

    let _ = write_http_response(&mut stream, response).await;
}

async fn read_http_request(stream: &mut TcpStream) -> io::Result<RecordedRequest> {
    let mut buffer = Vec::new();
    let header_end = loop {
        let mut chunk = [0_u8; 1024];
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed before headers",
            ));
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = find_header_end(&buffer) {
            break position;
        }
    };

    let headers = String::from_utf8_lossy(&buffer[..header_end]);
    let mut lines = headers.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().unwrap_or_default().to_string();
    let path = request_parts.next().unwrap_or_default().to_string();
    let content_length = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);

    let body_start = header_end + 4;
    while buffer.len().saturating_sub(body_start) < content_length {
        let mut chunk = [0_u8; 1024];
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed before body",
            ));
        }
        buffer.extend_from_slice(&chunk[..read]);
    }

    let body_end = body_start + content_length;
    Ok(RecordedRequest {
        method,
        path,
        body: buffer[body_start..body_end].to_vec(),
    })
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

async fn write_http_response(stream: &mut TcpStream, response: ScriptedResponse) -> io::Result<()> {
    let reason = reason_phrase(response.status);
    let header = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.status,
        reason,
        response.body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(&response.body).await?;
    stream.shutdown().await
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        408 => "Request Timeout",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parity_support::{case, fixture};
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::Mutex as StdMutex;

    #[derive(Debug, Error)]
    #[error("{message}")]
    struct WrapperError {
        message: &'static str,
        #[source]
        source: BoxError,
    }

    #[derive(Debug, Deserialize)]
    struct RetryDelayCase {
        #[serde(default)]
        go_test: Option<String>,
        base_ms: u64,
        max_ms: u64,
        jitter: f64,
        #[serde(default)]
        attempt: Option<i32>,
        #[serde(default)]
        attempts: Vec<i32>,
        #[serde(default)]
        delay_ns: Option<u64>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct DefaultDelayProbe {
        attempt: i32,
        jitter: f64,
        nanos: u64,
    }

    fn provider_core() -> Value {
        fixture("measurement-io", "providers_core.json").expect("provider fixture")
    }

    fn fixture_kind(value: &Value) -> ProviderErrorKind {
        value.as_str().expect("kind string").parse().expect("kind")
    }

    fn fixture_error(value: &str) -> BoxError {
        match value {
            "<context.DeadlineExceeded>" => Box::new(DeadlineExceeded),
            "<context.Canceled>" => Box::new(ContextCanceled),
            "<join(request failed, DeadlineExceeded)>" => Box::new(WrapperError {
                message: "request failed",
                source: Box::new(DeadlineExceeded),
            }),
            other => Box::new(MessageError(other.to_string())),
        }
    }

    /// parity: providers.TestWrapErrorClassification
    #[test]
    fn wrap_error_classification_matches_fixture() {
        let rows = case("fixtures/measurement-io/providers_core.json#wrap_error_classification")
            .expect("wrap rows");
        for row in rows.as_array().expect("rows") {
            let status = row["status"].as_i64().expect("status") as i32;
            let error = wrap_error("test", status, fixture_error(row["err"].as_str().unwrap()));
            assert_eq!(
                error.kind(),
                fixture_kind(&row["kind"]),
                "row {:?}",
                row.get("go_test").or_else(|| row.get("note"))
            );
        }
    }

    /// parity: providers.TestKindOf
    #[test]
    fn kind_of_matches_fixture() {
        let row = case("fixtures/measurement-io/providers_core.json#kind_of").expect("kind row");
        let wrapped = wrap_error("test", 429, "slow down");
        assert_eq!(kind_of(&wrapped), fixture_kind(&row["wrap_429"]));
        let plain = MessageError("plain".to_string());
        assert_eq!(kind_of(&plain), fixture_kind(&row["plain_error"]));
    }

    /// parity: providers.TestErrorMessage
    #[test]
    fn error_message_matches_fixture() {
        let row =
            case("fixtures/measurement-io/providers_core.json#error_message").expect("message row");
        let source = row["error"]["err"].as_str().expect("inner");
        let error = ProviderError::new(
            row["error"]["provider"].as_str().expect("provider"),
            fixture_kind(&row["error"]["kind"]),
        )
        .with_status(row["error"]["status"].as_i64().expect("status") as i32)
        .with_attempts(row["error"]["attempts"].as_u64().expect("attempts") as usize)
        .with_source(source.to_string());
        assert_eq!(error.to_string(), row["exact"].as_str().expect("exact"));

        let more = &row["more"];
        assert_eq!(
            ProviderError::new("fake", ProviderErrorKind::InvalidRequest)
                .with_status(400)
                .to_string(),
            more["bare"].as_str().unwrap()
        );
        assert_eq!(
            ProviderError::new("openai", ProviderErrorKind::ProviderError)
                .with_attempts(2)
                .with_source("x")
                .to_string(),
            more["no status"].as_str().unwrap()
        );
        assert_eq!(
            ProviderError::new("p", ProviderErrorKind::Unknown).to_string(),
            more["unknown kind"].as_str().unwrap()
        );
    }

    /// parity: providers.TestSettingHelpers
    #[test]
    fn settings_helpers_match_fixture() {
        let row = case("fixtures/measurement-io/providers_core.json#settings_helpers")
            .expect("settings row");
        let mut settings = Map::new();
        settings.insert("thinking".to_string(), json!(true));
        settings.insert("reasoning".to_string(), json!("high"));
        settings.insert("budget_int".to_string(), json!(1024));
        settings.insert("budget_float".to_string(), json!(2048.9));
        settings.insert("budget_i64".to_string(), json!(4096_i64));

        for item in row["cases"].as_array().expect("cases") {
            let key = item["key"].as_str().expect("key");
            let ok = item["ok"].as_bool().expect("ok");
            match item["fn"].as_str().expect("fn") {
                "SettingBool" => {
                    assert_eq!(setting_bool(&settings, key).is_some(), ok);
                    if ok {
                        assert_eq!(setting_bool(&settings, key), item["value"].as_bool());
                    }
                }
                "SettingString" => {
                    assert_eq!(setting_string(&settings, key).is_some(), ok);
                    if ok {
                        assert_eq!(setting_string(&settings, key), item["value"].as_str());
                    }
                }
                "SettingInt" => {
                    assert_eq!(setting_int(&settings, key).is_some(), ok);
                    if ok {
                        assert_eq!(setting_int(&settings, key), item["value"].as_i64());
                    }
                }
                unknown => panic!("unknown helper {unknown}"),
            }
        }
    }

    /// parity: providers.TestRetryPolicyDelayBounds
    /// parity: providers.TestRetryPolicyDelayUpperBoundExclusive
    #[test]
    fn retry_delay_matches_fixture() {
        let rows: Vec<RetryDelayCase> =
            serde_json::from_value(provider_core()["retry_delay"].clone()).expect("delay rows");

        for row in rows {
            let policy = RetryPolicy::new(
                3,
                Duration::from_millis(row.base_ms),
                Duration::from_millis(row.max_ms),
            );
            if let Some(attempt) = row.attempt {
                let delay = policy.delay_with_jitter(attempt, row.jitter);
                assert_eq!(
                    delay.as_nanos(),
                    u128::from(row.delay_ns.expect("delay_ns")),
                    "{:?}",
                    row.go_test
                );
            }
            for attempt in row.attempts {
                let delay = policy.delay_with_jitter(attempt, row.jitter);
                let full = (row.base_ms * 1_000_000)
                    .checked_shl((attempt - 1).max(0) as u32)
                    .filter(|candidate| *candidate < row.max_ms * 1_000_000)
                    .unwrap_or(row.max_ms * 1_000_000);
                assert!(delay.as_nanos() >= u128::from(full / 2));
                assert!(delay.as_nanos() < u128::from(full));
            }
        }
    }

    #[test]
    fn retry_delay_default_policy_probes_match_fixture() {
        let rows: Vec<DefaultDelayProbe> =
            serde_json::from_value(provider_core()["retry_delay_default_policy_probes"].clone())
                .expect("default probes");
        let default_policy = RetryPolicy::default();
        let base100_policy =
            RetryPolicy::new(3, Duration::from_millis(100), Duration::from_millis(400));

        for row in rows {
            let policy = if row.nanos == 66_666_666 {
                &base100_policy
            } else {
                &default_policy
            };
            assert_eq!(
                policy.delay_with_jitter(row.attempt, row.jitter).as_nanos(),
                u128::from(row.nanos),
                "attempt {} jitter {}",
                row.attempt,
                row.jitter
            );
        }
    }

    /// parity: providers.TestRetrySucceedsFirstTry
    /// parity: providers.TestRetryRecoversFromRetryableErrors
    /// parity: providers.TestRetryExhaustsAndStampsAttempts
    /// parity: providers.TestRetryStopsOnNonRetryable
    /// parity: providers.TestRetryStopsOnTimeoutKind
    /// parity: providers.TestRetryHonorsContextMidBackoff
    #[tokio::test]
    async fn retry_loop_matches_fixture() {
        let policy = RetryPolicy::default().with_fixed_jitter(0.0);

        let mut calls = 0;
        let first_try = retry(policy.clone(), || {
            calls += 1;
            async { Ok::<_, RetryError>(()) }
        })
        .await;
        assert!(first_try.is_ok());
        assert_eq!(calls, 1);

        let results = Arc::new(StdMutex::new(VecDeque::from([
            Err(wrap_error("test", 429, "slow down").into()),
            Err(wrap_error("test", 429, "slow down").into()),
            Ok(()),
        ])));
        let calls = Arc::new(StdMutex::new(0));
        let recovered = retry(policy.clone(), {
            let results = Arc::clone(&results);
            let calls = Arc::clone(&calls);
            move || {
                let results = Arc::clone(&results);
                let calls = Arc::clone(&calls);
                async move {
                    *calls.lock().expect("calls lock") += 1;
                    results
                        .lock()
                        .expect("results lock")
                        .pop_front()
                        .expect("result")
                }
            }
        })
        .await;
        assert!(recovered.is_ok());
        assert_eq!(*calls.lock().expect("calls lock"), 3);

        let mut calls = 0;
        let exhausted = retry(policy.clone(), || {
            calls += 1;
            async { Err::<(), _>(wrap_error("test", 503, "unavailable").into()) }
        })
        .await
        .expect_err("exhausted");
        assert_eq!(calls, 3);
        let exhausted = exhausted.provider_error().expect("provider error");
        assert_eq!(exhausted.kind(), ProviderErrorKind::ProviderError);
        assert_eq!(exhausted.attempts(), 3);

        let mut calls = 0;
        let invalid = retry(policy.clone(), || {
            calls += 1;
            async { Err::<(), _>(wrap_error("test", 400, "bad").into()) }
        })
        .await
        .expect_err("invalid");
        assert_eq!(calls, 1);
        let invalid = invalid.provider_error().expect("provider error");
        assert_eq!(invalid.kind(), ProviderErrorKind::InvalidRequest);
        assert_eq!(invalid.attempts(), 1);

        let mut calls = 0;
        let timeout = retry(policy.clone(), || {
            calls += 1;
            async { Err::<(), _>(wrap_error("test", 0, DeadlineExceeded).into()) }
        })
        .await
        .expect_err("timeout");
        assert_eq!(calls, 1);
        assert_eq!(
            timeout.provider_error().expect("provider error").kind(),
            ProviderErrorKind::Timeout
        );

        let mut calls = 0;
        let plain = retry(policy.clone(), || {
            calls += 1;
            async { Err::<(), _>(other_retry_error("plain")) }
        })
        .await
        .expect_err("plain");
        assert_eq!(plain.to_string(), "plain");
        assert_eq!(calls, 1);

        let mut calls = 0;
        let deadline_policy =
            RetryPolicy::new(3, Duration::from_millis(500), Duration::from_secs(1))
                .with_fixed_jitter(0.0);
        let deadline = Instant::now() + Duration::from_millis(20);
        let error = retry_until(deadline_policy, Some(deadline), || {
            calls += 1;
            async { Err::<(), _>(wrap_error("test", 429, "slow down").into()) }
        })
        .await
        .expect_err("deadline");
        assert_eq!(calls, 1);
        assert_eq!(
            error.to_string(),
            "test: timeout after 1 attempt(s): context deadline exceeded"
        );
    }

    #[tokio::test]
    async fn scripted_test_server_records_and_replays() {
        let server = ScriptedTestServer::start(vec![
            ScriptedResponse::json(200, r#"{"ok":true}"#),
            ScriptedResponse::json(429, r#"{"error":"slow"}"#),
        ])
        .await
        .expect("server starts");

        let client = reqwest::Client::new();
        let first = client
            .post(format!("{}/v1/test", server.url()))
            .body("one")
            .send()
            .await
            .expect("first response");
        assert_eq!(first.status().as_u16(), 200);
        assert_eq!(
            first
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .expect("content-type"),
            "application/json"
        );

        let second = client
            .post(format!("{}/v1/test", server.url()))
            .body("two")
            .send()
            .await
            .expect("second response");
        assert_eq!(second.status().as_u16(), 429);

        let third = client
            .post(format!("{}/v1/test", server.url()))
            .body("three")
            .send()
            .await
            .expect("third response");
        assert_eq!(third.status().as_u16(), 429);

        assert_eq!(server.call_count().await, 3);
        assert_eq!(
            server.last_request().await.expect("last request").body,
            b"three"
        );
    }
}
