//! Routing, the request pipeline, and the HTTP server (reference part 6 §2 to §4).
//!
//! # Routing
//!
//! `GET /up` (also `HEAD`), `POST /v1/chat/completions`, `POST /v1/messages`. A wrong method
//! on a known path is Go's 405 with `Allow`; any other path is Go's 404. Paths match exactly
//! and the query string never affects routing.
//!
//! # Pipeline (the first failing step decides)
//!
//! 0. Hosted mode only (D13): the tenant key from `X-Maverick-API-Key` (it must start with
//!    `mk_live_`), else `401 unauthorized`. Then the per-tenant rate limit (D28): over the
//!    limit is `429 rate_limited`, and neither the engine nor a provider is called.
//! 1. Read the body. Over the D38 cap (declared `Content-Length`, or bytes read): `413
//!    payload_too_large`. Read error: `400 invalid_request`.
//! 2. Decode `{model: string}` with Go's rules: `400 invalid_request` on error.
//! 3. Parse the alias: `400 invalid_model_alias` on any error. The engine is not called.
//! 4. Choose the workload (D5): a malformed `X-Maverick-Workload` is `400 invalid_request`.
//! 5. Recommend. Terminal privacy errors are relayed as `422`/`409` with their code (D17).
//!    Hosted mode maps an engine 401 to `401 unauthorized` and relays a 403 with its code
//!    (D13). Any other failure is `502 engine_error` and one ERROR log line.
//! 6. Hand the chain to the [`ChainRunner`].
//!
//! Logs never carry a body, a prompt, or a credential.

use std::future::Future;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::header::CONTENT_LENGTH;
use axum::http::{HeaderMap, HeaderName, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use http_body_util::{BodyExt, LengthLimitError, Limited};
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use hyper_util::service::TowerToHyperService;
use maverick_core::alias::parse_alias;
use maverick_core::gojson::{apply_string, type_error};
use maverick_core::recommend::{
    EngineFailure, RecommendClient, RecommendClientConfig, RecommendError, RecommendRequest,
    TenantKey, TraceHeaders, Workload,
};
use serde_json::Value;
use tokio::net::TcpListener;

use crate::chain::{ChainRequest, ChainRunner, Surface};
use crate::config::{Config, DeploymentMode};
use crate::credentials::{self, CallerCredential, EnvKeys};
use crate::envelope;
use crate::ratelimit::{self, RateLimiter};

/// The engine client's total timeout per call (Go constant).
pub const ENGINE_TIMEOUT: Duration = Duration::from_secs(10);

/// The inbound declared-workload override header (D5).
pub const WORKLOAD_HEADER: HeaderName = HeaderName::from_static("x-maverick-workload");

/// The hosted-mode tenant key header (D13).
pub const TENANT_KEY_HEADER: HeaderName = HeaderName::from_static("x-maverick-api-key");

/// Every Maverick API key starts with this prefix (spec §9.1).
pub const TENANT_KEY_PREFIX: &str = "mk_live_";

/// The proxy: configuration, the engine client, the chain runner, and the rate limiter.
pub struct Proxy {
    config: Config,
    engine: RecommendClient,
    chain: Arc<dyn ChainRunner>,
    limiter: RateLimiter,
}

impl Proxy {
    /// A proxy whose engine client calls `config.engine_url` with the Go 10 s timeout.
    pub fn new(config: Config, chain: Arc<dyn ChainRunner>) -> Result<Self, reqwest::Error> {
        let engine = RecommendClient::new(RecommendClientConfig {
            base_url: config.engine_url.clone(),
            timeout: ENGINE_TIMEOUT,
            max_retries: 0,
        })?;
        Ok(Self::with_engine(config, engine, chain))
    }

    /// A proxy on an existing engine client, limited to `config.ratelimit_rpm` requests per
    /// minute per tenant key.
    #[must_use]
    pub fn with_engine(
        config: Config,
        engine: RecommendClient,
        chain: Arc<dyn ChainRunner>,
    ) -> Self {
        let limiter = RateLimiter::new(config.ratelimit_rpm);
        Self {
            config,
            engine,
            chain,
            limiter,
        }
    }

    /// The same proxy with another rate limiter (tests pass one on a fixed clock).
    #[must_use]
    pub fn with_limiter(mut self, limiter: RateLimiter) -> Self {
        self.limiter = limiter;
        self
    }

    /// The router for this proxy.
    pub fn router(self) -> Router {
        let proxy = Arc::new(self);
        Router::new()
            .route(
                "/up",
                get(|| async { envelope::up() })
                    .fallback(|| async { envelope::method_not_allowed("GET, HEAD") }),
            )
            .route(
                "/v1/chat/completions",
                post(chat).fallback(|| async { envelope::method_not_allowed("POST") }),
            )
            .route(
                "/v1/messages",
                post(messages).fallback(|| async { envelope::method_not_allowed("POST") }),
            )
            .fallback(|| async { envelope::not_found() })
            .with_state(proxy)
    }

    async fn handle(&self, surface: Surface, request: Request) -> Response {
        let received_at = Instant::now();
        let (parts, body) = request.into_parts();

        // 0. Hosted mode: the tenant key arrives per request (D13). Checked before the body
        //    is read so an unauthenticated caller cannot make the proxy buffer a body.
        let tenant_key = match self.config.mode {
            DeploymentMode::SelfHosted => TenantKey::new(self.config.api_key.clone()),
            DeploymentMode::Hosted => match hosted_tenant_key(&parts.headers) {
                Some(key) => key,
                None => {
                    return envelope::error(
                        StatusCode::UNAUTHORIZED,
                        envelope::UNAUTHORIZED,
                        envelope::UNAUTHORIZED_MESSAGE,
                    );
                }
            },
        };
        let decision = self.limiter.check(tenant_key.expose());
        if !decision.allowed {
            tracing::debug!(
                limit = decision.limit,
                retry_after = decision.retry_after,
                "maverick-proxy: rate limited"
            );
            return ratelimit::rejection(&decision);
        }

        // 1. Body, with the D38 cap on the declared length and on the bytes read.
        let cap = self.config.max_body_bytes;
        if declared_length(&parts.headers).is_some_and(|length| length > cap) {
            return self.too_large();
        }
        let body = match read_body(body, cap).await {
            Ok(body) => body,
            Err(BodyError::TooLarge) => return self.too_large(),
            Err(BodyError::Read) => {
                return envelope::error(
                    StatusCode::BAD_REQUEST,
                    envelope::INVALID_REQUEST,
                    envelope::READ_BODY_FAILED,
                );
            }
        };

        // 2. `{model: string}`.
        let Ok(model) = decode_model(&body) else {
            return envelope::error(
                StatusCode::BAD_REQUEST,
                envelope::INVALID_REQUEST,
                envelope::MALFORMED_JSON,
            );
        };

        // 3. Alias.
        let Ok(alias) = parse_alias(&model) else {
            return envelope::error(
                StatusCode::BAD_REQUEST,
                envelope::INVALID_MODEL_ALIAS,
                envelope::INVALID_MODEL_ALIAS_MESSAGE,
            );
        };

        // 4. Workload (D5).
        let workload = workload_header(&parts.headers)
            .and_then(|header| Workload::for_chat_request(&body, header).map_err(|_| ()));
        let Ok(workload) = workload else {
            return envelope::error(
                StatusCode::BAD_REQUEST,
                envelope::INVALID_REQUEST,
                envelope::INVALID_WORKLOAD_HEADER_MESSAGE,
            );
        };

        // 5. Recommend.
        let request = RecommendRequest::for_alias(&alias, workload);
        // D31: the caller's W3C trace context continues into the engine. Providers get it
        // too: tracing headers are among the inbound headers Go forwards upstream.
        let trace = TraceHeaders {
            traceparent: first_header(&parts.headers, "traceparent"),
            tracestate: first_header(&parts.headers, "tracestate"),
        };
        let recommendation = match self
            .engine
            .recommend_traced(&tenant_key, &request, &trace)
            .await
        {
            Ok(recommendation) => recommendation,
            Err(RecommendError::ZeroRetentionRequiresDeclared { message, .. }) => {
                return relay_terminal(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    envelope::ZERO_RETENTION_REQUIRES_DECLARED,
                    &message,
                );
            }
            Err(RecommendError::NoCompliantColumns { message, .. }) => {
                return relay_terminal(
                    StatusCode::CONFLICT,
                    envelope::NO_COMPLIANT_COLUMNS,
                    &message,
                );
            }
            Err(RecommendError::Engine(EngineFailure::Status { status: 401, .. }))
                if self.config.mode == DeploymentMode::Hosted =>
            {
                // D13: the engine rejected this tenant's key.
                return envelope::error(
                    StatusCode::UNAUTHORIZED,
                    envelope::UNAUTHORIZED,
                    envelope::UNAUTHORIZED_MESSAGE,
                );
            }
            Err(RecommendError::Engine(EngineFailure::Status { status: 403, code }))
                if self.config.mode == DeploymentMode::Hosted =>
            {
                // D13: a suspended tenant's 403 is relayed with the engine's code.
                let code = code.as_deref().unwrap_or(envelope::TENANT_SUSPENDED);
                let message = if code == envelope::TENANT_SUSPENDED {
                    envelope::TENANT_SUSPENDED_MESSAGE
                } else {
                    code
                };
                return envelope::error(StatusCode::FORBIDDEN, code, message);
            }
            Err(RecommendError::Engine(failure)) => {
                tracing::error!(error = %failure, "maverick-proxy: engine recommend failed");
                return envelope::error(
                    StatusCode::BAD_GATEWAY,
                    envelope::ENGINE_ERROR,
                    envelope::ENGINE_ERROR_MESSAGE,
                );
            }
        };

        // 6. Chain walk, with the caller's credential and the upstream base headers (D12, D30).
        let credential = CallerCredential::from_headers(surface, &parts.headers, self.config.mode);
        let upstream_headers = credentials::upstream_base_headers(&parts.headers);
        self.chain
            .run(ChainRequest {
                surface,
                credential,
                upstream_headers,
                env_keys: EnvKeys::from_config(&self.config),
                headers: parts.headers,
                body,
                raw_query: parts.uri.query().unwrap_or_default().to_owned(),
                alias,
                recommendation,
                tenant_key,
                received_at,
            })
            .await
    }

    fn too_large(&self) -> Response {
        envelope::error(
            StatusCode::PAYLOAD_TOO_LARGE,
            envelope::PAYLOAD_TOO_LARGE,
            &format!(
                "request body is larger than {} bytes",
                self.config.max_body_bytes
            ),
        )
    }
}

async fn chat(State(proxy): State<Arc<Proxy>>, request: Request) -> Response {
    proxy.handle(Surface::Chat, request).await
}

async fn messages(State(proxy): State<Arc<Proxy>>, request: Request) -> Response {
    proxy.handle(Surface::Messages, request).await
}

/// The hosted tenant key (D13): the first `X-Maverick-API-Key` value, when it is a text
/// header that starts with [`TENANT_KEY_PREFIX`]. This mirrors the engine's own parse, so
/// the proxy never rejects a key the engine would accept.
fn hosted_tenant_key(headers: &HeaderMap) -> Option<TenantKey> {
    let value = headers.get(TENANT_KEY_HEADER)?.to_str().ok()?;
    value
        .starts_with(TENANT_KEY_PREFIX)
        .then(|| TenantKey::new(value))
}

/// The first `X-Maverick-Workload` value: `Ok(None)` when absent, `Err` when it is not text.
fn workload_header(headers: &HeaderMap) -> Result<Option<&str>, ()> {
    headers
        .get(WORKLOAD_HEADER)
        .map(|value| value.to_str().map_err(|_| ()))
        .transpose()
}

/// The first value of a text header.
fn first_header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get(name)?.to_str().ok().map(str::to_owned)
}

/// The declared `Content-Length`, when present and numeric.
fn declared_length(headers: &HeaderMap) -> Option<u64> {
    headers.get(CONTENT_LENGTH)?.to_str().ok()?.parse().ok()
}

enum BodyError {
    TooLarge,
    Read,
}

/// Read the whole body, failing with [`BodyError::TooLarge`] past `cap` bytes (chunked
/// bodies are cut off at the cap).
async fn read_body(body: Body, cap: u64) -> Result<Bytes, BodyError> {
    let limit = usize::try_from(cap).unwrap_or(usize::MAX);
    match Limited::new(body, limit).collect().await {
        Ok(collected) => Ok(collected.to_bytes()),
        Err(error) if error.downcast_ref::<LengthLimitError>().is_some() => {
            Err(BodyError::TooLarge)
        }
        Err(_) => Err(BodyError::Read),
    }
}

/// Decode the body into Go's `struct{ Model string }`. Invalid UTF-8 becomes U+FFFD first,
/// as Go's decoder does; `null` decodes to model "".
fn decode_model(body: &[u8]) -> Result<String, String> {
    let text = String::from_utf8_lossy(body);
    let value: Value = serde_json::from_str(&text).map_err(|error| error.to_string())?;
    let mut model = String::new();
    match &value {
        Value::Null => {}
        Value::Object(object) => apply_string(object, "model", &mut model)?,
        other => return Err(type_error("request body", other, "object")),
    }
    Ok(model)
}

/// A terminal privacy error from the engine, relayed with its status and code (D17). The
/// hosted proxy answers `409 no_compliant_columns` (openrouter spec §3.2).
fn relay_terminal(status: StatusCode, code: &str, message: &str) -> Response {
    tracing::warn!(
        code,
        "maverick-proxy: engine refused the request for privacy"
    );
    let message = if message.is_empty() { code } else { message };
    envelope::error(status, code, message)
}

/// Server settings (reference part 6 §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServeSettings {
    /// How long a client may take to send the request head (Go `ReadHeaderTimeout`).
    pub header_read_timeout: Duration,
    /// How long in-flight requests may run after shutdown starts.
    pub shutdown_grace: Duration,
}

impl Default for ServeSettings {
    fn default() -> Self {
        Self {
            header_read_timeout: Duration::from_secs(10),
            shutdown_grace: Duration::from_secs(10),
        }
    }
}

/// Go's default maximum header size, 1 MiB, plus the 4 KiB slop Go's server allows.
const MAX_HEADER_BYTES: usize = (1 << 20) + 4096;

/// Serve HTTP/1.1 on `listener` until `shutdown` completes, then drain in-flight requests
/// for up to `settings.shutdown_grace`. Returns an error when the drain times out.
pub async fn serve(
    listener: TcpListener,
    app: Router,
    settings: ServeSettings,
    shutdown: impl Future<Output = ()>,
) -> io::Result<()> {
    let mut http = hyper::server::conn::http1::Builder::new();
    http.timer(TokioTimer::new())
        .header_read_timeout(settings.header_read_timeout)
        .max_buf_size(MAX_HEADER_BYTES);
    let graceful = GracefulShutdown::new();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let stream = match accepted {
                    Ok((stream, _)) => stream,
                    Err(error) => {
                        tracing::warn!(error = %error, "maverick-proxy: accept failed");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };
                let service = TowerToHyperService::new(app.clone());
                let connection = graceful.watch(http.serve_connection(TokioIo::new(stream), service));
                tokio::spawn(async move {
                    let _ = connection.await;
                });
            }
            () = &mut shutdown => break,
        }
    }
    drop(listener);
    tokio::time::timeout(settings.shutdown_grace, graceful.shutdown())
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "graceful shutdown timed out"))
}

/// Resolves on SIGINT or SIGTERM.
pub async fn shutdown_signal() {
    let interrupt = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = interrupt => {}
        () = terminate => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_decoding_follows_go() {
        assert_eq!(
            decode_model(br#"{"model":"maverick/auto"}"#),
            Ok("maverick/auto".to_owned())
        );
        assert_eq!(
            decode_model(br#"{"MODEL":"maverick/cost","x":[1]}"#),
            Ok("maverick/cost".to_owned())
        );
        assert_eq!(decode_model(b"null"), Ok(String::new()));
        assert_eq!(decode_model(br#"{"model":null}"#), Ok(String::new()));
        assert_eq!(decode_model(br#"{"messages":[]}"#), Ok(String::new()));
        assert_eq!(
            decode_model(b"{\"model\":\"m\",\"x\":\"\xff\"}"),
            Ok("m".to_owned())
        );
        for bad in [
            &b""[..],
            b"not json",
            b"[]",
            b"\"m\"",
            br#"{"model":5}"#,
            b"{} x",
        ] {
            assert!(
                decode_model(bad).is_err(),
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
    }
}
