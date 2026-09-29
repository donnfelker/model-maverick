//! The provider call metrics (spec §14.3) through an in-memory exporter installed as the
//! global meter provider, with the recorded Anthropic wire cases on the scripted test
//! server. One test per binary, because the provider is process-wide.

use std::time::Duration;

use maverick_providers::anthropic::AnthropicClient;
use maverick_providers::{
    CompletionRequest, ProviderClient, RetryPolicy, ScriptedResponse, ScriptedTestServer,
};
use opentelemetry::{KeyValue, global};
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData, ResourceMetrics};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
use serde_json::{Map, Value};

/// `(metric, sorted labels, value)` of every counter point.
/// `(metric, sorted labels, value)`.
type Point = (String, Vec<(String, String)>, u64);

fn counters(exported: &[ResourceMetrics]) -> Vec<Point> {
    let mut out = Vec::new();
    for resource in exported {
        for scope in resource.scope_metrics() {
            for metric in scope.metrics() {
                if let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data() {
                    for point in sum.data_points() {
                        let mut labels: Vec<(String, String)> = point
                            .attributes()
                            .map(|kv: &KeyValue| (kv.key.to_string(), kv.value.to_string()))
                            .collect();
                        labels.sort();
                        out.push((metric.name().to_owned(), labels, point.value()));
                    }
                }
            }
        }
    }
    out
}

/// Replays one recorded Anthropic wire case and returns whether the call succeeded.
async fn replay(id: &str) -> bool {
    let wire =
        parity_support::fixture("measurement-io", "providers_wire.json").expect("fixture loads");
    let case = wire["cases"]
        .as_array()
        .expect("cases")
        .iter()
        .find(|c| c["id"] == id)
        .expect("case")
        .clone();
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
    let value = &case["completion_request"];
    let settings: Map<String, Value> = value["settings"].as_object().cloned().unwrap_or_default();
    let request = CompletionRequest {
        model: value["model"].as_str().expect("model").to_owned(),
        system: value["system"].as_str().expect("system").to_owned(),
        prompt: value["prompt"].as_str().expect("prompt").to_owned(),
        #[allow(clippy::cast_possible_truncation)]
        temperature: value["temperature_float32"].as_f64().expect("temperature") as f32,
        max_tokens: usize::try_from(value["max_tokens"].as_u64().expect("max_tokens"))
            .expect("fits"),
        settings,
    };
    client.complete(request).await.is_ok()
}

#[tokio::test]
async fn provider_calls_and_retries_are_recorded_with_fixed_labels() {
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(exporter.clone()).build())
        .build();
    global::set_meter_provider(provider.clone());

    assert!(replay("anthropic_429_then_ok").await, "one retry, then ok");
    assert!(!replay("anthropic_400_no_retry").await, "no retry");

    provider.force_flush().expect("flush");
    let points = counters(&exporter.get_finished_metrics().expect("metrics"));
    let find = |name: &str, labels: &[(&str, &str)]| {
        let mut wanted: Vec<(String, String)> = labels
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        wanted.sort();
        points
            .iter()
            .find(|(n, l, _)| n == name && *l == wanted)
            .map(|(_, _, v)| *v)
    };
    assert_eq!(
        find(
            "maverick_provider_calls_total",
            &[("provider", "anthropic"), ("status", "ok")]
        ),
        Some(1)
    );
    assert_eq!(
        find(
            "maverick_provider_calls_total",
            &[("provider", "anthropic"), ("status", "invalid_request")]
        ),
        Some(1)
    );
    assert_eq!(
        find(
            "maverick_provider_retries_total",
            &[("provider", "anthropic")]
        ),
        Some(1)
    );
    // D19: fixed label names only.
    for (_, labels, _) in &points {
        for (key, _) in labels {
            assert!(matches!(key.as_str(), "provider" | "status"), "label {key}");
        }
    }
}
