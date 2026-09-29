//! One scenario driven end to end through both mock servers (bead 8.1 "Done when").
//!
//! The mock engine answers `POST /v1/recommend` with a scripted chain, and the real
//! `maverick_core` recommend client decodes it: the harness is wire-compatible with the
//! Rust SDK core. The mock provider answers the chat call the chain would trigger. Both
//! recorders show what arrived, including the tenant key on recommend and the caller key on
//! the provider call.

use maverick_core::alias::Mode;
use maverick_core::recommend::{
    RecommendClient, RecommendClientConfig, RecommendRequest, TenantKey, Workload,
};
use sdk_conformance::{Harness, Scenario};

const SCENARIO: &[u8] = br#"{
    "name": "declared_happy_path",
    "engine": {
        "recommend": [
            {"kind": "respond", "status": 200, "body": {
                "recommendation_id": "rec_smoke",
                "measured": true,
                "classification": {"method": "declared"},
                "recommendations": [
                    {"provider": "anthropic", "model": "claude-opus-4-8",
                     "endpoint": "direct", "column_id": "col_1", "retention_mode": "standard"}
                ]
            }}
        ]
    },
    "provider": {
        "responses": [
            {"kind": "respond", "status": 200, "body": {
                "id": "chatcmpl_mock",
                "choices": [{"index": 0,
                             "message": {"role": "assistant", "content": "hello from the mock"},
                             "finish_reason": "stop"}]
            }}
        ]
    }
}"#;

#[tokio::test]
async fn smoke_drives_one_scenario_through_both_servers() {
    let harness = Harness::start().await.expect("start harness");
    harness.load(Scenario::from_json(SCENARIO).expect("parse scenario"));

    // 1. Recommend through the real SDK-core client against the mock engine.
    let client = RecommendClient::new(RecommendClientConfig::new(harness.engine_base_url()))
        .expect("build recommend client");
    let request = RecommendRequest {
        mode: Mode::Balanced,
        pool: None,
        workload: Workload::declared_fallback(),
    };
    let recommendation = client
        .recommend(&TenantKey::new("mk_live_smoke"), &request)
        .await
        .expect("recommend succeeds");
    assert_eq!(recommendation.recommendation_id, "rec_smoke");
    assert_eq!(recommendation.chain.len(), 1);
    assert_eq!(recommendation.chain[0].provider, "anthropic");
    assert_eq!(recommendation.chain[0].model, "claude-opus-4-8");

    // 2. Call the mock provider the way the chain executor would, with the caller's key.
    let http = reqwest::Client::new();
    let response = http
        .post(format!(
            "{}/v1/chat/completions",
            harness.provider_base_url()
        ))
        .header("authorization", "Bearer sk-caller")
        .header("content-type", "application/json")
        .body(r#"{"model":"claude-opus-4-8","messages":[]}"#)
        .send()
        .await
        .expect("provider call sent");
    assert_eq!(response.status().as_u16(), 200);
    let bytes = response.bytes().await.expect("provider body");
    let body: serde_json::Value = serde_json::from_slice(&bytes).expect("provider json");
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "hello from the mock"
    );

    // 3. The recorders show exactly what arrived.
    let recorded = harness.recorded();
    assert_eq!(recorded.recommend.len(), 1);
    assert_eq!(recorded.recommend[0].path, "/v1/recommend");
    assert_eq!(
        recorded.recommend[0]
            .headers
            .get("authorization")
            .map(String::as_str),
        Some("Bearer mk_live_smoke"),
        "the engine saw the tenant key, never a provider credential"
    );
    assert!(
        recorded.telemetry.is_empty(),
        "this scenario sends no telemetry"
    );
    assert_eq!(recorded.provider.len(), 1);
    assert_eq!(recorded.provider[0].path, "/v1/chat/completions");
    assert_eq!(
        recorded.provider[0]
            .headers
            .get("authorization")
            .map(String::as_str),
        Some("Bearer sk-caller"),
        "the provider saw the caller's own key"
    );
}
