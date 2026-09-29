//! Parity tests for the deterministic fake provider (bead 3hk.4.5).
//!
//! Fixture: `parity/fixtures/measurement-io/fake_provider.json` and its testdata dir. The Go
//! source is `internal/providers/fake/fake_test.go`.

use std::error::Error as _;
use std::time::{Duration, Instant};

use maverick_providers::fake::{FakeProvider, FixtureSpec, UnknownFixtureError, hash_prompt};
use maverick_providers::{CompletionRequest, ProviderClient, ProviderErrorKind};
use serde_json::Map;

fn request(model: &str, prompt: &str) -> CompletionRequest {
    CompletionRequest {
        model: model.to_string(),
        system: "ignored".to_string(),
        prompt: prompt.to_string(),
        temperature: 0.7,
        max_tokens: 256,
        settings: Map::new(),
    }
}

fn seed_provider() -> FakeProvider {
    let dir = parity_support::fixture_path(
        "measurement-io",
        "testdata/internal/providers/fake/testdata",
    )
    .expect("fake testdata dir path");
    FakeProvider::from_dir(dir).expect("fake fixtures load")
}

const INVOICE_PROMPT: &str = "Extract the invoice total as JSON.";

// parity: measurement-io/fake.TestCompleteIsDeterministic
#[tokio::test]
async fn complete_is_deterministic() {
    let provider = seed_provider();
    let first = provider
        .complete(request("claude-opus-4-8", INVOICE_PROMPT))
        .await
        .expect("completes");
    let second = provider
        .complete(request("claude-opus-4-8", INVOICE_PROMPT))
        .await
        .expect("completes");

    assert_eq!(first.text, "{\"total\": 42.50}");
    assert_eq!(first.tokens_in, 120);
    assert_eq!(first.tokens_out, 18);
    assert_eq!(first, second, "the fake is deterministic");
}

// parity: measurement-io/fake.TestCompleteCoversSeedModels
#[tokio::test]
async fn complete_covers_seed_models() {
    let provider = seed_provider();
    for model in [
        "claude-opus-4-8",
        "claude-haiku-4-5",
        "gpt-5.5",
        "gemini-3.1-pro",
        "grok-4",
    ] {
        let response = provider
            .complete(request(model, INVOICE_PROMPT))
            .await
            .unwrap_or_else(|error| panic!("{model} completes: {error}"));
        assert!(!response.text.is_empty(), "{model} text non-empty");
        assert!(response.tokens_in > 0, "{model} tokens_in > 0");
        assert!(response.tokens_out > 0, "{model} tokens_out > 0");
    }
}

// parity: measurement-io/fake.TestCompleteUnknownPairFailsLoudly
#[tokio::test]
async fn complete_unknown_pair_fails_loudly() {
    let provider = seed_provider();
    let cases = [
        ("claude-opus-4-8", "no fixture for this"),
        ("unknown-model", INVOICE_PROMPT),
    ];
    for (model, prompt) in cases {
        let error = provider
            .complete(request(model, prompt))
            .await
            .expect_err("unknown pair fails");
        assert_eq!(error.kind(), ProviderErrorKind::Unknown, "{model}");

        let source = error
            .source()
            .and_then(|source| source.downcast_ref::<UnknownFixtureError>())
            .expect("source is an UnknownFixtureError");
        assert_eq!(source.model, model);
        assert_eq!(source.prompt_hash, hash_prompt(prompt));
        assert_eq!(
            source.to_string(),
            format!(
                "fake provider: no fixture for model \"{model}\" prompt hash {} (add it to testdata)",
                hash_prompt(prompt)
            )
        );
    }
}

// parity: measurement-io/fake.TestCompleteScriptedErrors
#[tokio::test]
async fn complete_scripted_errors() {
    let provider = seed_provider();
    let cases = [
        (
            "always rate limited",
            ProviderErrorKind::RateLimited,
            "fake: rate_limited (status 429)",
        ),
        (
            "always provider error",
            ProviderErrorKind::ProviderError,
            "fake: provider_error (status 500)",
        ),
        (
            "always times out",
            ProviderErrorKind::Timeout,
            "fake: timeout: context deadline exceeded",
        ),
        (
            "always invalid",
            ProviderErrorKind::InvalidRequest,
            "fake: invalid_request (status 400)",
        ),
    ];
    for (prompt, kind, message) in cases {
        let error = provider
            .complete(request("gpt-5.5", prompt))
            .await
            .expect_err("scripted error");
        assert_eq!(error.kind(), kind, "{prompt}");
        assert_eq!(error.to_string(), message, "{prompt}");
    }
}

// parity: measurement-io/fake.TestCompleteArtificialLatency
#[tokio::test]
async fn complete_artificial_latency() {
    let provider = seed_provider();
    let latency_prompt = "Summarize the meeting notes in one sentence.";

    let started = Instant::now();
    let response = provider
        .complete(request("gemini-3.1-flash", latency_prompt))
        .await
        .expect("latency completes");
    assert!(
        started.elapsed() >= Duration::from_millis(20),
        "waited at least the fixture latency"
    );
    assert!(!response.text.is_empty());

    // A caller deadline shorter than the latency cancels the sleeping future.
    let timed_out = tokio::time::timeout(
        Duration::from_millis(1),
        provider.complete(request("gemini-3.1-flash", latency_prompt)),
    )
    .await;
    assert!(timed_out.is_err(), "a 1ms deadline expires before 20ms");
}

// parity: measurement-io/fake.TestHashPromptIsExactMatchOnly
#[test]
fn hash_prompt_is_exact_match_only() {
    assert_eq!(
        hash_prompt("a"),
        "ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48bb"
    );
    assert_ne!(
        hash_prompt("a"),
        hash_prompt("a "),
        "trailing space differs"
    );
}

// parity: measurement-io/fake.TestAddByPromptHash
#[tokio::test]
async fn add_by_prompt_hash() {
    let fixture = FixtureSpec {
        model: "m".to_string(),
        prompt: String::new(),
        prompt_sha256: hash_prompt("keyed by hash"),
        output: "ok".to_string(),
        tokens_in: 1,
        tokens_out: 1,
        latency_ms: 0,
        error: String::new(),
    };
    let provider = FakeProvider::from_fixtures(&[fixture]).expect("builds");
    let response = provider
        .complete(request("m", "keyed by hash"))
        .await
        .expect("hash-keyed fixture matches the prompt");
    assert_eq!(response.text, "ok");
}

// parity: measurement-io/fake.TestAddValidation
#[test]
fn add_validation_rejects_bad_specs() {
    let mut provider = FakeProvider::new();
    let mut missing_model = FixtureSpec {
        model: String::new(),
        prompt: "p".to_string(),
        prompt_sha256: String::new(),
        output: String::new(),
        tokens_in: 0,
        tokens_out: 0,
        latency_ms: 0,
        error: String::new(),
    };
    assert_eq!(
        provider.add(&missing_model).unwrap_err().to_string(),
        "model is required"
    );

    missing_model.model = "m".to_string();
    missing_model.prompt = String::new();
    assert_eq!(
        provider.add(&missing_model).unwrap_err().to_string(),
        "exactly one of prompt or prompt_sha256 is required"
    );
}

// parity: measurement-io/fake.TestNewRejectsMissingOrEmptyDir
#[test]
fn new_rejects_missing_or_empty_dir() {
    let base = std::env::temp_dir().join(format!("mm-fake-{}", std::process::id()));

    let missing = base.join("does-not-exist");
    let missing_error = FakeProvider::from_dir(&missing).expect_err("missing dir fails");
    assert!(
        missing_error
            .to_string()
            .starts_with("fake provider: reading fixture dir:"),
        "engine prefix (D36): {missing_error}"
    );

    let empty = base.join("empty");
    std::fs::create_dir_all(&empty).expect("create empty dir");
    let empty_error = FakeProvider::from_dir(&empty).expect_err("empty dir fails");
    assert_eq!(
        empty_error.to_string(),
        format!(
            "fake provider: no *.json fixture files in {}",
            empty.display()
        )
    );
    let _ = std::fs::remove_dir_all(&base);
}
