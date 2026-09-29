//! Parity: the OpenAI and xAI rows of `parity/fixtures/measurement-io/providers_wire.json`,
//! replayed against the scripted test server. Bodies compare as JSON values (numbers as
//! `f64`); key order is not part of the contract.

use std::time::Duration;

use maverick_providers::openai::{OPENAI_BASE_URL, OpenAiClient, XAI_BASE_URL};
use maverick_providers::{
    CompletionRequest, ProviderClient, ProviderError, ProviderErrorKind, RecordedRequest,
    RetryPolicy, ScriptedResponse, ScriptedTestServer,
};
use serde_json::{Map, Value};

fn wire() -> Value {
    parity_support::fixture("measurement-io", "providers_wire.json").expect("fixture loads")
}

fn case(id: &str) -> Value {
    wire()["cases"]
        .as_array()
        .expect("cases")
        .iter()
        .find(|c| c["id"] == id)
        .unwrap_or_else(|| panic!("no case {id}"))
        .clone()
}

fn normalize(value: Value) -> Value {
    match value {
        Value::Number(n) => serde_json::json!(n.as_f64().expect("f64")),
        Value::Array(items) => Value::Array(items.into_iter().map(normalize).collect()),
        Value::Object(members) => Value::Object(
            members
                .into_iter()
                .map(|(k, v)| (k, normalize(v)))
                .collect(),
        ),
        other => other,
    }
}

fn request(value: &Value) -> CompletionRequest {
    let settings: Map<String, Value> = match &value["settings"] {
        Value::Null => Map::new(),
        Value::Object(map) => map.clone(),
        other => panic!("settings {other}"),
    };
    CompletionRequest {
        model: value["model"].as_str().expect("model").to_owned(),
        system: value["system"].as_str().expect("system").to_owned(),
        prompt: value["prompt"].as_str().expect("prompt").to_owned(),
        #[allow(clippy::cast_possible_truncation)]
        temperature: value["temperature_float32"].as_f64().expect("temperature") as f32,
        max_tokens: usize::try_from(value["max_tokens"].as_u64().expect("max_tokens"))
            .expect("fits"),
        settings,
    }
}

/// An `unknown` error stands for a raw non-provider error in Go: its text is the
/// wrapped error's own.
fn go_text(error: &ProviderError) -> String {
    use std::error::Error as _;
    match error.source() {
        Some(inner) if error.kind() == ProviderErrorKind::Unknown => inner.to_string(),
        _ => error.to_string(),
    }
}

fn describe(calls: &[RecordedRequest]) -> String {
    calls
        .iter()
        .map(|c| {
            let headers: Vec<String> = c
                .headers
                .iter()
                .map(|(n, v)| {
                    if n.eq_ignore_ascii_case("authorization") {
                        format!("{n}: <redacted>")
                    } else {
                        format!("{n}: {v}")
                    }
                })
                .collect();
            format!(
                "{} {}\n  {}\n  {}",
                c.method,
                c.path,
                headers.join("\n  "),
                String::from_utf8_lossy(&c.body)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// D32: only what the API needs, never the Go SDK's identity headers.
fn assert_headers(call: &RecordedRequest) {
    let _ = parity_support::divergence!("D32");
    assert_eq!(call.header("authorization"), Some("Bearer test-key"));
    assert_eq!(call.header("content-type"), Some("application/json"));
    assert!(
        !call
            .header("user-agent")
            .unwrap_or("")
            .contains("OpenAI/Go")
    );
    assert!(
        call.headers
            .iter()
            .all(|(n, _)| !n.to_ascii_lowercase().starts_with("x-stainless"))
    );
}

fn client(provider: &str, base_url: String) -> OpenAiClient {
    let client = match provider {
        "openai" => OpenAiClient::openai("test-key"),
        "xai" => OpenAiClient::xai("test-key"),
        other => panic!("provider {other}"),
    };
    client
        .expect("client")
        .with_base_url(base_url)
        .with_retry(RetryPolicy::new(
            3,
            Duration::from_millis(1),
            Duration::from_millis(2),
        ))
}

async fn replay(id: &str) {
    let case = case(id);
    let script: Vec<ScriptedResponse> = case["scripted_responses"]
        .as_array()
        .expect("script")
        .iter()
        .map(|r| {
            ScriptedResponse::json(
                u16::try_from(r["status"].as_u64().expect("status")).expect("u16"),
                r["body"].as_str().expect("body").as_bytes().to_vec(),
            )
        })
        .collect();
    let server = ScriptedTestServer::start(script).await.expect("server");
    let provider = case["provider"].as_str().expect("provider");
    let client = client(provider, server.url());
    assert_eq!(client.name(), provider);
    let result = client.complete(request(&case["completion_request"])).await;
    let calls = server.calls().await;
    let context = format!("{id}\n{}", describe(&calls));

    let wire_requests = case["wire_requests"].as_array().expect("wire requests");
    assert_eq!(calls.len(), wire_requests.len(), "{context}");
    assert_eq!(Value::from(calls.len()), case["calls"], "{context}");
    for (call, want) in calls.iter().zip(wire_requests) {
        assert_eq!(call.method, want["method"], "{context}");
        assert_eq!(call.path, want["path"], "{context}");
        let got: Value = serde_json::from_slice(&call.body).expect("request body is JSON");
        let expected: Value =
            serde_json::from_str(want["body"].as_str().expect("body")).expect("fixture body");
        assert_eq!(normalize(got), normalize(expected), "{context}");
        assert_headers(call);
    }

    match (&result, case["completion_response"].as_object()) {
        (Ok(response), Some(want)) => {
            assert_eq!(response.text, want["Text"], "{context}");
            assert_eq!(response.tokens_in, want["TokensIn"], "{context}");
            assert_eq!(response.tokens_out, want["TokensOut"], "{context}");
        }
        (Err(error), None) => {
            let want = case["error_string"]
                .as_str()
                .expect("error string")
                .replace("{BASE_URL}", &server.url());
            assert_eq!(go_text(error), want, "{context}");
            assert_eq!(error.kind().as_str(), case["error_kind"], "{context}");
            if let Some(status) = case["error_status"].as_i64() {
                assert_eq!(i64::from(error.status()), status, "{context}");
            }
            if let Some(attempts) = case["error_attempts"].as_u64() {
                assert_eq!(error.attempts() as u64, attempts, "{context}");
            }
        }
        (got, _) => panic!("{context}: unexpected result {got:?}"),
    }
}

/// parity: openai.TestCompleteSuccessMapsUsage
#[tokio::test]
async fn success_maps_usage() {
    replay("openai_system_reasoning").await;
}

/// parity: openai.TestCompleteEmptyChoicesIsProviderError
#[tokio::test]
async fn empty_choices_is_provider_error() {
    replay("openai_empty_choices").await;
}

/// parity: openai.TestCompleteRateLimitRetriesThenFails
#[tokio::test]
async fn rate_limit_retries_then_fails() {
    replay("openai_429_exhausts").await;
}

/// parity: openai.TestCompleteServerErrorRetriesThenFails
#[tokio::test]
async fn server_error_retries_then_fails() {
    replay("openai_500_exhausts").await;
}

/// parity: openai.TestCompleteInvalidRequestDoesNotRetry
#[tokio::test]
async fn invalid_request_does_not_retry() {
    replay("openai_400_no_retry").await;
}

/// parity: openai.TestCompleteRecoversAfterRetryableError
#[tokio::test]
async fn recovers_after_retryable_error() {
    replay("openai_500_then_ok").await;
}

/// parity: xai.TestCompleteSuccessMapsUsage
#[tokio::test]
async fn xai_success_maps_usage() {
    replay("xai_plain").await;
}

/// parity: xai.TestCompleteRateLimitUsesXAIProviderLabel
#[tokio::test]
async fn xai_rate_limit_uses_the_xai_label() {
    replay("xai_429_exhausts").await;
}

/// Every OpenAI and xAI row, including the probes (plain, float32 temperature, ignored
/// response_schema, null content, two choices, and a plain-text error body).
#[tokio::test]
async fn every_openai_and_xai_row() {
    let ids: Vec<String> = wire()["cases"]
        .as_array()
        .expect("cases")
        .iter()
        .filter(|c| c["provider"] == "openai" || c["provider"] == "xai")
        .map(|c| c["id"].as_str().expect("id").to_owned())
        .collect();
    assert!(ids.len() >= 14);
    for id in ids {
        replay(&id).await;
    }
}

/// parity: xai.TestNewDefaultsToXAIBaseURL
/// parity: xai.TestNewKeepsExplicitBaseURL
#[test]
fn base_urls() {
    let doc = wire();
    let xai = &doc["xai_base_url"];
    assert_eq!(XAI_BASE_URL, xai["default"]);
    assert_eq!(
        OpenAiClient::xai("k").expect("client").base_url(),
        xai["default"]
    );
    let explicit = xai["explicit_kept"].as_str().expect("explicit");
    let kept = OpenAiClient::xai("k")
        .expect("client")
        .with_base_url(explicit);
    assert_eq!(kept.base_url(), explicit);
    assert_eq!(
        kept.completions_url(),
        format!("{explicit}/chat/completions")
    );
    // D23: the defaults come from code, never from ambient environment variables.
    let openai = OpenAiClient::openai("k").expect("client");
    assert_eq!(openai.base_url(), OPENAI_BASE_URL);
    assert_eq!(
        openai.completions_url(),
        "https://api.openai.com/v1/chat/completions"
    );
    assert_eq!(openai.name(), "openai");
    assert!(
        doc["default_base_urls"]["openai"]
            .as_str()
            .expect("openai")
            .starts_with(OPENAI_BASE_URL)
    );
}

/// A caller deadline in the middle of the retry backoff ends the call as a timeout
/// before the attempts run out.
///
/// parity: openai.TestCompleteContextTimeoutMidRetry
#[tokio::test]
async fn context_timeout_mid_retry() {
    let server = ScriptedTestServer::start(vec![ScriptedResponse::json(
        429,
        br#"{"error":{"message":"slow down","type":"rate_limit_error"}}"#.to_vec(),
    )])
    .await
    .expect("server");
    let client = OpenAiClient::openai("test-key")
        .expect("client")
        .with_base_url(server.url())
        .with_retry(RetryPolicy::new(
            3,
            Duration::from_millis(500),
            Duration::from_secs(1),
        ));
    let request = request(&case("openai_plain")["completion_request"]);
    let outcome = tokio::time::timeout(Duration::from_millis(100), client.complete(request)).await;
    assert!(outcome.is_err(), "the caller's deadline ends the call");
    assert!(server.call_count().await < 3);
}
