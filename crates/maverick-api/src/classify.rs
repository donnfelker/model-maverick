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
use std::sync::{Arc, Mutex, PoisonError};
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
use maverick_providers::{CompletionRequest, ProviderClient, SETTING_RESPONSE_SCHEMA};
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
        let mut attempts = Attempts {
            outcome: Err(ClassifyError {
                column: self.column.clone(),
                cause: "context deadline exceeded".to_owned(),
            }),
            calls: 0,
            tokens_in: 0,
            tokens_out: 0,
        };
        for _ in 0..=classify::RETRIES {
            if Instant::now() >= deadline {
                break;
            }
            attempts.calls += 1;
            let cause = match tokio::time::timeout_at(
                deadline,
                self.client.complete(request.clone()),
            )
            .await
            {
                Err(_) => format!("{}: timeout: context deadline exceeded", self.client.name()),
                Ok(Err(error)) => error.to_string(),
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
                        Err(error) => error,
                    }
                }
            };
            attempts.outcome = Err(ClassifyError {
                column: self.column.clone(),
                cause,
            });
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
    default: Option<Classifier>,
    zero_retention: Option<Classifier>,
    trackers: Mutex<HashMap<String, DegradedTracker>>,
    alerts: Option<Arc<dyn AlertNotifier>>,
}

impl fmt::Debug for Resolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Resolver")
            .field("settings", &self.settings)
            .field("default", &self.default)
            .field("zero_retention", &self.zero_retention)
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
            default: None,
            zero_retention: None,
            trackers: Mutex::new(HashMap::new()),
            alerts: None,
        }
    }

    /// Finds both classifier columns' clients in `clients` (by the catalog column's
    /// provider). A column that does not resolve logs ERROR `classifier column
    /// unavailable; ...` with `column` and `error` (serve step 10); its requests then get
    /// the fallback classification.
    #[must_use]
    pub fn connect(
        mut self,
        columns: &[ModelColumn],
        clients: &HashMap<String, Arc<dyn ProviderClient>>,
    ) -> Self {
        let version = self.settings.prompt_version.clone();
        self.default = connect_column(columns, clients, &self.settings.column, &version);
        self.zero_retention = self
            .settings
            .zero_retention_column
            .as_deref()
            .and_then(|column| connect_column(columns, clients, column, &version));
        self
    }

    /// The same resolver with `classifier` for ordinary tenants.
    #[must_use]
    pub fn with_classifier(mut self, classifier: Classifier) -> Self {
        self.default = Some(classifier);
        self
    }

    /// The same resolver with `classifier` for zero-retention tenants (D2).
    #[must_use]
    pub fn with_zero_retention_classifier(mut self, classifier: Classifier) -> Self {
        self.zero_retention = Some(classifier);
        self
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

    /// Classify `input.prompt` (see the module documentation).
    pub async fn resolve(
        &self,
        pool: &PgPool,
        input: &ResolveInput<'_>,
    ) -> Result<Resolution, ZeroRetentionRefused> {
        // D2 comes first: a refused prompt is not even hashed.
        let (column, classifier) = if input.zero_retention {
            let Some(column) = self.settings.zero_retention_column.as_deref() else {
                return Err(ZeroRetentionRefused);
            };
            if !has_zero_evidence(input.columns, column, input.now) {
                return Err(ZeroRetentionRefused);
            }
            (column, self.zero_retention.as_ref())
        } else {
            (self.settings.column.as_str(), self.default.as_ref())
        };
        let base_version = classifier.map_or_else(
            || classify::classifier_version(column, &self.settings.prompt_version),
            Classifier::version,
        );
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

        let Some(classifier) = classifier else {
            return Ok(resolution(
                Classification::fallback(),
                Method::FallbackDefault,
                false,
            ));
        };
        match self.budget(pool, input).await {
            Budget::Available => {}
            Budget::Exceeded => {
                return Ok(resolution(
                    Classification::fallback(),
                    Method::FallbackDefault,
                    true,
                ));
            }
            Budget::Unknown => {
                return Ok(resolution(
                    Classification::fallback(),
                    Method::FallbackDefault,
                    false,
                ));
            }
        }

        let attempts = classifier
            .classify(input.prompt, Instant::now() + self.settings.deadline)
            .await;
        self.record_spend(pool, input, column, &attempts).await;
        self.track(pool, &base_version, input.now, attempts.outcome.is_err())
            .await;
        match attempts.outcome {
            Ok(classification) => {
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
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    calls = attempts.calls,
                    "classifier failed; answering the fallback classification"
                );
                Ok(resolution(
                    Classification::fallback(),
                    Method::FallbackDefault,
                    false,
                ))
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
) -> Option<Classifier> {
    match client_for(columns, clients, column) {
        Ok(client) => Some(Classifier::new(client, column).with_prompt_version(prompt_version)),
        Err(error) => {
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

/// True when a catalog column for `column_ref` has valid zero-retention evidence at `now`.
fn has_zero_evidence(columns: &[ModelColumn], column_ref: &str, now: DateTime<Utc>) -> bool {
    let Ok((model, endpoint)) = classify::parse_column_ref(column_ref) else {
        return false;
    };
    columns
        .iter()
        .filter(|column| column.model_id == model && column.endpoint == endpoint)
        .any(|column| {
            let stored = match column.retention_mode.as_str() {
                "zero" => RetentionMode::Zero,
                "standard" => RetentionMode::Standard,
                _ => RetentionMode::Unknown,
            };
            stored.effective(column.retention_valid_until, now) == RetentionMode::Zero
        })
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
