// Self-test fixture: instrument-lint must accept every attribute below.
// This file is not compiled. The workspace walk skips this directory.

use tracing::instrument;

/// Doc text that mentions #[instrument] is not an attribute.
#[instrument(skip_all, fields(tenant_id = %tenant_id, prompt_fp = %fingerprint))]
pub async fn recommend(tenant_id: &str, fingerprint: &str, prompt: &str) {}

#[tracing::instrument(
    name = "classify",
    level = "debug",
    skip_all,
    fields(tenant_id = %tenant_id),
)]
pub fn classify(tenant_id: &str, prompt: &str) {}

#[cfg_attr(feature = "otel", tracing::instrument(skip_all))]
pub fn flush() {}
