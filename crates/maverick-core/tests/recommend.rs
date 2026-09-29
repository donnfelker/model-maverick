//! The recommend client (bead 6.1) against a mock engine.
//!
//! - `scenario_engine_calls`: the seven `scenarios.json` rows that exercise the engine
//!   call. Each row's inbound chat request goes through workload selection and the client;
//!   the test checks the exact request (method, path, headers, body) and how the client
//!   reads the scripted engine response. The Go-recorded body differs only by D5 (the
//!   prompt is projection `p1` with its version); mode and pool match Go exactly. The full
//!   proxy behavior of these rows (status to the caller, upstream calls) is 6.11's replay.
//! - Decoding rules, typed terminal privacy errors (D17), retries, timeout, connection
//!   failure, and the Rust response fields (D9, D14, D15).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use maverick_core::alias::parse_alias;
use maverick_core::recommend::{
    ChainEntry, EngineFailure, RecommendClient, RecommendClientConfig, RecommendError,
    RecommendRequest, Recommendation, RetentionMode, TenantKey, Workload,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

// ---------------------------------------------------------------------------------------
// A mock engine: one connection per scripted reply, every request recorded.

#[derive(Debug, Clone)]
struct Recorded {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Recorded {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

struct Reply {
    status: u16,
    body: String,
    delay: Duration,
}

fn reply(status: u16, body: impl Into<String>) -> Reply {
    Reply {
        status,
        body: body.into(),
        delay: Duration::ZERO,
    }
}

struct MockEngine {
    url: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl MockEngine {
    async fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock engine");
        let url = format!("http://{}", listener.local_addr().expect("mock address"));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        tokio::spawn(async move {
            for reply in replies {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let request = read_request(&mut socket).await;
                recorded.lock().expect("requests lock").push(request);
                tokio::time::sleep(reply.delay).await;
                let response = format!(
                    "HTTP/1.1 {} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    reply.status,
                    reply.body.len(),
                    reply.body
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        Self { url, requests }
    }

    fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().expect("requests lock").clone()
    }
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> Recorded {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 8192];
    let head_end = loop {
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
        let read = socket.read(&mut chunk).await.expect("read request");
        assert!(read > 0, "connection closed before the request head ended");
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8(buffer[..head_end].to_vec()).expect("utf-8 head");
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().expect("request line").split(' ');
    let method = request_line.next().expect("method").to_owned();
    let path = request_line.next().expect("path").to_owned();
    let headers: Vec<(String, String)> = lines
        .filter(|line| !line.is_empty())
        .filter_map(|line| line.split_once(": "))
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect();
    let length: usize = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map_or(0, |(_, value)| value.parse().expect("content-length"));
    let mut body = buffer[head_end..].to_vec();
    while body.len() < length {
        let read = socket.read(&mut chunk).await.expect("read body");
        assert!(read > 0, "connection closed before the body ended");
        body.extend_from_slice(&chunk[..read]);
    }
    Recorded {
        method,
        path,
        headers,
        body: String::from_utf8(body).expect("utf-8 body"),
    }
}

fn client(base_url: &str, timeout: Duration, max_retries: u8) -> RecommendClient {
    let http = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("http client");
    RecommendClient::with_http(
        http,
        RecommendClientConfig {
            base_url: base_url.to_owned(),
            timeout,
            max_retries,
        },
    )
}

fn key() -> TenantKey {
    TenantKey::new("mk_live_test")
}

fn prompt_request(alias: &str, prompt: &str) -> RecommendRequest {
    RecommendRequest::for_alias(
        &parse_alias(alias).expect("alias"),
        Workload::Prompt {
            prompt: prompt.to_owned(),
            projection_version: Some("p1".to_owned()),
        },
    )
}

/// Send one request to an engine that answers `status` / `body` once.
async fn call_with(status: u16, body: &str) -> Result<Recommendation, RecommendError> {
    let engine = MockEngine::start(vec![reply(status, body)]).await;
    client(&engine.url, Duration::from_secs(5), 0)
        .recommend(&key(), &prompt_request("maverick/auto", "q"))
        .await
}

fn entry(provider: &str, model: &str) -> ChainEntry {
    ChainEntry {
        provider: provider.to_owned(),
        model: model.to_owned(),
        ..ChainEntry::default()
    }
}

// ---------------------------------------------------------------------------------------
// scenarios.json rows that exercise the engine call.

/// The exact Rust request body for each row's inbound request, computed by hand.
/// D5: the prompt is projection p1 with `projection_version`; Go sent the last user text.
/// `chat_declared_fallback` has a system message only: p1 projects it, so Rust sends the
/// prompt form where Go sent the declared fallback.
const SCENARIO_BODIES: [(&str, &str); 7] = [
    (
        "chat_declared_fallback",
        r#"{"mode":"balanced","workload":{"projection_version":"p1","prompt":"[system]\nsys only, no user turn"}}"#,
    ),
    (
        "chat_prompt_last_user",
        r#"{"mode":"balanced","workload":{"projection_version":"p1","prompt":"[system]\nsys\n\n[conversation]\nuser: first question\nassistant: answer\nuser: second question, the real one"}}"#,
    ),
    (
        "x_chat_prompt_escaping_and_array",
        "{\"mode\":\"cost\",\"workload\":{\"projection_version\":\"p1\",\"prompt\":\"[conversation]\\nuser: a\\u003cb \\u0026 \\nc\\u003ed\"}}",
    ),
    (
        "chat_pool_mode_alias",
        r#"{"mode":"quality","pool":"prod-pool","workload":{"projection_version":"p1","prompt":"[conversation]\nuser: hi"}}"#,
    ),
    (
        "chat_engine_error_502",
        r#"{"mode":"balanced","workload":{"projection_version":"p1","prompt":"[conversation]\nuser: hi"}}"#,
    ),
    (
        "x_engine_malformed_json_502",
        r#"{"mode":"balanced","workload":{"projection_version":"p1","prompt":"[conversation]\nuser: hi"}}"#,
    ),
    (
        "x_engine_empty_chain_502",
        r#"{"mode":"balanced","workload":{"projection_version":"p1","prompt":"[conversation]\nuser: hi"}}"#,
    ),
];

fn assert_outcome(id: &str, outcome: &Result<Recommendation, RecommendError>) {
    let ok = |recommendation_id: &str, chain: Vec<ChainEntry>| Recommendation {
        recommendation_id: recommendation_id.to_owned(),
        chain,
        measured: true,
        classification_method: "declared".to_owned(),
        ..Recommendation::default()
    };
    let gpt_a = || vec![entry("openai", "gpt-a")];
    match id {
        "chat_declared_fallback" => assert_eq!(outcome, &Ok(ok("rec_declared", gpt_a())), "{id}"),
        "chat_prompt_last_user" => assert_eq!(outcome, &Ok(ok("rec_prompt", gpt_a())), "{id}"),
        "x_chat_prompt_escaping_and_array" => {
            assert_eq!(outcome, &Ok(ok("rec_esc", gpt_a())), "{id}")
        }
        "chat_pool_mode_alias" => assert_eq!(outcome, &Ok(ok("rec_pool", gpt_a())), "{id}"),
        // Go: 502 engine_error.
        "chat_engine_error_502" => assert_eq!(
            outcome,
            &Err(RecommendError::Engine(EngineFailure::Status {
                status: 500,
                code: Some("internal_error".to_owned()),
            })),
            "{id}"
        ),
        // Go: 502 engine_error.
        "x_engine_malformed_json_502" => assert!(
            matches!(
                outcome,
                Err(RecommendError::Engine(EngineFailure::Decode(_)))
            ),
            "{id}: {outcome:?}"
        ),
        // Go decoded this; its chain walk then answered 502 no_executable_candidates.
        "x_engine_empty_chain_502" => assert_eq!(outcome, &Ok(ok("rec_empty", Vec::new())), "{id}"),
        other => panic!("no expectation for scenario {other}"),
    }
}

#[tokio::test]
async fn scenario_engine_calls() {
    let doc = parity_support::fixture("proxy", "scenarios.json").expect("scenarios.json");
    let scenarios = doc["scenarios"].as_array().expect("scenarios");
    for (id, want_body) in SCENARIO_BODIES {
        let scenario = scenarios
            .iter()
            .find(|scenario| scenario["id"] == id)
            .unwrap_or_else(|| panic!("scenario {id}"));

        // The proxy's inputs: the inbound chat body, its alias, and the tenant key.
        let inbound = scenario["inbound_request"]["body"]
            .as_str()
            .expect("inbound body");
        let inbound_json: Value = serde_json::from_str(inbound).expect("inbound json");
        let alias = parse_alias(inbound_json["model"].as_str().expect("model")).expect("alias");
        let workload_header = scenario["inbound_request"]["headers"]
            .as_array()
            .expect("headers")
            .iter()
            .find(|header| {
                header[0]
                    .as_str()
                    .is_some_and(|name| name.eq_ignore_ascii_case("x-maverick-workload"))
            })
            .and_then(|header| header[1].as_str());
        let tenant_key = scenario["proxy_env"]["MAVERICK_API_KEY"]
            .as_str()
            .expect("key");

        let engine_response = &scenario["engine_recommend_response"];
        let engine = MockEngine::start(vec![reply(
            u16::try_from(engine_response["status"].as_u64().expect("status")).expect("u16"),
            engine_response["body"].as_str().expect("engine body"),
        )])
        .await;

        let workload =
            Workload::for_chat_request(inbound.as_bytes(), workload_header).expect("workload");
        let request = RecommendRequest::for_alias(&alias, workload);
        let outcome = client(&engine.url, Duration::from_secs(5), 0)
            .recommend(&TenantKey::new(tenant_key), &request)
            .await;

        let sent = engine.requests();
        assert_eq!(sent.len(), 1, "{id}: one engine call, no retries");
        let sent = &sent[0];
        let go = &scenario["expected"]["recommend_requests"][0];
        assert_eq!(sent.method, go["method"], "{id}: method");
        assert_eq!(sent.path, go["path"], "{id}: path");
        assert_eq!(
            sent.header("authorization"),
            Some(format!("Bearer {tenant_key}").as_str()),
            "{id}"
        );
        assert_eq!(
            sent.header("content-type"),
            Some("application/json"),
            "{id}"
        );
        assert_eq!(
            sent.header("x-api-key"),
            None,
            "{id}: no provider credential"
        );
        assert_eq!(sent.body, want_body, "{id}: body");

        // Against the Go-recorded body: mode and pool match; only the workload moved (D5).
        let rust: Value = serde_json::from_str(&sent.body).expect("rust body json");
        let go_body: Value =
            serde_json::from_str(go["body"].as_str().expect("go body")).expect("go json");
        assert_eq!(rust["mode"], go_body["mode"], "{id}: mode");
        assert_eq!(rust["pool"], go_body["pool"], "{id}: pool");

        assert_outcome(id, &outcome);
    }
}

#[tokio::test]
async fn nothing_to_project_sends_the_go_declared_fallback_body() {
    let engine = MockEngine::start(vec![reply(
        200,
        r#"{"recommendation_id":"rec_1","recommendations":[{"provider":"openai","model":"gpt-a"}]}"#,
    )])
    .await;
    // Only an image part: no projection, so the body is byte-for-byte Go's fallback.
    let inbound = br#"{"model":"maverick/auto","messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"x"}}]}]}"#;
    let workload = Workload::for_chat_request(inbound, None).expect("workload");
    assert_eq!(workload, Workload::declared_fallback());
    let request =
        RecommendRequest::for_alias(&parse_alias("maverick/auto").expect("alias"), workload);
    let outcome = client(&engine.url, Duration::from_secs(5), 0)
        .recommend(&key(), &request)
        .await
        .expect("recommendation");
    assert_eq!(outcome.chain, vec![entry("openai", "gpt-a")]);
    assert_eq!(
        engine.requests()[0].body,
        r#"{"mode":"balanced","workload":{"capability":"multi_step_reasoning","difficulty":3}}"#
    );
}

#[tokio::test]
async fn workload_header_sends_the_declared_form_and_a_bad_one_sends_nothing() {
    let inbound =
        br#"{"model":"maverick/auto","messages":[{"role":"user","content":"private question"}]}"#;
    let engine = MockEngine::start(vec![reply(200, r#"{"recommendations":[]}"#)]).await;
    let workload =
        Workload::for_chat_request(inbound, Some("code_generation:4")).expect("workload");
    let request =
        RecommendRequest::for_alias(&parse_alias("maverick/cost").expect("alias"), workload);
    client(&engine.url, Duration::from_secs(5), 0)
        .recommend(&key(), &request)
        .await
        .expect("recommendation");
    let sent = &engine.requests()[0].body;
    assert_eq!(
        sent,
        r#"{"mode":"cost","workload":{"capability":"code_generation","difficulty":4}}"#
    );
    assert!(!sent.contains("private question"));

    // A malformed header is an error before any request: never a projection fallback.
    assert!(Workload::for_chat_request(inbound, Some("code_generation:hard")).is_err());
}

// ---------------------------------------------------------------------------------------
// Response decoding.

#[tokio::test]
async fn rust_response_fields_decode() {
    let body = json!({
        "classification": {"capability": "structured_extraction", "method": "classified", "confidence": 0.91},
        "recommendations": [
            {
                "provider": "anthropic", "model": "claude-sonnet-4-6", "endpoint": "vertex",
                "column_id": "col_1", "settings": {"thinking": true, "effort": "high"},
                "retention_mode": "zero", "score": 94.1, "reason": "cheapest",
                "execution_aliases": {"openrouter": "anthropic/claude-sonnet-4.6"},
                "execution_alias_retention": {"openrouter": "zero"}
            },
            {"provider": "google", "model": "gemini-3.1-pro", "retention_mode": "standard"}
        ],
        "recommendation_id": "rec_x",
        "measured": false,
        "zero_retention": true,
        "classification_quota_exceeded": true,
        "matrix_version": "2026-07-01"
    });
    let decoded = call_with(200, &body.to_string())
        .await
        .expect("recommendation");
    let mut first = entry("anthropic", "claude-sonnet-4-6");
    first.endpoint = "vertex".to_owned();
    first.column_id = "col_1".to_owned();
    first.settings = json!({"thinking": true, "effort": "high"})
        .as_object()
        .expect("object")
        .clone();
    first.retention_mode = RetentionMode::Zero;
    first.execution_aliases = [(
        "openrouter".to_owned(),
        "anthropic/claude-sonnet-4.6".to_owned(),
    )]
    .into();
    first.execution_alias_retention = [("openrouter".to_owned(), RetentionMode::Zero)].into();
    let mut second = entry("google", "gemini-3.1-pro");
    second.retention_mode = RetentionMode::Standard;
    assert_eq!(
        decoded,
        Recommendation {
            recommendation_id: "rec_x".to_owned(),
            chain: vec![first, second],
            measured: false,
            classification_method: "classified".to_owned(),
            zero_retention: true,
            classification_quota_exceeded: true,
        }
    );
}

#[tokio::test]
async fn go_decoding_rules() {
    // Case-insensitive names, including Go's fold of U+017F LONG S; missing keys are zero.
    let decoded = call_with(
        200,
        "{\"RECOMMENDATION_ID\":\"r\",\"Recommendations\":[{\"PROVIDER\":\"p\",\"Model\":\"m\",\"\u{17f}ettings\":{\"a\":1}}],\"Classification\":{\"METHOD\":\"cached\"}}",
    )
    .await
    .expect("case-insensitive decode");
    assert_eq!(decoded.recommendation_id, "r");
    assert_eq!(decoded.classification_method, "cached");
    assert!(!decoded.measured && !decoded.zero_retention && !decoded.classification_quota_exceeded);
    assert_eq!(decoded.chain.len(), 1);
    assert_eq!(
        (
            decoded.chain[0].provider.as_str(),
            decoded.chain[0].model.as_str()
        ),
        ("p", "m")
    );
    assert_eq!(decoded.chain[0].settings.get("a"), Some(&json!(1)));
    assert_eq!(decoded.chain[0].retention_mode, RetentionMode::Unknown);

    // null: no effect on strings and bools, clears slices; a null element is a zero entry.
    let decoded = call_with(
        200,
        r#"{"recommendation_id":null,"measured":null,"recommendations":[null,{"model":"m","settings":null}]}"#,
    )
    .await
    .expect("nulls decode");
    assert_eq!(decoded.recommendation_id, "");
    assert_eq!(decoded.chain, vec![ChainEntry::default(), entry("", "m")]);
    assert_eq!(call_with(200, "null").await, Ok(Recommendation::default()));
    assert_eq!(
        call_with(200, r#"{"recommendations":null}"#).await,
        Ok(Recommendation::default())
    );

    // Unknown members of any type are ignored.
    assert_eq!(
        call_with(
            200,
            r#"{"unknown":[1,{"x":null}],"recommendations":[{"weird":5}]}"#
        )
        .await
        .map(|decoded| decoded.chain.len()),
        Ok(1)
    );

    // A wrong type anywhere in a known field fails the whole decode.
    for body in [
        "",
        "not json",
        "[]",
        "\"x\"",
        "{} trailing",
        r#"{"recommendation_id":5}"#,
        r#"{"measured":"true"}"#,
        r#"{"recommendations":{}}"#,
        r#"{"recommendations":[5]}"#,
        r#"{"recommendations":[{"model":1}]}"#,
        r#"{"recommendations":[{"settings":[]}]}"#,
        r#"{"recommendations":[{"execution_aliases":{"openrouter":1}}]}"#,
        r#"{"classification":"classified"}"#,
        r#"{"MODEL_ID":1,"recommendations":[{"Model":1,"model":"m"}]}"#,
    ] {
        let outcome = call_with(200, body).await;
        assert!(
            matches!(
                outcome,
                Err(RecommendError::Engine(EngineFailure::Decode(_)))
            ),
            "{body:?}: {outcome:?}"
        );
    }
}

// ---------------------------------------------------------------------------------------
// Errors.

#[tokio::test]
async fn terminal_privacy_errors_are_typed() {
    let zr = call_with(
        422,
        r#"{"error":{"code":"zero_retention_requires_declared","message":"send the declared form"}}"#,
    )
    .await;
    assert_eq!(
        zr,
        Err(RecommendError::ZeroRetentionRequiresDeclared {
            status: 422,
            message: "send the declared form".to_owned(),
        })
    );
    assert!(zr.expect_err("terminal").is_terminal());

    let ncc = call_with(
        409,
        r#"{"error":{"code":"no_compliant_columns","message":"no column in pool \"p\" holds valid zero-retention evidence","details":{"pool":"p"}}}"#,
    )
    .await;
    assert!(
        matches!(&ncc, Err(RecommendError::NoCompliantColumns { status: 409, message }) if message.contains("zero-retention evidence")),
        "{ncc:?}"
    );
    assert!(ncc.expect_err("terminal").is_terminal());

    // Other 409 and 422 codes are ordinary engine errors.
    for (status, code) in [
        (409, "zero_retention_conflict"),
        (422, "invalid_request"),
        (422, "unknown_pool"),
    ] {
        let outcome = call_with(
            status,
            &json!({"error": {"code": code, "message": "m"}}).to_string(),
        )
        .await;
        assert_eq!(
            outcome,
            Err(RecommendError::Engine(EngineFailure::Status {
                status,
                code: Some(code.to_owned()),
            }))
        );
        assert!(!outcome.expect_err("engine error").is_terminal());
    }
    assert_eq!(
        call_with(502, "<html>bad gateway</html>").await,
        Err(RecommendError::Engine(EngineFailure::Status {
            status: 502,
            code: None
        }))
    );
}

#[tokio::test]
async fn one_retry_on_5xx_and_never_on_terminal_errors() {
    let success = r#"{"recommendation_id":"r","recommendations":[{"provider":"p","model":"m"}]}"#;

    let engine = MockEngine::start(vec![reply(500, "{}"), reply(200, success)]).await;
    let outcome = client(&engine.url, Duration::from_secs(5), 1)
        .recommend(&key(), &prompt_request("maverick/auto", "q"))
        .await;
    assert_eq!(
        outcome.map(|decoded| decoded.recommendation_id),
        Ok("r".to_owned())
    );
    assert_eq!(engine.requests().len(), 2);
    assert_eq!(engine.requests()[0].body, engine.requests()[1].body);

    // max_retries is capped at one.
    let engine = MockEngine::start(vec![
        reply(503, "{}"),
        reply(503, "{}"),
        reply(200, success),
    ])
    .await;
    let outcome = client(&engine.url, Duration::from_secs(5), 7)
        .recommend(&key(), &prompt_request("maverick/auto", "q"))
        .await;
    assert!(matches!(
        outcome,
        Err(RecommendError::Engine(EngineFailure::Status {
            status: 503,
            ..
        }))
    ));
    assert_eq!(engine.requests().len(), 2);

    // No retries by default, and none after a terminal error or a 4xx.
    for (retries, first) in [
        (0, reply(500, "{}")),
        (
            1,
            reply(
                422,
                r#"{"error":{"code":"zero_retention_requires_declared"}}"#,
            ),
        ),
        (
            1,
            reply(409, r#"{"error":{"code":"no_compliant_columns"}}"#),
        ),
        (1, reply(422, r#"{"error":{"code":"invalid_request"}}"#)),
        (1, reply(200, "not json")),
    ] {
        let engine = MockEngine::start(vec![first, reply(200, success)]).await;
        let outcome = client(&engine.url, Duration::from_secs(5), retries)
            .recommend(&key(), &prompt_request("maverick/auto", "q"))
            .await;
        assert!(outcome.is_err());
        assert_eq!(engine.requests().len(), 1, "{outcome:?}");
    }
}

#[tokio::test]
async fn the_deadline_bounds_the_call() {
    let engine = MockEngine::start(vec![Reply {
        status: 200,
        body: r#"{"recommendations":[]}"#.to_owned(),
        delay: Duration::from_secs(3),
    }])
    .await;
    let started = Instant::now();
    let outcome = client(&engine.url, Duration::from_millis(250), 1)
        .recommend(&key(), &prompt_request("maverick/auto", "q"))
        .await;
    assert_eq!(outcome, Err(RecommendError::Engine(EngineFailure::Timeout)));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(engine.requests().len(), 1, "a timeout is not retried");
}

#[tokio::test]
async fn connection_failure_is_a_transport_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("address"));
    drop(listener);
    let outcome = client(&url, Duration::from_secs(2), 0)
        .recommend(&key(), &prompt_request("maverick/auto", "q"))
        .await;
    assert!(
        matches!(
            &outcome,
            Err(RecommendError::Engine(EngineFailure::Transport(_)))
        ),
        "{outcome:?}"
    );
    assert!(!outcome.expect_err("transport").is_terminal());
}

#[tokio::test]
async fn base_url_trailing_slash_and_invalid_key() {
    let engine = MockEngine::start(vec![reply(200, r#"{"recommendations":[]}"#)]).await;
    client(&format!("{}/", engine.url), Duration::from_secs(5), 0)
        .recommend(&key(), &prompt_request("maverick/auto", "q"))
        .await
        .expect("recommendation");
    assert_eq!(engine.requests()[0].path, "/v1/recommend");

    let outcome = client(&engine.url, Duration::from_secs(5), 1)
        .recommend(
            &TenantKey::new("mk_live_bad\nkey"),
            &prompt_request("maverick/auto", "q"),
        )
        .await;
    assert_eq!(
        outcome,
        Err(RecommendError::Engine(EngineFailure::InvalidTenantKey))
    );
    assert_eq!(
        engine.requests().len(),
        1,
        "nothing sent for an invalid key"
    );
}
