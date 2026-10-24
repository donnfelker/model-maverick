//! OTel emission (D31; reference part 6 §12, §18 T-otel-*; fixtures
//! `scenarios.json#otel_*`). The payload bytes are the SDK's protobuf, not Go's JSON, so
//! the tests decode each OTLP request a mock collector received and compare its content
//! with the fixture's JSON template.

mod otlp;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;
use maverick_core::recommend::{
    RecommendClient, RecommendClientConfig, RecommendRequest, TenantKey, TraceHeaders, Workload,
};
use maverick_core::telemetry::{Telemetry, TelemetryItem};
use maverick_proxy::chain::Surface;
use maverick_proxy::otel::ProxyOtel;
use maverick_proxy::reports::ProxyReports;
use maverick_proxy::surface::{ChainReport, ReportSink};
use otlp::{collector, counter_points, json_attributes, received_logs, sorted};
use serde_json::{Value, json};
use tokio::net::TcpListener;

fn item(model: &str, status: u16) -> TelemetryItem {
    TelemetryItem {
        recommendation_id: "rec".into(),
        model_used: model.into(),
        status,
        latency_ms: 5,
        tokens_in: None,
        tokens_out: None,
        error_class: None,
        endpoint_used: "direct".into(),
        check_pass: None,
        settings_applied: Some(false),
    }
}

fn report(
    surface: Surface,
    id: &str,
    model: &str,
    measured: bool,
    method: &str,
    attempts: usize,
    status: u16,
) -> ChainReport {
    ChainReport {
        tenant_key: TenantKey::new("mk_live_test"),
        surface,
        recommendation_id: id.into(),
        measured,
        classification_method: method.into(),
        mode: "balanced".into(),
        model_used: model.into(),
        final_status: status,
        items: (0..attempts).map(|_| item(model, status)).collect(),
        telemetry_opt_out: false,
        received_at: Instant::now(),
    }
}

async fn flush(otel: &Arc<ProxyOtel>) {
    let otel = otel.clone();
    tokio::task::spawn_blocking(move || otel.force_flush())
        .await
        .expect("flush");
}

/// One scenario: record its report, flush, and compare the decoded payloads with the
/// fixture's templates.
async fn check_scenario(id: &str, report: ChainReport) {
    let (url, received) = collector().await;
    let otel = Arc::new(ProxyOtel::init_with(&url, "maverick-proxy").expect("enabled"));
    otel.record(&report);
    flush(&otel).await;
    let scenarios = parity_support::fixture("proxy", "scenarios.json").expect("scenarios.json");
    let scenario = scenarios["scenarios"]
        .as_array()
        .expect("scenarios")
        .iter()
        .find(|s| s["id"] == id)
        .unwrap_or_else(|| panic!("scenario {id}"))
        .clone();
    otlp::assert_matches_fixture(id, &received, &scenario, None);
    otel.shutdown();
}

// parity: proxy.TestOTel_HappyPathEmitsLogRecord
#[tokio::test(flavor = "multi_thread")]
async fn happy_path_emits_the_log_record_and_counter() {
    check_scenario(
        "otel_happy_path_log_record",
        report(
            Surface::Chat,
            "rec_otel",
            "gpt-a",
            true,
            "classified",
            1,
            200,
        ),
    )
    .await;
}

// parity: proxy.TestOTel_MeasuredFalseFallbackCounted
#[tokio::test(flavor = "multi_thread")]
async fn measured_false_fallback_is_counted_as_a_string() {
    check_scenario(
        "otel_measured_false_fallback_counted",
        report(
            Surface::Messages,
            "rec_cold",
            "claude-y",
            false,
            "fallback_default",
            1,
            200,
        ),
    )
    .await;
}

// parity: proxy.TestOTel_CounterCumulativeAcrossExports
#[tokio::test(flavor = "multi_thread")]
async fn the_counter_is_cumulative_and_exhaustion_is_recorded() {
    let (url, received) = collector().await;
    let otel = Arc::new(ProxyOtel::init_with(&url, "maverick-proxy").expect("enabled"));
    let happy = report(Surface::Chat, "rec_1", "gpt-a", true, "classified", 1, 200);
    otel.record(&happy);
    otel.record(&happy);
    // Exhaustion: model_used "", final_status 502, and attempts can be 0.
    let exhausted = report(Surface::Messages, "rec_2", "", false, "cached", 0, 502);
    otel.record(&exhausted);
    flush(&otel).await;
    let point = |measured: &str, method: &str| {
        sorted(json!([
            {"key": "measured", "value": {"stringValue": measured}},
            {"key": "classification_method", "value": {"stringValue": method}},
        ]))
    };
    let mut points = counter_points(&received);
    points.sort_by_key(|(_, count)| *count);
    assert_eq!(
        points,
        [
            (point("false", "cached"), 1),
            (point("true", "classified"), 2)
        ]
    );
    let records: Vec<Value> = received_logs(&received)
        .iter()
        .flat_map(|export| export.resource_logs[0].scope_logs[0].log_records.clone())
        .map(|record| json_attributes(&record.attributes))
        .collect();
    let last = records.last().expect("records");
    assert_eq!(
        last[3],
        json!({"key": "model_used", "value": {"stringValue": ""}})
    );
    assert_eq!(
        last[4],
        json!({"key": "attempts", "value": {"intValue": "0"}})
    );
    assert_eq!(
        last[5],
        json!({"key": "final_status", "value": {"intValue": "502"}})
    );

    // The counter never resets between exports.
    otel.record(&happy);
    flush(&otel).await;
    let mut points = counter_points(&received);
    points.sort_by_key(|(_, count)| *count);
    assert_eq!(points[1], (point("true", "classified"), 3));
    otel.shutdown();
}

// parity: proxy.TestOTel_DisabledEmitsNothing
#[tokio::test(flavor = "multi_thread")]
async fn disabled_builds_nothing_and_sends_nothing() {
    let (_url, received) = collector().await;
    assert!(ProxyOtel::init_with("", "maverick-proxy").is_none());
    assert!(ProxyOtel::init_with("  ", "maverick-proxy").is_none());
    let reports = ProxyReports::new(Telemetry::off())
        .with_otel(ProxyOtel::init_with("", "maverick-proxy").map(Arc::new));
    reports.report(report(
        Surface::Chat,
        "rec",
        "gpt-a",
        true,
        "classified",
        1,
        200,
    ));
    reports.shutdown().await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(received.lock().unwrap().is_empty());
}

/// D19: no credential or caller data reaches an exported attribute, whatever the
/// request carried. The recorded attributes are a fixed, engine-sourced set.
#[tokio::test(flavor = "multi_thread")]
async fn the_redaction_canary_never_reaches_the_collector() {
    const CANARY: &str = "MAVERICK-CANARY-DO-NOT-LOG";
    let (url, received) = collector().await;
    let otel = Arc::new(ProxyOtel::init_with(&url, "maverick-proxy").expect("enabled"));
    let mut leaky = report(Surface::Chat, "rec_c", "gpt-a", true, "classified", 1, 200);
    leaky.tenant_key = TenantKey::new(format!("mk_live_{CANARY}"));
    let reports = ProxyReports::new(Telemetry::off()).with_otel(Some(otel.clone()));
    reports.report(leaky);
    flush(&otel).await;
    let received = received.lock().unwrap();
    assert!(!received.is_empty());
    for (path, body) in received.iter() {
        assert!(
            !body.windows(CANARY.len()).any(|w| w == CANARY.as_bytes()),
            "{path} carries the canary"
        );
    }
    drop(received);
    otel.shutdown();
}

/// W3C trace context continues into the engine call (spec §14.3).
#[tokio::test(flavor = "multi_thread")]
async fn trace_context_reaches_the_engine() {
    type Seen = Arc<Mutex<Option<HeaderMap>>>;
    let seen: Seen = Arc::default();
    let engine = Router::new()
        .route(
            "/v1/recommend",
            post(|State(seen): State<Seen>, headers: HeaderMap| async move {
                *seen.lock().unwrap() = Some(headers);
                (
                    [("content-type", "application/json")],
                    r#"{"recommendation_id":"rec_t","recommendations":[]}"#,
                )
            }),
        )
        .with_state(seen.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    tokio::spawn(async move { axum::serve(listener, engine).await.expect("serve") });

    let client = RecommendClient::new(RecommendClientConfig::new(url)).expect("client");
    let request = RecommendRequest {
        mode: maverick_core::alias::Mode::Balanced,
        pool: None,
        affinity: None,
        workload: Workload::Declared {
            capability: "factual_qa".into(),
            difficulty: None,
            domain_tags: Vec::new(),
        },
    };
    let traceparent = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";
    let trace = TraceHeaders {
        traceparent: Some(traceparent.into()),
        tracestate: Some("mm=1".into()),
    };
    client
        .recommend_traced(&TenantKey::new("mk_live_test"), &request, &trace)
        .await
        .expect("recommend");
    let headers = seen.lock().unwrap().clone().expect("the engine was called");
    assert_eq!(headers["traceparent"], traceparent);
    assert_eq!(headers["tracestate"], "mm=1");

    client
        .recommend(&TenantKey::new("mk_live_test"), &request)
        .await
        .expect("recommend");
    let headers = seen.lock().unwrap().clone().expect("called again");
    assert!(
        headers.get("traceparent").is_none(),
        "no context, no header"
    );
}
