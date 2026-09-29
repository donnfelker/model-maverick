//! The fake provider over HTTP, driven by the engine's real provider clients (bead 9.10):
//! each client, pointed at the server by its base URL alone, gets the scripted output and
//! token counts back, a forced Anthropic tool gets the output as its input, scripted errors
//! arrive as the provider's own classes, and a missing fixture fails fast and is recorded.

use std::time::Duration;

use maverick_e2e::{FakeProviderServer, Served, Wire};
use maverick_providers::anthropic::AnthropicClient;
use maverick_providers::fake::{FakeProvider, FixtureSpec, hash_prompt};
use maverick_providers::google::GoogleClient;
use maverick_providers::openai::OpenAiClient;
use maverick_providers::{
    CompletionRequest, ProviderClient, ProviderErrorKind, RetryPolicy, SETTING_RESPONSE_SCHEMA,
};
use serde_json::{Map, json};

fn fixture(model: &str, prompt: &str, output: &str, error: &str) -> FixtureSpec {
    serde_json::from_value(json!({
        "model": model, "prompt": prompt, "output": output,
        "tokens_in": 11, "tokens_out": 7, "error": error,
    }))
    .expect("fixture")
}

async fn server() -> FakeProviderServer {
    let fake = FakeProvider::from_fixtures(&[
        fixture("claude-e2e", "anthropic prompt", "from anthropic", ""),
        fixture("gpt-e2e", "openai prompt", "from openai", ""),
        fixture("grok-e2e", "xai prompt", "from xai", ""),
        fixture("gemini-e2e", "google prompt", "from google", ""),
        fixture(
            "claude-e2e",
            "classify this",
            r#"{"capability":"factual_qa","difficulty":2}"#,
            "",
        ),
        fixture("claude-e2e", "busy", "", "rate_limited"),
        fixture("gpt-e2e", "broken", "", "provider_error"),
        fixture("gemini-e2e", "bad", "", "invalid_request"),
    ])
    .expect("fake");
    FakeProviderServer::start(fake).await.expect("server")
}

fn once() -> RetryPolicy {
    RetryPolicy::new(1, Duration::ZERO, Duration::ZERO)
}

fn request(model: &str, prompt: &str) -> CompletionRequest {
    CompletionRequest {
        model: model.to_owned(),
        system: "a system prompt the fake ignores".to_owned(),
        prompt: prompt.to_owned(),
        temperature: 0.0,
        max_tokens: 64,
        settings: Map::new(),
    }
}

/// The four engine clients, each moved to the server by its base URL only.
fn clients(url: &str) -> [(&'static str, Box<dyn ProviderClient>); 4] {
    [
        (
            "anthropic",
            Box::new(
                AnthropicClient::new("k")
                    .unwrap()
                    .with_base_url(url)
                    .with_retry(once()),
            ),
        ),
        (
            "openai",
            Box::new(
                OpenAiClient::openai("k")
                    .unwrap()
                    .with_base_url(format!("{url}/openai"))
                    .with_retry(once()),
            ),
        ),
        (
            "xai",
            Box::new(
                OpenAiClient::xai("k")
                    .unwrap()
                    .with_base_url(format!("{url}/xai"))
                    .with_retry(once()),
            ),
        ),
        (
            "google",
            Box::new(
                GoogleClient::new("k")
                    .unwrap()
                    .with_base_url(url)
                    .with_retry(once()),
            ),
        ),
    ]
}

#[tokio::test]
async fn every_engine_client_round_trips() {
    let server = server().await;
    let [anthropic, openai, xai, google] = clients(server.url());
    for ((name, client), (model, prompt, output)) in
        [anthropic, openai, xai, google].into_iter().zip([
            ("claude-e2e", "anthropic prompt", "from anthropic"),
            ("gpt-e2e", "openai prompt", "from openai"),
            ("grok-e2e", "xai prompt", "from xai"),
            ("gemini-e2e", "google prompt", "from google"),
        ])
    {
        let response = client
            .complete(request(model, prompt))
            .await
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(
            (
                response.text.as_str(),
                response.tokens_in,
                response.tokens_out
            ),
            (output, 11, 7),
            "{name}"
        );
    }
    let served = server.served();
    assert_eq!(
        served
            .iter()
            .map(|s| (s.wire, s.path.as_str(), s.model.as_str(), s.status))
            .collect::<Vec<_>>(),
        [
            (Some(Wire::Anthropic), "/v1/messages", "claude-e2e", 200),
            (
                Some(Wire::OpenAi),
                "/openai/chat/completions",
                "gpt-e2e",
                200
            ),
            (Some(Wire::OpenAi), "/xai/chat/completions", "grok-e2e", 200),
            (
                Some(Wire::Google),
                "/v1beta/models/gemini-e2e:generateContent",
                "gemini-e2e",
                200
            ),
        ]
    );
    assert_eq!(served[0].prompt_sha256, hash_prompt("anthropic prompt"));
    assert!(server.misses().is_empty());
}

/// The classifier's structured output: a forced tool whose input is the fixture's JSON.
#[tokio::test]
async fn a_forced_anthropic_tool_gets_the_output_as_input() {
    let server = server().await;
    let [(_, anthropic), ..] = clients(server.url());
    let mut forced = request("claude-e2e", "classify this");
    forced.settings.insert(
        SETTING_RESPONSE_SCHEMA.to_owned(),
        json!({"type": "object", "properties": {"capability": {"type": "string"}}}),
    );
    let response = anthropic.complete(forced).await.expect("forced");
    assert_eq!(
        response.text,
        r#"{"capability":"factual_qa","difficulty":2}"#
    );
}

#[tokio::test]
async fn scripted_errors_keep_their_class() {
    let server = server().await;
    let [(_, anthropic), (_, openai), _, (_, google)] = clients(server.url());
    for (client, request, kind, status) in [
        (
            &anthropic,
            request("claude-e2e", "busy"),
            ProviderErrorKind::RateLimited,
            429,
        ),
        (
            &openai,
            request("gpt-e2e", "broken"),
            ProviderErrorKind::ProviderError,
            500,
        ),
        (
            &google,
            request("gemini-e2e", "bad"),
            ProviderErrorKind::InvalidRequest,
            400,
        ),
    ] {
        let error = client.complete(request).await.expect_err("scripted");
        assert_eq!((error.kind(), error.status()), (kind, status), "{error}");
    }
    assert!(server.misses().is_empty(), "scripted errors are not misses");
}

/// A request without a fixture fails fast (400, no retry) and is recorded by hash only.
#[tokio::test]
async fn a_missing_fixture_is_a_recorded_miss() {
    let server = server().await;
    let anthropic = AnthropicClient::new("k")
        .unwrap()
        .with_base_url(server.url());
    let error = anthropic
        .complete(request("claude-e2e", "SECRET-NO-FIXTURE"))
        .await
        .expect_err("miss");
    assert_eq!(error.kind(), ProviderErrorKind::InvalidRequest, "{error}");
    let misses = server.misses();
    assert_eq!(
        misses,
        [Served {
            wire: Some(Wire::Anthropic),
            path: "/v1/messages".to_owned(),
            model: "claude-e2e".to_owned(),
            prompt_sha256: hash_prompt("SECRET-NO-FIXTURE"),
            status: 400,
            miss: true,
        }],
        "one attempt, no retry on a 400"
    );
    assert!(!format!("{misses:?}").contains("SECRET"), "no prompt text");
    assert!(
        error.to_string().contains("no fixture for model"),
        "the fake's message reaches the caller: {error}"
    );
}
