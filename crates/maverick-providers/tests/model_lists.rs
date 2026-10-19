//! DIVERGENCE: D51. Provider model discovery (spec §4.6): each client's model-list call,
//! replayed against the scripted test server. The payloads are hand-written in each
//! provider's documented shape, never recorded from a live key. Every client gets the
//! mock's URL, so no request can leave the machine, and each test asserts that its calls
//! reached the mock.

use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use maverick_providers::anthropic::AnthropicClient;
use maverick_providers::fake::FakeModelLister;
use maverick_providers::google::GoogleClient;
use maverick_providers::models::{ListedModel, MAX_LIST_PAGES, ModelLister};
use maverick_providers::openai::OpenAiClient;
use maverick_providers::{
    ProviderErrorKind, RecordedRequest, RetryPolicy, ScriptedResponse, ScriptedTestServer,
};

fn fast_retry() -> RetryPolicy {
    RetryPolicy::new(3, Duration::from_millis(1), Duration::from_millis(2))
}

fn utc(year: i32, month: u32, day: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(year, month, day, 0, 0, 0)
        .single()
        .expect("valid date")
}

fn listed(
    provider: &str,
    model_id: &str,
    display_name: Option<&str>,
    created_at: Option<DateTime<Utc>>,
) -> ListedModel {
    ListedModel {
        provider: provider.to_owned(),
        model_id: model_id.to_owned(),
        display_name: display_name.map(str::to_owned),
        created_at,
    }
}

async fn server(pages: &[&str]) -> ScriptedTestServer {
    ScriptedTestServer::start(
        pages
            .iter()
            .map(|page| ScriptedResponse::json(200, page.as_bytes().to_vec()))
            .collect(),
    )
    .await
    .expect("mock server")
}

fn paths(calls: &[RecordedRequest]) -> Vec<&str> {
    calls.iter().map(|call| call.path.as_str()).collect()
}

/// A list call is a bare GET: no body, and none of the Go SDKs' identity headers (D32).
fn assert_bare_get(call: &RecordedRequest) {
    assert_eq!(call.method, "GET");
    assert!(call.body.is_empty(), "a list call sends no body");
    assert!(
        call.headers
            .iter()
            .all(|(name, _)| !name.to_ascii_lowercase().starts_with("x-stainless"))
    );
}

fn anthropic(base_url: String) -> AnthropicClient {
    AnthropicClient::new("test-key")
        .expect("client")
        .with_base_url(base_url)
        .with_retry(fast_retry())
}

fn openai(base_url: String) -> OpenAiClient {
    OpenAiClient::openai("test-key")
        .expect("client")
        .with_base_url(base_url)
        .with_retry(fast_retry())
}

fn google(base_url: String) -> GoogleClient {
    GoogleClient::new("test-key")
        .expect("client")
        .with_base_url(base_url)
        .with_retry(fast_retry())
}

#[tokio::test]
async fn anthropic_reads_every_page_and_keeps_the_text_models() {
    let _ = parity_support::divergence!("D51");
    let server = server(&[
        r#"{"data":[
            {"type":"model","id":"claude-opus-4-1-20250805","display_name":"Claude Opus 4.1","created_at":"2025-08-05T00:00:00Z"},
            {"type":"model","id":"claude-haiku-4-5-20251001","display_name":"Claude Haiku 4.5","created_at":"2025-10-15T00:00:00+00:00"}
        ],"has_more":true,"first_id":"claude-opus-4-1-20250805","last_id":"claude-haiku-4-5-20251001"}"#,
        r#"{"data":[
            {"type":"model","id":"claude-3-haiku-20240307","display_name":"Claude Haiku 3","created_at":"2024-03-07T00:00:00Z"},
            {"type":"model","id":"claude-sonnet-4-5-latest","display_name":"Claude Sonnet 4.5"},
            {"type":"model","id":"claude-next-1","display_name":"","created_at":"not a time"}
        ],"has_more":false,"first_id":"claude-3-haiku-20240307","last_id":"claude-next-1"}"#,
    ])
    .await;
    let client = anthropic(server.url());
    assert_eq!(client.provider(), "anthropic");

    let models = client.list_models().await.expect("list");

    assert_eq!(
        models,
        vec![
            listed(
                "anthropic",
                "claude-opus-4-1-20250805",
                Some("Claude Opus 4.1"),
                Some(utc(2025, 8, 5))
            ),
            listed(
                "anthropic",
                "claude-haiku-4-5-20251001",
                Some("Claude Haiku 4.5"),
                Some(utc(2025, 10, 15))
            ),
            listed(
                "anthropic",
                "claude-3-haiku-20240307",
                Some("Claude Haiku 3"),
                Some(utc(2024, 3, 7))
            ),
            // An empty display name and a time that does not parse are simply absent.
            listed("anthropic", "claude-next-1", None, None),
        ]
    );
    let calls = server.calls().await;
    assert_eq!(
        paths(&calls),
        [
            "/v1/models?limit=1000",
            "/v1/models?limit=1000&after_id=claude-haiku-4-5-20251001"
        ]
    );
    for call in &calls {
        assert_bare_get(call);
        assert_eq!(call.header("x-api-key"), Some("test-key"));
        assert_eq!(call.header("anthropic-version"), Some("2023-06-01"));
    }
}

/// A page that says it has more but not where they start would cut the list short, and a
/// cut list reads as withdrawn models, so it fails.
#[tokio::test]
async fn anthropic_has_more_without_last_id_fails() {
    let server = server(&[r#"{"data":[],"has_more":true}"#]).await;

    let error = anthropic(server.url())
        .list_models()
        .await
        .expect_err("an unfinished list fails");

    assert_eq!(error.kind(), ProviderErrorKind::ProviderError);
    assert_eq!(
        error.to_string(),
        "anthropic: provider_error: model list: has_more without last_id"
    );
    assert_eq!(server.call_count().await, 1);
}

#[tokio::test]
async fn a_list_that_never_ends_stops_at_the_page_cap() {
    // The server repeats its last scripted answer, so every page says there is more.
    let server = server(&[r#"{"data":[],"has_more":true,"last_id":"claude-x"}"#]).await;

    let error = anthropic(server.url())
        .list_models()
        .await
        .expect_err("the cap ends the list");

    assert_eq!(
        error.to_string(),
        "anthropic: provider_error: model list: not finished after 20 pages"
    );
    assert_eq!(server.call_count().await, MAX_LIST_PAGES);
}

/// G5: a failed list call names the provider, the class, the status, and the URL, never
/// the response body.
#[tokio::test]
async fn a_server_error_is_retried_and_never_carries_the_body() {
    let server = ScriptedTestServer::start(vec![ScriptedResponse::json(
        503,
        br#"{"error":{"message":"body-text-must-not-leak"}}"#.to_vec(),
    )])
    .await
    .expect("mock server");

    let error = anthropic(server.url())
        .list_models()
        .await
        .expect_err("503 exhausts the retries");

    assert_eq!(error.kind(), ProviderErrorKind::ProviderError);
    assert_eq!(error.status(), 503);
    assert_eq!(error.attempts(), 3);
    assert_eq!(server.call_count().await, 3);
    assert_eq!(
        error.to_string(),
        format!(
            "anthropic: provider_error (status 503) after 3 attempt(s): GET \"{}/v1/models\": 503 Service Unavailable",
            server.url()
        )
    );
    assert!(!error.to_string().contains("body-text-must-not-leak"));
}

#[tokio::test]
async fn an_auth_failure_is_not_retried() {
    let server = ScriptedTestServer::start(vec![ScriptedResponse::json(
        401,
        br#"{"type":"error","error":{"type":"authentication_error"}}"#.to_vec(),
    )])
    .await
    .expect("mock server");

    let error = anthropic(server.url())
        .list_models()
        .await
        .expect_err("401 fails");

    assert_eq!(error.kind(), ProviderErrorKind::InvalidRequest);
    assert_eq!(error.status(), 401);
    assert_eq!(server.call_count().await, 1);
}

#[tokio::test]
async fn a_malformed_page_is_a_retried_provider_error() {
    let server = server(&["not json"]).await;

    let error = anthropic(server.url())
        .list_models()
        .await
        .expect_err("not JSON");

    assert_eq!(error.kind(), ProviderErrorKind::ProviderError);
    assert_eq!(error.status(), 0);
    assert!(
        error.to_string().contains("error parsing model list json"),
        "{error}"
    );
    assert_eq!(server.call_count().await, 3);
}

/// Spec §4.6: one example of every excluded class, plus the moving aliases, next to the
/// models that stay.
#[tokio::test]
async fn openai_keeps_the_text_models_and_drops_every_excluded_class() {
    let server = server(&[r#"{"object":"list","data":[
        {"id":"gpt-5.5","object":"model","created":1735689600,"owned_by":"system"},
        {"id":"o3","object":"model","created":0,"owned_by":"system"},
        {"id":"gpt-4o-mini-search-preview","object":"model","owned_by":"system"},
        {"id":"text-embedding-3-small","object":"model","created":1,"owned_by":"system"},
        {"id":"omni-moderation-2024-09-26","object":"model","created":1,"owned_by":"system"},
        {"id":"gpt-image-1","object":"model","created":1,"owned_by":"system"},
        {"id":"dall-e-3","object":"model","created":1,"owned_by":"system"},
        {"id":"gpt-4o-audio-preview","object":"model","created":1,"owned_by":"system"},
        {"id":"tts-1-hd","object":"model","created":1,"owned_by":"system"},
        {"id":"whisper-1","object":"model","created":1,"owned_by":"system"},
        {"id":"gpt-4o-transcribe","object":"model","created":1,"owned_by":"system"},
        {"id":"gpt-realtime","object":"model","created":1,"owned_by":"system"},
        {"id":"sora-2","object":"model","created":1,"owned_by":"system"},
        {"id":"ft:gpt-4o-mini:acme::abc123","object":"model","created":1,"owned_by":"acme"},
        {"id":"chatgpt-4o-latest","object":"model","created":1,"owned_by":"system"},
        {"id":"codex-mini-latest","object":"model","created":1,"owned_by":"system"}
    ]}"#])
    .await;
    let client = openai(format!("{}/v1", server.url()));
    assert_eq!(client.provider(), "openai");

    let models = client.list_models().await.expect("list");

    assert_eq!(
        models,
        vec![
            // 1735689600 is 2025-01-01T00:00:00Z. A zero or absent `created` is absent.
            listed("openai", "gpt-5.5", None, Some(utc(2025, 1, 1))),
            listed("openai", "o3", None, None),
            listed("openai", "gpt-4o-mini-search-preview", None, None),
        ]
    );
    let calls = server.calls().await;
    assert_eq!(paths(&calls), ["/v1/models"]);
    assert_bare_get(&calls[0]);
    assert_eq!(calls[0].header("authorization"), Some("Bearer test-key"));
}

#[tokio::test]
async fn xai_lists_through_the_openai_client_under_its_own_label() {
    let server = server(&[r#"{"object":"list","data":[
        {"id":"grok-4-0709","created":1752019200,"object":"model","owned_by":"xai"},
        {"id":"grok-2-image-1212","created":1,"object":"model","owned_by":"xai"},
        {"id":"grok-imagine-video","created":1,"object":"model","owned_by":"xai"}
    ]}"#])
    .await;
    let client = OpenAiClient::xai("test-key")
        .expect("client")
        .with_base_url(format!("{}/v1", server.url()))
        .with_retry(fast_retry());
    assert_eq!(client.provider(), "xai");

    let models = client.list_models().await.expect("list");

    // 1752019200 is 2025-07-09T00:00:00Z.
    assert_eq!(
        models,
        vec![listed("xai", "grok-4-0709", None, Some(utc(2025, 7, 9)))]
    );
    assert_eq!(paths(&server.calls().await), ["/v1/models"]);
}

#[tokio::test]
async fn an_xai_failure_carries_the_xai_label() {
    let server = ScriptedTestServer::start(vec![ScriptedResponse::json(429, b"{}".to_vec())])
        .await
        .expect("mock server");
    let client = OpenAiClient::xai("test-key")
        .expect("client")
        .with_base_url(server.url())
        .with_retry(fast_retry());

    let error = client.list_models().await.expect_err("429 exhausts");

    assert_eq!(error.provider(), "xai");
    assert_eq!(error.kind(), ProviderErrorKind::RateLimited);
    assert_eq!(server.call_count().await, 3);
}

#[tokio::test]
async fn google_reads_every_page_and_keeps_the_generate_content_models() {
    let server = server(&[
        r#"{"models":[
            {"name":"models/gemini-2.5-pro","version":"2.5","displayName":"Gemini 2.5 Pro","supportedGenerationMethods":["generateContent","countTokens"]},
            {"name":"models/text-embedding-004","displayName":"Text Embedding 004","supportedGenerationMethods":["embedContent"]},
            {"name":"models/imagen-4.0-generate-001","supportedGenerationMethods":["predict"]},
            {"name":"models/aqa","supportedGenerationMethods":[]}
        ],"nextPageToken":"tok/2="}"#,
        r#"{"models":[
            {"name":"models/gemini-2.5-flash-preview-tts","supportedGenerationMethods":["generateContent"]},
            {"name":"models/gemma-3-27b-it","displayName":"Gemma 3 27B","supportedGenerationMethods":["generateContent"]},
            {"name":"models/gemini-flash-latest","supportedGenerationMethods":["generateContent"]},
            {"name":"gemini-no-prefix","supportedGenerationMethods":["generateContent"]}
        ],"nextPageToken":""}"#,
    ])
    .await;
    let client = google(server.url());
    assert_eq!(client.provider(), "google");

    let models = client.list_models().await.expect("list");

    assert_eq!(
        models,
        vec![
            listed("google", "gemini-2.5-pro", Some("Gemini 2.5 Pro"), None),
            listed("google", "gemma-3-27b-it", Some("Gemma 3 27B"), None),
            listed("google", "gemini-no-prefix", None, None),
        ]
    );
    let calls = server.calls().await;
    // The page token is query-encoded, and an empty `nextPageToken` ends the list.
    assert_eq!(
        paths(&calls),
        [
            "/v1beta/models?pageSize=1000",
            "/v1beta/models?pageSize=1000&pageToken=tok%2F2%3D"
        ]
    );
    for call in &calls {
        assert_bare_get(call);
        assert_eq!(call.header("x-goog-api-key"), Some("test-key"));
    }
}

#[tokio::test]
async fn a_google_failure_uses_the_http_status() {
    let server = ScriptedTestServer::start(vec![ScriptedResponse::json(
        500,
        br#"{"error":{"code":13,"message":"internal","status":"INTERNAL"}}"#.to_vec(),
    )])
    .await
    .expect("mock server");

    let error = google(server.url())
        .list_models()
        .await
        .expect_err("500 exhausts");

    // Unlike a completion call, a list call keeps the HTTP status, and never the body's
    // `error.code` or message.
    assert_eq!(error.kind(), ProviderErrorKind::ProviderError);
    assert_eq!(error.status(), 500);
    assert_eq!(
        error.to_string(),
        format!(
            "google: provider_error (status 500) after 3 attempt(s): GET \"{}/v1beta/models\": 500 Internal Server Error",
            server.url()
        )
    );
    assert_eq!(server.call_count().await, 3);
}

#[tokio::test]
async fn the_fake_lister_returns_its_script() {
    let models = vec![listed(
        "anthropic",
        "claude-haiku-4-5-20251001",
        Some("Claude Haiku 4.5"),
        Some(utc(2025, 10, 15)),
    )];
    let lister = FakeModelLister::new("anthropic", models.clone());
    assert_eq!(lister.provider(), "anthropic");
    assert_eq!(lister.list_models().await.expect("list"), models);

    let failing = FakeModelLister::failing("openai", ProviderErrorKind::RateLimited);
    assert_eq!(failing.provider(), "openai");
    let error = failing.list_models().await.expect_err("scripted failure");
    assert_eq!(error.to_string(), "openai: rate_limited (status 429)");
}

/// D23: the list URLs come from each client's own config.
#[test]
fn the_default_list_urls() {
    assert_eq!(
        AnthropicClient::new("k").expect("client").models_url(),
        "https://api.anthropic.com/v1/models"
    );
    assert_eq!(
        OpenAiClient::openai("k").expect("client").models_url(),
        "https://api.openai.com/v1/models"
    );
    assert_eq!(
        OpenAiClient::xai("k").expect("client").models_url(),
        "https://api.x.ai/v1/models"
    );
    assert_eq!(
        GoogleClient::new("k").expect("client").models_url(),
        "https://generativelanguage.googleapis.com/v1beta/models"
    );
}
