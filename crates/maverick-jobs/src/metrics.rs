//! Job metrics (spec §14.3). They go through the global meter provider, which
//! `maverick serve` installs when OTel export is on; without one they are no-ops. Each
//! label is a fixed, low-cardinality value (D19): a job kind or an outcome name.

use std::time::Duration;

use opentelemetry::{KeyValue, global};

use crate::JobKind;

const METER: &str = "maverick";

/// A handler's wall-clock duration by kind and outcome (`ok`, `error`, `timeout`), and a
/// failure count for the other two.
pub(crate) fn job_finished(kind: JobKind, outcome: &'static str, elapsed: Duration) {
    let meter = global::meter(METER);
    let labels = [
        KeyValue::new("kind", kind.as_str()),
        KeyValue::new("outcome", outcome),
    ];
    meter
        .f64_histogram("maverick_job_duration_ms")
        .with_unit("ms")
        .with_description("Job handler duration by kind and outcome")
        .build()
        .record(elapsed.as_secs_f64() * 1000.0, &labels);
    if outcome != "ok" {
        meter
            .u64_counter("maverick_job_failures_total")
            .with_description("Failed job deliveries by kind and outcome")
            .build()
            .add(1, &labels);
    }
}

/// A delivery that took a lease over from an earlier delivery (a crash, a timeout, or a
/// graceful stop) and resumes its window.
pub(crate) fn lease_recovered(kind: JobKind) {
    global::meter(METER)
        .u64_counter("maverick_job_recoveries_total")
        .with_description("Batch leases taken over to resume a window, by kind")
        .build()
        .add(1, &[KeyValue::new("kind", kind.as_str())]);
}

/// The actual provider spend of a completed batch.
pub(crate) fn batch_spend(kind: JobKind, actual_usd: f64) {
    global::meter(METER)
        .f64_histogram("maverick_batch_spend_usd")
        .with_unit("USD")
        .with_description("Actual provider spend per completed batch, by kind")
        .build()
        .record(actual_usd, &[KeyValue::new("kind", kind.as_str())]);
}
