//! Parity: the Google rows of `parity/fixtures/measurement-io/providers_wire.json`,
//! replayed against the scripted test server. Google's request bodies compare as exact
//! bytes (the reference fixes them, trailing newline included).

use std::time::Duration;

use maverick_providers::google::{GoogleClient, encode_body};
use maverick_providers::{
    CompletionRequest, ProviderClient, RecordedRequest, RetryPolicy, ScriptedResponse,
    ScriptedTestServer,
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

fn describe(calls: &[RecordedRequest]) -> String {
    calls
        .iter()
        .map(|c| {
            let headers: Vec<String> = c
                .headers
                .iter()
                .map(|(n, v)| {
                    if n.eq_ignore_ascii_case("x-goog-api-key") {
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
    assert_eq!(call.header("x-goog-api-key"), Some("test-key"));
    assert_eq!(call.header("content-type"), Some("application/json"));
    assert_eq!(call.header("x-goog-api-client"), None);
    assert!(
        !call
            .header("user-agent")
            .unwrap_or("")
            .contains("google-genai-sdk")
    );
}

/// Replays one fixture row: the scripted responses, the requests the client sent (method,
/// path, exact body bytes, and headers), the result or error text, and the call count.
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
    let client = GoogleClient::new("test-key")
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
    assert_eq!(calls.len(), wire_requests.len(), "{context}");
    assert_eq!(Value::from(calls.len()), case["calls"], "{context}");
    for (call, want) in calls.iter().zip(wire_requests) {
        assert_eq!(call.method, want["method"], "{context}");
        assert_eq!(call.path, want["path"], "{context}");
        assert_eq!(
            String::from_utf8_lossy(&call.body),
            want["body"].as_str().expect("body"),
            "{context}"
        );
        assert_headers(call);
    }

    match (&result, case["completion_response"].as_object()) {
        (Ok(response), Some(want)) => {
            assert_eq!(response.text, want["Text"], "{context}");
            assert_eq!(response.tokens_in, want["TokensIn"], "{context}");
            assert_eq!(response.tokens_out, want["TokensOut"], "{context}");
        }
        (Err(error), None) => {
            let want = case["error_string"].as_str().expect("error string");
            assert_eq!(error.to_string(), want, "{context}");
            assert_eq!(error.kind().as_str(), case["error_kind"], "{context}");
            let status = case["error_status"].as_i64().unwrap_or(0);
            assert_eq!(i64::from(error.status()), status, "{context}");
            if let Some(attempts) = case["error_attempts"].as_u64() {
                assert_eq!(error.attempts() as u64, attempts, "{context}");
            }
        }
        (got, _) => panic!("{context}: unexpected result {got:?}"),
    }
}

/// parity: google.TestCompleteSuccessMapsUsage
#[tokio::test]
async fn system_thinking_budget() {
    replay("google_system_thinking_budget").await;
}

/// parity: google.TestCompleteRateLimitRetriesThenFails
#[tokio::test]
async fn rate_limit_retries_then_fails() {
    replay("google_429_exhausts").await;
}

/// parity: google.TestCompleteServerErrorRetriesThenFails
#[tokio::test]
async fn server_error_retries_then_fails() {
    replay("google_500_exhausts").await;
}

/// parity: google.TestCompleteInvalidRequestDoesNotRetry
#[tokio::test]
async fn invalid_request_does_not_retry() {
    replay("google_400_no_retry").await;
}

/// parity: google.TestCompleteRecoversAfterRetryableError
#[tokio::test]
async fn recovers_after_retryable_error() {
    replay("google_429_then_ok").await;
}

/// Every Google row, including the probes (float32 temperature, thought parts, missing
/// usage, a blocked prompt, the body code over the HTTP status, a body without a code,
/// and non-JSON or `error`-less error bodies).
#[tokio::test]
async fn every_google_row() {
    let ids: Vec<String> = wire()["cases"]
        .as_array()
        .expect("cases")
        .iter()
        .filter(|c| c["provider"] == "google")
        .map(|c| c["id"].as_str().expect("id").to_owned())
        .collect();
    assert_eq!(ids.len(), 14);
    for id in ids {
        replay(&id).await;
    }
}

/// A caller deadline in the middle of the retry backoff ends the call as a timeout
/// before the attempts run out.
///
/// parity: google.TestCompleteContextTimeoutMidRetry
#[tokio::test]
async fn context_timeout_mid_retry() {
    let setup = &wire()["context_timeout_mid_retry"];
    assert_eq!(setup["expect"]["kind"], "timeout");
    let server = ScriptedTestServer::start(vec![ScriptedResponse::json(
        429,
        br#"{"error":{"code":429,"message":"rate limited","status":"RESOURCE_EXHAUSTED"}}"#
            .to_vec(),
    )])
    .await
    .expect("server");
    let client = GoogleClient::new("test-key")
        .expect("client")
        .with_base_url(server.url())
        .with_retry(RetryPolicy::new(
            3,
            Duration::from_millis(500),
            Duration::from_secs(1),
        ));
    let request = request(&case("google_plain")["completion_request"]);
    let outcome = tokio::time::timeout(Duration::from_millis(100), client.complete(request)).await;
    assert!(outcome.is_err(), "the caller's deadline ends the call");
    assert!(server.call_count().await < 3);
}

/// D23: the base URL comes only from config; `GOOGLE_GEMINI_BASE_URL` changes nothing.
#[test]
fn default_base_url_ignores_the_environment() {
    let _ = parity_support::divergence!("D23");
    let client = GoogleClient::new("k").expect("client");
    assert_eq!(
        client.generate_url("gemini-3.1-pro"),
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-3.1-pro:generateContent"
    );
    assert_eq!(
        client.clone().with_base_url("http://h:1").generate_url("m"),
        "http://h:1/v1beta/models/m:generateContent"
    );
    assert_eq!(client.name(), "google");
}

/// The body is Go's `json.Encoder` output: HTML characters and U+2028/U+2029 escaped.
#[test]
fn body_escapes_html_like_go() {
    let request = CompletionRequest {
        model: "m".to_owned(),
        system: "a & b".to_owned(),
        prompt: "<p>\u{2028}".to_owned(),
        temperature: 1.0,
        max_tokens: 8,
        settings: Map::new(),
    };
    assert_eq!(
        String::from_utf8(encode_body(&request).expect("encodes")).expect("utf-8"),
        concat!(
            r#"{"contents":[{"parts":[{"text":"\u003cp\u003e\u2028"}],"role":"user"}],"#,
            r#""generationConfig":{"maxOutputTokens":8,"temperature":1},"#,
            r#""systemInstruction":{"parts":[{"text":"a \u0026 b"}],"role":"user"}}"#,
            "\n"
        )
    );
}

/// The API key never appears in the client's Debug output (G5).
#[test]
fn debug_hides_the_key() {
    let client = GoogleClient::new("secret-key").expect("client");
    assert!(!format!("{client:?}").contains("secret-key"));
}
