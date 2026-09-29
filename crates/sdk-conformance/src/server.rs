//! The mock servers (sdk-v0 plan §2.3).
//!
//! Two HTTP servers share one state. The engine server answers `POST /v1/recommend` and
//! `POST /v1/telemetry`, and carries the control endpoints under `/control`. The provider
//! server answers any path with the scripted reply and records what it received. Neither
//! server parses provider wire formats: the scenario supplies the exact bytes, so there is
//! no schema translation here.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::scenario::{EngineScript, ProviderScript, Reply, Scenario, reply_at};

/// One recorded HTTP request, kept for scenario assertions.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RecordedRequest {
    /// The HTTP method.
    pub method: String,
    /// The request path.
    pub path: String,
    /// The raw query string, empty when there was none.
    pub query: String,
    /// The request headers, lower-cased names to lossy string values.
    pub headers: BTreeMap<String, String>,
    /// The request body as lossy UTF-8.
    pub body: String,
    /// The request body length in bytes, for exact byte-count assertions.
    pub body_len: usize,
}

impl RecordedRequest {
    fn capture(method: &Method, uri: &Uri, headers: &HeaderMap, body: &Bytes) -> Self {
        let headers = headers
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    value.to_str().unwrap_or_default().to_owned(),
                )
            })
            .collect();
        RecordedRequest {
            method: method.as_str().to_owned(),
            path: uri.path().to_owned(),
            query: uri.query().unwrap_or_default().to_owned(),
            headers,
            body: String::from_utf8_lossy(body).into_owned(),
            body_len: body.len(),
        }
    }
}

/// Everything the two servers recorded during one scenario.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct Recorded {
    /// Every `POST /v1/recommend` the engine received.
    pub recommend: Vec<RecordedRequest>,
    /// Every `POST /v1/telemetry` the engine received. Empty proves zero telemetry bytes.
    pub telemetry: Vec<RecordedRequest>,
    /// Every request the provider received.
    pub provider: Vec<RecordedRequest>,
}

/// The mock engine's mutable state.
#[derive(Debug, Default)]
struct EngineState {
    recommend: Vec<Reply>,
    telemetry_status: u16,
    calls: usize,
    recorded_recommend: Vec<RecordedRequest>,
    recorded_telemetry: Vec<RecordedRequest>,
}

impl EngineState {
    fn load(&mut self, script: EngineScript) {
        self.recommend = script.recommend;
        self.telemetry_status = script.telemetry_status.unwrap_or(200);
        self.reset();
    }

    fn reset(&mut self) {
        self.calls = 0;
        self.recorded_recommend.clear();
        self.recorded_telemetry.clear();
    }
}

/// The mock provider's mutable state.
#[derive(Debug, Default)]
struct ProviderState {
    responses: Vec<Reply>,
    calls: usize,
    recorded: Vec<RecordedRequest>,
}

impl ProviderState {
    fn load(&mut self, script: ProviderScript) {
        self.responses = script.responses;
        self.reset();
    }

    fn reset(&mut self) {
        self.calls = 0;
        self.recorded.clear();
    }
}

/// The state shared by both servers.
#[derive(Clone, Default)]
struct AppState {
    engine: Arc<Mutex<EngineState>>,
    provider: Arc<Mutex<ProviderState>>,
}

impl AppState {
    fn new() -> Self {
        AppState {
            engine: Arc::new(Mutex::new(EngineState {
                telemetry_status: 200,
                ..EngineState::default()
            })),
            provider: Arc::new(Mutex::new(ProviderState::default())),
        }
    }
}

/// A running pair of mock servers on ephemeral ports. Dropping it stops both.
pub struct Harness {
    /// The engine server address (`/v1/recommend`, `/v1/telemetry`, `/control/*`).
    pub engine_addr: SocketAddr,
    /// The provider server address (any path).
    pub provider_addr: SocketAddr,
    state: AppState,
    tasks: Vec<JoinHandle<()>>,
}

impl Harness {
    /// Bind both servers on `127.0.0.1` ephemeral ports and start serving.
    ///
    /// # Errors
    /// Returns the bind error when a listener cannot be opened.
    pub async fn start() -> std::io::Result<Self> {
        Self::start_on(0, 0).await
    }

    /// Bind the engine and provider servers on the given `127.0.0.1` ports and start
    /// serving. A port of `0` picks an ephemeral port.
    ///
    /// # Errors
    /// Returns the bind error when a listener cannot be opened.
    pub async fn start_on(engine_port: u16, provider_port: u16) -> std::io::Result<Self> {
        let state = AppState::new();
        let engine_listener = TcpListener::bind(("127.0.0.1", engine_port)).await?;
        let provider_listener = TcpListener::bind(("127.0.0.1", provider_port)).await?;
        let engine_addr = engine_listener.local_addr()?;
        let provider_addr = provider_listener.local_addr()?;

        let engine = engine_router(state.clone());
        let provider = provider_router(state.clone());
        let engine_task = tokio::spawn(async move {
            let _ = axum::serve(engine_listener, engine).await;
        });
        let provider_task = tokio::spawn(async move {
            let _ = axum::serve(provider_listener, provider).await;
        });

        Ok(Harness {
            engine_addr,
            provider_addr,
            state,
            tasks: vec![engine_task, provider_task],
        })
    }

    /// The engine base URL, such as `http://127.0.0.1:54321`.
    #[must_use]
    pub fn engine_base_url(&self) -> String {
        format!("http://{}", self.engine_addr)
    }

    /// The provider base URL.
    #[must_use]
    pub fn provider_base_url(&self) -> String {
        format!("http://{}", self.provider_addr)
    }

    /// Load a scenario and clear the recorders.
    pub fn load(&self, scenario: Scenario) {
        self.state
            .engine
            .lock()
            .expect("engine lock")
            .load(scenario.engine);
        self.state
            .provider
            .lock()
            .expect("provider lock")
            .load(scenario.provider);
    }

    /// Everything the servers recorded so far.
    #[must_use]
    pub fn recorded(&self) -> Recorded {
        collect_recorded(&self.state)
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

fn collect_recorded(state: &AppState) -> Recorded {
    let engine = state.engine.lock().expect("engine lock");
    let provider = state.provider.lock().expect("provider lock");
    Recorded {
        recommend: engine.recorded_recommend.clone(),
        telemetry: engine.recorded_telemetry.clone(),
        provider: provider.recorded.clone(),
    }
}

fn engine_router(state: AppState) -> Router {
    Router::new()
        .route("/v1/recommend", post(handle_recommend))
        .route("/v1/telemetry", post(handle_telemetry))
        .route("/control/scenario", post(control_load))
        .route("/control/recorded", get(control_recorded))
        .route("/control/reset", post(control_reset))
        .with_state(state)
}

fn provider_router(state: AppState) -> Router {
    Router::new().fallback(handle_provider).with_state(state)
}

async fn handle_recommend(
    axum::extract::State(state): axum::extract::State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let reply = {
        let mut engine = state.engine.lock().expect("engine lock");
        engine
            .recorded_recommend
            .push(RecordedRequest::capture(&method, &uri, &headers, &body));
        let index = engine.calls;
        engine.calls += 1;
        reply_at(&engine.recommend, index)
    };
    apply(reply).await
}

async fn handle_telemetry(
    axum::extract::State(state): axum::extract::State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let status = {
        let mut engine = state.engine.lock().expect("engine lock");
        engine
            .recorded_telemetry
            .push(RecordedRequest::capture(&method, &uri, &headers, &body));
        engine.telemetry_status
    };
    StatusCode::from_u16(status)
        .unwrap_or(StatusCode::OK)
        .into_response()
}

async fn handle_provider(
    axum::extract::State(state): axum::extract::State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let reply = {
        let mut provider = state.provider.lock().expect("provider lock");
        provider
            .recorded
            .push(RecordedRequest::capture(&method, &uri, &headers, &body));
        let index = provider.calls;
        provider.calls += 1;
        reply_at(&provider.responses, index)
    };
    apply(reply).await
}

async fn control_load(
    axum::extract::State(state): axum::extract::State<AppState>,
    body: Bytes,
) -> Response {
    match Scenario::from_json(&body) {
        Ok(scenario) => {
            state
                .engine
                .lock()
                .expect("engine lock")
                .load(scenario.engine);
            state
                .provider
                .lock()
                .expect("provider lock")
                .load(scenario.provider);
            (StatusCode::OK, "loaded").into_response()
        }
        Err(error) => (
            StatusCode::BAD_REQUEST,
            format!("invalid scenario: {error}"),
        )
            .into_response(),
    }
}

async fn control_recorded(axum::extract::State(state): axum::extract::State<AppState>) -> Response {
    Json(collect_recorded(&state)).into_response()
}

async fn control_reset(axum::extract::State(state): axum::extract::State<AppState>) -> Response {
    state.engine.lock().expect("engine lock").reset();
    state.provider.lock().expect("provider lock").reset();
    (StatusCode::OK, "reset").into_response()
}

/// Peel any delays, sleeping for each, then render the reply.
async fn apply(mut reply: Reply) -> Response {
    loop {
        match reply {
            Reply::Delay { delay_ms, then } => {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                reply = *then;
            }
            other => return render(other),
        }
    }
}

fn render(reply: Reply) -> Response {
    match reply {
        Reply::Respond {
            status,
            headers,
            body,
        } => {
            let (bytes, default_content_type) = body_bytes(body);
            build(status, &headers, default_content_type, bytes)
        }
        Reply::Sse {
            status,
            headers,
            events,
        } => {
            let mut payload = String::new();
            for event in events {
                payload.push_str("data: ");
                payload.push_str(&event);
                payload.push_str("\n\n");
            }
            build(status, &headers, "text/event-stream", payload.into_bytes())
        }
        Reply::Delay { .. } => unreachable!("apply peels every delay before render"),
    }
}

fn body_bytes(body: serde_json::Value) -> (Vec<u8>, &'static str) {
    match body {
        serde_json::Value::Null => (Vec::new(), "application/json"),
        serde_json::Value::String(text) => (text.into_bytes(), "application/json"),
        other => (
            serde_json::to_vec(&other).unwrap_or_default(),
            "application/json",
        ),
    }
}

fn build(
    status: u16,
    headers: &BTreeMap<String, String>,
    default_content_type: &str,
    body: Vec<u8>,
) -> Response {
    let mut builder = Response::builder().status(status);
    let mut has_content_type = false;
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("content-type") {
            has_content_type = true;
        }
        builder = builder.header(name.as_str(), value.as_str());
    }
    if !has_content_type {
        builder = builder.header("content-type", default_content_type);
    }
    builder
        .body(Body::from(body))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}
