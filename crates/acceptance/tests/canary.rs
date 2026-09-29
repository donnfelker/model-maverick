//! Test 1 of the privacy acceptance suite: the canary (bead 9.2; spec §14.3, §15; plan
//! §6; D19; G5).
//!
//! A prompt-form recommend, a proxy chat request, a proxy messages request, and an MCP
//! `recommend_model` call each carry their own canary: in the prompt, in the other body
//! fields the caller controls, and in the provider credential. The engine key cannot carry
//! one (it must authenticate), so the key itself is watched. The upstream completion
//! carries a canary too.
//!
//! This binary installs one global subscriber for the whole process: a layer that renders
//! every event and every span field, at DEBUG and above, from every target (including
//! `log` records), plus the engine's OTLP span export over all spans. The proxy exports
//! its log records and metrics over OTLP. Both exports go to one mock collector. After the
//! traffic both pipelines are flushed, and no canary may appear anywhere: not in a log
//! line, a span field, an exported span, an exported log record, or a metric label.

use std::fmt::{self, Write as _};
use std::sync::{Arc, Mutex, PoisonError};

use maverick_acceptance::{CLAUDE, Collector, GPT, Options, Stack, Upstreams};
use serde_json::{Value, json};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Subscriber};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;

const ENGINE: &str = "CANARY-ENGINE-7F3A";
const CHAT: &str = "CANARY-CHAT-81B2";
const MESSAGES: &str = "CANARY-MESSAGES-C4D9";
const MCP: &str = "CANARY-MCP-2E6F";
const COMPLETION: &str = "CANARY-COMPLETION-9A0C";

/// Every event and span record, rendered as one text line each.
#[derive(Clone, Default)]
struct Everything(Arc<Mutex<Vec<String>>>);

impl Everything {
    fn push(&self, line: String) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(line);
    }

    fn lines(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Appends ` name=value` for every field.
struct Render<'a>(&'a mut String);

impl Visit for Render<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let _ = write!(self.0, " {}={value:?}", field.name());
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        let _ = write!(self.0, " {}={value}", field.name());
    }
}

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for Everything {
    fn on_new_span(&self, attrs: &Attributes<'_>, _id: &Id, _ctx: Context<'_, S>) {
        let metadata = attrs.metadata();
        let mut line = format!(
            "span {} {} {}",
            metadata.level(),
            metadata.target(),
            metadata.name()
        );
        attrs.record(&mut Render(&mut line));
        self.push(line);
    }

    fn on_record(&self, _id: &Id, values: &Record<'_>, _ctx: Context<'_, S>) {
        let mut line = String::from("record");
        values.record(&mut Render(&mut line));
        self.push(line);
    }

    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let metadata = event.metadata();
        let mut line = format!("event {} {}", metadata.level(), metadata.target());
        event.record(&mut Render(&mut line));
        self.push(line);
    }
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

/// One MCP JSON-RPC request; the reply's SSE frame, parsed.
async fn mcp(
    http: &reqwest::Client,
    stack: &Stack,
    key: &str,
    session: Option<&str>,
    body: &Value,
) -> (u16, Option<String>, Value) {
    let mut request = http
        .post(format!("{}/mcp", stack.engine_url))
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(body.to_string());
    if let Some(session) = session {
        request = request.header("mcp-session-id", session);
    }
    let response = request.send().await.expect("mcp response");
    let status = response.status().as_u16();
    let session = response
        .headers()
        .get("mcp-session-id")
        .map(|value| value.to_str().unwrap().to_owned());
    let text = response.text().await.unwrap_or_default();
    let frame = text
        .strip_prefix("event: message\ndata: ")
        .and_then(|rest| rest.trim_end().parse::<Value>().ok())
        .unwrap_or(Value::Null);
    (status, session, frame)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_canary_reaches_a_log_a_span_or_a_metric() {
    let everything = Everything::default();
    let collector = Collector::start().await;
    let otel = Arc::new(
        maverick_cli::otel::Otel::init_with(&collector.url, "maverick").expect("engine otel"),
    );
    tracing_subscriber::registry()
        .with(everything.clone().with_filter(LevelFilter::DEBUG))
        .with(otel.layer())
        .try_init()
        .expect("the global subscriber");

    let Some(stack) = Stack::start(Options::default()).await else {
        return;
    };
    let tenant = stack.tenant("canary", &[CLAUDE, GPT], false, false).await;
    let secrets: Vec<String> = [ENGINE, CHAT, MESSAGES, MCP, COMPLETION]
        .iter()
        .map(|canary| (*canary).to_owned())
        .chain([tenant.key.clone()])
        .collect();
    // A window is open, so the capture path runs too.
    let reply = stack
        .engine(
            "POST",
            "/v1/debug/capture-windows",
            &tenant.key,
            r#"{"duration_minutes":60}"#,
        )
        .await;
    assert_eq!(reply.status, 201, "{}", reply.body);

    // 1. The engine's prompt form.
    let reply = stack
        .recommend_prompt(&tenant, &format!("Extract the parties from {ENGINE}."))
        .await;
    assert_eq!(reply.status, 200, "{}", reply.body);

    // 2. and 3. The proxy's two surfaces.
    let upstreams = Upstreams::start(&format!("The parties are {COMPLETION}.")).await;
    let proxy = stack
        .proxy(
            &upstreams,
            &[
                ("MAVERICK_PROXY_TELEMETRY", "on"),
                ("MAVERICK_PROXY_OTLP_ENDPOINT", collector.url.as_str()),
            ],
        )
        .await;
    let chat = json!({
        "model": "maverick/auto",
        "user": CHAT,
        "messages": [
            {"role": "system", "content": format!("You serve {CHAT}.")},
            {"role": "user", "content": format!("Extract the parties from {CHAT}.")}
        ]
    });
    let reply = proxy
        .chat(&tenant, Some(&format!("sk-proj-{CHAT}")), &chat)
        .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let messages = json!({
        "model": "maverick/auto",
        "max_tokens": 64,
        "system": format!("You serve {MESSAGES}."),
        "metadata": {"user_id": MESSAGES},
        "messages": [{"role": "user", "content": format!("Extract the parties from {MESSAGES}.")}]
    });
    let reply = proxy
        .messages(&tenant, Some(&format!("sk-ant-{MESSAGES}")), &messages)
        .await;
    assert_eq!(reply.status, 200, "{}", reply.body);

    // 4. MCP `recommend_model`.
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let initialize = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "clientInfo": {"name": "acceptance", "version": "v1"},
        "protocolVersion": "2025-11-25", "capabilities": {}}});
    let (status, session, _) = mcp(&http, &stack, &tenant.key, None, &initialize).await;
    assert_eq!(status, 200);
    let session = session.expect("an MCP session");
    let initialized =
        json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}});
    let (status, _, _) = mcp(&http, &stack, &tenant.key, Some(&session), &initialized).await;
    assert_eq!(status, 202);
    let call = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
        "name": "recommend_model",
        "arguments": {"workload": {"prompt": format!("Extract the parties from {MCP}.")}}}});
    let (status, _, frame) = mcp(&http, &stack, &tenant.key, Some(&session), &call).await;
    assert_eq!(status, 200);
    assert_ne!(frame["result"]["isError"], true, "{frame}");
    assert!(
        frame["result"]["structuredContent"]["recommendation_id"].is_string(),
        "{frame}"
    );

    // Flush both pipelines: the proxy's (telemetry and OTel) at its shutdown, then the
    // engine's span and metric export.
    proxy.stop().await;
    let engine_otel = otel.clone();
    tokio::task::spawn_blocking(move || engine_otel.shutdown())
        .await
        .expect("engine otel shutdown");

    // The canaries went through: each surface carried its own.
    let classified = stack.classifier.prompts().join("\n");
    for canary in [ENGINE, CHAT, MESSAGES, MCP] {
        assert!(classified.contains(canary), "{canary} was not classified");
    }
    assert!(
        upstreams
            .openai
            .seen()
            .iter()
            .any(|seen| seen.body.contains(CHAT))
    );
    assert!(
        upstreams
            .anthropic
            .seen()
            .iter()
            .any(|seen| seen.body.contains(MESSAGES))
    );
    assert!(
        stack.count("debug_captures").await >= 4,
        "every prompt was captured"
    );

    // Everything was observed: debug lines, spans, and every export.
    let lines = everything.lines();
    assert!(
        lines.iter().any(|line| line.starts_with("event DEBUG")),
        "no DEBUG line"
    );
    assert!(
        lines.iter().any(|line| line.starts_with("span ")),
        "no span"
    );
    let received = collector.received();
    for path in ["/v1/traces", "/v1/metrics", "/v1/logs"] {
        assert!(
            received
                .iter()
                .any(|(at, body)| at == path && !body.is_empty()),
            "no {path} export: {:?}",
            received
                .iter()
                .map(|(at, body)| (at, body.len()))
                .collect::<Vec<_>>()
        );
    }

    // The net catches: a value planted in a DEBUG span field and in a DEBUG event field is
    // captured, so a canary in either place would have been found.
    tracing::debug_span!("sentinel", planted = "SENTINEL-SPAN-5B1")
        .in_scope(|| tracing::debug!(planted = "SENTINEL-EVENT-5B1", "sentinel"));
    let caught = everything.lines();
    for sentinel in ["SENTINEL-SPAN-5B1", "SENTINEL-EVENT-5B1"] {
        assert!(
            caught.iter().any(|line| line.contains(sentinel)),
            "{sentinel} was not captured"
        );
    }

    let mut leaks = Vec::new();
    for secret in &secrets {
        for line in lines.iter().filter(|line| line.contains(secret.as_str())) {
            leaks.push(format!("{secret} in a captured line: {line}"));
        }
        for (path, body) in &received {
            if contains(body, secret) {
                leaks.push(format!("{secret} in an OTLP export to {path}"));
            }
        }
    }
    assert!(
        leaks.is_empty(),
        "{} leak(s) (G5, D19):\n{}",
        leaks.len(),
        leaks.join("\n")
    );
    eprintln!(
        "canary: {} captured lines and {} OTLP exports checked, no leak",
        lines.len(),
        received.len()
    );
}
