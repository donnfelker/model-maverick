// Self-test fixture: every attribute below must fail instrument-lint.
// This file is not compiled. The workspace walk skips this directory.

use tracing::instrument;

#[instrument]
pub async fn recommend(prompt: &str, api_key: &str) -> String {
    format!("{prompt}{api_key}")
}

#[tracing::instrument(
    level = "debug",
    skip(api_key),
)]
pub fn classify(prompt: &str, api_key: &str) {}

#[instrument(skip_all, ret)]
pub fn complete() -> String {
    String::new()
}
