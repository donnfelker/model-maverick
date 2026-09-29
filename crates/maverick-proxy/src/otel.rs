//! OTel emission (D31; reference part 6 §12; spec v1 §8.1 and §14.3): one
//! `maverick.recommendation` log record per settled request and the cumulative counter
//! `maverick_proxy_recommendations_total`, over OTLP/HTTP to
//! `{MAVERICK_PROXY_OTLP_ENDPOINT}/v1/logs` and `/v1/metrics`.
//!
//! Enabled only when the endpoint is set: otherwise nothing is built, no task runs, and
//! no connection is made. Export is best-effort and fail-open: a setup failure logs one
//! WARN line and the proxy runs on without it. The payload bytes are the SDK's, not Go's
//! hand-built JSON (D31); the content is the same.
//!
//! Only fixed, engine-sourced attributes are recorded (§12.1). The request body, the
//! prompt, and every credential (the caller's provider key and the tenant key) never
//! reach an attribute, so the D19 redaction canary cannot leak through this path. The
//! endpoint is never logged: it can carry collector credentials.

use std::borrow::Cow;
use std::fmt;
use std::time::{Duration, SystemTime};

use opentelemetry::logs::{AnyValue, LogRecord as _, Logger as _, LoggerProvider as _};
use opentelemetry::metrics::{Counter, MeterProvider as _};
use opentelemetry::{InstrumentationScope, KeyValue, global};
use opentelemetry_otlp::{ExporterBuildError, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::log_processor_with_async_runtime::BatchLogProcessor;
use opentelemetry_sdk::logs::{BatchConfigBuilder, SdkLogger, SdkLoggerProvider};
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::metrics::periodic_reader_with_async_runtime::PeriodicReader;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::runtime::Tokio;

use crate::chain::Surface;
use crate::config::Config;
use crate::surface::ChainReport;

/// The log record body.
pub const RECORD_BODY: &str = "maverick.recommendation";
/// The counter name.
pub const COUNTER: &str = "maverick_proxy_recommendations_total";
/// Log records are sent every 5 s, as Go flushed its queue.
pub const LOG_INTERVAL: Duration = Duration::from_secs(5);
/// The counter is exported every 15 s, as in Go.
pub const METRIC_INTERVAL: Duration = Duration::from_secs(15);
/// At most this many records wait for the next send; a newer record is dropped (Go's
/// 1024-record queue). The counter counts every request regardless.
pub const MAX_QUEUED_RECORDS: usize = 1024;
/// The bound on the flush at shutdown.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// The proxy's log and metric pipelines.
pub struct ProxyOtel {
    logger_provider: SdkLoggerProvider,
    meter_provider: SdkMeterProvider,
    logger: SdkLogger,
    counter: Counter<u64>,
}

impl fmt::Debug for ProxyOtel {
    /// Never prints the endpoint.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProxyOtel")
    }
}

impl ProxyOtel {
    /// Makes W3C trace context the global propagator, then builds the pipelines when the
    /// config names an endpoint. Call it inside the Tokio runtime: the exporters' tasks
    /// run there. `None` means no export.
    pub fn init(config: &Config) -> Option<ProxyOtel> {
        Self::init_with(&config.otlp_endpoint, &config.otlp_service_name)
    }

    /// [`ProxyOtel::init`] from the two settings.
    pub fn init_with(endpoint: &str, service_name: &str) -> Option<ProxyOtel> {
        global::set_text_map_propagator(TraceContextPropagator::new());
        let endpoint = endpoint.trim().trim_end_matches('/');
        if endpoint.is_empty() {
            return None;
        }
        match Self::otlp(endpoint, service_name) {
            Ok(otel) => Some(otel),
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "maverick-proxy: otel exporter setup failed; continuing without export"
                );
                None
            }
        }
    }

    fn otlp(endpoint: &str, service_name: &str) -> Result<ProxyOtel, ExporterBuildError> {
        // No resource detectors: the service name comes only from proxy config.
        let resource = Resource::builder_empty()
            .with_service_name(service_name.to_owned())
            .build();
        let logs = opentelemetry_otlp::LogExporter::builder()
            .with_http()
            .with_endpoint(format!("{endpoint}/v1/logs"))
            .build()?;
        let metrics = opentelemetry_otlp::MetricExporter::builder()
            .with_http()
            .with_endpoint(format!("{endpoint}/v1/metrics"))
            .build()?;
        let batch = BatchConfigBuilder::default()
            .with_scheduled_delay(LOG_INTERVAL)
            .with_max_queue_size(MAX_QUEUED_RECORDS)
            .build();
        let logger_provider = SdkLoggerProvider::builder()
            .with_log_processor(
                BatchLogProcessor::builder(logs, Tokio)
                    .with_batch_config(batch)
                    .build(),
            )
            .with_resource(resource.clone())
            .build();
        let meter_provider = SdkMeterProvider::builder()
            .with_reader(
                PeriodicReader::builder(metrics, Tokio)
                    .with_interval(METRIC_INTERVAL)
                    .build(),
            )
            .with_resource(resource)
            .build();
        Ok(Self::from_providers(
            logger_provider,
            meter_provider,
            service_name,
        ))
    }

    /// A recorder over the given providers; the scope of both is the service name, as
    /// in Go.
    pub fn from_providers(
        logger_provider: SdkLoggerProvider,
        meter_provider: SdkMeterProvider,
        service_name: &str,
    ) -> ProxyOtel {
        let name: Cow<'static, str> = Cow::Owned(service_name.to_owned());
        let logger = logger_provider.logger(name.clone());
        let counter = meter_provider
            .meter_with_scope(InstrumentationScope::builder(name).build())
            .u64_counter(COUNTER)
            .build();
        ProxyOtel {
            logger_provider,
            meter_provider,
            logger,
            counter,
        }
    }

    /// Records one settled request (relay or exhaustion; §12.1).
    pub fn record(&self, report: &ChainReport) {
        let mut record = self.logger.create_log_record();
        record.set_body(AnyValue::String(RECORD_BODY.into()));
        record.set_timestamp(SystemTime::now());
        let surface = surface_name(report.surface);
        record.add_attribute("recommendation_id", report.recommendation_id.clone());
        record.add_attribute("surface", surface);
        record.add_attribute("mode", report.mode.clone());
        record.add_attribute("model_used", report.model_used.clone());
        record.add_attribute(
            "attempts",
            i64::try_from(report.items.len()).unwrap_or(i64::MAX),
        );
        record.add_attribute("final_status", i64::from(report.final_status));
        record.add_attribute("measured", report.measured);
        record.add_attribute(
            "classification_method",
            report.classification_method.clone(),
        );
        self.logger.emit(record);
        // QUIRK 18: `measured` is the STRING "true"/"false" on the counter.
        self.counter.add(
            1,
            &[
                KeyValue::new("measured", if report.measured { "true" } else { "false" }),
                KeyValue::new(
                    "classification_method",
                    report.classification_method.clone(),
                ),
            ],
        );
    }

    /// Sends every queued record and the counter now. It blocks until the exporters'
    /// Tokio tasks answer: call it off the runtime's worker threads (`spawn_blocking`).
    pub fn force_flush(&self) {
        if let Err(error) = self.logger_provider.force_flush() {
            tracing::warn!(error = %error, "maverick-proxy: otel log flush failed");
        }
        if let Err(error) = self.meter_provider.force_flush() {
            tracing::warn!(error = %error, "maverick-proxy: otel metric flush failed");
        }
    }

    /// Flushes and stops both pipelines, each within [`SHUTDOWN_TIMEOUT`]. A failure is
    /// logged, never fatal. Blocking, like [`ProxyOtel::force_flush`].
    pub fn shutdown(&self) {
        if let Err(error) = self.logger_provider.shutdown_with_timeout(SHUTDOWN_TIMEOUT) {
            tracing::warn!(error = %error, "maverick-proxy: otel log export shutdown failed");
        }
        if let Err(error) = self.meter_provider.shutdown_with_timeout(SHUTDOWN_TIMEOUT) {
            tracing::warn!(error = %error, "maverick-proxy: otel metric export shutdown failed");
        }
    }
}

/// The surface attribute value.
fn surface_name(surface: Surface) -> &'static str {
    match surface {
        Surface::Chat => "chat_completions",
        Surface::Messages => "messages",
    }
}
