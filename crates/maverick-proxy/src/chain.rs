//! The chain-walk seam (pipeline step 6).
//!
//! The shell hands every recommended chain to a [`ChainRunner`]. The chain executor
//! (bead 6.2) and the chat and messages surfaces (bead 6.7) provide the real runner. Until
//! they land, [`NoExecutor`] answers as Go did when no chain entry could be executed.

use std::future::Future;
use std::pin::Pin;
use std::time::Instant;

use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use maverick_core::alias::Alias;
use maverick_core::recommend::{Recommendation, TenantKey};

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

/// Everything the chain walk needs from one inbound request.
#[derive(Debug)]
pub struct ChainRequest {
    /// The surface the caller used.
    pub surface: Surface,
    /// The inbound headers, including the caller's provider credential. Never log them.
    pub headers: HeaderMap,
    /// The inbound body, exactly as received.
    pub body: Bytes,
    /// The parsed alias; `settings_mode` says whether entry settings apply (D14).
    pub alias: Alias,
    /// The engine's recommendation.
    pub recommendation: Recommendation,
    /// The tenant key used for the recommend call; telemetry goes out under it (D27).
    pub tenant_key: TenantKey,
    /// When the proxy received the request.
    pub received_at: Instant,
}

/// Walks a recommended chain and produces the caller's response.
pub trait ChainRunner: Send + Sync + 'static {
    /// Run the chain for one request.
    fn run(&self, request: ChainRequest) -> BoxFuture<'_, Response>;
}

/// The placeholder runner: every chain is treated as exhausted. Replaced by beads 6.2 and
/// 6.7.
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
