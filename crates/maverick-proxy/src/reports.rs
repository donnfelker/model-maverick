//! Where each settled request's [`ChainReport`] goes (reference part 6 §11; D27).
//!
//! The report's telemetry items go to the `maverick-core` producer: batched per tenant key
//! (sent every 10 s or at 20 items, flushed at shutdown), fire-and-forget. With telemetry
//! off (`MAVERICK_PROXY_TELEMETRY=off`) the producer has no task and sends nothing. A
//! request that dispatched no attempt has no items and sends nothing. OTel emission
//! (D31) joins here in bead `model-maverick-7sa`.

use std::sync::Arc;

use maverick_core::telemetry::{BatcherConfig, HttpSink, Telemetry};

use crate::config::Config;
use crate::surface::{ChainReport, ReportSink};

/// The proxy's report sink.
#[derive(Debug, Clone)]
pub struct ProxyReports {
    telemetry: Telemetry,
}

impl ProxyReports {
    /// A sink over an existing telemetry producer.
    #[must_use]
    pub fn new(telemetry: Telemetry) -> Self {
        Self { telemetry }
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
        Ok(Self::new(telemetry))
    }

    /// Flush every queued item and wait, within the batcher's bound, for the sends.
    pub async fn shutdown(&self) {
        self.telemetry.shutdown().await;
    }
}

impl ReportSink for ProxyReports {
    fn report(&self, report: ChainReport) {
        self.telemetry
            .record(report.tenant_key.expose(), report.items);
    }
}
