//! The telemetry producer: item shape, usage parsing (`proxy/unit.json#parse_usage-*`),
//! the D27 batcher (size, interval, and exit flushes; one stream per tenant key; silent
//! drops), and off meaning zero bytes. Failures print the captured batches.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use maverick_core::telemetry::{
    BatcherConfig, ErrorClass, HttpSink, Telemetry, TelemetryItem, TelemetrySink, UsageShape,
    encode_batch, is_off, parse_usage,
};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Records every batch as `(tenant key, parsed body)`.
#[derive(Default)]
struct Recorder {
    batches: Mutex<Vec<(String, Value)>>,
}

impl TelemetrySink for Recorder {
    fn send<'a>(
        &'a self,
        tenant_key: &'a str,
        body: String,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let parsed: Value = serde_json::from_str(&body).expect("batch body is JSON");
            self.batches
                .lock()
                .expect("lock")
                .push((tenant_key.to_owned(), parsed));
        })
    }
}

impl Recorder {
    fn batches(&self) -> Vec<(String, Value)> {
        self.batches.lock().expect("lock").clone()
    }

    fn sizes(&self) -> Vec<usize> {
        self.batches()
            .iter()
            .map(|(_, body)| body["items"].as_array().expect("items").len())
            .collect()
    }
}

fn item(rec: &str) -> TelemetryItem {
    TelemetryItem {
        recommendation_id: rec.to_owned(),
        model_used: "claude-sonnet-4-6".to_owned(),
        status: 200,
        latency_ms: 812,
        tokens_in: None,
        tokens_out: None,
        error_class: None,
        endpoint_used: "direct".to_owned(),
        check_pass: None,
        settings_applied: None,
    }
}

fn items(rec: &str, n: usize) -> Vec<TelemetryItem> {
    (0..n).map(|i| item(&format!("{rec}_{i}"))).collect()
}

fn config(interval: Duration) -> BatcherConfig {
    BatcherConfig {
        interval,
        max_batch: 20,
        shutdown_flush: Duration::from_secs(2),
    }
}

async fn eventually(recorder: &Recorder, want: usize) {
    for _ in 0..200 {
        if recorder.batches().len() >= want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("wanted {want} batches, got {:#?}", recorder.batches());
}

#[test]
fn parse_usage_rows() {
    let doc = parity_support::fixture("proxy", "unit.json").expect("fixture");
    let rows = doc["parse_usage"].as_array().expect("rows");
    assert!(rows.len() >= 7);
    for row in rows {
        let body = row["body"].as_str().expect("body").as_bytes();
        let want = |key: &str| row[key].as_i64();
        assert_eq!(
            parse_usage(UsageShape::Chat, body),
            (want("openai_tokens_in"), want("openai_tokens_out")),
            "{}",
            row["id"]
        );
        assert_eq!(
            parse_usage(UsageShape::Messages, body),
            (want("messages_tokens_in"), want("messages_tokens_out")),
            "{}",
            row["id"]
        );
    }
    // Wrong types make both counts absent; case-insensitive names match as in Go.
    assert_eq!(
        parse_usage(
            UsageShape::Chat,
            br#"{"usage":{"prompt_tokens":"3","completion_tokens":1}}"#
        ),
        (None, None)
    );
    assert_eq!(
        parse_usage(UsageShape::Chat, br#"{"usage":5}"#),
        (None, None)
    );
    assert_eq!(parse_usage(UsageShape::Chat, br#"[1]"#), (None, None));
    assert_eq!(
        parse_usage(UsageShape::Chat, br#"{"usage":null}"#),
        (None, None)
    );
    assert_eq!(
        parse_usage(UsageShape::Messages, br#"{"Usage":{"INPUT_TOKENS":4}}"#),
        (Some(4), None)
    );
}

/// The Go field order, absent optional fields omitted; `check_pass` is never defaulted
/// (D16) and `settings_applied` is carried (D14).
#[test]
fn item_shape() {
    let _ = parity_support::divergence!("D16");
    let _ = parity_support::divergence!("D14");
    let full = TelemetryItem {
        tokens_in: Some(10),
        tokens_out: Some(0),
        settings_applied: Some(true),
        ..item("rec_1")
    };
    let failed = TelemetryItem {
        status: 0,
        latency_ms: 30_000,
        error_class: Some(ErrorClass::Timeout),
        endpoint_used: "openrouter".to_owned(),
        check_pass: Some(false),
        model_used: "a<b>&c".to_owned(),
        ..item("rec_2")
    };
    assert_eq!(
        encode_batch(&[full, failed]),
        r#"{"items":[{"recommendation_id":"rec_1","model_used":"claude-sonnet-4-6","status":200,"latency_ms":812,"tokens_in":10,"tokens_out":0,"endpoint_used":"direct","settings_applied":true},{"recommendation_id":"rec_2","model_used":"a\u003cb\u003e\u0026c","status":0,"latency_ms":30000,"error_class":"timeout","endpoint_used":"openrouter","check_pass":false}]}"#
    );
    assert_eq!(ErrorClass::ConnectError.as_str(), "connect_error");
}

#[test]
fn off_switch_is_exactly_off() {
    for value in ["off", "OFF", "Off", "oFf"] {
        assert!(is_off(Some(value)), "{value}");
    }
    for value in [
        None,
        Some(""),
        Some(" off"),
        Some("off "),
        Some("false"),
        Some("0"),
    ] {
        assert!(!is_off(value), "{value:?}");
    }
}

/// Twenty queued items send at once, without waiting for the interval.
#[tokio::test]
async fn size_flush() {
    let recorder = Arc::new(Recorder::default());
    let telemetry = Telemetry::start(recorder.clone(), config(Duration::from_secs(3600)));
    telemetry.record("mk_live_a", items("rec", 19));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(recorder.batches().is_empty(), "{:#?}", recorder.batches());
    telemetry.record("mk_live_a", items("more", 3));
    eventually(&recorder, 1).await;
    assert_eq!(recorder.sizes(), [20]);
    telemetry.shutdown().await;
    assert_eq!(recorder.sizes(), [20, 2]);
}

/// Queued items send when the interval ticks.
#[tokio::test]
async fn interval_flush() {
    let recorder = Arc::new(Recorder::default());
    let telemetry = Telemetry::start(recorder.clone(), config(Duration::from_millis(100)));
    telemetry.record("mk_live_a", items("rec", 3));
    eventually(&recorder, 1).await;
    assert_eq!(recorder.sizes(), [3]);
    let (key, body) = &recorder.batches()[0];
    assert_eq!(key, "mk_live_a");
    assert_eq!(body["items"][0]["recommendation_id"], "rec_0");
    assert!(body["items"][0].get("check_pass").is_none());
    telemetry.shutdown().await;
}

/// Shutdown flushes what is queued.
#[tokio::test]
async fn exit_flush() {
    let recorder = Arc::new(Recorder::default());
    let telemetry = Telemetry::start(recorder.clone(), config(Duration::from_secs(3600)));
    telemetry.record("mk_live_a", items("rec", 5));
    telemetry.record("mk_live_a", Vec::new());
    telemetry.shutdown().await;
    assert_eq!(recorder.sizes(), [5], "{:#?}", recorder.batches());
    // A second shutdown and later records are harmless.
    telemetry.shutdown().await;
    telemetry.record("mk_live_a", items("late", 1));
}

/// Batches never mix tenant keys.
#[tokio::test]
async fn tenants_never_share_a_batch() {
    let recorder = Arc::new(Recorder::default());
    let telemetry = Telemetry::start(recorder.clone(), config(Duration::from_secs(3600)));
    for round in 0..3 {
        telemetry.record("mk_live_a", items(&format!("a{round}"), 2));
        telemetry.record("mk_live_b", items(&format!("b{round}"), 1));
    }
    telemetry.shutdown().await;
    let batches = recorder.batches();
    assert_eq!(batches.len(), 2, "{batches:#?}");
    for (key, body) in &batches {
        let prefix = if key == "mk_live_a" { "a" } else { "b" };
        for item in body["items"].as_array().expect("items") {
            assert!(
                item["recommendation_id"]
                    .as_str()
                    .expect("id")
                    .starts_with(prefix),
                "{batches:#?}"
            );
        }
    }
}

/// Off means no task and no bytes.
#[tokio::test]
async fn off_sends_nothing() {
    let telemetry = Telemetry::off();
    assert!(!telemetry.is_on());
    assert!(!Telemetry::default().is_on());
    telemetry.record("mk_live_a", items("rec", 50));
    telemetry.shutdown().await;
}

/// A one-shot HTTP server that captures each request and answers with `status`.
async fn capture_server(status: u16) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&seen);
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let captured = Arc::clone(&captured);
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = stream.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buffer.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buffer).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let length = text[..end]
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if buffer.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                captured
                    .lock()
                    .expect("lock")
                    .push(String::from_utf8_lossy(&buffer).to_string());
                let response = format!(
                    "HTTP/1.1 {status} X\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{{}}"
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    (url, seen)
}

/// The HTTP sink posts to `/v1/telemetry` with the tenant's bearer key.
#[tokio::test]
async fn http_sink_posts_the_batch() {
    let (url, seen) = capture_server(200).await;
    let telemetry = Telemetry::start(
        Arc::new(HttpSink::new(&format!("{url}/")).expect("sink")),
        config(Duration::from_secs(3600)),
    );
    telemetry.record("mk_live_tenant", items("rec", 2));
    telemetry.shutdown().await;
    let requests = seen.lock().expect("lock").clone();
    assert_eq!(requests.len(), 1, "{requests:#?}");
    let request = &requests[0];
    assert!(
        request.starts_with("POST /v1/telemetry HTTP/1.1\r\n"),
        "{request}"
    );
    assert!(
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer mk_live_tenant"),
        "{request}"
    );
    let body = &request[request.find("\r\n\r\n").expect("body") + 4..];
    assert_eq!(body, encode_batch(&items("rec", 2)));
}

/// A failing engine or no engine at all is dropped silently and never blocks shutdown.
#[tokio::test]
async fn failures_are_dropped_silently() {
    let (url, seen) = capture_server(500).await;
    let telemetry = Telemetry::start(
        Arc::new(HttpSink::new(&url).expect("sink")),
        config(Duration::from_secs(3600)),
    );
    telemetry.record("mk_live_tenant", items("rec", 1));
    telemetry.shutdown().await;
    assert_eq!(seen.lock().expect("lock").len(), 1);

    let closed = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let dead = format!("http://{}", closed.local_addr().expect("addr"));
    drop(closed);
    let telemetry = Telemetry::start(
        Arc::new(HttpSink::new(&dead).expect("sink")),
        config(Duration::from_secs(3600)),
    );
    telemetry.record("mk_live_tenant", items("rec", 1));
    tokio::time::timeout(Duration::from_secs(10), telemetry.shutdown())
        .await
        .expect("shutdown finishes");
}
