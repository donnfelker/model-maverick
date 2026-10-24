//! Conversation affinity on the client side (D44; spec §8.1 "Conversation header", §9.2).
//!
//! A caller names its conversation, and the recommend request carries
//! `affinity {key, context_tokens, cost_aware}`. The engine keeps the last pick per key
//! and reports whether this turn kept it. The key is handled like prompt text (G5): it is
//! sent to the engine and nowhere else, and no `Debug` output shows it. The request field
//! is optional, so it is written only when the engine advertised the `affinity` feature
//! (spec §14.5, D50). Vectors: `crates/sdk-conformance/vectors/affinity.v1.json`.

use std::fmt;

/// The proxy's request header that names the conversation.
pub const CONVERSATION_HEADER: &str = "x-maverick-conversation";

/// The proxy's response header with the engine's affinity status.
pub const AFFINITY_HEADER: &str = "x-maverick-affinity";

/// The `X-Maverick-API-Features` name that lets a client send `affinity`.
pub const AFFINITY_FEATURE: &str = "affinity";

/// The longest conversation id, in characters.
pub const MAX_CONVERSATION_ID: usize = 256;

/// The `400 invalid_request` message for a malformed `X-Maverick-Conversation`.
pub const INVALID_CONVERSATION_MESSAGE: &str =
    "X-Maverick-Conversation must be 1 to 256 printable ASCII characters";

/// Whether `value` is a valid conversation id: 1 to 256 printable ASCII characters (0x20
/// to 0x7E).
#[must_use]
pub fn is_conversation_id(value: &[u8]) -> bool {
    (1..=MAX_CONVERSATION_ID).contains(&value.len())
        && value.iter().all(|byte| (0x20..=0x7e).contains(byte))
}

/// The estimate of the input tokens a turn resends: the serialized request body's length
/// in bytes divided by 4, rounded up.
#[must_use]
pub fn context_tokens(body_bytes: usize) -> u64 {
    u64::try_from(body_bytes).unwrap_or(u64::MAX).div_ceil(4)
}

/// The `affinity` member of a recommend request.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct AffinityRequest {
    /// `affinity.key`: the caller's conversation id, opaque to the engine.
    pub key: String,
    /// `affinity.context_tokens`: the estimate from [`context_tokens`].
    pub context_tokens: Option<u64>,
    /// `affinity.cost_aware`: overrides the tenant setting for this request.
    pub cost_aware: Option<bool>,
}

impl AffinityRequest {
    /// The request for a conversation whose turn has `body_bytes` of request body, with
    /// the tenant's cost-aware setting.
    #[must_use]
    pub fn for_turn(key: impl Into<String>, body_bytes: usize) -> Self {
        Self {
            key: key.into(),
            context_tokens: Some(context_tokens(body_bytes)),
            cost_aware: None,
        }
    }
}

impl fmt::Debug for AffinityRequest {
    /// Never shows the key (G5).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AffinityRequest")
            .field("key", &format_args!("<{} bytes>", self.key.len()))
            .field("context_tokens", &self.context_tokens)
            .field("cost_aware", &self.cost_aware)
            .finish()
    }
}

/// The `affinity` member of a recommend response.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Affinity {
    /// `status`, as the engine wrote it: `new`, `kept`, `switched`, or `broken` today. A
    /// status this client does not know is kept as it is.
    pub status: String,
    /// `previous_column_id`, or `None` for a new conversation.
    pub previous_column_id: Option<String>,
    /// `reason`, when the engine gave one.
    pub reason: Option<String>,
    /// `cache_warm`, when present.
    pub cache_warm: Option<bool>,
    /// `cost_aware`: the effective setting, when present.
    pub cost_aware: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_ids_are_one_to_256_printable_ascii_bytes() {
        assert!(is_conversation_id(b"conv_8f2a41"));
        assert!(is_conversation_id(b" "));
        assert!(is_conversation_id(b"~"));
        assert!(is_conversation_id(&[b'a'; 256]));
        assert!(!is_conversation_id(b""));
        assert!(!is_conversation_id(&[b'a'; 257]));
        assert!(!is_conversation_id(b"tab\there"));
        assert!(!is_conversation_id(b"\x7f"));
        assert!(!is_conversation_id("caf\u{e9}".as_bytes()));
    }

    #[test]
    fn the_token_estimate_rounds_a_quarter_of_the_bytes_up() {
        assert_eq!(context_tokens(0), 0);
        assert_eq!(context_tokens(1), 1);
        assert_eq!(context_tokens(4), 1);
        assert_eq!(context_tokens(5), 2);
        assert_eq!(context_tokens(400_001), 100_001);
    }

    #[test]
    fn debug_never_shows_the_key() {
        let shown = format!("{:?}", AffinityRequest::for_turn("conv-secret", 9));
        assert!(!shown.contains("conv-secret"), "{shown}");
        assert!(shown.contains("<11 bytes>"), "{shown}");
        assert!(shown.contains("Some(3)"), "{shown}");
    }
}
