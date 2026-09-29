//! The proxy's exact responses (reference part 6 §3 and §4).
//!
//! Errors use the envelope `{"error":{"code":"<code>","message":"<message>"}}` followed by
//! one `\n`, `Content-Type: application/json`, and Go's HTML-escaped JSON strings. The
//! routing errors are Go's `ServeMux` plain-text responses.

use axum::body::Body;
use axum::http::header::{
    ALLOW, CONTENT_LENGTH, CONTENT_TYPE, HeaderValue, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderName, StatusCode};
use axum::response::Response;
use maverick_core::gojson::quote;

/// `invalid_request`.
pub const INVALID_REQUEST: &str = "invalid_request";
/// The message when the body cannot be read.
pub const READ_BODY_FAILED: &str = "failed to read request body";
/// The message when the body is not a JSON object with a string `model`.
pub const MALFORMED_JSON: &str = "malformed JSON request body";
/// `invalid_model_alias`.
pub const INVALID_MODEL_ALIAS: &str = "invalid_model_alias";
/// The alias rejection message, exact (U+2014 is written raw; `<` and `>` are escaped by
/// the envelope).
pub const INVALID_MODEL_ALIAS_MESSAGE: &str = "the proxy only routes maverick/* aliases (maverick/auto | maverick/quality | maverick/cost | maverick/<pool>:<mode>) \u{2014} see docs/api.md \"SDK integration conventions\"";
/// The message when `X-Maverick-Workload` cannot be read (D5; Go forwarded the header).
pub const INVALID_WORKLOAD_HEADER_MESSAGE: &str =
    "X-Maverick-Workload must be <capability>[:<tier>]";
/// `engine_error`.
pub const ENGINE_ERROR: &str = "engine_error";
/// The message for every non-terminal recommend failure.
pub const ENGINE_ERROR_MESSAGE: &str = "the engine's POST /v1/recommend call failed";
/// `no_executable_candidates`.
pub const NO_EXECUTABLE_CANDIDATES: &str = "no_executable_candidates";
/// The chat surface's exhausted-chain message.
pub const NO_EXECUTABLE_CANDIDATES_CHAT: &str =
    "no chain entry could be executed (no credential for any candidate, or every attempt failed)";
/// The messages surface's exhausted-chain message.
pub const NO_EXECUTABLE_CANDIDATES_MESSAGES: &str = "no anthropic chain entry could be executed (no anthropic candidate, no credential, or every attempt failed)";
/// `payload_too_large` (D38).
pub const PAYLOAD_TOO_LARGE: &str = "payload_too_large";
/// `unauthorized` (hosted mode, D13).
pub const UNAUTHORIZED: &str = "unauthorized";
/// The hosted-mode message for a missing or unusable tenant key.
pub const UNAUTHORIZED_MESSAGE: &str = "missing or invalid X-Maverick-API-Key";
/// `tenant_suspended`, relayed from the engine in hosted mode (D13).
pub const TENANT_SUSPENDED: &str = "tenant_suspended";
/// The engine's exact `tenant_suspended` message.
pub const TENANT_SUSPENDED_MESSAGE: &str = "tenant is suspended";
/// `zero_retention_requires_declared`, relayed from the engine (D17).
pub const ZERO_RETENTION_REQUIRES_DECLARED: &str = "zero_retention_requires_declared";
/// `no_compliant_columns`, relayed from the engine (D17).
pub const NO_COMPLIANT_COLUMNS: &str = "no_compliant_columns";
/// The proxy's own `409 no_compliant_columns` for OpenRouter execution (D15): a
/// zero-retention tenant, and no chain entry has an OpenRouter alias with valid
/// zero-retention evidence.
pub const NO_COMPLIANT_OPENROUTER_ALIAS_MESSAGE: &str =
    "no chain entry has an OpenRouter alias with valid zero-retention evidence";

/// `rate_limited` (D28).
pub const RATE_LIMITED: &str = "rate_limited";
/// The rate-limit message (the API's text).
pub const RATE_LIMITED_MESSAGE: &str = "rate limit exceeded";

/// The correlation header on relayed and `no_executable_candidates` responses.
pub const RECOMMENDATION_ID_HEADER: HeaderName =
    HeaderName::from_static("x-maverick-recommendation-id");

/// The correlation header naming the executed model on relayed responses.
pub const MODEL_USED_HEADER: HeaderName = HeaderName::from_static("x-maverick-model-used");

/// The exact envelope bytes: `{"error":{"code":..,"message":..}}` and one newline.
#[must_use]
pub fn envelope_body(code: &str, message: &str) -> String {
    format!(
        "{{\"error\":{{\"code\":{},\"message\":{}}}}}\n",
        quote(code),
        quote(message)
    )
}

/// An error response with the envelope body.
#[must_use]
pub fn error(status: StatusCode, code: &str, message: &str) -> Response {
    fixed(status, "application/json", envelope_body(code, message))
}

/// `200 {"status":"ok"}`, no trailing newline (Content-Length 15).
#[must_use]
pub fn up() -> Response {
    fixed(
        StatusCode::OK,
        "application/json",
        "{\"status\":\"ok\"}".to_owned(),
    )
}

/// Go's `405`: `Allow`, plain text, `nosniff`, body `Method Not Allowed\n`.
#[must_use]
pub fn method_not_allowed(allow: &'static str) -> Response {
    let mut response = plain(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed\n");
    response
        .headers_mut()
        .insert(ALLOW, HeaderValue::from_static(allow));
    response
}

/// Go's `404`: plain text, `nosniff`, body `404 page not found\n`.
#[must_use]
pub fn not_found() -> Response {
    plain(StatusCode::NOT_FOUND, "404 page not found\n")
}

fn plain(status: StatusCode, body: &'static str) -> Response {
    let mut response = fixed(status, "text/plain; charset=utf-8", body.to_owned());
    response
        .headers_mut()
        .insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    response
}

/// A response with a fixed body and an explicit `Content-Length`, as Go sent.
fn fixed(status: StatusCode, content_type: &'static str, body: String) -> Response {
    let length = HeaderValue::from(body.len());
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(CONTENT_LENGTH, length);
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelopes_match_go_bytes() {
        assert_eq!(
            envelope_body(INVALID_REQUEST, MALFORMED_JSON),
            "{\"error\":{\"code\":\"invalid_request\",\"message\":\"malformed JSON request body\"}}\n"
        );
        assert_eq!(
            envelope_body(NO_EXECUTABLE_CANDIDATES, NO_EXECUTABLE_CANDIDATES_CHAT).len(),
            150
        );
        assert_eq!(
            envelope_body(NO_EXECUTABLE_CANDIDATES, NO_EXECUTABLE_CANDIDATES_MESSAGES).len(),
            166
        );
        let alias = envelope_body(INVALID_MODEL_ALIAS, INVALID_MODEL_ALIAS_MESSAGE);
        assert_eq!(alias.len(), 242);
        assert!(alias.contains("maverick/\\u003cpool\\u003e:\\u003cmode\\u003e) \u{2014} see"));
    }
}
