//! D43: a tenant that opted out of telemetry (D39) gets no operator OTel record from the
//! proxy: no `maverick.recommendation` log record, and the request is not counted in
//! `maverick_proxy_recommendations_total`. Another tenant's request still records both.
//! The reports go through the real `ProxyReports` sink to a mock OTLP collector.

mod otlp;

use std::sync::Arc;
use std::time::Instant;

use maverick_core::recommend::TenantKey;
use maverick_core::telemetry::{Telemetry, TelemetryItem};
use maverick_proxy::chain::Surface;
use maverick_proxy::otel::{COUNTER, ProxyOtel};
use maverick_proxy::reports::ProxyReports;
use maverick_proxy::surface::{ChainReport, ReportSink};
use otlp::{collector, counter_points, json_attributes, received_logs, received_metrics};
use serde_json::json;

fn report(recommendation_id: &str, telemetry_opt_out: bool) -> ChainReport {
    ChainReport {
        tenant_key: TenantKey::new("mk_live_test"),
        surface: Surface::Chat,
        recommendation_id: recommendation_id.into(),
        measured: true,
        classification_method: "declared".into(),
        mode: "balanced".into(),
        model_used: "gpt-a".into(),
        final_status: 200,
        items: vec![TelemetryItem {
            recommendation_id: recommendation_id.into(),
            model_used: "gpt-a".into(),
            status: 200,
            latency_ms: 5,
            tokens_in: None,
            tokens_out: None,
            error_class: None,
            endpoint_used: "direct".into(),
            check_pass: None,
            settings_applied: Some(false),
        }],
        telemetry_opt_out,
        received_at: Instant::now(),
    }
}

async fn flush(otel: &Arc<ProxyOtel>) {
    let otel = otel.clone();
    tokio::task::spawn_blocking(move || otel.force_flush())
        .await
        .expect("flush");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_opted_out_tenant_gets_no_otel_record() {
    let d43 = parity_support::divergence!("D43");
    assert_eq!(d43.id(), "D43");
    let (url, received) = collector().await;
    let otel = Arc::new(ProxyOtel::init_with(&url, "maverick-proxy").expect("enabled"));
    let reports = ProxyReports::new(Telemetry::off()).with_otel(Some(otel.clone()));
    let records = |received| -> Vec<serde_json::Value> {
        received_logs(received)
            .iter()
            .flat_map(|export| export.resource_logs.clone())
            .flat_map(|logs| logs.scope_logs)
            .flat_map(|scope| scope.log_records)
            .map(|record| json_attributes(&record.attributes))
            .collect()
    };

    reports.report(report("rec_opted_out", true));
    flush(&otel).await;
    assert!(records(&received).is_empty(), "no log record");
    let counter_exports = received_metrics(&received)
        .iter()
        .flat_map(|export| export.resource_metrics.clone())
        .flat_map(|metrics| metrics.scope_metrics)
        .flat_map(|scope| scope.metrics)
        .filter(|metric| metric.name == COUNTER)
        .count();
    assert_eq!(counter_exports, 0, "not counted");

    reports.report(report("rec_counted", false));
    flush(&otel).await;
    let logged = records(&received);
    assert_eq!(logged.len(), 1);
    assert_eq!(
        logged[0][0],
        json!({"key": "recommendation_id", "value": {"stringValue": "rec_counted"}})
    );
    let counts: Vec<i64> = counter_points(&received)
        .into_iter()
        .map(|(_, count)| count)
        .collect();
    assert_eq!(counts, [1]);
    otel.shutdown();
}
