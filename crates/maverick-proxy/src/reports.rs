//! Where each settled request's [`ChainReport`] goes (reference part 6 §11; D27).
//!
//! The report's telemetry items go to the `maverick-core` producer: batched per tenant key
//! (sent every 10 s or at 20 items, flushed at shutdown), fire-and-forget. With telemetry
//! off (`MAVERICK_PROXY_TELEMETRY=off`) the producer has no task and sends nothing. A
//! request that dispatched no attempt has no items and sends nothing.
//!
//! OTel (D31) records every settled request, including one that dispatched nothing, when
//! `MAVERICK_PROXY_OTLP_ENDPOINT` is set ([`ProxyOtel`]).

use std::sync::Arc;

use maverick_core::telemetry::{BatcherConfig, HttpSink, Telemetry};

use crate::config::Config;
use crate::otel::{ProxyOtel, SHUTDOWN_TIMEOUT};
use crate::surface::{ChainReport, ReportSink};

/// The proxy's report sink.
#[derive(Debug, Clone)]
pub struct ProxyReports {
    telemetry: Telemetry,
    otel: Option<Arc<ProxyOtel>>,
}

impl ProxyReports {
    /// A sink over an existing telemetry producer, without OTel.
    #[must_use]
    pub fn new(telemetry: Telemetry) -> Self {
        Self {
            telemetry,
            otel: None,
        }
    }

    /// Adds the OTel recorder (D31).
    #[must_use]
    pub fn with_otel(mut self, otel: Option<Arc<ProxyOtel>>) -> Self {
        self.otel = otel;
        self
    }

    /// The sink the configuration asks for: telemetry to `{engine}/v1/telemetry` under each
    /// request's tenant key, or none when telemetry is off. Starts the batcher task, so call
    /// it inside the runtime.
    pub fn from_config(config: &Config) -> Result<Self, reqwest::Error> {
        let telemetry = if config.telemetry {
            Telemetry::start(
                Arc::new(HttpSink::new(&config.engine_url)?),
                BatcherConfig::default(),
            )
        } else {
            Telemetry::off()
        };
        Ok(Self::new(telemetry).with_otel(ProxyOtel::init(config).map(Arc::new)))
    }

    /// Flush every queued item and wait, within the batcher's bound, for the sends. Then
    /// flush and stop OTel, within its own bound.
    pub async fn shutdown(&self) {
        self.telemetry.shutdown().await;
        if let Some(otel) = self.otel.clone() {
            // The SDK shutdown blocks on its Tokio tasks, so it runs off the worker thread.
            let stop = tokio::task::spawn_blocking(move || otel.shutdown());
            if tokio::time::timeout(SHUTDOWN_TIMEOUT * 3, stop)
                .await
                .is_err()
            {
                tracing::warn!("maverick-proxy: otel shutdown timed out");
            }
        }
    }
}

impl ReportSink for ProxyReports {
    fn report(&self, report: ChainReport) {
        if let Some(otel) = &self.otel {
            otel.record(&report);
        }
        self.telemetry
            .record(report.tenant_key.expose(), report.items);
    }
}
