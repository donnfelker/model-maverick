//! OpenTelemetry export (spec §14.3, D19): the engine's spans and metrics over OTLP/HTTP
//! to the ops Collector when `MAVERICK_OTEL_EXPORTER_ENDPOINT` is set.
//!
//! Export is best-effort and fail-open. Without an endpoint nothing is built and no
//! connection is made. A setup failure logs one WARN line, and the engine runs on
//! without export. W3C trace context is the global propagator either way, so a
//! `traceparent` from the proxy or an SDK continues into the engine's request span.
//! Only the engine's own spans and events are exported, with their allowlisted fields
//! ([`crate::logging::dispatch_with`]). There is no metrics endpoint to scrape.
//!
//! The exporters use the async reqwest client, so [`Otel::init`] runs inside the Tokio
//! runtime, and the batch processor and the periodic reader run on it.

use std::collections::HashMap;
use std::fmt;

use maverick_config::Config;
use opentelemetry::global;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{ExporterBuildError, WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::metrics::periodic_reader_with_async_runtime::PeriodicReader;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::runtime::Tokio;
use opentelemetry_sdk::trace::span_processor_with_async_runtime::BatchSpanProcessor;
use opentelemetry_sdk::trace::{SdkTracer, SdkTracerProvider};
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::registry::LookupSpan;

/// The instrumentation scope of the engine's spans and metrics.
pub const SCOPE: &str = "maverick";

/// The engine's trace and metric pipelines.
pub struct Otel {
    tracer_provider: SdkTracerProvider,
    meter_provider: SdkMeterProvider,
    tracer: SdkTracer,
}

impl fmt::Debug for Otel {
    /// Never prints the endpoint (it can carry collector credentials).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Otel")
    }
}

impl Otel {
    /// Sets the W3C propagator, then builds and installs the OTLP pipelines when the
    /// config names an endpoint. `None` means no export.
    pub fn init(config: &Config) -> Option<Otel> {
        Self::init_with(&config.otel_exporter_endpoint, &config.otel_service_name)
    }

    /// [`Otel::init`] from the two settings.
    pub fn init_with(endpoint: &str, service_name: &str) -> Option<Otel> {
        install_propagator();
        let endpoint = endpoint.trim();
        if endpoint.is_empty() {
            return None;
        }
        match Self::otlp(endpoint, service_name) {
            Ok(otel) => {
                global::set_tracer_provider(otel.tracer_provider.clone());
                global::set_meter_provider(otel.meter_provider.clone());
                Some(otel)
            }
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "otel exporter setup failed; continuing without export"
                );
                None
            }
        }
    }

    fn otlp(endpoint: &str, service_name: &str) -> Result<Otel, ExporterBuildError> {
        let target = Endpoint::parse(endpoint);
        // No resource detectors: the service name comes only from engine config (D23).
        let resource = Resource::builder_empty()
            .with_service_name(service_name.to_owned())
            .build();
        let spans = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_endpoint(target.signal("traces"))
            .with_headers(target.headers())
            .build()?;
        let metrics = opentelemetry_otlp::MetricExporter::builder()
            .with_http()
            .with_endpoint(target.signal("metrics"))
            .with_headers(target.headers())
            .build()?;
        let tracer_provider = SdkTracerProvider::builder()
            .with_span_processor(BatchSpanProcessor::builder(spans, Tokio).build())
            .with_resource(resource.clone())
            .build();
        let meter_provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(metrics, Tokio).build())
            .with_resource(resource)
            .build();
        Ok(Otel::from_providers(tracer_provider, meter_provider))
    }

    /// An `Otel` over the given providers (tests pass in-memory exporters).
    pub fn from_providers(
        tracer_provider: SdkTracerProvider,
        meter_provider: SdkMeterProvider,
    ) -> Otel {
        let tracer = tracer_provider.tracer(SCOPE);
        Otel {
            tracer_provider,
            meter_provider,
            tracer,
        }
    }

    /// The `tracing` layer that exports spans (and their events) through this pipeline.
    pub fn layer<S>(&self) -> OpenTelemetryLayer<S, SdkTracer>
    where
        S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    {
        tracing_opentelemetry::layer()
            .with_tracer(self.tracer.clone())
            .with_threads(false)
    }

    /// The meter provider, for instruments that must not go through the global one.
    pub fn meter_provider(&self) -> &SdkMeterProvider {
        &self.meter_provider
    }

    /// Flushes and stops both pipelines at shutdown. A failure is logged, never fatal.
    /// It blocks until the pipelines' Tokio tasks answer, so call it from a
    /// multi-thread runtime (as `serve` runs) or from outside the runtime.
    pub fn shutdown(&self) {
        if let Err(error) = self.tracer_provider.shutdown() {
            tracing::warn!(error = %error, "otel trace export shutdown failed");
        }
        if let Err(error) = self.meter_provider.shutdown() {
            tracing::warn!(error = %error, "otel metric export shutdown failed");
        }
    }
}

/// Makes W3C trace context (`traceparent`, `tracestate`) the global propagator.
pub fn install_propagator() {
    global::set_text_map_propagator(TraceContextPropagator::new());
}

/// The configured base URL without its userinfo. Userinfo becomes a basic
/// `Authorization` header that only the collector receives. Deliberately not `Debug`.
struct Endpoint {
    base: String,
    authorization: Option<String>,
}

impl Endpoint {
    fn parse(url: &str) -> Endpoint {
        let url = url.trim_end_matches('/');
        let plain = || Endpoint {
            base: url.to_owned(),
            authorization: None,
        };
        let Some((scheme, rest)) = url.split_once("://") else {
            return plain();
        };
        let authority_end = rest.find('/').unwrap_or(rest.len());
        let (authority, path) = rest.split_at(authority_end);
        let Some((userinfo, host)) = authority.rsplit_once('@') else {
            return plain();
        };
        let (user, password) = userinfo.split_once(':').unwrap_or((userinfo, ""));
        let mut credentials = percent_decode(user);
        credentials.push(b':');
        credentials.extend(percent_decode(password));
        Endpoint {
            base: format!("{scheme}://{host}{path}"),
            authorization: Some(format!("Basic {}", base64(&credentials))),
        }
    }

    /// `{base}/v1/{signal}`: an endpoint set in code is used as given, so the signal
    /// path is added here.
    fn signal(&self, signal: &str) -> String {
        format!("{}/v1/{signal}", self.base)
    }

    fn headers(&self) -> HashMap<String, String> {
        self.authorization
            .iter()
            .map(|value| ("Authorization".to_owned(), value.clone()))
            .collect()
    }
}

/// `%XX` escapes to bytes. A malformed escape stays as written.
fn percent_decode(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let escaped = (bytes[i] == b'%')
            .then(|| bytes.get(i + 1..i + 3))
            .flatten()
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        if let Some(byte) = escaped {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

/// Standard base64 with padding (RFC 4648 §4).
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[(n >> (18 - 6 * i) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc_4648_vectors() {
        for (input, want) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), want, "{input}");
        }
        assert_eq!(base64(&[0xff, 0xfe, 0xfd]), "//79");
    }

    #[test]
    fn endpoint_without_userinfo_is_kept() {
        let endpoint = Endpoint::parse("http://collector:4318/");
        assert_eq!(endpoint.signal("traces"), "http://collector:4318/v1/traces");
        assert!(endpoint.headers().is_empty());
        let endpoint = Endpoint::parse("collector:4318");
        assert_eq!(endpoint.signal("metrics"), "collector:4318/v1/metrics");
    }

    #[test]
    fn endpoint_userinfo_becomes_basic_auth() {
        let endpoint = Endpoint::parse("https://otel:p%40ss@collector:4318/base");
        assert_eq!(
            endpoint.signal("metrics"),
            "https://collector:4318/base/v1/metrics"
        );
        // base64("otel:p@ss")
        assert_eq!(
            endpoint.headers().get("Authorization").map(String::as_str),
            Some("Basic b3RlbDpwQHNz")
        );
        let user_only = Endpoint::parse("http://token@collector");
        assert_eq!(user_only.signal("traces"), "http://collector/v1/traces");
        // base64("token:")
        assert_eq!(
            user_only.headers().get("Authorization").map(String::as_str),
            Some("Basic dG9rZW46")
        );
    }

    #[test]
    fn percent_decode_leaves_malformed_escapes() {
        assert_eq!(percent_decode("a%2Fb%zz%4"), b"a/b%zz%4".to_vec());
    }

    #[test]
    fn no_endpoint_means_no_export() {
        assert!(Otel::init_with("", "maverick").is_none());
        assert!(Otel::init_with("   ", "maverick").is_none());
    }

    // Shutdown blocks until the pipelines' Tokio tasks answer, so it needs a runtime
    // with another worker thread.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_endpoint_builds_both_pipelines_without_connecting() {
        // Nothing listens on port 9; building makes no connection.
        let otel = Otel::init_with("http://127.0.0.1:9", "maverick-test").expect("built");
        assert_eq!(format!("{otel:?}"), "Otel");
        otel.shutdown();
    }
}
