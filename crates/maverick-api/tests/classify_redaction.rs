//! G5 and D19 (bead model-maverick-bxvh): the classifier-failure log line never carries
//! the classifier's output or a provider's error body. A classifier can echo the prompt
//! into `capability`, and a provider's error body can quote the request, so the line
//! names the column and the failed check and nothing the model or the provider wrote.
//! The privacy canary suite cannot see this: its fake classifier never echoes the prompt.
//!
//! Each test installs the capturing subscriber before the resolver logs, and this binary
//! holds only these tests, so no earlier test caches the warn callsite as uninterested.
//! The resolver runs on real Postgres (its budget and spend reads and writes).

use std::sync::{Arc, LazyLock};

use chrono::Utc;
use maverick_api::classify::{Classifier, Method, ResolveInput, Resolver, ResolverSettings};
use maverick_providers::{
    CompletionRequest, CompletionResponse, ProviderClient, ProviderError, ProviderErrorKind,
    ProviderFuture,
};
use maverick_store::{MIGRATOR, tenancy};
use maverick_testdb::Schema;
use serde_json::Value;
use test_support::{TestTracing, init_test_tracing_with_secrets};

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

const TENANT: &str = "ten_redact";
const KEY: [u8; 32] = [0; 32];
const COLUMN: &str = "claude-haiku-4-5/direct";
const CANARY: &str = "REDACT-CANARY-5521";
const FAILURE: &str = "classifier failed; answering the fallback classification";

/// What the classifier provider answers, on every call.
#[derive(Clone)]
enum Reply {
    /// A 2xx whose text is the classifier's output.
    Text(String),
    /// A provider error whose wrapped source is the provider's response line and body,
    /// as the Anthropic client writes it.
    ErrorBody(String),
}

struct Fixed(Reply);

impl ProviderClient for Fixed {
    fn complete<'a>(&'a self, _request: CompletionRequest) -> ProviderFuture<'a> {
        let reply = self.0.clone();
        Box::pin(async move {
            match reply {
                Reply::Text(text) => Ok(CompletionResponse {
                    text,
                    tokens_in: 100,
                    tokens_out: 20,
                }),
                Reply::ErrorBody(body) => Err(ProviderError::new(
                    "anthropic",
                    ProviderErrorKind::InvalidRequest,
                )
                .with_status(400)
                .with_source(format!(
                    "POST \"https://api.anthropic.com/v1/messages\": 400 Bad Request {body}"
                ))),
            }
        })
    }

    fn name(&self) -> &str {
        "anthropic"
    }
}

fn prompt() -> String {
    format!("Summarize the board memo {CANARY} in three bullet points.")
}

/// Resolves `prompt()` against a classifier that always answers `reply`, and returns the
/// method and the one failure line. The subscriber is `tracing`'s: install it first.
async fn resolve_failing(reply: Reply, tracing: &TestTracing) -> Option<(Method, Value)> {
    let db = maverick_testdb::database(&SCHEMA).await?;
    let pool = db.pool().await;
    tenancy::create_tenant(&pool, TENANT, TENANT, &KEY)
        .await
        .unwrap();
    let resolver = Resolver::new(ResolverSettings {
        column: COLUMN.to_owned(),
        ..ResolverSettings::default()
    })
    .with_classifier(Classifier::new(Arc::new(Fixed(reply)), COLUMN));
    let prompt = prompt();
    let input = ResolveInput {
        tenant_id: TENANT,
        prompt_hmac_key: &KEY,
        quota_override: None,
        zero_retention: false,
        prompt: &prompt,
        projection_version: None,
        columns: &[],
        now: Utc::now(),
    };
    let resolution = resolver
        .resolve(&pool, &input)
        .await
        .expect("not a zero-retention tenant");
    db.close().await;

    let failures: Vec<Value> = tracing
        .captured_json()
        .into_iter()
        .filter(|line| line["msg"] == FAILURE)
        .collect();
    assert_eq!(failures.len(), 1, "one failure line: {failures:?}");
    Some((resolution.method, failures[0].clone()))
}

/// No captured line holds the canary, and none leaked (the guard fails the test too).
fn assert_no_canary(tracing: &TestTracing) {
    let leaks: Vec<String> = tracing
        .captured_lines()
        .into_iter()
        .filter(|line| line.contains(CANARY))
        .collect();
    assert!(leaks.is_empty(), "the canary reached the log: {leaks:?}");
    assert!(tracing.secret_violations().is_empty());
}

#[tokio::test]
async fn a_classifier_that_echoes_the_prompt_into_capability_is_not_logged() {
    let tracing = init_test_tracing_with_secrets(&[CANARY]);
    let echo = format!(
        r#"{{"capability":"{}","difficulty":2,"domain_tags":[],"confidence":0.5}}"#,
        prompt()
    );
    let Some((method, line)) = resolve_failing(Reply::Text(echo), &tracing).await else {
        return;
    };
    assert_eq!(method, Method::FallbackDefault);
    assert_eq!(
        line["error"],
        "classify: column claude-haiku-4-5/direct: capability is not a v1 capability"
    );
    assert_eq!(line["level"], "WARN");
    assert_no_canary(&tracing);
}

#[tokio::test]
async fn a_provider_error_body_is_not_logged() {
    let tracing = init_test_tracing_with_secrets(&[CANARY]);
    let body = format!(
        r#"{{"type":"error","error":{{"type":"invalid_request_error","message":"messages.0.content: {} is not allowed"}}}}"#,
        prompt()
    );
    let Some((method, line)) = resolve_failing(Reply::ErrorBody(body), &tracing).await else {
        return;
    };
    assert_eq!(method, Method::FallbackDefault);
    assert_eq!(
        line["error"],
        "classify: column claude-haiku-4-5/direct: anthropic: invalid_request (status 400)"
    );
    assert_no_canary(&tracing);
}

#[tokio::test]
async fn a_malformed_answer_names_only_the_failed_check() {
    let tracing = init_test_tracing_with_secrets(&[CANARY]);
    let answer = format!(r#"{{"capability":5,"difficulty":2,"confidence":0.5,"note":"{CANARY}"}}"#);
    let Some((method, line)) = resolve_failing(Reply::Text(answer), &tracing).await else {
        return;
    };
    assert_eq!(method, Method::FallbackDefault);
    assert_eq!(
        line["error"],
        "classify: column claude-haiku-4-5/direct: \
         parsing structured output: not a JSON object of the response schema"
    );
    assert_eq!(line["calls"], 2, "the retry happened and is still counted");
    assert_no_canary(&tracing);
}
