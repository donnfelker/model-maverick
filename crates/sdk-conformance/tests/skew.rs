//! DIVERGENCE: D50. Version skew (spec §14.5): the current SDK core against an engine from
//! the first release, which advertises no optional feature and decodes strictly. The mock
//! answers `422 invalid_request` for any member the first release does not know, exactly
//! as the engine's strict decoding does (spec §9.2). Every request shape the core builds
//! must pass, on the first call and on every call after it. A new optional field that skips
//! the feature gate fails here.

use axum::Router;
use axum::body::Bytes;
use axum::http::StatusCode;
use axum::routing::post;
use maverick_core::affinity::AffinityRequest;
use maverick_core::alias::Mode;
use maverick_core::recommend::{
    RecommendClient, RecommendClientConfig, RecommendRequest, TenantKey, Workload,
};
use serde_json::Value;

/// The request members of the first release (spec §9.2).
const TOP_LEVEL: [&str; 3] = ["mode", "pool", "workload"];
const WORKLOAD: [&str; 5] = [
    "capability",
    "difficulty",
    "domain_tags",
    "projection_version",
    "prompt",
];

/// The first-release engine's verdict on a body: `None` when it accepts it, or the unknown
/// member it rejects.
fn unknown_member(body: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(body).expect("the core sends JSON");
    let object = value.as_object().expect("an object");
    if let Some(name) = object
        .keys()
        .find(|name| !TOP_LEVEL.contains(&name.as_str()))
    {
        return Some(name.clone());
    }
    object["workload"]
        .as_object()
        .expect("a workload object")
        .keys()
        .find(|name| !WORKLOAD.contains(&name.as_str()))
        .map(|name| format!("workload.{name}"))
}

async fn first_release_engine(body: Bytes) -> (StatusCode, String) {
    match unknown_member(&body) {
        None => (
            StatusCode::OK,
            r#"{"recommendation_id":"rec_1","recommendations":[]}"#.to_owned(),
        ),
        Some(name) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                r#"{{"error":{{"code":"invalid_request","message":"unknown field {name}","details":{{}}}}}}"#
            ),
        ),
    }
}

#[tokio::test]
async fn the_core_never_gets_a_422_from_a_first_release_engine() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    let app = Router::new().route("/v1/recommend", post(first_release_engine));
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });

    let client = RecommendClient::new(RecommendClientConfig::new(format!("http://{address}")))
        .expect("client");
    let key = TenantKey::new("mk_test_conformance");
    let workloads = [
        Workload::declared_fallback(),
        Workload::Declared {
            capability: "code_generation".to_owned(),
            difficulty: Some(4),
            domain_tags: vec!["legal".to_owned()],
        },
        Workload::Prompt {
            prompt: "hello".to_owned(),
            projection_version: Some("p1".to_owned()),
        },
        Workload::Prompt {
            prompt: "hello".to_owned(),
            projection_version: None,
        },
    ];
    for round in 0..2 {
        for mode in [Mode::Quality, Mode::Balanced, Mode::Cost] {
            for pool in [None, Some("eu".to_owned())] {
                for workload in &workloads {
                    // D44: the conversation never reaches an engine that did not
                    // advertise `affinity`, so this strict engine accepts the request.
                    let request = RecommendRequest {
                        mode,
                        pool: pool.clone(),
                        workload: workload.clone(),
                        affinity: Some(AffinityRequest::for_turn("conv_skew", 400)),
                    };
                    let outcome = client.recommend(&key, &request).await;
                    assert!(outcome.is_ok(), "round {round}: {request:?}: {outcome:?}");
                }
            }
        }
        // The engine sent no feature header, so the core still advertises nothing.
        assert_eq!(client.features().names().count(), 0);
    }
}

#[test]
fn the_mock_rejects_what_a_first_release_engine_rejects() {
    assert_eq!(
        unknown_member(br#"{"mode":"quality","workload":{"capability":"x"}}"#),
        None
    );
    assert_eq!(
        unknown_member(br#"{"affinity":{"key":"c"},"mode":"quality","workload":{}}"#),
        Some("affinity".to_owned())
    );
    assert_eq!(
        unknown_member(br#"{"mode":"quality","workload":{"capability":"x","tokens":1}}"#),
        Some("workload.tokens".to_owned())
    );
}
