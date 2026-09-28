//! The HTTP API and MCP surface.
//!
//! This crate owns the wire contract and middleware. Endpoint handlers are kept
//! deliberately small: business rules belong to the service crates.

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::Request,
    http::{HeaderValue, Method, Response, StatusCode, header},
    middleware::{self, Next},
    routing::any,
};
use futures_util::FutureExt;
use http_body_util::{LengthLimitError, Limited};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{collections::BTreeMap, error::Error, panic::AssertUnwindSafe, time::Instant};
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

pub fn unauthorized() -> Response<Body> {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        ErrorCode::Unauthorized,
        "missing or invalid API key",
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

fn is_length_limit_error(error: &axum::Error) -> bool {
    error
        .source()
        .is_some_and(|source| source.downcast_ref::<LengthLimitError>().is_some())
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

/// Privacy-safe request logger. It records only method, path, status, timing,
/// and request ID. Request bodies and authorization headers are never fields.
pub async fn request_logger(request: Request, next: Next) -> Response<Body> {
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .map(|id| id.0.as_str())
        .unwrap_or("")
        .to_owned();
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let started = Instant::now();
    let response = next.run(request).await;
    tracing::info!(
        request_id = %request_id,
        method = %method,
        path = %path,
        status = response.status().as_u16(),
        ms = started.elapsed().as_millis() as u64,
        "request"
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

async fn public_auth(request: Request, next: Next) -> Response<Body> {
    let valid = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("Bearer mk_live_") && value.len() > 15);
    if valid {
        next.run(request).await
    } else {
        unauthorized()
    }
}

async fn internal_auth(request: Request, next: Next) -> Response<Body> {
    let valid = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .strip_prefix("Bearer ")
                .is_some_and(|token| !token.is_empty())
        });
    if valid {
        next.run(request).await
    } else {
        internal_unauthorized()
    }
}

async fn up(request: Request) -> Response<Body> {
    if request.method() != Method::GET {
        return not_found();
    }
    response_with_json(StatusCode::OK, br#"{"status":"ok"}"#.to_vec())
}

async fn route_stub(_: Request) -> Response<Body> {
    not_found()
}

fn public_v1_router() -> Router {
    Router::new()
        .route("/config/pools", any(route_stub))
        .route("/config/weights", any(route_stub))
        .route("/config/constraints", any(route_stub))
        .route("/policies", any(route_stub))
        .route("/recommend", any(route_stub))
        .route("/checks/{*path}", any(route_stub))
        .route("/models", any(route_stub))
        .route("/telemetry", any(route_stub))
        .route("/history/models/{column_id}", any(route_stub))
        .route("/history/recommendations", any(route_stub))
        .route("/history/recommendations/{id}", any(route_stub))
        .route("/history/outcomes", any(route_stub))
        .route("/history/calibration", any(route_stub))
        .route("/debug/capture-windows", any(route_stub))
        .route("/debug/capture-windows/{id}", any(route_stub))
        .route("/debug/replay/{capture_id}", any(route_stub))
        .route("/debug/captures", any(route_stub))
        .route("/debug/captures/{id}/prompt", any(route_stub))
        .fallback(route_stub)
        .layer(middleware::from_fn(body_limit))
        .layer(middleware::from_fn(public_auth))
}

fn internal_v1_router() -> Router {
    Router::new()
        .route("/tenants", any(route_stub))
        .route("/tenants/{id}", any(route_stub))
        .route("/tenants/{id}/suspend", any(route_stub))
        .route("/tenants/{id}/activate", any(route_stub))
        .route("/tenants/{id}/api-keys", any(route_stub))
        .route("/api-keys/{id}", any(route_stub))
        .route("/tenants/{id}/pools", any(route_stub))
        .route("/tenants/{id}/weights", any(route_stub))
        .route("/tenants/{id}/constraints", any(route_stub))
        .route("/alerts", any(route_stub))
        .route("/columns", any(route_stub))
        .route("/columns/{id}", any(route_stub))
        .route("/corpus/activate", any(route_stub))
        .route("/captures", any(route_stub))
        .route("/captures/{id}/prompt", any(route_stub))
        .fallback(route_stub)
        .layer(middleware::from_fn(body_limit))
        .layer(middleware::from_fn(internal_auth))
}

fn with_outer_middleware(router: Router) -> Router {
    // Axum applies the last layer first. This produces RequestID -> Logger ->
    // Recoverer -> router, matching the engine contract.
    router
        .layer(middleware::from_fn(recoverer))
        .layer(middleware::from_fn(request_logger))
        .layer(middleware::from_fn(request_id))
}

/// Build the public listener router.
pub fn public_router() -> Router {
    let mcp = Router::new()
        .route("/", any(route_stub))
        .fallback(route_stub)
        .layer(middleware::from_fn(body_limit))
        .layer(middleware::from_fn(public_auth));

    with_outer_middleware(
        Router::new()
            .route("/up", any(up))
            .nest("/mcp", mcp)
            .nest("/v1", public_v1_router())
            .fallback(route_stub),
    )
}

/// Alias used by binaries that call the public listener an application.
pub fn public_app() -> Router {
    public_router()
}

/// Build the internal listener router.
pub fn internal_router() -> Router {
    let health = Router::new()
        .route("/", any(route_stub))
        .layer(middleware::from_fn(internal_auth));

    with_outer_middleware(
        Router::new()
            .nest("/health/detail", health)
            .nest("/internal/v1", internal_v1_router())
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
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use serde::Deserialize;
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
}
