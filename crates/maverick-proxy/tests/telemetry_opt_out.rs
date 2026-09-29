//! The tenant telemetry opt-out on the proxy (D39; v1 spec §9.10; integration-surfaces
//! plan §4; bead model-maverick-cd9).
//!
//! When the engine's recommend response says `telemetry: false`, the proxy still routes
//! and relays the request, and it sends zero bytes to `/v1/telemetry` for it. With
//! `telemetry: true`, or no member at all (a Go-era engine), the attempt's item is sent as
//! before. The real report sink runs here: the `maverick-core` batcher over HTTP to the
//! mock engine, flushed at shutdown.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::Request;
use maverick_core::chain::ChainExecutor;
use maverick_core::recommend::{RecommendClient, RecommendClientConfig};
use maverick_proxy::config::load_config;
use maverick_proxy::reports::ProxyReports;
use maverick_proxy::server::{Proxy, ServeSettings, serve};
use maverick_proxy::surface::{ChainReport, ExecutorRunner, ReportSink, Upstreams};
use serde_json::{Value, json};
use tokio::net::TcpListener;

const CLOSED_PORT: &str = "http://127.0.0.1:1";
const TENANT_KEY: &str = "mk_live_tenant";

/// One request a mock received: path and body.
#[derive(Debug, Clone)]
struct Seen {
    path: String,
    body: Value,
}

type SeenLog = Arc<Mutex<Vec<Seen>>>;

/// A mock that records each request and answers from `replies` in order, then
/// `200 {"accepted":0,"dropped":0}` (a telemetry batch).
async fn mock(replies: Vec<Value>) -> (String, SeenLog) {
    let seen: SeenLog = Arc::default();
    let replies = Arc::new(Mutex::new(replies.into_iter().rev().collect::<Vec<_>>()));
    let handler = {
        let seen = seen.clone();
        move |request: Request| {
            let seen = seen.clone();
            let replies = replies.clone();
            async move {
                let path = request.uri().path().to_owned();
                let bytes: Bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
                    .await
                    .expect("body");
                let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                seen.lock().unwrap().push(Seen { path, body });
                let reply = replies
                    .lock()
                    .unwrap()
                    .pop()
                    .unwrap_or_else(|| json!({"accepted": 0, "dropped": 0}));
                ([("content-type", "application/json")], reply.to_string())
            }
        }
    };
    let app = Router::new().fallback(handler);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
    (url, seen)
}

/// Forwards to the real sink and notes that a report arrived.
#[derive(Clone)]
struct Tee {
    reports: ProxyReports,
    arrived: Arc<Mutex<Vec<bool>>>,
}

impl ReportSink for Tee {
    fn report(&self, report: ChainReport) {
        self.arrived.lock().unwrap().push(report.telemetry_opt_out);
        self.reports.report(report);
    }
}

struct Run {
    status: u16,
    upstream_requests: usize,
    opted_out: Vec<bool>,
    /// The bodies the mock engine received on `/v1/telemetry`.
    telemetry: Vec<Value>,
}

/// One chat request through a hosted proxy with telemetry on, whose engine answers
/// `recommendation`. The sink is flushed before the engine's log is read.
async fn run(recommendation: Value) -> Run {
    let (engine, engine_seen) = mock(vec![recommendation]).await;
    let (openai, openai_seen) = mock(vec![json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"},
                     "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 3, "completion_tokens": 1},
    })])
    .await;
    let env = [
        ("MAVERICK_PROXY_MODE", "hosted"),
        ("MAVERICK_ENGINE_URL", engine.as_str()),
        ("MAVERICK_PROXY_TELEMETRY", "on"),
        ("MAVERICK_UPSTREAM_OPENROUTER", CLOSED_PORT),
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
    assert!(config.telemetry, "telemetry is on for this test");
    assert!(config.upstream_openai.starts_with("http://127.0.0.1:"));
    let reports = ProxyReports::from_config(&config).expect("reports");
    let sink = Tee {
        reports: reports.clone(),
        arrived: Arc::default(),
    };
    let http = reqwest::Client::builder().no_proxy().build().expect("http");
    let runner = ExecutorRunner::with_executor(
        ChainExecutor::new(http.clone()),
        Upstreams::from_config(&config),
        Arc::new(sink.clone()),
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
        .header("Authorization", "Bearer sk-caller-openai")
        .header("Content-Type", "application/json")
        .body(
            json!({"model": "maverick/auto", "messages": [{"role": "user", "content": "hi"}]})
                .to_string(),
        )
        .send()
        .await
        .expect("proxy response");
    let status = response.status().as_u16();
    let _ = response.bytes().await;
    // The report arrives after the relay ends.
    for _ in 0..400 {
        if !sink.arrived.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    reports.shutdown().await;
    let _ = stop.send(());
    server.await.expect("server task").expect("clean shutdown");

    let engine_seen = engine_seen.lock().unwrap().clone();
    assert_eq!(engine_seen[0].path, "/v1/recommend");
    let run = Run {
        status,
        upstream_requests: openai_seen.lock().unwrap().len(),
        opted_out: sink.arrived.lock().unwrap().clone(),
        telemetry: engine_seen
            .iter()
            .filter(|seen| seen.path == "/v1/telemetry")
            .map(|seen| seen.body.clone())
            .collect(),
    };
    eprintln!(
        "status {} upstream {} reports {:?} telemetry {:#?}",
        run.status, run.upstream_requests, run.opted_out, run.telemetry
    );
    run
}

fn recommendation(telemetry: Option<bool>) -> Value {
    let mut body = json!({
        "recommendation_id": "rec_d39",
        "classification": {"method": "declared"},
        "measured": true,
        "recommendations": [{"provider": "openai", "model": "gpt-a", "endpoint": "direct",
                             "column_id": "col_gpt_a"}],
        "zero_retention": false,
    });
    if let Some(telemetry) = telemetry {
        body["telemetry"] = json!(telemetry);
    }
    body
}

/// DIVERGENCE: D39. An opted-out tenant's request is routed and relayed, and the proxy
/// sends nothing to `/v1/telemetry`.
#[tokio::test]
async fn an_opted_out_tenant_is_routed_and_sends_no_telemetry() {
    let d39 = parity_support::divergence!("D39");
    assert_eq!(d39.id(), "D39");
    let run = run(recommendation(Some(false))).await;
    assert_eq!(run.status, 200);
    assert_eq!(run.upstream_requests, 1, "the request is still routed");
    assert_eq!(
        run.opted_out,
        [true],
        "one settled report, marked opted out"
    );
    assert!(run.telemetry.is_empty(), "zero telemetry requests");
}

/// DIVERGENCE: D39. With `telemetry: true`, and with no member at all (a Go-era engine),
/// the attempt's item is sent under the tenant key as before.
#[tokio::test]
async fn telemetry_is_sent_unless_the_tenant_opted_out() {
    let _d39 = parity_support::divergence!("D39");
    for telemetry in [Some(true), None] {
        let run = run(recommendation(telemetry)).await;
        assert_eq!(run.status, 200, "{telemetry:?}");
        assert_eq!(run.opted_out, [false], "{telemetry:?}");
        assert_eq!(run.telemetry.len(), 1, "{telemetry:?}");
        let items = run.telemetry[0]["items"].as_array().expect("items");
        assert_eq!(items.len(), 1, "{telemetry:?}");
        assert_eq!(items[0]["recommendation_id"], "rec_d39", "{telemetry:?}");
        assert_eq!(items[0]["model_used"], "gpt-a", "{telemetry:?}");
    }
}
