//! The HTTP API and MCP surface.
//!
//! This crate owns the wire contract and middleware. Endpoint handlers are kept
//! deliberately small: business rules belong to the service crates.

pub mod audit;
pub mod capture;
pub mod classifier_evidence;
pub mod classify;
pub mod config;
mod debug;
mod health;
mod history;
mod internal;
pub mod mcp;
mod metrics;
mod models;
pub mod observe;
mod policies;
mod query;
mod recommend;
mod strict;
mod telemetry;
pub mod version;

pub use health::HealthState;
pub use internal::InternalSettings;
pub use recommend::{RecommendConfig, RecommendError, Recommended, recommend};

/// What the endpoint handlers read: the engine pool and the health inputs. Handlers that
/// need the database answer `500 internal_error` on a detached state (router tests).
#[derive(Clone)]
pub struct ApiState {
    pool: Option<PgPool>,
    health: HealthState,
    recommend: RecommendConfig,
    internal: InternalSettings,
    mcp: mcp::McpSessions,
}

impl ApiState {
    /// The production state.
    pub fn new(pool: PgPool, config: &Config) -> Self {
        Self::with_health(pool.clone(), HealthState::new(pool, config))
            .with_recommend_config(RecommendConfig::from_config(config))
            .with_internal(InternalSettings::from_config(config))
    }

    /// A state with an explicit health configuration and the default recommend knobs.
    pub fn with_health(pool: PgPool, health: HealthState) -> Self {
        Self {
            pool: Some(pool),
            health,
            recommend: RecommendConfig::default(),
            internal: InternalSettings::default(),
            mcp: mcp::McpSessions::default(),
        }
    }

    /// No database.
    pub fn detached() -> Self {
        Self {
            pool: None,
            health: HealthState::detached(),
            recommend: RecommendConfig::default(),
            internal: InternalSettings::default(),
            mcp: mcp::McpSessions::default(),
        }
    }

    /// Replaces the recommend knobs (ranking, ceiling buffer, alert notifier).
    pub fn with_recommend_config(mut self, recommend: RecommendConfig) -> Self {
        self.recommend = recommend;
        self
    }

    /// The engine pool, when there is one.
    pub fn pool(&self) -> Option<&PgPool> {
        self.pool.as_ref()
    }

    /// The recommend knobs.
    pub fn recommend(&self) -> &RecommendConfig {
        &self.recommend
    }

    /// Replaces the internal seam's settings (cold-start window, default quota, clock).
    pub fn with_internal(mut self, internal: InternalSettings) -> Self {
        self.internal = internal;
        self
    }

    /// The internal seam's settings.
    pub fn internal(&self) -> &InternalSettings {
        &self.internal
    }

    /// Replaces the models refresh's listers (spec §4.6): one per provider with an
    /// operator key.
    pub fn with_model_listers(
        mut self,
        listers: Vec<Arc<dyn maverick_providers::models::ModelLister>>,
    ) -> Self {
        self.internal.model_listers = listers;
        self
    }

    /// The live MCP streamable-HTTP sessions.
    pub fn mcp_sessions(&self) -> &mcp::McpSessions {
        &self.mcp
    }
}

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderValue, Response, StatusCode, header},
    middleware::{self, Next},
    routing::any,
};
use futures_util::{FutureExt, future::BoxFuture};
use http_body_util::{LengthLimitError, Limited};
use maverick_config::Config;
use maverick_domain::{keys, keys::InternalToken};
use maverick_store::{self, tenancy};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use sqlx::PgPool;
use std::{
    collections::{BTreeMap, HashMap},
    error::Error,
    panic::AssertUnwindSafe,
    sync::{Arc, Mutex},
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use ulid::Ulid;

/// The maximum request body accepted by public and internal JSON endpoints.
pub const BODY_LIMIT: usize = 2_097_152;

/// The historical Go error codes in fixture order.
pub const GO_ERROR_CODES: [&str; 14] = [
    "unauthorized",
    "tenant_suspended",
    "not_found",
    "zero_retention_conflict",
    "capture_expired",
    "payload_too_large",
    "invalid_request",
    "unknown_model",
    "model_not_in_pool",
    "unknown_pool",
    "pool_would_be_empty",
    "grader_unsupported",
    "rate_limited",
    "internal_error",
];

/// All current Rust API error codes, including the two privacy failures added
/// by the v1 Rust port.
pub const ERROR_CODES: [&str; 16] = [
    "unauthorized",
    "tenant_suspended",
    "not_found",
    "zero_retention_conflict",
    "no_compliant_columns",
    "capture_expired",
    "payload_too_large",
    "invalid_request",
    "unknown_model",
    "model_not_in_pool",
    "unknown_pool",
    "pool_would_be_empty",
    "zero_retention_requires_declared",
    "grader_unsupported",
    "rate_limited",
    "internal_error",
];

/// Named API error codes. Keeping this typed prevents a typo in a handler from
/// changing the wire contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorCode {
    Unauthorized,
    TenantSuspended,
    NotFound,
    ZeroRetentionConflict,
    NoCompliantColumns,
    CaptureExpired,
    PayloadTooLarge,
    InvalidRequest,
    UnknownModel,
    ModelNotInPool,
    UnknownPool,
    PoolWouldBeEmpty,
    ZeroRetentionRequiresDeclared,
    GraderUnsupported,
    RateLimited,
    InternalError,
}

impl ErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unauthorized => "unauthorized",
            Self::TenantSuspended => "tenant_suspended",
            Self::NotFound => "not_found",
            Self::ZeroRetentionConflict => "zero_retention_conflict",
            Self::NoCompliantColumns => "no_compliant_columns",
            Self::CaptureExpired => "capture_expired",
            Self::PayloadTooLarge => "payload_too_large",
            Self::InvalidRequest => "invalid_request",
            Self::UnknownModel => "unknown_model",
            Self::ModelNotInPool => "model_not_in_pool",
            Self::UnknownPool => "unknown_pool",
            Self::PoolWouldBeEmpty => "pool_would_be_empty",
            Self::ZeroRetentionRequiresDeclared => "zero_retention_requires_declared",
            Self::GraderUnsupported => "grader_unsupported",
            Self::RateLimited => "rate_limited",
            Self::InternalError => "internal_error",
        }
    }
}

/// A JSON API error before it is rendered on the wire.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: ErrorCode,
    pub message: String,
    pub details: BTreeMap<String, Value>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            details: BTreeMap::new(),
        }
    }

    pub fn detail(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.details.insert(key.into(), value.into());
        self
    }

    pub fn into_response(self) -> Response<Body> {
        error_response(self.status, self.code, &self.message, self.details)
    }
}

/// Render the canonical error envelope.
pub fn error_response(
    status: StatusCode,
    code: ErrorCode,
    message: &str,
    details: BTreeMap<String, Value>,
) -> Response<Body> {
    let encoded = serde_json::to_vec(&ErrorEnvelope {
        error: ErrorBody {
            code: code.as_str(),
            message: message.to_owned(),
            details,
        },
    })
    .expect("API error envelope contains only serializable values");
    let mut bytes = escape_go_json(&encoded);
    bytes.push(b'\n');

    response_with_json(status, bytes)
}

#[derive(Serialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Serialize)]
struct ErrorBody {
    code: &'static str,
    message: String,
    details: BTreeMap<String, Value>,
}

fn escape_go_json(encoded: &[u8]) -> Vec<u8> {
    let mut escaped = Vec::with_capacity(encoded.len());
    let mut index = 0;
    while index < encoded.len() {
        let replacement = match encoded[index] {
            b'<' => Some((br#"\u003c"#.as_slice(), 1)),
            b'>' => Some((br#"\u003e"#.as_slice(), 1)),
            b'&' => Some((br#"\u0026"#.as_slice(), 1)),
            0xe2 if encoded.get(index..index + 3) == Some([0xe2, 0x80, 0xa8].as_slice()) => {
                Some((br#"\u2028"#.as_slice(), 3))
            }
            0xe2 if encoded.get(index..index + 3) == Some([0xe2, 0x80, 0xa9].as_slice()) => {
                Some((br#"\u2029"#.as_slice(), 3))
            }
            _ => None,
        };
        if let Some((replacement, consumed)) = replacement {
            escaped.extend_from_slice(replacement);
            index += consumed;
        } else {
            escaped.push(encoded[index]);
            index += 1;
        }
    }
    escaped
}

fn response_with_json(status: StatusCode, body: Vec<u8>) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CONTENT_LENGTH, body.len())
        .body(Body::from(body))
        .expect("valid API response headers")
}

/// Identity attached to an authenticated public request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Identity {
    pub tenant_id: String,
    pub api_key_id: String,
}

/// Identity attached to an authenticated internal request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InternalActor(pub String);

/// The result of authenticating a public API key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PublicAuthDecision {
    Missing,
    Suspended,
    Authenticated(Identity),
}

#[derive(Debug, thiserror::Error)]
pub enum AuthBackendError {
    #[error("store error")]
    Store(#[source] sqlx::Error),
}

/// Store boundary used by public authentication. The trait keeps middleware
/// tests deterministic and lets the server use the Postgres implementation.
pub trait PublicAuthBackend: Send + Sync {
    fn authenticate<'a>(
        &'a self,
        token: &'a str,
    ) -> BoxFuture<'a, Result<PublicAuthDecision, AuthBackendError>>;
}

/// Fixed public-auth decision backend used by middleware tests and the
/// compatibility router constructors that do not receive application state.
#[derive(Clone, Debug)]
pub struct StaticAuthBackend {
    decision: PublicAuthDecision,
}

impl StaticAuthBackend {
    pub fn authenticated(identity: Identity) -> Self {
        Self {
            decision: PublicAuthDecision::Authenticated(identity),
        }
    }

    pub fn missing() -> Self {
        Self {
            decision: PublicAuthDecision::Missing,
        }
    }

    pub fn suspended() -> Self {
        Self {
            decision: PublicAuthDecision::Suspended,
        }
    }
}

impl PublicAuthBackend for StaticAuthBackend {
    fn authenticate<'a>(
        &'a self,
        _token: &'a str,
    ) -> BoxFuture<'a, Result<PublicAuthDecision, AuthBackendError>> {
        Box::pin(async move { Ok(self.decision.clone()) })
    }
}

/// Postgres-backed public authentication.
#[derive(Clone)]
pub struct PgAuthBackend {
    pool: PgPool,
}

impl PgAuthBackend {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl PublicAuthBackend for PgAuthBackend {
    fn authenticate<'a>(
        &'a self,
        token: &'a str,
    ) -> BoxFuture<'a, Result<PublicAuthDecision, AuthBackendError>> {
        Box::pin(async move {
            let hash = keys::key_hash(token);
            // The key and its tenant in one round trip (e3x); the decisions are Go's: an
            // unknown or inactive key, or a missing tenant, is 401; a suspended tenant 403.
            let Some(auth) = tenancy::get_key_auth_by_hash(&self.pool, &hash)
                .await
                .map_err(AuthBackendError::Store)?
            else {
                return Ok(PublicAuthDecision::Missing);
            };
            if auth.key_status != "active" {
                return Ok(PublicAuthDecision::Missing);
            }
            match auth.tenant_status.as_deref() {
                None => Ok(PublicAuthDecision::Missing),
                Some("suspended") => Ok(PublicAuthDecision::Suspended),
                Some(_) => Ok(PublicAuthDecision::Authenticated(Identity {
                    tenant_id: auth.tenant_id,
                    api_key_id: auth.key_id,
                })),
            }
        })
    }
}

/// Shared authentication and rate-limit state for both listeners.
#[derive(Clone)]
pub struct AuthState {
    backend: Arc<dyn PublicAuthBackend>,
    internal_tokens: Arc<Vec<InternalToken>>,
    limiter: Arc<RateLimiter>,
}

impl AuthState {
    pub fn new(
        backend: Arc<dyn PublicAuthBackend>,
        internal_tokens: Vec<InternalToken>,
        limiter: RateLimiter,
    ) -> Self {
        Self {
            backend,
            internal_tokens: Arc::new(internal_tokens),
            limiter: Arc::new(limiter),
        }
    }

    pub fn from_config(pool: PgPool, config: &Config) -> Result<Self, keys::InternalTokenError> {
        let token_spec = config
            .internal_tokens
            .iter()
            .map(|token| format!("{}:{}", token.label, token.secret))
            .collect::<Vec<_>>()
            .join(",");
        let internal_tokens = keys::parse_internal_tokens(&token_spec)?;
        Ok(Self::new(
            Arc::new(PgAuthBackend::new(pool)),
            internal_tokens,
            RateLimiter::new(config.ratelimit_rpm),
        ))
    }
}

fn default_auth_state() -> AuthState {
    AuthState::new(
        Arc::new(StaticAuthBackend::authenticated(Identity {
            tenant_id: "ten_test".to_owned(),
            api_key_id: "key_test".to_owned(),
        })),
        keys::parse_internal_tokens("ops:alpha").expect("valid internal test token"),
        RateLimiter::new(300),
    )
}

/// Clock abstraction for deterministic fixed-window rate-limit tests.
pub trait Clock: Send + Sync {
    fn unix_seconds(&self) -> i64;
}

#[derive(Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn unix_seconds(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is before Unix epoch")
            .as_secs() as i64
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RateLimitDecision {
    pub allowed: bool,
    pub limit: u64,
    pub remaining: u64,
    pub reset_at: i64,
    pub retry_after: u64,
}

#[derive(Debug)]
struct RateBucket {
    window_start: i64,
    count: u64,
}

#[derive(Debug)]
struct RateState {
    buckets: HashMap<String, RateBucket>,
    last_sweep_window: i64,
}

/// In-memory fixed-window limiter. A request is allowed while its count is at
/// most the configured limit, and rejected requests continue incrementing the
/// same bucket until the next aligned minute.
pub struct RateLimiter {
    limit: u64,
    clock: Arc<dyn Clock>,
    state: Mutex<RateState>,
}

impl RateLimiter {
    pub fn new(limit: u32) -> Self {
        Self::with_clock(limit, Arc::new(SystemClock))
    }

    pub fn with_clock(limit: u32, clock: Arc<dyn Clock>) -> Self {
        Self {
            limit: u64::from(limit),
            clock,
            state: Mutex::new(RateState {
                buckets: HashMap::new(),
                last_sweep_window: i64::MIN,
            }),
        }
    }

    pub fn check(&self, key_id: &str) -> RateLimitDecision {
        let now = self.clock.unix_seconds();
        let window_start = now.div_euclid(60) * 60;
        let reset_at = window_start + 60;
        let mut state = self.state.lock().expect("rate limiter mutex poisoned");

        if state.last_sweep_window != window_start {
            state
                .buckets
                .retain(|_, bucket| bucket.window_start == window_start);
            state.last_sweep_window = window_start;
        }

        let bucket = state
            .buckets
            .entry(key_id.to_owned())
            .or_insert(RateBucket {
                window_start,
                count: 0,
            });
        if bucket.window_start != window_start {
            bucket.window_start = window_start;
            bucket.count = 0;
        }
        bucket.count = bucket.count.saturating_add(1);

        RateLimitDecision {
            allowed: bucket.count <= self.limit,
            limit: self.limit,
            remaining: self.limit.saturating_sub(bucket.count),
            reset_at,
            retry_after: (reset_at - now).max(1) as u64,
        }
    }
}

pub fn unauthorized() -> Response<Body> {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        ErrorCode::Unauthorized,
        "missing or invalid API key",
    )
    .into_response()
}

pub fn tenant_suspended() -> Response<Body> {
    ApiError::new(
        StatusCode::FORBIDDEN,
        ErrorCode::TenantSuspended,
        "tenant is suspended",
    )
    .into_response()
}

pub fn internal_unauthorized() -> Response<Body> {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        ErrorCode::Unauthorized,
        "missing or invalid internal token",
    )
    .into_response()
}

pub fn not_found() -> Response<Body> {
    ApiError::new(
        StatusCode::NOT_FOUND,
        ErrorCode::NotFound,
        "resource not found",
    )
    .into_response()
}

pub fn internal_error() -> Response<Body> {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        ErrorCode::InternalError,
        "internal error",
    )
    .into_response()
}

pub fn payload_too_large(limit: usize) -> Response<Body> {
    ApiError::new(
        StatusCode::PAYLOAD_TOO_LARGE,
        ErrorCode::PayloadTooLarge,
        "request body exceeds the limit",
    )
    .detail("limit_bytes", limit)
    .into_response()
}

pub fn rate_limited(limit: u64) -> Response<Body> {
    ApiError::new(
        StatusCode::TOO_MANY_REQUESTS,
        ErrorCode::RateLimited,
        "rate limit exceeded",
    )
    .detail("limit", limit)
    .into_response()
}

/// Read a JSON request body after the body-limit middleware has wrapped it.
/// Endpoint request types should use `#[serde(deny_unknown_fields)]` when the
/// strict decoder contract applies.
#[allow(clippy::result_large_err)]
pub async fn decode_json<T: DeserializeOwned>(req: Request) -> Result<T, Response<Body>> {
    let bytes = read_body(req.into_body()).await?;
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Err(invalid_request("request body is required"));
    }

    let mut deserializer = serde_json::Deserializer::from_slice(&bytes);
    let value = T::deserialize(&mut deserializer)
        .map_err(|error| invalid_request(format!("malformed request body: {error}")))?;
    if deserializer.end().is_err() {
        return Err(invalid_request("request body has trailing content"));
    }
    Ok(value)
}

#[allow(clippy::result_large_err)]
pub async fn read_body(body: Body) -> Result<axum::body::Bytes, Response<Body>> {
    match to_bytes(body, BODY_LIMIT).await {
        Ok(bytes) => Ok(bytes),
        Err(error) if is_length_limit_error(&error) => Err(payload_too_large(BODY_LIMIT)),
        Err(_) => Err(internal_error()),
    }
}

/// True when a body read failed on the size limit. The limit error can sit several
/// `source()` levels down: `body_limit` wraps the body in a `Limited`, and `to_bytes`
/// wraps it again, so a chunked oversize body arrives doubly wrapped.
fn is_length_limit_error(error: &axum::Error) -> bool {
    let mut source = error.source();
    while let Some(current) = source {
        if current.downcast_ref::<LengthLimitError>().is_some() {
            return true;
        }
        source = current.source();
    }
    false
}

fn invalid_request(message: impl Into<String>) -> Response<Body> {
    ApiError::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        ErrorCode::InvalidRequest,
        message,
    )
    .into_response()
}

/// Request-ID middleware. Invalid inbound IDs are replaced with an uppercase
/// 26-character ULID and the value is also stored in request extensions.
pub async fn request_id(mut request: Request, next: Next) -> Response<Body> {
    let request_id = request
        .headers()
        .get("X-Request-ID")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty() && value.len() <= 128)
        .map_or_else(|| Ulid::generate().to_string(), str::to_owned);

    request
        .extensions_mut()
        .insert(RequestId(request_id.clone()));
    let mut response = next.run(request).await;
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response.headers_mut().insert("X-Request-ID", value);
    }
    response
}

/// The request ID made available to handlers and middleware.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequestId(pub String);

/// Privacy-safe request logger: one `request` line per request (see
/// [`observe::RequestLog`]). Request bodies and headers are never fields.
pub async fn request_logger(mut request: Request, next: Next) -> Response<Body> {
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .map(|id| id.0.as_str())
        .unwrap_or("")
        .to_owned();
    let method = request.method().to_string();
    let path = request.uri().path().to_owned();
    let log = observe::RequestLog::default();
    request.extensions_mut().insert(log.clone());
    let started = Instant::now();
    let response = next.run(request).await;
    observe::log_request(
        &method,
        &path,
        response.status().as_u16(),
        started.elapsed(),
        &request_id,
        &log,
    );
    response
}

/// Recover panics from handlers without exposing panic content to callers.
pub async fn recoverer(request: Request, next: Next) -> Response<Body> {
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .map(|id| id.0.as_str())
        .unwrap_or("")
        .to_owned();
    match AssertUnwindSafe(next.run(request)).catch_unwind().await {
        Ok(response) => response,
        Err(panic) => {
            let panic_text = panic
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("non-string panic");
            tracing::error!(
                request_id = %request_id,
                panic = %panic_text,
                stack = %std::backtrace::Backtrace::capture(),
                "panic recovered"
            );
            internal_error()
        }
    }
}

/// Enforce the public and internal 2 MiB cap. Content-Length is rejected before
/// the handler runs; chunked bodies are limited as they are read.
pub async fn body_limit(request: Request, next: Next) -> Response<Body> {
    if request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > BODY_LIMIT as u64)
    {
        return payload_too_large(BODY_LIMIT);
    }

    next.run(request.map(|body| Body::new(Limited::new(body, BODY_LIMIT))))
        .await
}

pub async fn public_auth(
    State(state): State<AuthState>,
    mut request: Request,
    next: Next,
) -> Response<Body> {
    let Some(token) = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|token| !token.is_empty() && token.starts_with(keys::KEY_PREFIX))
    else {
        return unauthorized();
    };

    match state.backend.authenticate(token).await {
        Ok(PublicAuthDecision::Authenticated(identity)) => {
            if let Some(log) = request.extensions().get::<observe::RequestLog>() {
                log.set_identity(&identity.tenant_id, &identity.api_key_id);
            }
            request.extensions_mut().insert(identity);
            next.run(request).await
        }
        Ok(PublicAuthDecision::Missing) => unauthorized(),
        Ok(PublicAuthDecision::Suspended) => tenant_suspended(),
        Err(error) => {
            let request_id = request
                .extensions()
                .get::<RequestId>()
                .map(|id| id.0.as_str())
                .unwrap_or("");
            tracing::error!(
                request_id = %request_id,
                error = %error,
                "authenticating request"
            );
            internal_error()
        }
    }
}

pub async fn internal_auth(
    State(state): State<AuthState>,
    mut request: Request,
    next: Next,
) -> Response<Body> {
    let authorization = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");

    if let Some(label) = keys::authenticate_internal(authorization, state.internal_tokens.as_ref())
    {
        if let Some(log) = request.extensions().get::<observe::RequestLog>() {
            log.set_actor(label);
        }
        request
            .extensions_mut()
            .insert(InternalActor(label.to_owned()));
        next.run(request).await
    } else {
        internal_unauthorized()
    }
}

pub async fn rate_limit(
    State(state): State<AuthState>,
    request: Request,
    next: Next,
) -> Response<Body> {
    let Some(identity) = request.extensions().get::<Identity>() else {
        return next.run(request).await;
    };
    let decision = state.limiter.check(&identity.api_key_id);

    if decision.allowed {
        let mut response = next.run(request).await;
        insert_rate_limit_headers(&mut response, &decision);
        response
    } else {
        let mut response = rate_limited(decision.limit);
        insert_rate_limit_headers(&mut response, &decision);
        if let Ok(value) = HeaderValue::from_str(&decision.retry_after.to_string()) {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        response
    }
}

fn insert_rate_limit_headers(response: &mut Response<Body>, decision: &RateLimitDecision) {
    for (name, value) in [
        ("X-RateLimit-Limit", decision.limit.to_string()),
        ("X-RateLimit-Remaining", decision.remaining.to_string()),
        ("X-RateLimit-Reset", decision.reset_at.to_string()),
    ] {
        if let Ok(value) = HeaderValue::from_str(&value) {
            response.headers_mut().insert(name, value);
        }
    }
}

async fn route_stub(_: Request) -> Response<Body> {
    not_found()
}

fn public_v1_router(state: AuthState, api: ApiState) -> Router {
    Router::new()
        .route(
            "/config/pools",
            any(config::pools_handler).with_state(api.clone()),
        )
        .route(
            "/config/weights",
            any(config::weights_handler).with_state(api.clone()),
        )
        .route(
            "/config/constraints",
            any(config::constraints_handler).with_state(api.clone()),
        )
        .route(
            "/policies",
            any(policies::policies_handler).with_state(api.clone()),
        )
        .route(
            "/recommend",
            any(recommend::recommend_handler).with_state(api.clone()),
        )
        .route("/checks/{*path}", any(recommend::checks_handler))
        .route(
            "/models",
            any(models::models_handler).with_state(api.clone()),
        )
        .route(
            "/telemetry",
            any(telemetry::telemetry).with_state(api.clone()),
        )
        .route(
            "/history/models/{column_id}",
            any(history::model_handler).with_state(api.clone()),
        )
        .route(
            "/history/recommendations",
            any(history::recommendations_handler).with_state(api.clone()),
        )
        .route(
            "/history/recommendations/{id}",
            any(history::recommendation_handler).with_state(api.clone()),
        )
        .route(
            "/history/outcomes",
            any(history::outcomes_handler).with_state(api.clone()),
        )
        .route(
            "/history/calibration",
            any(history::calibration_handler).with_state(api.clone()),
        )
        .route(
            "/debug/capture-windows",
            any(debug::windows_handler).with_state(api.clone()),
        )
        .route(
            "/debug/capture-windows/{id}",
            any(debug::window_handler).with_state(api.clone()),
        )
        .route(
            "/debug/replay/{capture_id}",
            any(debug::replay_handler).with_state(api.clone()),
        )
        .route(
            "/debug/captures",
            any(debug::captures_handler).with_state(api.clone()),
        )
        .route(
            "/debug/captures/{id}/prompt",
            any(debug::prompt_handler).with_state(api),
        )
        .fallback(route_stub)
        .layer(middleware::from_fn(body_limit))
        .layer(middleware::from_fn_with_state(state.clone(), rate_limit))
        .layer(middleware::from_fn_with_state(state, public_auth))
}

fn internal_v1_router(state: AuthState, api: ApiState) -> Router {
    Router::new()
        .route("/tenants", any(internal::tenants))
        .route("/tenants/{id}", any(internal::tenant))
        .route("/tenants/{id}/suspend", any(internal::suspend))
        .route("/tenants/{id}/activate", any(internal::activate))
        .route("/tenants/{id}/api-keys", any(internal::issue_key))
        .route("/api-keys/{id}", any(internal::revoke_key))
        .route("/tenants/{id}/pools", any(internal::pools))
        .route("/tenants/{id}/weights", any(internal::weights))
        .route("/tenants/{id}/constraints", any(internal::constraints))
        .route("/tenants/{id}/limits", any(internal::limits))
        .route("/alerts", any(internal::alerts))
        .route("/columns", any(internal::create_column))
        .route("/columns/{id}", any(internal::update_column))
        .route("/models/refresh", any(internal::refresh::models_refresh))
        .route("/corpus/activate", any(internal::activate_corpus))
        .route("/captures", any(debug::operator_captures_handler))
        .route("/captures/{id}/prompt", any(debug::operator_prompt_handler))
        .fallback(route_stub)
        .with_state(api)
        .layer(middleware::from_fn(body_limit))
        .layer(middleware::from_fn_with_state(state, internal_auth))
}

fn with_outer_middleware(router: Router) -> Router {
    // Axum applies the last layer first. This produces VersionHeaders -> TraceContext ->
    // RequestID -> Logger -> Recoverer -> router: the engine contract, inside the request
    // span, and the D50 version headers on every response.
    router
        .layer(middleware::from_fn(recoverer))
        .layer(middleware::from_fn(request_logger))
        .layer(middleware::from_fn(request_id))
        .layer(middleware::from_fn(observe::trace_context))
        .layer(middleware::from_fn(version::version_headers))
}

/// Build the public listener router.
pub fn public_router() -> Router {
    public_router_with_state(default_auth_state())
}

/// Build the public listener router with explicit auth state and no database
/// (router tests).
pub fn public_router_with_state(state: AuthState) -> Router {
    public_router_with(state, ApiState::detached())
}

/// Build the public listener router with explicit auth and endpoint state.
pub fn public_router_with(state: AuthState, api: ApiState) -> Router {
    let up = Router::new()
        .route("/up", any(health::up))
        .with_state(api.health.clone());
    let mcp = Router::new()
        .route("/", any(mcp::mcp_handler))
        .fallback(route_stub)
        .with_state(api.clone())
        .layer(middleware::from_fn(body_limit))
        .layer(middleware::from_fn_with_state(state.clone(), rate_limit))
        .layer(middleware::from_fn_with_state(state.clone(), public_auth));

    with_outer_middleware(
        Router::new()
            .merge(up)
            .nest("/mcp", mcp)
            .nest("/v1", public_v1_router(state, api))
            .fallback(route_stub),
    )
}

/// Alias used by binaries that call the public listener an application.
pub fn public_app() -> Router {
    public_router()
}

/// Build the internal listener router.
pub fn internal_router() -> Router {
    internal_router_with_state(default_auth_state())
}

/// Build the internal listener router with explicit auth state and no database
/// (router tests).
pub fn internal_router_with_state(state: AuthState) -> Router {
    internal_router_with(state, ApiState::detached())
}

/// Build the internal listener router with explicit auth and endpoint state.
pub fn internal_router_with(state: AuthState, api: ApiState) -> Router {
    let health = Router::new()
        .route("/", any(health::detail))
        .with_state(api.health.clone())
        .layer(middleware::from_fn_with_state(state.clone(), internal_auth));

    with_outer_middleware(
        Router::new()
            .nest("/health/detail", health)
            .nest("/internal/v1", internal_v1_router(state, api))
            .fallback(route_stub),
    )
}

/// Alias used by binaries that call the internal listener an application.
pub fn internal_app() -> Router {
    internal_router()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        extract::Extension,
        http::{Method, Request},
    };
    use http_body_util::BodyExt;
    use serde::Deserialize;
    use std::sync::{
        Arc,
        atomic::{AtomicI64, AtomicUsize, Ordering},
    };
    use tower::ServiceExt;

    #[allow(dead_code)]
    #[derive(Debug, Deserialize)]
    struct CompileCheck {
        value: String,
    }

    async fn body(response: Response<Body>) -> Vec<u8> {
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec()
    }

    async fn ok_handler() -> Response<Body> {
        response_with_json(StatusCode::OK, br#"{"ok":true}"#.to_vec())
    }

    async fn identity_handler(Extension(identity): Extension<Identity>) -> Response<Body> {
        response_with_json(
            StatusCode::OK,
            format!(
                r#"{{"tenant_id":"{}","api_key_id":"{}"}}"#,
                identity.tenant_id, identity.api_key_id
            )
            .into_bytes(),
        )
    }

    async fn actor_handler(Extension(actor): Extension<InternalActor>) -> Response<Body> {
        response_with_json(
            StatusCode::OK,
            format!(r#"{{"actor":"{}"}}"#, actor.0).into_bytes(),
        )
    }

    #[derive(Clone)]
    enum BackendReply {
        Decision(PublicAuthDecision),
        StoreError,
    }

    #[derive(Clone)]
    struct TestAuthBackend {
        calls: Arc<AtomicUsize>,
        reply: BackendReply,
    }

    impl TestAuthBackend {
        fn new(reply: BackendReply) -> Arc<Self> {
            Arc::new(Self {
                calls: Arc::new(AtomicUsize::new(0)),
                reply,
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl PublicAuthBackend for TestAuthBackend {
        fn authenticate<'a>(
            &'a self,
            _token: &'a str,
        ) -> BoxFuture<'a, Result<PublicAuthDecision, AuthBackendError>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                match &self.reply {
                    BackendReply::Decision(decision) => Ok(decision.clone()),
                    BackendReply::StoreError => Err(AuthBackendError::Store(
                        sqlx::Error::Protocol("db down".to_owned()),
                    )),
                }
            })
        }
    }

    #[derive(Debug)]
    struct FixedClock {
        now: AtomicI64,
    }

    impl FixedClock {
        fn new(now: i64) -> Arc<Self> {
            Arc::new(Self {
                now: AtomicI64::new(now),
            })
        }

        fn set(&self, now: i64) {
            self.now.store(now, Ordering::SeqCst);
        }
    }

    impl Clock for FixedClock {
        fn unix_seconds(&self) -> i64 {
            self.now.load(Ordering::SeqCst)
        }
    }

    fn tokens() -> Vec<InternalToken> {
        keys::parse_internal_tokens("ops:alpha,rails:beta").unwrap()
    }

    fn state_with_backend(backend: Arc<dyn PublicAuthBackend>, limiter: RateLimiter) -> AuthState {
        AuthState::new(backend, tokens(), limiter)
    }

    fn public_auth_test_app(state: AuthState) -> Router {
        Router::new()
            .route("/", any(identity_handler))
            .layer(middleware::from_fn_with_state(state, public_auth))
    }

    fn public_chain_test_app(state: AuthState) -> Router {
        Router::new()
            .route("/", any(ok_handler))
            .layer(middleware::from_fn(body_limit))
            .layer(middleware::from_fn_with_state(state.clone(), rate_limit))
            .layer(middleware::from_fn_with_state(state, public_auth))
    }

    fn internal_auth_test_app(state: AuthState) -> Router {
        Router::new()
            .route("/", any(actor_handler))
            .layer(middleware::from_fn_with_state(state, internal_auth))
    }

    #[tokio::test]
    async fn error_envelope_is_byte_exact_and_details_are_sorted() {
        let response = ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::ModelNotInPool,
            "allowed_models entry 'gpt-5.5' is not in pool 'vertex-only'",
        )
        .detail("pool", "vertex-only")
        .detail("entry", "gpt-5.5")
        .into_response();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
        assert_eq!(
            body(response).await,
            br#"{"error":{"code":"model_not_in_pool","message":"allowed_models entry 'gpt-5.5' is not in pool 'vertex-only'","details":{"entry":"gpt-5.5","pool":"vertex-only"}}}
"#
        );
    }

    #[test]
    fn error_codes_match_appendix_b_order() {
        assert_eq!(
            GO_ERROR_CODES,
            [
                "unauthorized",
                "tenant_suspended",
                "not_found",
                "zero_retention_conflict",
                "capture_expired",
                "payload_too_large",
                "invalid_request",
                "unknown_model",
                "model_not_in_pool",
                "unknown_pool",
                "pool_would_be_empty",
                "grader_unsupported",
                "rate_limited",
                "internal_error",
            ]
        );
    }

    #[test]
    fn error_codes_include_current_privacy_failures() {
        assert!(ERROR_CODES.contains(&"no_compliant_columns"));
        assert!(ERROR_CODES.contains(&"zero_retention_requires_declared"));
        assert_eq!(
            ErrorCode::NoCompliantColumns.as_str(),
            "no_compliant_columns"
        );
        assert_eq!(
            ErrorCode::ZeroRetentionRequiresDeclared.as_str(),
            "zero_retention_requires_declared"
        );
    }

    #[tokio::test]
    async fn error_envelope_uses_go_html_escaping() {
        let response = ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::InvalidRequest,
            "<unsafe>&",
        )
        .into_response();
        let body = String::from_utf8(body(response).await).unwrap();
        assert!(body.contains(r#"\u003cunsafe\u003e\u0026"#));
    }

    #[tokio::test]
    async fn public_auth_rejects_malformed_authorization_without_backend_lookup() {
        let backend = TestAuthBackend::new(BackendReply::Decision(
            PublicAuthDecision::Authenticated(Identity {
                tenant_id: "ten_1".to_owned(),
                api_key_id: "key_1".to_owned(),
            }),
        ));
        let state = state_with_backend(backend.clone(), RateLimiter::new(300));
        let app = public_auth_test_app(state);

        for authorization in [
            None,
            Some("Bearer"),
            Some("Bearer "),
            Some("Basic bWs6bGl2ZQ=="),
            Some("bearer mk_live_abc"),
            Some("Bearer sk_live_wrongprefix"),
            Some("mk_live_abcnoscheme"),
        ] {
            let mut builder = Request::builder().uri("/");
            if let Some(authorization) = authorization {
                builder = builder.header(header::AUTHORIZATION, authorization);
            }
            let response = app
                .clone()
                .oneshot(builder.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            assert_eq!(body(response).await, body(unauthorized()).await);
        }

        assert_eq!(backend.calls(), 0);
    }

    #[tokio::test]
    async fn public_auth_maps_backend_decisions_and_attaches_identity() {
        for (reply, expected) in [
            (
                BackendReply::Decision(PublicAuthDecision::Missing),
                StatusCode::UNAUTHORIZED,
            ),
            (
                BackendReply::Decision(PublicAuthDecision::Suspended),
                StatusCode::FORBIDDEN,
            ),
            (BackendReply::StoreError, StatusCode::INTERNAL_SERVER_ERROR),
        ] {
            let backend = TestAuthBackend::new(reply);
            let state = state_with_backend(backend.clone(), RateLimiter::new(300));
            let response = public_auth_test_app(state)
                .oneshot(
                    Request::builder()
                        .uri("/")
                        .header(
                            header::AUTHORIZATION,
                            "Bearer mk_live_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                        )
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
            assert_eq!(backend.calls(), 1);
        }

        let backend = TestAuthBackend::new(BackendReply::Decision(
            PublicAuthDecision::Authenticated(Identity {
                tenant_id: "ten_1".to_owned(),
                api_key_id: "key_1".to_owned(),
            }),
        ));
        let state = state_with_backend(backend.clone(), RateLimiter::new(300));
        let response = public_auth_test_app(state)
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(
                        header::AUTHORIZATION,
                        "Bearer mk_live_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            body(response).await,
            br#"{"tenant_id":"ten_1","api_key_id":"key_1"}"#.to_vec()
        );
        assert_eq!(backend.calls(), 1);
    }

    #[tokio::test]
    async fn internal_auth_accepts_rotation_tokens_and_records_actor() {
        let state = AuthState::new(
            Arc::new(StaticAuthBackend::missing()),
            tokens(),
            RateLimiter::new(300),
        );
        let app = internal_auth_test_app(state);

        for authorization in [None, Some("Bearer ops"), Some("Bearer ops:beta")] {
            let mut builder = Request::builder().uri("/");
            if let Some(authorization) = authorization {
                builder = builder.header(header::AUTHORIZATION, authorization);
            }
            let response = app
                .clone()
                .oneshot(builder.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            assert_eq!(body(response).await, body(internal_unauthorized()).await);
        }

        for (authorization, expected_actor) in
            [("Bearer ops:alpha", "ops"), ("Bearer rails:beta", "rails")]
        {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/")
                        .header(header::AUTHORIZATION, authorization)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                body(response).await,
                format!(r#"{{"actor":"{expected_actor}"}}"#).into_bytes()
            );
        }
    }

    #[test]
    fn rate_limiter_uses_aligned_fixed_windows_and_isolates_keys() {
        let clock = FixedClock::new(1_783_339_230);
        let limiter = RateLimiter::with_clock(2, clock.clone());

        assert_eq!(
            limiter.check("key_1"),
            RateLimitDecision {
                allowed: true,
                limit: 2,
                remaining: 1,
                reset_at: 1_783_339_260,
                retry_after: 30,
            }
        );
        assert_eq!(limiter.check("key_1").remaining, 0);
        let rejected = limiter.check("key_1");
        assert!(!rejected.allowed);
        assert_eq!(rejected.remaining, 0);
        assert_eq!(rejected.retry_after, 30);

        assert!(limiter.check("key_2").allowed);

        clock.set(1_783_339_261);
        let next_window = limiter.check("key_1");
        assert!(next_window.allowed);
        assert_eq!(next_window.remaining, 1);
        assert_eq!(next_window.reset_at, 1_783_339_320);
    }

    #[tokio::test]
    async fn rate_limit_middleware_sets_headers_and_rejects_over_limit() {
        let clock = FixedClock::new(1_783_339_230);
        let state = AuthState::new(
            Arc::new(StaticAuthBackend::authenticated(Identity {
                tenant_id: "ten_1".to_owned(),
                api_key_id: "key_1".to_owned(),
            })),
            tokens(),
            RateLimiter::with_clock(1, clock),
        );
        let app = public_chain_test_app(state);

        let first = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(header::AUTHORIZATION, "Bearer mk_live_valid")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(first.headers()["X-RateLimit-Limit"], "1");
        assert_eq!(first.headers()["X-RateLimit-Remaining"], "0");
        assert_eq!(first.headers()["X-RateLimit-Reset"], "1783339260");

        let second = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(header::AUTHORIZATION, "Bearer mk_live_valid")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(second.headers()[header::RETRY_AFTER], "30");
        assert_eq!(second.headers()["X-RateLimit-Limit"], "1");
        assert_eq!(second.headers()["X-RateLimit-Remaining"], "0");
        assert_eq!(second.headers()["X-RateLimit-Reset"], "1783339260");
        assert_eq!(body(second).await, body(rate_limited(1)).await);
    }

    #[tokio::test]
    async fn public_routing_and_request_id_follow_contract() {
        let response = public_router()
            .oneshot(Request::builder().uri("/nope").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(response.headers().get("X-Request-ID").is_some());

        let response = public_router()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/up")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(body(response).await, body(not_found()).await);
    }

    #[tokio::test]
    async fn public_v1_auth_precedes_unknown_route_and_body_cap() {
        let response = public_router()
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/nope")
                    .body(Body::from(vec![b'a'; BODY_LIMIT + 1]))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let response = public_router()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/recommend")
                    .header(header::AUTHORIZATION, "Bearer mk_live_test")
                    .header(header::CONTENT_LENGTH, BODY_LIMIT + 1)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            body(response).await,
            body(payload_too_large(BODY_LIMIT)).await
        );
    }

    #[tokio::test]
    async fn decode_json_rejects_empty_and_trailing_content() {
        let empty = Request::new(Body::from("  \n"));
        assert_eq!(
            decode_json::<CompileCheck>(empty)
                .await
                .unwrap_err()
                .status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        let trailing = Request::new(Body::from(r#"{"value":"ok"} {"value":"again"}"#));
        let response = decode_json::<CompileCheck>(trailing).await.unwrap_err();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            String::from_utf8(body(response).await)
                .unwrap()
                .contains("trailing content")
        );
    }

    #[tokio::test]
    async fn body_limit_maps_chunked_reads() {
        let response = read_body(Body::from(vec![b'a'; BODY_LIMIT + 1]))
            .await
            .unwrap_err();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn body_limit_maps_reads_behind_the_middleware() {
        // `body_limit` wraps the body once and `read_body` again (model-maverick 5.3).
        let body = Body::new(Limited::new(
            Body::from(vec![b'a'; BODY_LIMIT + 1]),
            BODY_LIMIT,
        ));
        let response = read_body(body).await.unwrap_err();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }
}
