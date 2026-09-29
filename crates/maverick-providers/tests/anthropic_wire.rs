//! Parity: the Anthropic rows of `parity/fixtures/measurement-io/providers_wire.json`,
//! replayed against the scripted test server. Bodies compare as JSON values (numbers as
//! `f64`); key order is not part of the contract.

use std::time::Duration;

use maverick_providers::anthropic::{AnthropicClient, go_compact};
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

/// Every JSON number as an `f64`.
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

/// The text Go stored for an error: an `unknown` error is a raw non-provider error in Go,
/// so its text is the wrapped error's own.
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
                    if n.eq_ignore_ascii_case("x-api-key") {
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
    assert_eq!(call.header("x-api-key"), Some("test-key"));
    assert_eq!(call.header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(call.header("content-type"), Some("application/json"));
    assert!(
        !call
            .header("user-agent")
            .unwrap_or("")
            .contains("Anthropic/Go")
    );
    assert!(
        call.headers
            .iter()
            .all(|(n, _)| !n.to_ascii_lowercase().starts_with("x-stainless"))
    );
}

/// Replays one fixture row: the scripted responses, the requests the client sent (method,
/// path, JSON body, and headers), the result or error text, and the call count.
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
    let client = AnthropicClient::new("test-key")
        .expect("client")
        .with_base_url(server.url())
        .with_retry(RetryPolicy::new(
            3,
            Duration::from_millis(1),
            Duration::from_millis(2),
        ));
    let result = client.complete(request(&case["completion_request"])).await;
    let calls = server.calls().await;
    let context = format!("{id}\n{}", describe(&calls));

    let wire_requests = case["wire_requests"].as_array().expect("wire requests");
    let presend = case["error_string"]
        .as_str()
        .is_some_and(|e| e.contains("streaming is required"));
    if presend {
        // D33: the Rust client sends the request instead of refusing it.
        let _ = parity_support::divergence!("D33");
        assert_eq!(calls.len(), 1, "{context}");
        assert!(result.is_ok(), "{context}");
        return;
    }
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

/// parity: anthropic.TestCompleteWithoutSchemaRequestBodyUnchanged
#[tokio::test]
async fn plain() {
    replay("anthropic_plain").await;
}

/// parity: anthropic.TestCompleteSuccessMapsUsage
#[tokio::test]
async fn system_thinking_budget() {
    replay("anthropic_system_thinking_budget").await;
}

/// parity: anthropic.TestCompleteForcedToolUseRequestShapeAndResponseParsing
#[tokio::test]
async fn forced_tool() {
    replay("anthropic_forced_tool").await;
}

/// parity: anthropic.TestCompleteForcedToolUseMissingToolUseBlockErrors
#[tokio::test]
async fn forced_tool_missing_block() {
    replay("anthropic_forced_tool_missing_block").await;
}

/// parity: anthropic.TestCompleteRateLimitRetriesThenFails
#[tokio::test]
async fn rate_limit_retries_then_fails() {
    replay("anthropic_429_exhausts").await;
}

/// parity: anthropic.TestCompleteServerErrorRetriesThenFails
#[tokio::test]
async fn server_error_retries_then_fails() {
    replay("anthropic_500_exhausts").await;
}

/// parity: anthropic.TestCompleteInvalidRequestDoesNotRetry
#[tokio::test]
async fn invalid_request_does_not_retry() {
    replay("anthropic_400_no_retry").await;
}

/// parity: anthropic.TestCompleteRecoversAfterRetryableError
#[tokio::test]
async fn recovers_after_retryable_error() {
    replay("anthropic_429_then_ok").await;
}

/// Every Anthropic row, including the probes (thinking defaults, float32 temperature,
/// block concatenation, refusals, 529, 408, plain-text errors, malformed JSON, the
/// schema type error, and the D33 streaming-limit rows).
#[tokio::test]
async fn every_anthropic_row() {
    let ids: Vec<String> = wire()["cases"]
        .as_array()
        .expect("cases")
        .iter()
        .filter(|c| c["provider"] == "anthropic")
        .map(|c| c["id"].as_str().expect("id").to_owned())
        .collect();
    assert!(ids.len() >= 20);
    for id in ids {
        replay(&id).await;
    }
}

/// A caller deadline in the middle of the retry backoff ends the call as a timeout
/// before the attempts run out.
///
/// parity: anthropic.TestCompleteContextTimeoutMidRetry
#[tokio::test]
async fn context_timeout_mid_retry() {
    let setup = &wire()["context_timeout_mid_retry"];
    assert_eq!(setup["expect"]["kind"], "timeout");
    let server = ScriptedTestServer::start(vec![ScriptedResponse::json(
        429,
        br#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#.to_vec(),
    )])
    .await
    .expect("server");
    let client = AnthropicClient::new("test-key")
        .expect("client")
        .with_base_url(server.url())
        .with_retry(RetryPolicy::new(
            3,
            Duration::from_millis(500),
            Duration::from_secs(1),
        ));
    let request = request(&case("anthropic_plain")["completion_request"]);
    let outcome = tokio::time::timeout(Duration::from_millis(100), client.complete(request)).await;
    assert!(outcome.is_err(), "the caller's deadline ends the call");
    assert!(server.call_count().await < 3);
}

/// D23: the base URL comes only from config; `ANTHROPIC_BASE_URL` changes nothing.
#[test]
fn default_base_url_ignores_the_environment() {
    let client = AnthropicClient::new("k").expect("client");
    assert_eq!(
        client.messages_url(),
        "https://api.anthropic.com/v1/messages"
    );
    assert_eq!(
        client.clone().with_base_url("http://h:1/").messages_url(),
        "http://h:1/v1/messages"
    );
    assert_eq!(client.name(), "anthropic");
}

#[test]
fn compact_keeps_order_and_escapes_html() {
    assert_eq!(
        go_compact("{ \"b\" : 1.50 ,\n \"a\": [ \"<x> & y\", \"\\u00e9\", \"q\\\"<\" ] }"),
        r#"{"b":1.50,"a":["\u003cx\u003e \u0026 y","\u00e9","q\"\u003c"]}"#
    );
    assert_eq!(go_compact("\"\u{2028}\u{2029}\""), r#""\u2028\u2029""#);
}
