//! DIVERGENCE: D50. Every response on both listeners carries `X-Maverick-Engine-Version`
//! and `X-Maverick-API-Features` (spec §14.5): every route, its errors, the fallback 404,
//! and `/up`. The Go engine had no version surface.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use maverick_api::version::{
    API_FEATURES, API_FEATURES_HEADER, ENGINE_VERSION, ENGINE_VERSION_HEADER, api_features_value,
};
use maverick_api::{
    AuthState, Identity, RateLimiter, StaticAuthBackend, internal_router_with_state,
    public_router_with_state,
};
use maverick_domain::keys;
use tower::ServiceExt;

/// Every public route (spec Appendix A), with one method each, and a path no route serves.
const PUBLIC: [(&str, &str); 22] = [
    ("GET", "/up"),
    ("GET", "/v1/config/pools"),
    ("PUT", "/v1/config/weights"),
    ("GET", "/v1/config/constraints"),
    ("GET", "/v1/policies"),
    ("POST", "/v1/recommend"),
    ("GET", "/v1/checks/code/v1"),
    ("GET", "/v1/models"),
    ("POST", "/v1/telemetry"),
    ("GET", "/v1/history/models/col_1"),
    ("GET", "/v1/history/recommendations"),
    ("GET", "/v1/history/recommendations/rec_1"),
    ("GET", "/v1/history/outcomes"),
    ("GET", "/v1/history/calibration"),
    ("POST", "/v1/debug/capture-windows"),
    ("DELETE", "/v1/debug/capture-windows/cw_1"),
    ("POST", "/v1/debug/replay/cap_1"),
    ("GET", "/v1/debug/captures"),
    ("GET", "/v1/debug/captures/cap_1/prompt"),
    ("POST", "/mcp"),
    ("GET", "/v1/no-such-route"),
    ("GET", "/no-such-route"),
];

/// Every internal route (spec §13), health detail, and a path no route serves.
const INTERNAL: [(&str, &str); 20] = [
    ("GET", "/health/detail"),
    ("POST", "/internal/v1/tenants"),
    ("GET", "/internal/v1/tenants/ten_1"),
    ("POST", "/internal/v1/tenants/ten_1/suspend"),
    ("POST", "/internal/v1/tenants/ten_1/activate"),
    ("POST", "/internal/v1/tenants/ten_1/api-keys"),
    ("DELETE", "/internal/v1/api-keys/key_1"),
    ("PUT", "/internal/v1/tenants/ten_1/pools"),
    ("PUT", "/internal/v1/tenants/ten_1/weights"),
    ("PUT", "/internal/v1/tenants/ten_1/constraints"),
    ("PUT", "/internal/v1/tenants/ten_1/limits"),
    ("GET", "/internal/v1/alerts"),
    ("POST", "/internal/v1/columns"),
    ("PUT", "/internal/v1/columns/col_1"),
    ("POST", "/internal/v1/models/refresh"),
    ("POST", "/internal/v1/corpus/activate"),
    ("GET", "/internal/v1/captures"),
    ("GET", "/internal/v1/captures/cap_1/prompt"),
    ("GET", "/internal/v1/no-such-route"),
    ("GET", "/no-such-route"),
];

fn auth_state() -> AuthState {
    AuthState::new(
        Arc::new(StaticAuthBackend::authenticated(Identity {
            tenant_id: "ten_1".to_owned(),
            api_key_id: "key_1".to_owned(),
        })),
        keys::parse_internal_tokens("ops:alpha-token").expect("tokens"),
        RateLimiter::new(300),
    )
}

/// Sends `method path` to `router` and asserts both headers; returns the status.
async fn assert_version_headers(
    router: &Router,
    method: &str,
    path: &str,
    auth: Option<&str>,
) -> StatusCode {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(auth) = auth {
        request = request.header("authorization", auth);
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::empty()).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    let headers = response.headers();
    assert_eq!(
        headers.get_all(ENGINE_VERSION_HEADER).iter().count(),
        1,
        "{method} {path} ({status}): one {ENGINE_VERSION_HEADER}"
    );
    assert_eq!(
        headers[ENGINE_VERSION_HEADER], ENGINE_VERSION,
        "{method} {path} ({status})"
    );
    assert_eq!(
        headers.get_all(API_FEATURES_HEADER).iter().count(),
        1,
        "{method} {path} ({status}): one {API_FEATURES_HEADER}"
    );
    assert_eq!(
        headers[API_FEATURES_HEADER],
        api_features_value().as_str(),
        "{method} {path} ({status})"
    );
    status
}

#[tokio::test]
async fn every_public_response_carries_the_version_headers() {
    let router = public_router_with_state(auth_state());
    let mut statuses = Vec::new();
    for (method, path) in PUBLIC {
        statuses.push(assert_version_headers(&router, method, path, None).await);
        // With a key the request reaches the handler (no database here, so most fail).
        assert_version_headers(&router, method, path, Some("Bearer mk_live_test")).await;
    }
    // The set covers success, auth errors, and the fallback.
    assert!(statuses.contains(&StatusCode::UNAUTHORIZED), "{statuses:?}");
    assert!(statuses.contains(&StatusCode::NOT_FOUND), "{statuses:?}");
}

#[tokio::test]
async fn every_internal_response_carries_the_version_headers() {
    let router = internal_router_with_state(auth_state());
    let mut statuses = Vec::new();
    for (method, path) in INTERNAL {
        statuses.push(assert_version_headers(&router, method, path, None).await);
        assert_version_headers(&router, method, path, Some("Bearer ops:alpha-token")).await;
    }
    assert!(statuses.contains(&StatusCode::UNAUTHORIZED), "{statuses:?}");
    assert!(statuses.contains(&StatusCode::NOT_FOUND), "{statuses:?}");
}

#[test]
fn the_feature_list_is_sorted_unique_and_header_safe() {
    let mut sorted = API_FEATURES.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted, API_FEATURES);
    for feature in API_FEATURES {
        assert!(
            !feature.is_empty()
                && feature
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
            "feature {feature:?}"
        );
    }
    assert_eq!(api_features_value(), API_FEATURES.join(","));
}

#[test]
fn the_engine_version_is_the_workspace_semver() {
    assert_eq!(ENGINE_VERSION, env!("CARGO_PKG_VERSION"));
    let core = ENGINE_VERSION.split(['-', '+']).next().expect("core");
    let parts: Vec<&str> = core.split('.').collect();
    assert_eq!(parts.len(), 3, "{ENGINE_VERSION}");
    assert!(
        parts.iter().all(|part| part.parse::<u64>().is_ok()),
        "{ENGINE_VERSION}"
    );
}
