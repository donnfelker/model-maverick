//! A mock OTLP/HTTP collector and the D31 comparison: the payload bytes are the SDK's
//! protobuf, not Go's JSON, so a test decodes each received request and compares its
//! content with the fixture's proto3 JSON template (`otlp_requests_after_forced_flush`).
//! Shared by `tests/otel.rs` and the scenario replay in `tests/surfaces.rs`.

#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::routing::post;
use maverick_proxy::otel::{COUNTER, RECORD_BODY};
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue, any_value};
use opentelemetry_proto::tonic::metrics::v1::metric::Data;
use opentelemetry_proto::tonic::metrics::v1::number_data_point::Value as Point;
use prost::Message;
use serde_json::{Value, json};
use tokio::net::TcpListener;

/// Every request the collector received: (path, body).
pub type Received = Arc<Mutex<Vec<(String, Bytes)>>>;

/// A collector on a local port that stores every request's path and body.
pub async fn collector() -> (String, Received) {
    let received: Received = Arc::default();
    let store = |State(received): State<Received>, request: Request| async move {
        let path = request.uri().path().to_owned();
        let body = axum::body::to_bytes(request.into_body(), usize::MAX)
            .await
            .expect("body");
        received.lock().unwrap().push((path, body));
    };
    let app = Router::new()
        .route("/v1/logs", post(store))
        .route("/v1/metrics", post(store))
        .fallback(store)
        .with_state(received.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
    (url, received)
}

/// A decoded OTLP value as the fixture's proto3 JSON writes it.
pub fn json_value(value: &Option<AnyValue>) -> Value {
    match value.as_ref().and_then(|v| v.value.as_ref()) {
        Some(any_value::Value::StringValue(s)) => json!({"stringValue": s}),
        Some(any_value::Value::IntValue(i)) => json!({"intValue": i.to_string()}),
        Some(any_value::Value::BoolValue(b)) => json!({"boolValue": b}),
        other => panic!("unexpected OTLP value {other:?}"),
    }
}

pub fn json_attributes(attributes: &[KeyValue]) -> Value {
    Value::Array(
        attributes
            .iter()
            .map(|kv| json!({"key": kv.key, "value": json_value(&kv.value)}))
            .collect(),
    )
}

/// Attributes sorted by key: the SDK does not keep a data point's attribute order, and
/// D31 leaves the payload layout free.
pub fn sorted(attributes: Value) -> Value {
    let mut items = attributes.as_array().cloned().unwrap_or_default();
    items.sort_by(|a, b| a["key"].as_str().cmp(&b["key"].as_str()));
    Value::Array(items)
}

pub fn received_logs(received: &Received) -> Vec<ExportLogsServiceRequest> {
    received
        .lock()
        .unwrap()
        .iter()
        .filter(|(path, _)| path == "/v1/logs")
        .map(|(_, body)| ExportLogsServiceRequest::decode(body.clone()).expect("logs protobuf"))
        .collect()
}

pub fn received_metrics(received: &Received) -> Vec<ExportMetricsServiceRequest> {
    received
        .lock()
        .unwrap()
        .iter()
        .filter(|(path, _)| path == "/v1/metrics")
        .map(|(_, body)| {
            ExportMetricsServiceRequest::decode(body.clone()).expect("metrics protobuf")
        })
        .collect()
}

/// The counter's data points of the newest metrics export: (attributes sorted by key,
/// count). Checks the scope, the cumulative temporality, and monotonicity.
pub fn counter_points(received: &Received) -> Vec<(Value, i64)> {
    let exports = received_metrics(received);
    let latest = exports.last().expect("a metrics export");
    let resource = &latest.resource_metrics[0];
    let scope = &resource.scope_metrics[0];
    assert_eq!(scope.scope.as_ref().expect("scope").name, "maverick-proxy");
    let metric = scope
        .metrics
        .iter()
        .find(|m| m.name == COUNTER)
        .expect("the counter");
    let Some(Data::Sum(sum)) = &metric.data else {
        panic!("{COUNTER} is not a sum: {metric:?}");
    };
    assert_eq!(sum.aggregation_temporality, 2, "cumulative");
    assert!(sum.is_monotonic);
    sum.data_points
        .iter()
        .map(|point| {
            let Some(Point::AsInt(count)) = point.value else {
                panic!("not an int point: {point:?}");
            };
            assert!(point.start_time_unix_nano > 0 && point.time_unix_nano > 0);
            (sorted(json_attributes(&point.attributes)), count)
        })
        .collect()
}

/// The (logs, metrics) templates of a scenario's `otlp_requests_after_forced_flush`.
pub fn fixture_templates(scenario: &Value) -> (Value, Value) {
    let requests = scenario["expected"]["otlp_requests_after_forced_flush"]
        .as_array()
        .expect("otlp requests");
    let template = |path: &str| -> Value {
        let body = requests
            .iter()
            .find(|r| r["path"] == path)
            .unwrap_or_else(|| panic!("no {path} request"))["body_template"]
            .as_str()
            .expect("template")
            .replace("<UNIX_NANO>", "0");
        serde_json::from_str(&body).expect("template JSON")
    };
    (template("/v1/logs"), template("/v1/metrics"))
}

/// DIVERGENCE: D31. The content of what the collector received after a flush equals the
/// scenario's OTLP templates: one resource and scope named after the service, one
/// `maverick.recommendation` record with the §12.1 attributes in order and a timestamp,
/// and the counter's one point with the same count and (sorted) attributes.
pub fn assert_matches_fixture(
    context: &str,
    received: &Received,
    scenario: &Value,
    route_mode: Option<&str>,
) {
    let d31 = parity_support::divergence!("D31");
    assert_eq!(d31.id(), "D31");
    let (mut want_logs, mut want_metrics) = fixture_templates(scenario);
    // D45 changes the routed mode. Direct report tests retain the captured report's mode.
    if let Some(mode) = route_mode {
        let logs = &mut want_logs["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0]["attributes"];
        for attribute in logs.as_array_mut().expect("record attributes") {
            if attribute["key"] == "mode" {
                attribute["value"]["stringValue"] = mode.into();
            }
        }
        let metrics = &mut want_metrics["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0]["sum"]
            ["dataPoints"][0]["attributes"];
        for attribute in metrics.as_array_mut().expect("counter attributes") {
            if attribute["key"] == "mode" {
                attribute["value"]["stringValue"] = mode.into();
            }
        }
    }
    let logs = received_logs(received);
    let records: Vec<_> = logs
        .iter()
        .flat_map(|export| &export.resource_logs)
        .flat_map(|resource| &resource.scope_logs)
        .flat_map(|scope| &scope.log_records)
        .collect();
    assert_eq!(records.len(), 1, "{context}: one log record");
    let resource = &logs
        .iter()
        .find(|export| !export.resource_logs.is_empty())
        .expect("a logs export")
        .resource_logs[0];
    assert_eq!(
        json_attributes(&resource.resource.as_ref().expect("resource").attributes),
        want_logs["resourceLogs"][0]["resource"]["attributes"],
        "{context}: resource"
    );
    assert_eq!(
        resource.scope_logs[0].scope.as_ref().expect("scope").name,
        want_logs["resourceLogs"][0]["scopeLogs"][0]["scope"]["name"],
        "{context}: scope"
    );
    let record = records[0];
    assert_eq!(
        json_value(&record.body),
        json!({"stringValue": RECORD_BODY})
    );
    assert_ne!(record.time_unix_nano, 0, "{context}: timeUnixNano set");
    assert_eq!(
        json_attributes(&record.attributes),
        want_logs["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0]["attributes"],
        "{context}: record attributes"
    );

    let want_point = &want_metrics["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0]["sum"]["dataPoints"]
        [0];
    let want_count = want_point["asInt"]
        .as_str()
        .expect("asInt")
        .parse::<i64>()
        .expect("count");
    assert_eq!(
        counter_points(received),
        [(sorted(want_point["attributes"].clone()), want_count)],
        "{context}: counter"
    );
}
