//! The prompt-path classifier and its resolver (spec §8.3; reference Part 4 §3; D2, D3, D4,
//! D5).
//!
//! [`Classifier`] makes the provider call: the fenced prompt (head and tail only past
//! `max_input_chars`), temperature 0, 512 output tokens, and the response schema as a
//! forced tool. There are at most two attempts (one retry). D3: both run inside ONE total
//! deadline (default 1.5 s), so a hung first attempt uses the whole budget. A transport
//! error, a timeout, or output that does not parse moves on to the next attempt.
//!
//! [`Resolver::resolve`] never fails a request, with one exception (D2):
//! 1. A zero-retention tenant classifies only on `classifier.zero_retention_column`, and
//!    only while that column's zero-retention evidence is valid. Otherwise the request is
//!    refused before any lookup or call, so the prompt never leaves the engine.
//! 2. The prompt hash is the tenant's HMAC fingerprint. The cache key adds the classifier
//!    version (`<column>/<prompt version>`) and, when the request carries one, the
//!    projection version (D5). A hit is `cached`.
//! 3. D4: past the global daily spend cap or the tenant's daily quota, no call is made and
//!    the answer is the fallback with `quota_exceeded`. Cache hits never count.
//! 4. The call. Its outcome feeds the degraded tracker, which raises one critical
//!    `classifier_degraded` alert per degraded episode. Its token cost is added to the
//!    tenant's daily spend.
//! 5. Success is cached (first write wins) and answered `classified`. Failure is the
//!    fallback classification, `fallback_default`, never cached.
//!
//! The prompt is never logged, and neither is the classifier's output.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use maverick_alerts::{AlertKind, AlertNotifier, Severity};
use maverick_config::Config;
use maverick_domain::capture::fingerprint;
use maverick_domain::classify::{
    self, Classification, DegradedTracker, degraded_alert_message, degraded_alert_subject,
};
use maverick_domain::gofmt::quote;
use maverick_domain::policy::RetentionMode;
use maverick_domain::pricing;
use maverick_providers::{
    CompletionRequest, ProviderClient, ProviderError, SETTING_RESPONSE_SCHEMA,
};
use maverick_store::catalog::ModelColumn;
use maverick_store::{Decimal, JsonRawValue, decisions, is_not_found};
use serde_json::{Map, Value};
use sqlx::PgPool;
use tokio::time::Instant;

/// The default total classifier deadline, retry included (D3).
pub const DEFAULT_DEADLINE: Duration = Duration::from_millis(1500);
/// The default per-tenant daily classifier quota (D4).
pub const DEFAULT_DAILY_QUOTA: u32 = 1000;
/// The default global daily classifier spend cap in USD (D4).
pub const DEFAULT_DAILY_SPEND_CAP_USD: f64 = 5.0;

/// How a prompt-path classification was obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// A classifier call answered.
    Classified,
    /// The cache answered.
    Cached,
    /// No classification: the classifier failed, has no client, or the budget is spent.
    FallbackDefault,
}

impl Method {
    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Classified => "classified",
            Self::Cached => "cached",
            Self::FallbackDefault => "fallback_default",
        }
    }
}

/// Why classification failed: `classify: column <column>: <last error>`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("classify: column {column}: {cause}")]
pub struct ClassifyError {
    /// The classifier column ref.
    pub column: String,
    /// The last attempt's error.
    pub cause: String,
    /// `cause` without any text the model or the provider wrote (G5, D19): a parse error
    /// names only the check that failed, and a provider error drops the provider's
    /// response body.
    pub redacted_cause: String,
}

impl ClassifyError {
    fn new(column: &str, cause: String, redacted_cause: String) -> Self {
        Self {
            column: column.to_owned(),
            cause,
            redacted_cause,
        }
    }

    /// The error as a log line may carry it: `classify: column <column>: <redacted cause>`.
    /// `cause` can quote the classifier's output (which can echo the prompt) or a
    /// provider's error body, so it never goes into a log field (G5, D19).
    #[must_use]
    pub fn redacted(&self) -> String {
        format!("classify: column {}: {}", self.column, self.redacted_cause)
    }
}

/// A provider error as [`ProviderError`]'s `Display` writes it, without the wrapped
/// source: that is where the provider's response body goes.
fn provider_summary(error: &ProviderError) -> String {
    let mut summary = format!("{}: {}", error.provider(), error.kind());
    if error.status() != 0 {
        summary.push_str(&format!(" (status {})", error.status()));
    }
    if error.attempts() > 0 {
        summary.push_str(&format!(" after {} attempt(s)", error.attempts()));
    }
    summary
}

/// A structured-output parse error (`classify::parse`) reduced to the check that failed.
/// Only `trailing data` and the `missing` errors carry no model text, so only they pass
/// through as they are.
fn parse_summary(error: &str) -> String {
    const TRAILING: &str = "parsing structured output: trailing data after JSON object";
    let summary = if error == TRAILING || error.starts_with("structured output missing ") {
        error
    } else if error.starts_with("parsing structured output: ") {
        "parsing structured output: not a JSON object of the response schema"
    } else if error.starts_with("capability ") {
        "capability is not a v1 capability"
    } else if error.starts_with("difficulty ") {
        "difficulty is out of range 1-5"
    } else if error.starts_with("confidence ") {
        "confidence is out of range 0-1"
    } else {
        "structured output did not parse"
    };
    summary.to_owned()
}

/// The outcome of one [`Classifier::classify`] call.
#[derive(Debug, Clone, PartialEq)]
pub struct Attempts {
    /// The classification or the last error.
    pub outcome: Result<Classification, ClassifyError>,
    /// Provider calls made (0, 1, or 2).
    pub calls: usize,
    /// Input tokens over every answered call, parsed or not (they are all paid for).
    pub tokens_in: usize,
    /// Output tokens over every answered call.
    pub tokens_out: usize,
}

/// One classifier column and its provider client.
#[derive(Clone)]
pub struct Classifier {
    client: Arc<dyn ProviderClient>,
    column: String,
    prompt_version: String,
    max_input_chars: usize,
}

impl fmt::Debug for Classifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Classifier")
            .field("column", &self.column)
            .field("prompt_version", &self.prompt_version)
            .field("max_input_chars", &self.max_input_chars)
            .finish_non_exhaustive()
    }
}

impl Classifier {
    /// A classifier on `column` (`model/endpoint`) with the current prompt version and the
    /// default input budget.
    pub fn new(client: Arc<dyn ProviderClient>, column: impl Into<String>) -> Self {
        Self {
            client,
            column: column.into(),
            prompt_version: classify::PROMPT_VERSION.to_owned(),
            max_input_chars: classify::DEFAULT_MAX_INPUT_CHARS,
        }
    }

    /// The same classifier with another prompt version (a new cache namespace).
    #[must_use]
    pub fn with_prompt_version(mut self, prompt_version: impl Into<String>) -> Self {
        self.prompt_version = prompt_version.into();
        self
    }

    /// The same classifier with another input budget, in characters.
    #[must_use]
    pub fn with_max_input_chars(mut self, max_input_chars: usize) -> Self {
        self.max_input_chars = max_input_chars;
        self
    }

    /// The column ref.
    #[must_use]
    pub fn column(&self) -> &str {
        &self.column
    }

    /// `<column>/<prompt version>`.
    #[must_use]
    pub fn version(&self) -> String {
        classify::classifier_version(&self.column, &self.prompt_version)
    }

    /// The provider request for `prompt`, and whether the prompt was truncated.
    #[must_use]
    pub fn request(&self, prompt: &str) -> (CompletionRequest, bool) {
        let (input, truncated) = classify::truncate(prompt, self.max_input_chars);
        let mut settings = Map::new();
        settings.insert(
            SETTING_RESPONSE_SCHEMA.to_owned(),
            Value::Object(classify::response_schema()),
        );
        let request = CompletionRequest {
            model: classify::model_from_column(&self.column).to_owned(),
            system: classify::SYSTEM_PROMPT.to_owned(),
            prompt: classify::fence(&input),
            temperature: classify::TEMPERATURE,
            max_tokens: classify::MAX_OUTPUT_TOKENS,
            settings,
        };
        (request, truncated)
    }

    /// Classify `prompt`. Every attempt must finish by `deadline` (D3); none starts after it.
    pub async fn classify(&self, prompt: &str, deadline: Instant) -> Attempts {
        let (request, truncated) = self.request(prompt);
        let deadline_exceeded = "context deadline exceeded";
        let mut attempts = Attempts {
            outcome: Err(ClassifyError::new(
                &self.column,
                deadline_exceeded.to_owned(),
                deadline_exceeded.to_owned(),
            )),
            calls: 0,
            tokens_in: 0,
            tokens_out: 0,
        };
        for _ in 0..=classify::RETRIES {
            if Instant::now() >= deadline {
                break;
            }
            attempts.calls += 1;
            let (cause, redacted_cause) = match tokio::time::timeout_at(
                deadline,
                self.client.complete(request.clone()),
            )
            .await
            {
                Err(_) => {
                    let timeout =
                        format!("{}: timeout: context deadline exceeded", self.client.name());
                    (timeout.clone(), timeout)
                }
                Ok(Err(error)) => (error.to_string(), provider_summary(&error)),
                Ok(Ok(response)) => {
                    attempts.tokens_in += response.tokens_in;
                    attempts.tokens_out += response.tokens_out;
                    match classify::parse(&response.text) {
                        Ok(mut classification) => {
                            if truncated {
                                classification.mark_truncated();
                            }
                            attempts.outcome = Ok(classification);
                            return attempts;
                        }
                        Err(error) => {
                            let redacted = parse_summary(&error);
                            (error, redacted)
                        }
                    }
                }
            };
            attempts.outcome = Err(ClassifyError::new(&self.column, cause, redacted_cause));
        }
        attempts
    }
}

/// Go `ClientFor`: the client of the provider whose catalog column is `column_ref`.
pub fn client_for(
    columns: &[ModelColumn],
    clients: &HashMap<String, Arc<dyn ProviderClient>>,
    column_ref: &str,
) -> Result<Arc<dyn ProviderClient>, String> {
    let column = find_column(columns, column_ref)?.ok_or_else(|| {
        format!(
            "classify: no model column matches classifier column {}",
            quote(column_ref)
        )
    })?;
    clients.get(&column.provider).cloned().ok_or_else(|| {
        format!(
            "classify: no provider client for {} (classifier column {})",
            quote(&column.provider),
            quote(column_ref)
        )
    })
}

/// The first catalog column whose model id and endpoint are `column_ref`'s.
fn find_column<'a>(
    columns: &'a [ModelColumn],
    column_ref: &str,
) -> Result<Option<&'a ModelColumn>, String> {
    let (model, endpoint) = classify::parse_column_ref(column_ref)?;
    Ok(columns
        .iter()
        .find(|column| column.model_id == model && column.endpoint == endpoint))
}

/// The resolver's settings (spec §8.3, §14.1).
#[derive(Debug, Clone, PartialEq)]
pub struct ResolverSettings {
    /// `classifier.column`.
    pub column: String,
    /// `classifier.zero_retention_column` (D2).
    pub zero_retention_column: Option<String>,
    /// `classifier.prompt_version`.
    pub prompt_version: String,
    /// The total classifier deadline (D3).
    pub deadline: Duration,
    /// The per-tenant daily quota, unless the tenant overrides it (D4).
    pub daily_quota: u32,
    /// The global daily spend cap in USD (D4).
    pub daily_spend_cap_usd: f64,
}

impl Default for ResolverSettings {
    fn default() -> Self {
        Self {
            column: classify::DEFAULT_COLUMN.to_owned(),
            zero_retention_column: None,
            prompt_version: classify::PROMPT_VERSION.to_owned(),
            deadline: DEFAULT_DEADLINE,
            daily_quota: DEFAULT_DAILY_QUOTA,
            daily_spend_cap_usd: DEFAULT_DAILY_SPEND_CAP_USD,
        }
    }
}

impl ResolverSettings {
    /// The settings from the configuration.
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        Self {
            column: config.classifier_column.clone(),
            zero_retention_column: config.classifier_zero_retention_column.clone(),
            prompt_version: classify::PROMPT_VERSION.to_owned(),
            deadline: config.classifier_deadline,
            daily_quota: config.classifier_daily_quota,
            daily_spend_cap_usd: config.classifier_daily_spend_cap_usd,
        }
    }
}

/// What the resolver needs to know about the request.
#[derive(Debug, Clone, Copy)]
pub struct ResolveInput<'a> {
    /// The tenant id.
    pub tenant_id: &'a str,
    /// The tenant's fingerprint key.
    pub prompt_hmac_key: &'a [u8],
    /// The tenant's quota override (D4).
    pub quota_override: Option<i32>,
    /// The tenant's zero-retention constraint (D2).
    pub zero_retention: bool,
    /// The prompt.
    pub prompt: &'a str,
    /// The request's `workload.projection_version` (D5).
    pub projection_version: Option<&'a str>,
    /// The catalog, for the zero-retention evidence and the classifier's pricing.
    pub columns: &'a [ModelColumn],
    /// The request time.
    pub now: DateTime<Utc>,
}

/// A resolved prompt-path classification.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolution {
    /// The classification (the fallback when `method` is `fallback_default`).
    pub classification: Classification,
    /// How it was obtained.
    pub method: Method,
    /// `hmac_sha256:<hex>`.
    pub prompt_hash: String,
    /// True when the D4 budget stopped the classifier call.
    pub quota_exceeded: bool,
}

/// Which classifier columns resolved against the catalog (D41).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Connection {
    /// `classifier.column`.
    pub default: bool,
    /// `classifier.zero_retention_column`, when one is configured.
    pub zero_retention: Option<bool>,
}

impl Connection {
    /// True when every configured classifier column resolved.
    #[must_use]
    pub fn complete(&self) -> bool {
        self.default && self.zero_retention != Some(false)
    }
}

fn read<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(PoisonError::into_inner)
}

fn write<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(PoisonError::into_inner)
}

/// D2: the tenant is zero-retention and no usable zero-retention classifier exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "workload.prompt cannot be classified for a zero-retention tenant: no zero-retention \
     classifier column with valid evidence is configured; send the declared form"
)]
pub struct ZeroRetentionRefused;

/// The prompt-path resolver: one per process.
pub struct Resolver {
    settings: ResolverSettings,
    /// Swappable after startup (D41): [`Resolver::reconnect`] replaces both.
    default: RwLock<Option<Arc<Classifier>>>,
    zero_retention: RwLock<Option<Arc<Classifier>>>,
    trackers: Mutex<HashMap<String, DegradedTracker>>,
    alerts: Option<Arc<dyn AlertNotifier>>,
}

impl fmt::Debug for Resolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Resolver")
            .field("settings", &self.settings)
            .field("default", &self.classifier())
            .field("zero_retention", &self.zero_retention_classifier())
            .field("alerts", &self.alerts.is_some())
            .finish_non_exhaustive()
    }
}

impl Default for Resolver {
    fn default() -> Self {
        Self::new(ResolverSettings::default())
    }
}

impl Resolver {
    /// A resolver without classifier clients: until [`Resolver::connect`] or
    /// [`Resolver::with_classifier`], every cache miss answers the fallback.
    #[must_use]
    pub fn new(settings: ResolverSettings) -> Self {
        Self {
            settings,
            default: RwLock::new(None),
            zero_retention: RwLock::new(None),
            trackers: Mutex::new(HashMap::new()),
            alerts: None,
        }
    }

    /// Finds both classifier columns' clients in `clients` (by the catalog column's
    /// provider). A column that does not resolve logs ERROR `classifier column
    /// unavailable; ...` with `column` and `error` (serve step 10); its requests then get
    /// the fallback classification until a later [`Resolver::reconnect`] resolves it (D41).
    #[must_use]
    pub fn connect(
        self,
        columns: &[ModelColumn],
        clients: &HashMap<String, Arc<dyn ProviderClient>>,
    ) -> Self {
        self.resolve_columns(columns, clients, true);
        self
    }

    /// Resolves both classifier columns again against the current catalog and swaps the
    /// result in, without a restart (D41). It logs nothing: the caller logs transitions.
    pub fn reconnect(
        &self,
        columns: &[ModelColumn],
        clients: &HashMap<String, Arc<dyn ProviderClient>>,
    ) -> Connection {
        self.resolve_columns(columns, clients, false)
    }

    fn resolve_columns(
        &self,
        columns: &[ModelColumn],
        clients: &HashMap<String, Arc<dyn ProviderClient>>,
        log: bool,
    ) -> Connection {
        let version = self.settings.prompt_version.as_str();
        let default = connect_column(columns, clients, &self.settings.column, version, log);
        let zero_retention = self
            .settings
            .zero_retention_column
            .as_deref()
            .map(|column| connect_column(columns, clients, column, version, log));
        let connection = Connection {
            default: default.is_some(),
            zero_retention: zero_retention.as_ref().map(Option::is_some),
        };
        *write(&self.default) = default.map(Arc::new);
        *write(&self.zero_retention) = zero_retention.flatten().map(Arc::new);
        connection
    }

    /// Which classifier columns are resolved now.
    #[must_use]
    pub fn connection(&self) -> Connection {
        Connection {
            default: self.classifier().is_some(),
            zero_retention: self
                .settings
                .zero_retention_column
                .as_ref()
                .map(|_| self.zero_retention_classifier().is_some()),
        }
    }

    /// The same resolver with `classifier` for ordinary tenants.
    #[must_use]
    pub fn with_classifier(self, classifier: Classifier) -> Self {
        *write(&self.default) = Some(Arc::new(classifier));
        self
    }

    /// The same resolver with `classifier` for zero-retention tenants (D2).
    #[must_use]
    pub fn with_zero_retention_classifier(self, classifier: Classifier) -> Self {
        *write(&self.zero_retention) = Some(Arc::new(classifier));
        self
    }

    fn classifier(&self) -> Option<Arc<Classifier>> {
        read(&self.default).clone()
    }

    fn zero_retention_classifier(&self) -> Option<Arc<Classifier>> {
        read(&self.zero_retention).clone()
    }

    /// The same resolver, notifying `alerts` after a `classifier_degraded` row.
    #[must_use]
    pub fn with_alerts(mut self, alerts: Arc<dyn AlertNotifier>) -> Self {
        self.alerts = Some(alerts);
        self
    }

    /// The settings.
    #[must_use]
    pub fn settings(&self) -> &ResolverSettings {
        &self.settings
    }

    /// The classifier version an ordinary tenant's prompt is classified with,
    /// `<classifier.column>/<prompt version>`: the only one `POST /v1/debug/replay` accepts.
    #[must_use]
    pub fn current_version(&self) -> String {
        self.version_of(&self.settings.column, self.classifier().as_deref())
    }

    /// Classify `input.prompt` (see the module documentation).
    pub async fn resolve(
        &self,
        pool: &PgPool,
        input: &ResolveInput<'_>,
    ) -> Result<Resolution, ZeroRetentionRefused> {
        // D2 comes first: a refused prompt is not even hashed.
        let (column, classifier) = self.select(input)?;
        let base_version = self.version_of(column, classifier.as_deref());
        let cache_version = match input.projection_version {
            Some(projection) => format!("{base_version}/{projection}"),
            None => base_version.clone(),
        };
        let prompt_hash = fingerprint(input.prompt_hmac_key, input.prompt.as_bytes());
        let resolution = |classification, method, quota_exceeded| Resolution {
            classification,
            method,
            prompt_hash: prompt_hash.clone(),
            quota_exceeded,
        };

        match decisions::get_classification_cache(
            pool,
            input.tenant_id,
            &prompt_hash,
            &cache_version,
        )
        .await
        {
            Ok(row) => match decode_cached(row.classification.get()) {
                Some(classification) => {
                    crate::metrics::classifier_cache(true);
                    return Ok(resolution(classification, Method::Cached, false));
                }
                None => {
                    tracing::warn!("classification cache row does not decode; treated as a miss")
                }
            },
            Err(error) if is_not_found(&error) => {}
            Err(error) => {
                tracing::warn!(error = %error, "classification cache lookup failed; treated as a miss");
            }
        }

        crate::metrics::classifier_cache(false);
        let started = std::time::Instant::now();
        let called = self
            .call(pool, input, column, classifier.as_deref(), &base_version)
            .await;
        crate::metrics::classifier_call(
            started.elapsed(),
            match &called {
                Called::Classified(_) => None,
                Called::Fallback {
                    quota_exceeded: true,
                } => Some("quota"),
                Called::Fallback { .. } => Some("failure"),
            },
        );
        match called {
            Called::Classified(classification) => {
                let cached = JsonRawValue::from_string(classification.cache_json())
                    .map_err(|error| error.to_string());
                let stored = match cached {
                    Ok(json) => decisions::upsert_classification_cache(
                        pool,
                        input.tenant_id,
                        &prompt_hash,
                        &cache_version,
                        &json,
                    )
                    .await
                    .map_err(|error| error.to_string()),
                    Err(error) => Err(error),
                };
                if let Err(error) = stored {
                    tracing::warn!(error = %error, "classification cache store failed");
                }
                Ok(resolution(classification, Method::Classified, false))
            }
            Called::Fallback { quota_exceeded } => Ok(resolution(
                Classification::fallback(),
                Method::FallbackDefault,
                quota_exceeded,
            )),
        }
    }

    /// Re-classify a captured prompt for `POST /v1/debug/replay/{capture_id}` (reference
    /// Part 5 §6.16): [`Resolver::resolve`] without the cache, so the classifier is asked
    /// again. D2 holds as on the prompt path: a zero-retention tenant's prompt goes only to
    /// the zero-retention column while its evidence is valid, and otherwise to no column
    /// at all, so the answer is the fallback. The call is paid for like any other, so D4,
    /// the spend record, and the degraded tracker apply.
    pub async fn replay(
        &self,
        pool: &PgPool,
        input: &ResolveInput<'_>,
    ) -> (Classification, Method) {
        let Ok((column, classifier)) = self.select(input) else {
            return (Classification::fallback(), Method::FallbackDefault);
        };
        let version = self.version_of(column, classifier.as_deref());
        match self
            .call(pool, input, column, classifier.as_deref(), &version)
            .await
        {
            Called::Classified(classification) => (classification, Method::Classified),
            Called::Fallback { .. } => (Classification::fallback(), Method::FallbackDefault),
        }
    }

    /// D2: the column and classifier for the tenant. A zero-retention tenant gets only the
    /// zero-retention column, and only while its evidence is valid.
    fn select(
        &self,
        input: &ResolveInput<'_>,
    ) -> Result<(&str, Option<Arc<Classifier>>), ZeroRetentionRefused> {
        if !input.zero_retention {
            return Ok((self.settings.column.as_str(), self.classifier()));
        }
        let Some(column) = self.settings.zero_retention_column.as_deref() else {
            return Err(ZeroRetentionRefused);
        };
        if !has_zero_evidence(input.columns, column, input.now) {
            return Err(ZeroRetentionRefused);
        }
        Ok((column, self.zero_retention_classifier()))
    }

    /// `<column>/<prompt version>`, from the connected classifier when there is one.
    fn version_of(&self, column: &str, classifier: Option<&Classifier>) -> String {
        classifier.map_or_else(
            || classify::classifier_version(column, &self.settings.prompt_version),
            Classifier::version,
        )
    }

    /// Steps 3 and 4: the D4 budget, then the call, its spend, and the degraded tracker.
    async fn call(
        &self,
        pool: &PgPool,
        input: &ResolveInput<'_>,
        column: &str,
        classifier: Option<&Classifier>,
        version: &str,
    ) -> Called {
        let Some(classifier) = classifier else {
            return Called::Fallback {
                quota_exceeded: false,
            };
        };
        match self.budget(pool, input).await {
            Budget::Available => {}
            Budget::Exceeded => {
                return Called::Fallback {
                    quota_exceeded: true,
                };
            }
            Budget::Unknown => {
                return Called::Fallback {
                    quota_exceeded: false,
                };
            }
        }

        let attempts = classifier
            .classify(input.prompt, Instant::now() + self.settings.deadline)
            .await;
        self.record_spend(pool, input, column, &attempts).await;
        self.track(pool, version, input.now, attempts.outcome.is_err())
            .await;
        match attempts.outcome {
            Ok(classification) => Called::Classified(classification),
            Err(error) => {
                // G5, D19: the redacted form, never the error's own text (see ClassifyError).
                tracing::warn!(
                    error = %error.redacted(),
                    calls = attempts.calls,
                    "classifier failed; answering the fallback classification"
                );
                Called::Fallback {
                    quota_exceeded: false,
                }
            }
        }
    }

    /// D4: the global spend cap first, then one count against the tenant's quota. A store
    /// failure spends nothing.
    async fn budget(&self, pool: &PgPool, input: &ResolveInput<'_>) -> Budget {
        let today = input.now.date_naive();
        match decisions::classifier_spend_on(pool, today).await {
            Ok(spent) => {
                let cap = Decimal::try_from(self.settings.daily_spend_cap_usd).unwrap_or_default();
                if spent >= cap {
                    tracing::info!("classifier daily spend cap reached; answering the fallback");
                    return Budget::Exceeded;
                }
            }
            Err(error) => {
                tracing::error!(error = %error, "classifier spend lookup failed; no classifier call");
                return Budget::Unknown;
            }
        }
        let quota = input
            .quota_override
            .unwrap_or_else(|| i32::try_from(self.settings.daily_quota).unwrap_or(i32::MAX));
        match decisions::count_classifier_call(pool, today, input.tenant_id, quota).await {
            Ok(Some(_)) => Budget::Available,
            Ok(None) => {
                tracing::info!("tenant classifier quota reached; answering the fallback");
                Budget::Exceeded
            }
            Err(error) => {
                tracing::error!(error = %error, "classifier quota count failed; no classifier call");
                Budget::Unknown
            }
        }
    }

    /// Adds the calls' token cost, at the classifier column's catalog pricing, to the
    /// tenant's daily spend. A column without pricing costs nothing; errors are logged.
    async fn record_spend(
        &self,
        pool: &PgPool,
        input: &ResolveInput<'_>,
        column: &str,
        attempts: &Attempts,
    ) {
        if attempts.tokens_in == 0 && attempts.tokens_out == 0 {
            return;
        }
        let pricing = find_column(input.columns, column)
            .ok()
            .flatten()
            .and_then(|column| pricing::parse_pricing(column.pricing.get()).ok());
        let Some(pricing) = pricing else {
            tracing::debug!("classifier column has no pricing; its spend is not counted");
            return;
        };
        let usd = pricing::cost(
            i64::try_from(attempts.tokens_in).unwrap_or(i64::MAX),
            i64::try_from(attempts.tokens_out).unwrap_or(i64::MAX),
            &pricing,
        );
        let Ok(usd) = Decimal::try_from(usd) else {
            return;
        };
        if let Err(error) = decisions::add_classifier_spend(
            pool,
            input.now.date_naive(),
            input.tenant_id,
            usd.round_dp(6),
        )
        .await
        {
            tracing::warn!(error = %error, "classifier spend was not recorded");
        }
    }

    /// Feeds the degraded tracker and writes the alert when this outcome raises it.
    async fn track(&self, pool: &PgPool, version: &str, now: DateTime<Utc>, failed: bool) {
        let observation = self
            .trackers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(version.to_owned())
            .or_default()
            .record(now, failed);
        if !observation.fire {
            return;
        }
        let subject = degraded_alert_subject(version, observation.rate, observation.samples)
            .and_then(|subject| JsonRawValue::from_string(subject).map_err(|e| e.to_string()));
        let written = match subject {
            Ok(subject) => maverick_alerts::insert_alert(
                pool,
                self.alerts.as_deref(),
                AlertKind::ClassifierDegraded,
                Severity::Critical,
                &subject,
                &degraded_alert_message(version, observation.rate),
            )
            .await
            .map(|_| ())
            .map_err(|error| error.to_string()),
            Err(error) => Err(error),
        };
        if let Err(error) = written {
            tracing::error!(error = %error, "classifier_degraded alert was not written");
        }
    }
}

/// The classifier for `column`, or `None` (logged) when it does not resolve.
fn connect_column(
    columns: &[ModelColumn],
    clients: &HashMap<String, Arc<dyn ProviderClient>>,
    column: &str,
    prompt_version: &str,
    log: bool,
) -> Option<Classifier> {
    match client_for(columns, clients, column) {
        Ok(client) => Some(Classifier::new(client, column).with_prompt_version(prompt_version)),
        Err(error) => {
            if !log {
                return None;
            }
            tracing::error!(
                column,
                error = %error,
                "classifier column unavailable; prompt-path requests will receive the \
                 fallback classification"
            );
            None
        }
    }
}

enum Budget {
    Available,
    Exceeded,
    Unknown,
}

/// What [`Resolver::call`] got: a classification, or the fallback (with whether D4 stopped
/// the call).
enum Called {
    Classified(Classification),
    Fallback { quota_exceeded: bool },
}

/// True when the column the classifier calls for `column_ref` has valid zero-retention
/// evidence at `now` (D2, spec §8.3). That is the column [`find_column`] picks, the first
/// catalog match by id, because its provider gets the call. A sibling with the same model
/// and endpoint (other settings, or another provider) does not count: its evidence says
/// nothing about the column that receives the prompt.
fn has_zero_evidence(columns: &[ModelColumn], column_ref: &str, now: DateTime<Utc>) -> bool {
    matches!(
        find_column(columns, column_ref),
        Ok(Some(column)) if crate::recommend::effective_retention(column, now) == RetentionMode::Zero
    )
}

/// A cached classification, decoded leniently: unknown fields are ignored and a stored
/// `truncated` is honored. `None` for anything that is not a classification.
fn decode_cached(text: &str) -> Option<Classification> {
    let value: Value = serde_json::from_str(text).ok()?;
    let object = value.as_object()?;
    let domain_tags = match object.get("domain_tags") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()?,
        Some(_) => return None,
    };
    Some(Classification {
        capability: object.get("capability")?.as_str()?.to_owned(),
        difficulty: object.get("difficulty")?.as_i64()?,
        domain_tags,
        confidence: object.get("confidence")?.as_f64()?,
        truncated: object
            .get("truncated")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_classifications_decode_leniently() {
        let full = decode_cached(
            r#"{"capability":"structured_extraction","difficulty":3,"domain_tags":["legal"],"confidence":0.8,"truncated":true,"extra":1}"#,
        )
        .expect("decodes");
        assert_eq!(
            full,
            Classification {
                capability: "structured_extraction".to_owned(),
                difficulty: 3,
                domain_tags: vec!["legal".to_owned()],
                confidence: 0.8,
                truncated: true,
            }
        );
        let bare = decode_cached(r#"{"capability":"factual_qa","difficulty":1,"confidence":1}"#)
            .expect("decodes");
        assert!(bare.domain_tags.is_empty() && !bare.truncated);
        for corrupt in [
            "[]",
            "null",
            r#"{"capability":5,"difficulty":1,"confidence":1}"#,
            r#"{"capability":"x","difficulty":1.5,"confidence":1}"#,
            r#"{"capability":"x","difficulty":1}"#,
            r#"{"capability":"x","difficulty":1,"confidence":1,"domain_tags":"legal"}"#,
            r#"{"capability":"x","difficulty":1,"confidence":1,"domain_tags":[1]}"#,
        ] {
            assert_eq!(decode_cached(corrupt), None, "{corrupt}");
        }
    }

    #[test]
    fn methods_have_wire_names() {
        assert_eq!(Method::Classified.as_str(), "classified");
        assert_eq!(Method::Cached.as_str(), "cached");
        assert_eq!(Method::FallbackDefault.as_str(), "fallback_default");
    }
}
