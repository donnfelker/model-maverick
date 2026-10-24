//! The version surface on every response (spec §14.5, D50): the engine's SemVer and the
//! optional request features it accepts. Self-hosted engines lag the SDK cores and the
//! proxy, which send an optional request field only when the engine's last response
//! listed its feature, so strict decoding never turns a newer client into a 422.

use axum::extract::Request;
use axum::http::HeaderValue;
use axum::middleware::Next;
use axum::response::Response;

/// The engine release (the workspace version).
pub const ENGINE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The response header that carries [`ENGINE_VERSION`].
pub const ENGINE_VERSION_HEADER: &str = "x-maverick-engine-version";

/// The response header that carries [`API_FEATURES`], comma separated.
pub const API_FEATURES_HEADER: &str = "x-maverick-api-features";

/// The optional request features this engine accepts beyond the first release, sorted
/// and without duplicates. The bead that makes the engine accept a new optional field
/// adds its feature name here (conversation affinity adds `affinity`, D44).
pub const API_FEATURES: &[&str] = &[];

/// The `X-Maverick-API-Features` value: [`API_FEATURES`] joined with commas.
pub fn api_features_value() -> String {
    API_FEATURES.join(",")
}

/// Stamps both version headers on every response, errors and `/up` included. It is the
/// outermost layer of both listeners, so a recovered panic and a 404 carry them too.
pub async fn version_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        ENGINE_VERSION_HEADER,
        HeaderValue::from_static(ENGINE_VERSION),
    );
    headers.insert(
        API_FEATURES_HEADER,
        HeaderValue::from_str(&api_features_value()).expect("feature names are header-safe"),
    );
    response
}
