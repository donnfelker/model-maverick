//! D45 direct provider calls. Only a native author prefix changes the request body.
//! This path never recommends, applies settings, normalizes parameters, or reports.

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use maverick_core::alias::{ParsedModel, parse_alias};
use maverick_core::chain::ChainExecutor;

use crate::chain::{Surface, exhausted};
use crate::credentials::{
    CallerCredential, EnvKeys, attempt_headers, is_hop_by_hop, native_provider, resolve_key,
};
use crate::envelope;
use crate::rewrite::{Normalizer, rewrite_model};
use crate::surface::Upstreams;

/// The explicit-model request. It has no tenant key or recommendation id.
/// It deliberately has no Debug implementation: the body and headers are private.
pub struct DirectRequest {
    /// The provider wire format selected by the inbound endpoint.
    pub surface: Surface,
    /// The original model string, including any author prefix.
    pub model: String,
    /// The caller's provider credential, redacted by its Debug implementation.
    pub credential: CallerCredential,
    /// Customer-owned env credentials, empty in hosted mode (D12).
    pub env_keys: EnvKeys,
    /// Base headers after removing credentials and Maverick control headers.
    pub upstream_headers: HeaderMap,
    /// The original request body. Never logged.
    pub body: Bytes,
    /// The original query, appended without re-encoding. Never logged.
    pub raw_query: String,
}

/// Execute exactly one provider request with the existing client's header timeout.
pub async fn execute(
    executor: &ChainExecutor,
    upstreams: &Upstreams,
    request: DirectRequest,
) -> Response {
    let surface = request.surface;
    let upstream = match prepare(upstreams, request) {
        Ok(upstream) => upstream,
        Err(response) => return *response,
    };
    let response = match executor.send_one(upstream).await {
        Ok(response) => response,
        Err(_) => return exhausted(surface, ""),
    };
    let status = response.status();
    let headers = response
        .headers()
        .iter()
        .filter(|(name, _)| !is_hop_by_hop(name))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    // No callback, usage parsing, correlation headers, retry, or response translation.
    let mut relayed = Response::new(Body::new(reqwest::Body::from(response)));
    *relayed.status_mut() = status;
    *relayed.headers_mut() = headers;
    relayed
}

fn prepare(
    upstreams: &Upstreams,
    request: DirectRequest,
) -> Result<reqwest::Request, Box<Response>> {
    let DirectRequest {
        surface,
        model,
        credential,
        env_keys,
        upstream_headers,
        body,
        raw_query,
    } = request;
    let Ok(ParsedModel::Direct { author, slug }) = parse_alias(&model) else {
        return Err(Box::new(envelope::error(
            StatusCode::BAD_REQUEST,
            envelope::INVALID_MODEL_ALIAS,
            envelope::INVALID_MODEL_ALIAS_MESSAGE,
        )));
    };
    if model.is_empty() {
        return Err(Box::new(envelope::error(
            StatusCode::BAD_REQUEST,
            envelope::INVALID_MODEL_ALIAS,
            envelope::INVALID_MODEL_ALIAS_MESSAGE,
        )));
    }
    let native = native_provider(surface);
    let (url, key, strip_author) = match (&credential, surface) {
        (CallerCredential::OpenRouter(key), Surface::Chat) => {
            (upstreams.openrouter_url(&raw_query), key.as_str(), false)
        }
        _ => {
            if author.is_some_and(|author| author != native) {
                return Err(Box::new(envelope::error(
                    StatusCode::BAD_REQUEST,
                    envelope::INVALID_REQUEST,
                    &format!("model {model} is not served by this surface"),
                )));
            }
            let Some(key) = resolve_key(native, surface, &credential, &env_keys) else {
                return Err(Box::new(exhausted(surface, "")));
            };
            let Some(url) = upstreams.url(surface, native, &raw_query) else {
                return Err(Box::new(exhausted(surface, "")));
            };
            (url, key, author.is_some())
        }
    };
    let Some(headers) = attempt_headers(surface, &upstream_headers, key) else {
        return Err(Box::new(exhausted(surface, "")));
    };
    let url = reqwest::Url::parse(&url).map_err(|_| exhausted(surface, ""))?;
    let body = if strip_author {
        rewrite_model(&body, slug, Normalizer::None)
            .map_err(|_| {
                envelope::error(
                    StatusCode::BAD_REQUEST,
                    envelope::INVALID_REQUEST,
                    envelope::MALFORMED_JSON,
                )
            })?
            .into()
    } else {
        body
    };
    let mut upstream = reqwest::Request::new(reqwest::Method::POST, url);
    *upstream.headers_mut() = headers;
    *upstream.body_mut() = Some(body.into());
    Ok(upstream)
}
