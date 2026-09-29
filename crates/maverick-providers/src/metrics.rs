//! Provider call metrics (spec §14.3). They go through the global meter provider, which
//! `maverick serve` installs when OTel export is on; without one they are no-ops. Each
//! label is a fixed, low-cardinality value (D19): the provider name and a status name.

use opentelemetry::{KeyValue, global};

/// One finished call (all its attempts): the outcome by provider and status (`ok` or the
/// error kind), and the retries it took.
pub(crate) fn provider_call(provider: &'static str, status: &'static str, attempts: usize) {
    let meter = global::meter("maverick");
    meter
        .u64_counter("maverick_provider_calls_total")
        .with_description("Provider calls by provider and final status")
        .build()
        .add(
            1,
            &[
                KeyValue::new("provider", provider),
                KeyValue::new("status", status),
            ],
        );
    let retries = u64::try_from(attempts.saturating_sub(1)).unwrap_or(u64::MAX);
    if retries > 0 {
        meter
            .u64_counter("maverick_provider_retries_total")
            .with_description("Provider call retries by provider")
            .build()
            .add(retries, &[KeyValue::new("provider", provider)]);
    }
}
