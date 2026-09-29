//! The chain-walk seam (pipeline step 6).
//!
//! The shell hands every recommended chain to a [`ChainRunner`]. The real runner is
//! [`crate::surface::ExecutorRunner`]: the chat and messages surfaces on the
//! `maverick-core` chain executor. [`NoExecutor`] answers as Go did when no chain entry
//! could be executed; tests use it.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::Instant;

use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use maverick_core::alias::Alias;
use maverick_core::recommend::{Recommendation, TenantKey};

use crate::credentials::{CallerCredential, EnvKeys};
use crate::envelope;

/// A boxed, sendable future.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The inbound surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    /// `POST /v1/chat/completions` (OpenAI chat completions).
    Chat,
    /// `POST /v1/messages` (Anthropic Messages).
    Messages,
}

impl Surface {
    /// The surface's exhausted-chain message (reference part 6 §4).
    #[must_use]
    pub fn exhausted_message(self) -> &'static str {
        match self {
            Self::Chat => envelope::NO_EXECUTABLE_CANDIDATES_CHAT,
            Self::Messages => envelope::NO_EXECUTABLE_CANDIDATES_MESSAGES,
        }
    }
}

/// Everything the chain walk needs from one inbound request. `Debug` output shows no
/// header value, body byte, or key.
pub struct ChainRequest {
    /// The surface the caller used.
    pub surface: Surface,
    /// The caller's provider credential from the surface's native slot (D12). Pass it to
    /// [`crate::credentials::resolve_key`] for each entry.
    pub credential: CallerCredential,
    /// The upstream base headers (Go `buildUpstreamHeaders` plus D30). Add the credential
    /// per attempt with [`crate::credentials::attempt_headers`].
    pub upstream_headers: HeaderMap,
    /// The env keys the configuration loaded (none in hosted mode, D12).
    pub env_keys: EnvKeys,
    /// The inbound headers, including the caller's provider credential. Never log them.
    pub headers: HeaderMap,
    /// The inbound body, exactly as received.
    pub body: Bytes,
    /// The inbound raw query (without `?`), appended verbatim to every upstream URL when
    /// it is not empty.
    pub raw_query: String,
    /// The parsed alias; `settings_mode` says whether entry settings apply (D14).
    pub alias: Alias,
    /// The engine's recommendation.
    pub recommendation: Recommendation,
    /// The tenant key used for the recommend call; telemetry goes out under it (D27).
    pub tenant_key: TenantKey,
    /// When the proxy received the request.
    pub received_at: Instant,
}

impl fmt::Debug for ChainRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChainRequest")
            .field("surface", &self.surface)
            .field("credential", &self.credential)
            .field(
                "upstream_headers",
                &self.upstream_headers.keys().collect::<Vec<_>>(),
            )
            .field("env_keys", &self.env_keys)
            .field("headers", &self.headers.keys().collect::<Vec<_>>())
            .field("body", &format_args!("<{} bytes>", self.body.len()))
            .field(
                "raw_query",
                &format_args!("<{} bytes>", self.raw_query.len()),
            )
            .field("alias", &self.alias)
            .field("recommendation_id", &self.recommendation.recommendation_id)
            .field("chain_len", &self.recommendation.chain.len())
            .field("tenant_key", &self.tenant_key)
            .finish_non_exhaustive()
    }
}

/// Walks a recommended chain and produces the caller's response.
pub trait ChainRunner: Send + Sync + 'static {
    /// Run the chain for one request.
    fn run(&self, request: ChainRequest) -> BoxFuture<'_, Response>;
}

/// A runner that attempts nothing: every chain is treated as exhausted.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoExecutor;

impl ChainRunner for NoExecutor {
    fn run(&self, request: ChainRequest) -> BoxFuture<'_, Response> {
        Box::pin(
            async move { exhausted(request.surface, &request.recommendation.recommendation_id) },
        )
    }
}

/// `502 no_executable_candidates` with the surface's message, and
/// `X-Maverick-Recommendation-Id` when the engine's id is non-empty.
#[must_use]
pub fn exhausted(surface: Surface, recommendation_id: &str) -> Response {
    let mut response = envelope::error(
        StatusCode::BAD_GATEWAY,
        envelope::NO_EXECUTABLE_CANDIDATES,
        surface.exhausted_message(),
    );
    if !recommendation_id.is_empty() {
        if let Ok(value) = HeaderValue::from_str(recommendation_id) {
            response
                .headers_mut()
                .insert(envelope::RECOMMENDATION_ID_HEADER, value);
        }
    }
    response
}
