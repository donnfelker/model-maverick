//! The deterministic fake provider: the only "model" allowed in CI (reference Part 2 §3).
//!
//! It looks up a completion by `(model, lowercase-hex sha256 of the exact prompt bytes)` and
//! returns the scripted output, latency, or error. `system`, `temperature`, `max_tokens`, and
//! `settings` are ignored. It never retries. Because the Rust [`ProviderClient`] trait carries
//! no cancellation context, the caller enforces any deadline (for example with
//! `tokio::time::timeout`); a fixture's `latency_ms` is an ordinary `tokio` sleep.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::{
    CompletionRequest, CompletionResponse, DeadlineExceeded, ProviderClient, ProviderError,
    ProviderErrorKind, ProviderFuture, boxed_source,
};

/// Returned by [`FakeProvider::complete`] when no fixture matches. Mirrors Go's
/// `*UnknownFixtureError`: the classified kind is `unknown` and the message is exact.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "fake provider: no fixture for model \"{model}\" prompt hash {prompt_hash} (add it to testdata)"
)]
pub struct UnknownFixtureError {
    /// The requested model.
    pub model: String,
    /// The lowercase-hex sha256 of the requested prompt.
    pub prompt_hash: String,
}

/// A build error from adding a fixture or loading a directory. `Display` is the exact parity
/// string (any OS or JSON tail is the library's own text, per DIVERGENCES D36).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct FakeError(String);

impl FakeError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// One fixture as written in a `*.json` file. All fields but `model` are optional.
#[derive(Debug, Clone, Deserialize)]
pub struct FixtureSpec {
    /// The model id.
    #[serde(default)]
    pub model: String,
    /// The exact prompt text; hashed to the lookup key. Exactly one of `prompt` or
    /// `prompt_sha256` must be set.
    #[serde(default)]
    pub prompt: String,
    /// A prompt hash used verbatim as the lookup key (an uppercase hash never matches a
    /// prompt, since lookups hash to lowercase hex; QUIRK Q-F1).
    #[serde(default)]
    pub prompt_sha256: String,
    /// The completion text to return.
    #[serde(default)]
    pub output: String,
    /// Reported input tokens.
    #[serde(default)]
    pub tokens_in: usize,
    /// Reported output tokens.
    #[serde(default)]
    pub tokens_out: usize,
    /// Artificial latency before the response or error.
    #[serde(default)]
    pub latency_ms: u64,
    /// A scripted error kind: `""`, `rate_limited`, `provider_error`, `timeout`, or
    /// `invalid_request`.
    #[serde(default)]
    pub error: String,
}

#[derive(Debug, Deserialize)]
struct FixtureFile {
    #[serde(default)]
    fixtures: Vec<FixtureSpec>,
}

#[derive(Debug, Clone)]
struct Fixture {
    output: String,
    tokens_in: usize,
    tokens_out: usize,
    latency: Duration,
    error: Option<ProviderErrorKind>,
}

/// The lowercase-hex SHA-256 of the exact prompt bytes.
#[must_use]
pub fn hash_prompt(prompt: &str) -> String {
    let digest = Sha256::digest(prompt.as_bytes());
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

fn scripted_kind(error: &str) -> Result<Option<ProviderErrorKind>, FakeError> {
    match error {
        "" => Ok(None),
        "rate_limited" => Ok(Some(ProviderErrorKind::RateLimited)),
        "provider_error" => Ok(Some(ProviderErrorKind::ProviderError)),
        "timeout" => Ok(Some(ProviderErrorKind::Timeout)),
        "invalid_request" => Ok(Some(ProviderErrorKind::InvalidRequest)),
        other => Err(FakeError::new(format!("unknown error kind {other:?}"))),
    }
}

fn scripted_error(kind: ProviderErrorKind) -> ProviderError {
    match kind {
        ProviderErrorKind::RateLimited => ProviderError::new("fake", kind).with_status(429),
        ProviderErrorKind::ProviderError => ProviderError::new("fake", kind).with_status(500),
        ProviderErrorKind::InvalidRequest => ProviderError::new("fake", kind).with_status(400),
        ProviderErrorKind::Timeout => {
            ProviderError::new("fake", kind).with_source(DeadlineExceeded)
        }
        ProviderErrorKind::Unknown => ProviderError::new("fake", kind),
    }
}

/// A deterministic provider backed by fixtures.
#[derive(Debug, Default)]
pub struct FakeProvider {
    fixtures: HashMap<String, Fixture>,
}

impl FakeProvider {
    /// An empty provider.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn key(model: &str, hash: &str) -> String {
        format!("{model}\u{0}{hash}")
    }

    /// Add one fixture, validating in order: `model` is required; exactly one of `prompt` or
    /// `prompt_sha256` (an empty string counts as absent); the error kind must be known; the
    /// `(model, prompt hash)` key must be new.
    pub fn add(&mut self, spec: &FixtureSpec) -> Result<(), FakeError> {
        if spec.model.is_empty() {
            return Err(FakeError::new("model is required"));
        }
        let has_prompt = !spec.prompt.is_empty();
        let has_hash = !spec.prompt_sha256.is_empty();
        if has_prompt == has_hash {
            return Err(FakeError::new(
                "exactly one of prompt or prompt_sha256 is required",
            ));
        }
        let error = scripted_kind(&spec.error)?;
        let hash = if has_prompt {
            hash_prompt(&spec.prompt)
        } else {
            spec.prompt_sha256.clone()
        };
        let key = Self::key(&spec.model, &hash);
        if self.fixtures.contains_key(&key) {
            return Err(FakeError::new(format!(
                "duplicate fixture for model {:?} prompt hash {hash}",
                spec.model
            )));
        }
        self.fixtures.insert(
            key,
            Fixture {
                output: spec.output.clone(),
                tokens_in: spec.tokens_in,
                tokens_out: spec.tokens_out,
                latency: Duration::from_millis(spec.latency_ms),
                error,
            },
        );
        Ok(())
    }

    /// Build from an in-memory list, wrapping each error as `fake provider: fixture <i>: <err>`.
    pub fn from_fixtures(specs: &[FixtureSpec]) -> Result<Self, FakeError> {
        let mut provider = Self::new();
        for (index, spec) in specs.iter().enumerate() {
            provider
                .add(spec)
                .map_err(|err| FakeError::new(format!("fake provider: fixture {index}: {err}")))?;
        }
        Ok(provider)
    }

    /// Load every `*.json` file in `dir` (non-recursive, lexical order). The engine-authored
    /// message prefixes are exact; any OS or JSON tail is the library's own text (D36).
    pub fn from_dir(dir: impl AsRef<Path>) -> Result<Self, FakeError> {
        let dir = dir.as_ref();
        let entries = fs::read_dir(dir)
            .map_err(|err| FakeError::new(format!("fake provider: reading fixture dir: {err}")))?;
        let mut files: Vec<_> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_file() && path.extension().is_some_and(|ext| ext == "json"))
            .collect();
        files.sort();
        if files.is_empty() {
            return Err(FakeError::new(format!(
                "fake provider: no *.json fixture files in {}",
                dir.display()
            )));
        }

        let mut provider = Self::new();
        for file in files {
            let text = fs::read_to_string(&file).map_err(|err| {
                FakeError::new(format!("fake provider: parsing {}: {err}", file.display()))
            })?;
            let parsed: FixtureFile = serde_json::from_str(&text).map_err(|err| {
                FakeError::new(format!("fake provider: parsing {}: {err}", file.display()))
            })?;
            for (index, spec) in parsed.fixtures.iter().enumerate() {
                provider.add(spec).map_err(|err| {
                    FakeError::new(format!(
                        "fake provider: {} fixture {index}: {err}",
                        file.display()
                    ))
                })?;
            }
        }
        Ok(provider)
    }

    /// The number of loaded fixtures.
    #[must_use]
    pub fn len(&self) -> usize {
        self.fixtures.len()
    }

    /// Whether no fixtures are loaded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fixtures.is_empty()
    }
}

impl ProviderClient for FakeProvider {
    fn complete<'a>(&'a self, request: CompletionRequest) -> ProviderFuture<'a> {
        Box::pin(async move {
            let hash = hash_prompt(&request.prompt);
            let key = Self::key(&request.model, &hash);
            let Some(fixture) = self.fixtures.get(&key) else {
                return Err(
                    ProviderError::new("fake provider", ProviderErrorKind::Unknown).with_source(
                        boxed_source(UnknownFixtureError {
                            model: request.model.clone(),
                            prompt_hash: hash,
                        }),
                    ),
                );
            };
            if !fixture.latency.is_zero() {
                tokio::time::sleep(fixture.latency).await;
            }
            if let Some(kind) = fixture.error {
                return Err(scripted_error(kind));
            }
            Ok(CompletionResponse {
                text: fixture.output.clone(),
                tokens_in: fixture.tokens_in,
                tokens_out: fixture.tokens_out,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(model: &str, prompt: &str) -> FixtureSpec {
        FixtureSpec {
            model: model.to_string(),
            prompt: prompt.to_string(),
            prompt_sha256: String::new(),
            output: "ok".to_string(),
            tokens_in: 1,
            tokens_out: 1,
            latency_ms: 0,
            error: String::new(),
        }
    }

    #[test]
    fn hash_prompt_matches_known_vectors() {
        assert_eq!(
            hash_prompt(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hash_prompt("a"),
            "ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48bb"
        );
        assert_ne!(hash_prompt("a"), hash_prompt("a "));
    }

    #[test]
    fn add_validation_in_order() {
        let mut provider = FakeProvider::new();

        let mut missing_model = spec("", "p");
        missing_model.model = String::new();
        assert_eq!(
            provider.add(&missing_model).unwrap_err().to_string(),
            "model is required"
        );

        let mut neither = spec("m", "");
        neither.prompt = String::new();
        assert_eq!(
            provider.add(&neither).unwrap_err().to_string(),
            "exactly one of prompt or prompt_sha256 is required"
        );

        let mut both = spec("m", "p");
        both.prompt_sha256 = "abc".to_string();
        assert_eq!(
            provider.add(&both).unwrap_err().to_string(),
            "exactly one of prompt or prompt_sha256 is required"
        );

        let mut bad_error = spec("m", "p");
        bad_error.error = "explodes".to_string();
        assert_eq!(
            provider.add(&bad_error).unwrap_err().to_string(),
            "unknown error kind \"explodes\""
        );

        provider.add(&spec("m", "p")).expect("first add");
        let hash = hash_prompt("p");
        assert_eq!(
            provider.add(&spec("m", "p")).unwrap_err().to_string(),
            format!("duplicate fixture for model \"m\" prompt hash {hash}")
        );
    }

    #[test]
    fn from_fixtures_wraps_duplicate_with_index() {
        let err = FakeProvider::from_fixtures(&[spec("m", "p"), spec("m", "p")]).unwrap_err();
        let hash = hash_prompt("p");
        assert_eq!(
            err.to_string(),
            format!(
                "fake provider: fixture 1: duplicate fixture for model \"m\" prompt hash {hash}"
            )
        );
    }

    #[test]
    fn prompt_sha256_is_used_verbatim() {
        let mut by_hash = spec("m", "");
        by_hash.prompt = String::new();
        by_hash.prompt_sha256 = hash_prompt("keyed by hash");
        by_hash.output = "ok".to_string();
        let provider = FakeProvider::from_fixtures(&[by_hash]).expect("builds");
        assert_eq!(provider.len(), 1);
        // An uppercase hash would never be reachable, since complete hashes to lowercase.
    }

    #[test]
    fn scripted_errors_render_exactly() {
        assert_eq!(
            scripted_error(ProviderErrorKind::RateLimited).to_string(),
            "fake: rate_limited (status 429)"
        );
        assert_eq!(
            scripted_error(ProviderErrorKind::ProviderError).to_string(),
            "fake: provider_error (status 500)"
        );
        assert_eq!(
            scripted_error(ProviderErrorKind::InvalidRequest).to_string(),
            "fake: invalid_request (status 400)"
        );
        assert_eq!(
            scripted_error(ProviderErrorKind::Timeout).to_string(),
            "fake: timeout: context deadline exceeded"
        );
    }
}
