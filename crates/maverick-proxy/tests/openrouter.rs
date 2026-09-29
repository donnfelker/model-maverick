//! OpenRouter execution on the hosted proxy (bead 6.10; D15; openrouter execution spec
//! §3.2, §4, §6 scenario 10 and 11; v1 spec §8.1).
//!
//! A hosted caller whose chat credential is an `sk-or-` key has every chain entry run on
//! OpenRouter with the entry's alias slug. For a zero-retention tenant every attempt
//! carries `provider.zdr: true`, an alias that is not verified `zero` is skipped, and a
//! chain with no such alias answers `409 no_compliant_columns` without any upstream
//! request. No attempt ever goes to a direct provider.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::Request;
use maverick_core::chain::ChainExecutor;
use maverick_core::recommend::{RecommendClient, RecommendClientConfig};
use maverick_proxy::config::load_config;
use maverick_proxy::server::{Proxy, ServeSettings, serve};
use maverick_proxy::surface::{ChainReport, ExecutorRunner, ReportSink, Upstreams};
use serde_json::{Value, json};
use tokio::net::TcpListener;

const CLOSED_PORT: &str = "http://127.0.0.1:1";
const TENANT_KEY: &str = "mk_live_tenant";
const OPENROUTER_KEY: &str = "sk-or-v1-caller";

/// One request a mock received: path and query, headers, body.
#[derive(Debug, Clone)]
struct Seen {
    target: String,
    headers: Vec<(String, String)>,
    body: Value,
}

type SeenLog = Arc<Mutex<Vec<Seen>>>;

/// A mock that records each request and answers from `replies` in order, then
/// `500 {"error":"unscripted request"}`.
async fn mock(replies: Vec<(u16, Value)>) -> (String, SeenLog) {
    let seen: SeenLog = Arc::default();
    let replies = Arc::new(Mutex::new(replies.into_iter().rev().collect::<Vec<_>>()));
    let handler = {
        let seen = seen.clone();
        move |request: Request| {
            let seen = seen.clone();
            let replies = replies.clone();
            async move {
                let target = request
                    .uri()
                    .path_and_query()
                    .map(ToString::to_string)
                    .unwrap_or_default();
                let headers = request
                    .headers()
                    .iter()
                    .map(|(name, value)| {
                        (
                            name.as_str().to_owned(),
                            value.to_str().unwrap_or_default().to_owned(),
                        )
                    })
                    .collect();
                let bytes: Bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
                    .await
                    .expect("body");
                let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                seen.lock().unwrap().push(Seen {
                    target,
                    headers,
                    body,
                });
                let (status, reply) = replies
                    .lock()
                    .unwrap()
                    .pop()
                    .unwrap_or((500, json!({"error": "unscripted request"})));
                (
                    axum::http::StatusCode::from_u16(status).expect("status"),
                    [("content-type", "application/json")],
                    reply.to_string(),
                )
            }
        }
    };
    let app = Router::new().fallback(handler);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
    (url, seen)
}

#[derive(Clone, Default)]
struct Reports(Arc<Mutex<Vec<ChainReport>>>);

impl ReportSink for Reports {
    fn report(&self, report: ChainReport) {
        self.0.lock().unwrap().push(report);
    }
}

struct Run {
    status: u16,
    body: Value,
    openrouter: Vec<Seen>,
    direct: Vec<Seen>,
    reports: Vec<ChainReport>,
}

/// One chat request through a hosted proxy whose engine answers `recommendation`.
async fn run(
    recommendation: Value,
    request_body: Value,
    openrouter_replies: Vec<(u16, Value)>,
) -> Run {
    let (engine, _) = mock(vec![(200, recommendation)]).await;
    let (openrouter, openrouter_seen) = mock(openrouter_replies).await;
    let (openai, direct_seen) = mock(Vec::new()).await;
    let env = [
        ("MAVERICK_PROXY_MODE", "hosted"),
        ("MAVERICK_ENGINE_URL", engine.as_str()),
        ("MAVERICK_PROXY_TELEMETRY", "off"),
        ("MAVERICK_UPSTREAM_OPENROUTER", openrouter.as_str()),
        ("MAVERICK_UPSTREAM_OPENAI", openai.as_str()),
        ("MAVERICK_UPSTREAM_ANTHROPIC", CLOSED_PORT),
        ("MAVERICK_UPSTREAM_GOOGLE", CLOSED_PORT),
        ("MAVERICK_UPSTREAM_XAI", CLOSED_PORT),
    ];
    let config = load_config(|name| {
        env.iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| (*value).to_owned())
    })
    .expect("hosted config");
    assert!(config.upstream_openrouter.starts_with("http://127.0.0.1:"));
    let reports = Reports::default();
    let http = reqwest::Client::builder().no_proxy().build().expect("http");
    let runner = ExecutorRunner::with_executor(
        ChainExecutor::new(http.clone()),
        Upstreams::from_config(&config),
        Arc::new(reports.clone()),
    );
    let engine_client = RecommendClient::with_http(
        http.clone(),
        RecommendClientConfig {
            base_url: config.engine_url.clone(),
            timeout: Duration::from_secs(5),
            max_retries: 0,
        },
    );
    let app = Proxy::with_engine(config, engine_client, Arc::new(runner)).router();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind proxy");
    let addr = listener.local_addr().expect("addr");
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(serve(listener, app, ServeSettings::default(), async move {
        let _ = stopped.await;
    }));

    let response = http
        .post(format!("http://{addr}/v1/chat/completions"))
        .header("X-Maverick-API-Key", TENANT_KEY)
        .header("Authorization", format!("Bearer {OPENROUTER_KEY}"))
        .header("Content-Type", "application/json")
        .body(request_body.to_string())
        .send()
        .await
        .expect("proxy response");
    let status = response.status().as_u16();
    let body = response.json::<Value>().await.unwrap_or(Value::Null);
    // The report arrives after the relay ends.
    for _ in 0..200 {
        if !reports.0.lock().unwrap().is_empty() || status == 409 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let _ = stop.send(());
    server.await.expect("server task").expect("clean shutdown");
    let run = Run {
        status,
        body,
        openrouter: openrouter_seen.lock().unwrap().clone(),
        direct: direct_seen.lock().unwrap().clone(),
        reports: reports.0.lock().unwrap().clone(),
    };
    eprintln!(
        "status {} body {}\nopenrouter requests {:#?}",
        run.status, run.body, run.openrouter
    );
    run
}

fn entry(model: &str, alias: Option<&str>, alias_retention: Option<&str>) -> Value {
    let mut entry = json!({"provider": "anthropic", "model": model, "endpoint": "direct",
                           "column_id": format!("col_{model}")});
    if let Some(alias) = alias {
        entry["execution_aliases"] = json!({"openrouter": alias});
    }
    if let Some(retention) = alias_retention {
        entry["execution_alias_retention"] = json!({"openrouter": retention});
    }
    entry
}

fn recommendation(chain: Vec<Value>, zero_retention: bool) -> Value {
    json!({
        "recommendation_id": "rec_or",
        "classification": {"method": "declared"},
        "measured": true,
        "recommendations": chain,
        "zero_retention": zero_retention,
    })
}

fn chat_body(extra: Value) -> Value {
    let mut body =
        json!({"model": "maverick/auto", "messages": [{"role": "user", "content": "hi"}]});
    if let (Value::Object(body), Value::Object(extra)) = (&mut body, extra) {
        body.extend(extra);
    }
    body
}

fn header<'a>(seen: &'a Seen, name: &str) -> Option<&'a str> {
    seen.headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// Every attempt runs on OpenRouter with its slug; an alias-less entry is skipped; no
/// direct provider is called; telemetry says `endpoint_used: openrouter`.
#[tokio::test]
async fn every_entry_runs_on_openrouter_with_its_slug() {
    let run = run(
        recommendation(
            vec![
                entry("claude-a", Some("anthropic/claude-a"), Some("standard")),
                entry("claude-b", None, None),
                entry("gpt-c", Some("openai/gpt-c"), None),
            ],
            false,
        ),
        chat_body(json!({})),
        vec![
            (500, json!({"error": "overloaded"})),
            (200, json!({"id": "or_1", "choices": []})),
        ],
    )
    .await;
    assert_eq!(run.status, 200);
    assert_eq!(run.body["id"], "or_1");
    let models: Vec<&Value> = run
        .openrouter
        .iter()
        .map(|seen| &seen.body["model"])
        .collect();
    assert_eq!(
        models,
        [&json!("anthropic/claude-a"), &json!("openai/gpt-c")]
    );
    for seen in &run.openrouter {
        assert_eq!(seen.target, "/chat/completions");
        assert_eq!(
            header(seen, "authorization"),
            Some("Bearer sk-or-v1-caller")
        );
        assert_eq!(header(seen, "x-maverick-api-key"), None, "D30");
        assert!(
            seen.body.get("provider").is_none(),
            "no ZDR for a standard tenant"
        );
    }
    assert!(run.direct.is_empty(), "never a direct provider");
    let items = &run.reports[0].items;
    assert_eq!(items.len(), 2);
    assert!(items.iter().all(|item| item.endpoint_used == "openrouter"));
}

/// D15: a zero-retention tenant's attempts all force `provider.zdr: true`, keeping the
/// caller's other routing options; an alias that is not verified `zero` is skipped.
#[tokio::test]
async fn zero_retention_forces_zdr_and_skips_unverified_aliases() {
    let run = run(
        recommendation(
            vec![
                entry("claude-a", Some("anthropic/claude-a"), Some("standard")),
                entry("claude-b", Some("anthropic/claude-b"), Some("zero")),
                entry("gpt-c", Some("openai/gpt-c"), Some("unknown")),
                entry("claude-d", Some("anthropic/claude-d"), Some("zero")),
            ],
            true,
        ),
        chat_body(json!({"provider": {"order": ["anthropic"], "zdr": false}})),
        vec![
            (503, json!({"error": "busy"})),
            (200, json!({"id": "or_2", "choices": []})),
        ],
    )
    .await;
    assert_eq!(run.status, 200);
    let models: Vec<&Value> = run
        .openrouter
        .iter()
        .map(|seen| &seen.body["model"])
        .collect();
    assert_eq!(
        models,
        [&json!("anthropic/claude-b"), &json!("anthropic/claude-d")]
    );
    for seen in &run.openrouter {
        assert_eq!(
            seen.body["provider"],
            json!({"order": ["anthropic"], "zdr": true}),
            "ZDR forced on every attempt, the caller's options kept"
        );
    }
    assert!(run.direct.is_empty());

    // A `provider` that is not an object is replaced: the option must be there.
    let run = self::run(
        recommendation(
            vec![entry("claude-b", Some("anthropic/claude-b"), Some("zero"))],
            true,
        ),
        chat_body(json!({"provider": "anything"})),
        vec![(200, json!({"id": "or_3", "choices": []}))],
    )
    .await;
    assert_eq!(run.openrouter[0].body["provider"], json!({"zdr": true}));
}

/// D15: a zero-retention tenant with no verified `zero` alias gets 409, with no upstream
/// request at all and no direct fallback.
#[tokio::test]
async fn zero_retention_without_a_zero_alias_fails_closed() {
    let run = run(
        recommendation(
            vec![
                entry("claude-a", Some("anthropic/claude-a"), Some("standard")),
                entry("claude-b", None, None),
            ],
            true,
        ),
        chat_body(json!({})),
        Vec::new(),
    )
    .await;
    assert_eq!(run.status, 409);
    assert_eq!(run.body["error"]["code"], "no_compliant_columns");
    assert_eq!(
        run.body["error"]["message"],
        "no chain entry has an OpenRouter alias with valid zero-retention evidence"
    );
    assert!(run.openrouter.is_empty() && run.direct.is_empty());
    assert!(run.reports.is_empty(), "nothing was attempted");
}

/// Spec §6 scenario 11: an alias-less chain is a catalog gap, answered like an exhausted
/// chain (502 `no_executable_candidates`), with no upstream request.
#[tokio::test]
async fn an_alias_less_chain_has_no_executable_candidates() {
    let run = run(
        recommendation(
            vec![entry("claude-a", None, None), entry("claude-b", None, None)],
            false,
        ),
        chat_body(json!({})),
        Vec::new(),
    )
    .await;
    assert_eq!(run.status, 502);
    assert_eq!(run.body["error"]["code"], "no_executable_candidates");
    assert!(run.openrouter.is_empty() && run.direct.is_empty());
}
