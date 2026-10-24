//! Engine request metrics (spec §14.3). They go through the global meter provider, which
//! `maverick serve` installs when OTel export is on; without one they are no-ops. Each
//! label is a fixed, low-cardinality value (D19): never a prompt, a body, a credential,
//! a tenant id, or any other tenant-supplied text.

use std::time::Duration;

use opentelemetry::{KeyValue, global};

const METER: &str = "maverick";

fn ms(elapsed: Duration) -> f64 {
    elapsed.as_secs_f64() * 1000.0
}

/// A classification method as a label: one of the fixed names, else `other`.
pub(crate) fn method_label(method: &str) -> &'static str {
    match method {
        "declared" => "declared",
        "classified" => "classified",
        "cached" => "cached",
        "fallback_default" => "fallback_default",
        _ => "other",
    }
}

/// One `recommend` call (REST or MCP): its latency by outcome, and on success the
/// recommendations counter by `measured` and `classification_method`.
pub(crate) fn recommend_finished(elapsed: Duration, succeeded: Option<(bool, &'static str)>) {
    let meter = global::meter(METER);
    let outcome = if succeeded.is_some() { "ok" } else { "error" };
    meter
        .f64_histogram("maverick_recommend_duration_ms")
        .with_unit("ms")
        .with_description("Recommend latency by outcome")
        .build()
        .record(ms(elapsed), &[KeyValue::new("outcome", outcome)]);
    if let Some((measured, method)) = succeeded {
        meter
            .u64_counter("maverick_recommendations_total")
            .with_description("Recommendations by measured and classification_method")
            .build()
            .add(
                1,
                &[
                    KeyValue::new("measured", if measured { "true" } else { "false" }),
                    KeyValue::new("classification_method", method),
                ],
            );
    }
}

/// One conversation affinity decision (D44) by `status` and `cost_aware`. No key, hash, or
/// column id is a label.
pub(crate) fn affinity_decision(status: &'static str, cost_aware: bool) {
    global::meter(METER)
        .u64_counter("maverick_affinity_decisions_total")
        .with_description("Conversation affinity decisions by status and cost_aware")
        .build()
        .add(
            1,
            &[
                KeyValue::new("status", status),
                KeyValue::new("cost_aware", if cost_aware { "true" } else { "false" }),
            ],
        );
}

/// One classification-cache lookup on the prompt path: `hit` or `miss`.
pub(crate) fn classifier_cache(hit: bool) {
    global::meter(METER)
        .u64_counter("maverick_classifier_cache_total")
        .with_description("Classification cache lookups by result")
        .build()
        .add(
            1,
            &[KeyValue::new("result", if hit { "hit" } else { "miss" })],
        );
}

/// One classifier resolution after a cache miss: its latency by outcome (`classified` or
/// `fallback_default`), and for a fallback the reason (`quota` or `failure`).
pub(crate) fn classifier_call(elapsed: Duration, fallback: Option<&'static str>) {
    let meter = global::meter(METER);
    let outcome = if fallback.is_some() {
        "fallback_default"
    } else {
        "classified"
    };
    meter
        .f64_histogram("maverick_classifier_duration_ms")
        .with_unit("ms")
        .with_description("Classifier latency by outcome")
        .build()
        .record(ms(elapsed), &[KeyValue::new("outcome", outcome)]);
    if let Some(reason) = fallback {
        meter
            .u64_counter("maverick_classifier_fallback_total")
            .with_description("Classifier fallbacks to the default classification, by reason")
            .build()
            .add(1, &[KeyValue::new("reason", reason)]);
    }
}

/// The job queue at a health read: runnable jobs, and the age of the oldest one.
pub(crate) fn job_queue(depth: i64, oldest_age_seconds: f64) {
    let meter = global::meter(METER);
    let queue = [KeyValue::new("queue", "default")];
    meter
        .i64_gauge("maverick_queue_depth")
        .with_description("Runnable graphile_worker jobs")
        .build()
        .record(depth, &queue);
    meter
        .f64_gauge("maverick_queue_oldest_job_age_seconds")
        .with_unit("s")
        .with_description("Age of the oldest runnable graphile_worker job")
        .build()
        .record(oldest_age_seconds, &queue);
}
