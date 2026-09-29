//! Request observability on both listeners: the `request` log line (reference Part 5
//! §10.1, spec §14.3) and the W3C trace context of the request span (D19).
//!
//! The request logger runs outside auth, so auth and handlers record what the line
//! needs in a shared [`RequestLog`] slot that the logger puts in the request extensions.
//! The slot keeps what was recorded even when a handler panics. The line never carries
//! a header or a body, and the request span records only the method and the path.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, Response};
use axum::middleware::Next;
use opentelemetry::propagation::Extractor;
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// What auth and handlers add to the request line.
#[derive(Clone, Debug, Default)]
pub struct RequestLog(Arc<Mutex<Fields>>);

#[derive(Clone, Debug, Default)]
struct Fields {
    tenant_id: Option<String>,
    key_id: Option<String>,
    recommendation_id: Option<String>,
    actor: Option<String>,
}

impl RequestLog {
    /// Public auth succeeded: the line gets `tenant_id` and `key_id`.
    pub fn set_identity(&self, tenant_id: &str, key_id: &str) {
        let mut fields = self.lock();
        fields.tenant_id = Some(tenant_id.to_owned());
        fields.key_id = Some(key_id.to_owned());
    }

    /// Internal auth succeeded: the line gets `actor` (the token label).
    pub fn set_actor(&self, actor: &str) {
        self.lock().actor = Some(actor.to_owned());
    }

    /// A recommendation was created: the line gets `recommendation_id` (spec §14.3).
    pub fn set_recommendation_id(&self, recommendation_id: &str) {
        self.lock().recommendation_id = Some(recommendation_id.to_owned());
    }

    fn lock(&self) -> MutexGuard<'_, Fields> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Writes the INFO `request` line: `method`, `path` (no query), `status`, `ms` (float,
/// microsecond resolution), `request_id`, then `tenant_id` and `key_id` when public auth
/// succeeded, `recommendation_id` when one was created, and `actor` when internal auth
/// succeeded.
pub fn log_request(
    method: &str,
    path: &str,
    status: u16,
    elapsed: Duration,
    request_id: &str,
    log: &RequestLog,
) {
    let fields = log.lock().clone();
    tracing::info!(
        method,
        path,
        status,
        ms = millis(elapsed),
        request_id,
        tenant_id = fields.tenant_id.as_deref(),
        key_id = fields.key_id.as_deref(),
        recommendation_id = fields.recommendation_id.as_deref(),
        actor = fields.actor.as_deref(),
        "request"
    );
}

/// Go `float64(d.Microseconds()) / 1000`.
#[allow(clippy::cast_precision_loss)]
fn millis(elapsed: Duration) -> f64 {
    elapsed.as_micros() as f64 / 1000.0
}

/// Reads only the W3C trace-context headers, so no other header can reach a span.
struct TraceHeaders<'a>(&'a HeaderMap);

const TRACE_HEADERS: [&str; 2] = ["traceparent", "tracestate"];

impl Extractor for TraceHeaders<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        if !TRACE_HEADERS.contains(&key) {
            return None;
        }
        self.0.get(key).and_then(|value| value.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        TRACE_HEADERS.to_vec()
    }
}

/// The caller's trace context (a `traceparent` from the proxy or an SDK), through the
/// global propagator. Without a valid header the context is empty and the request span
/// starts a new trace.
pub(crate) fn parent_context(headers: &HeaderMap) -> opentelemetry::Context {
    opentelemetry::global::get_text_map_propagator(|propagator| {
        propagator.extract(&TraceHeaders(headers))
    })
}

/// Runs the request inside a `request` span that continues the caller's W3C trace
/// context. The span records only `method` and `path`. A context that cannot be
/// attached is dropped: tracing is best-effort and never fails a request.
pub async fn trace_context(request: Request, next: Next) -> Response<Body> {
    let span = tracing::info_span!(
        "request",
        method = %request.method(),
        path = request.uri().path()
    );
    let _ = span.set_parent(parent_context(request.headers()));
    next.run(request).instrument(span).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn millis_keeps_microsecond_resolution() {
        assert_eq!(millis(Duration::from_nanos(1_234_567)), 1.234);
        assert_eq!(millis(Duration::from_micros(250)), 0.25);
        assert_eq!(millis(Duration::from_nanos(999)), 0.0);
    }

    #[test]
    fn trace_headers_expose_only_trace_context() {
        let mut headers = HeaderMap::new();
        headers.insert("traceparent", "00-abc-def-01".parse().unwrap());
        headers.insert("authorization", "Bearer secret".parse().unwrap());
        let extractor = TraceHeaders(&headers);
        assert_eq!(extractor.get("traceparent"), Some("00-abc-def-01"));
        assert_eq!(extractor.get("authorization"), None);
        assert_eq!(extractor.get("tracestate"), None);
        assert_eq!(extractor.keys(), vec!["traceparent", "tracestate"]);
    }
}
