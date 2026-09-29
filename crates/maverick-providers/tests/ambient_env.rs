//! D23: the hand-written provider clients build every request from engine config alone. Unlike
//! the Go provider SDKs, they contain no reads of ambient environment variables
//! (`ANTHROPIC_BASE_URL`, `OPENAI_ORG_ID`, `GOOGLE_GEMINI_BASE_URL`, and the rest), so no such
//! variable can change a request's URL or headers (parity/DIVERGENCES.md D23;
//! reference/engine-reference.md Part 2 Q-P4).
//!
//! The Go test proved this by setting those variables and asserting no effect. The workspace
//! forbids `unsafe`, and Rust 2024's `std::env::set_var` is unsafe, so this test does not mutate
//! the process environment. It instead proves the same guarantee from the other side: the base
//! URL comes only from the client's config, and each request lands there. The client modules read
//! no `std::env` at all (verified: no `env::var` in anthropic.rs, openai.rs, or google.rs), so
//! there is no code path an ambient variable could take.

use maverick_providers::anthropic::AnthropicClient;
use maverick_providers::google::GoogleClient;
use maverick_providers::openai::OpenAiClient;
use maverick_providers::{CompletionRequest, ProviderClient, ScriptedResponse, ScriptedTestServer};
use serde_json::Map;

fn request() -> CompletionRequest {
    CompletionRequest {
        model: "test-model".to_owned(),
        system: String::new(),
        prompt: "hello".to_owned(),
        temperature: 0.0,
        max_tokens: 16,
        settings: Map::new(),
    }
}

/// A `200` with an empty object: the request reaches the mock; the parse result does not matter,
/// because the point is where the request went, not what came back.
async fn ok_mock() -> ScriptedTestServer {
    ScriptedTestServer::start(vec![ScriptedResponse::json(200, b"{}".to_vec())])
        .await
        .expect("mock server")
}

#[tokio::test]
async fn requests_use_only_the_configured_base_url() {
    // This test asserts the D23 behavior; record the divergence it relies on.
    let _ = parity_support::divergence!("D23");

    // Anthropic: the URL is built from the configured base, and the request lands there.
    let anthropic = ok_mock().await;
    let base = anthropic.url();
    let client = AnthropicClient::new("k")
        .expect("anthropic client")
        .with_base_url(base.clone());
    assert_eq!(
        client.messages_url(),
        format!("{}/v1/messages", base.trim_end_matches('/'))
    );
    let _ = client.complete(request()).await;
    let calls = anthropic.calls().await;
    assert!(!calls.is_empty(), "anthropic sent at least one request");
    assert!(
        calls.iter().all(|call| call.path == "/v1/messages"),
        "every anthropic request used the configured base URL, not an ambient one"
    );

    // OpenAI: same, and no ambient organization header is sent.
    let openai = ok_mock().await;
    let base = openai.url();
    let client = OpenAiClient::openai("k")
        .expect("openai client")
        .with_base_url(base.clone());
    assert_eq!(
        client.completions_url(),
        format!("{}/chat/completions", base.trim_end_matches('/'))
    );
    let _ = client.complete(request()).await;
    let calls = openai.calls().await;
    assert!(!calls.is_empty(), "openai sent at least one request");
    assert!(
        calls.iter().all(|call| call.path == "/chat/completions"),
        "every openai request used the configured base URL, not an ambient one"
    );
    assert!(
        calls.iter().all(|call| {
            !call
                .headers
                .iter()
                .any(|(name, _)| name.to_ascii_lowercase().contains("organization"))
        }),
        "no request carried an ambient OpenAI-Organization header"
    );

    // Google: same.
    let google = ok_mock().await;
    let base = google.url();
    let client = GoogleClient::new("k")
        .expect("google client")
        .with_base_url(base.clone());
    assert_eq!(
        client.generate_url("test-model"),
        format!(
            "{}/v1beta/models/test-model:generateContent",
            base.trim_end_matches('/')
        )
    );
    let _ = client.complete(request()).await;
    let calls = google.calls().await;
    assert!(!calls.is_empty(), "google sent at least one request");
    assert!(
        calls
            .iter()
            .all(|call| call.path.contains("generateContent")),
        "every google request used the configured base URL, not an ambient one"
    );
}
