//! The fail-open ladder and its zero-retention rules (G14, D17; sdk-v0 plan §2.3).
//!
//! An MM failure never breaks the customer's LLM call: the last-known-good chain for the
//! workload, then the configured default model, then the caller's own concrete model.
//! MM's absence shows only in logs. Privacy outranks G14: the terminal privacy errors
//! (`422 zero_retention_requires_declared`, `409 no_compliant_columns`) are surfaced,
//! never failed open, and purge the tenant's cached chains; a zero-retention tenant falls
//! open only to `zero`-stamped cached entries or to models configured as verified
//! zero-retention, and otherwise fails closed. All state is in memory.
//!
//! The trial proxy is exempt from fail-open by position (an engine failure is its
//! `502 engine_error`); it still obeys the zero-retention rules.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use tokio::time::Instant;

use crate::recommend::{ChainEntry, EngineFailure, RecommendError, Recommendation, RetentionMode};

/// How long a last-known-good chain stays usable.
pub const LAST_KNOWN_GOOD_TTL: Duration = Duration::from_secs(5 * 60);

/// The SDK privacy mode (`MODELMAVERICK_PRIVACY`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Privacy {
    /// Send the declared workload only.
    Declared,
    /// Send the prompt for classification.
    Prompt,
    /// Never contact MM.
    Off,
}

/// The ladder's local configuration.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FailOpenConfig {
    /// `MODELMAVERICK_DEFAULT_MODEL`.
    pub default_model: Option<String>,
    /// `MODELMAVERICK_ZERO_RETENTION_VERIFIED`: models the operator has verified as zero
    /// retention. Only these may be fallen open to for a zero-retention tenant.
    pub zero_retention_verified: BTreeSet<String>,
}

/// What to call.
#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    /// MM answered: run its chain.
    Recommended(Recommendation),
    /// MM failed: the last-known-good chain (for a zero-retention tenant, only its
    /// `zero`-stamped entries).
    LastKnownGood(Vec<ChainEntry>),
    /// MM failed or is off, and there is no usable cached chain: the configured default.
    DefaultModel(String),
    /// MM failed or is off, and there is no default: the caller's concrete model.
    OriginalModel(String),
}

/// Why nothing may be called.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LadderError {
    /// A terminal privacy error from the engine, surfaced unchanged.
    #[error(transparent)]
    Terminal(RecommendError),
    /// A zero-retention tenant with no compliant fallback: fail closed.
    #[error("no_compliant_columns")]
    NoCompliantColumns,
    /// MM failed and there is nothing to fall back to.
    #[error("maverick unavailable and no fallback model: {0}")]
    Unavailable(EngineFailure),
    /// Privacy is off and there is no model to call.
    #[error("maverick is off and no default or concrete model is configured")]
    NoModel,
}

#[derive(Debug, Clone)]
struct Cached {
    chain: Vec<ChainEntry>,
    stored_at: Instant,
}

/// The ladder and its in-memory state: last-known-good chains by (tenant, workload key)
/// and the tenants known to be zero retention.
#[derive(Debug, Default)]
pub struct FailOpen {
    config: FailOpenConfig,
    ttl: Duration,
    chains: Mutex<HashMap<(String, String), Cached>>,
    zero_retention: Mutex<HashSet<String>>,
}

impl FailOpen {
    pub fn new(config: FailOpenConfig) -> Self {
        Self::with_ttl(config, LAST_KNOWN_GOOD_TTL)
    }

    pub fn with_ttl(config: FailOpenConfig, ttl: Duration) -> Self {
        FailOpen {
            config,
            ttl,
            chains: Mutex::default(),
            zero_retention: Mutex::default(),
        }
    }

    /// True once MM said the tenant is zero retention: the core must send the declared
    /// form only.
    pub fn is_zero_retention(&self, tenant: &str) -> bool {
        self.zero_retention
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(tenant)
    }

    fn set_zero_retention(&self, tenant: &str, on: bool) {
        let mut tenants = self
            .zero_retention
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if on {
            tenants.insert(tenant.to_owned());
        } else {
            tenants.remove(tenant);
        }
    }

    fn purge(&self, tenant: &str) {
        self.chains
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(t, _), _| t != tenant);
    }

    /// The fresh cached chain for the workload, if any.
    fn cached(&self, tenant: &str, workload: &str) -> Option<Vec<ChainEntry>> {
        let mut chains = self.chains.lock().unwrap_or_else(PoisonError::into_inner);
        let key = (tenant.to_owned(), workload.to_owned());
        let fresh = chains
            .get(&key)
            .is_some_and(|cached| cached.stored_at.elapsed() < self.ttl);
        if fresh {
            chains.get(&key).map(|cached| cached.chain.clone())
        } else {
            chains.remove(&key);
            None
        }
    }

    /// A model the ladder may fall open to: any model for an ordinary tenant, only a
    /// verified zero-retention model for a zero-retention tenant.
    fn allowed(&self, model: Option<&str>, zero_retention: bool) -> Option<String> {
        model
            .filter(|m| !zero_retention || self.config.zero_retention_verified.contains(*m))
            .map(str::to_owned)
    }

    /// Resolves one call: asks MM through `fetch` unless privacy is off, records the
    /// outcome, and falls open when MM fails.
    pub async fn plan<F, Fut>(
        &self,
        tenant: &str,
        workload: &str,
        privacy: Privacy,
        original_model: Option<&str>,
        fetch: F,
    ) -> Result<Plan, LadderError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Recommendation, RecommendError>>,
    {
        if privacy == Privacy::Off {
            return self
                .allowed(self.config.default_model.as_deref(), false)
                .map(Plan::DefaultModel)
                .or_else(|| original_model.map(|m| Plan::OriginalModel(m.to_owned())))
                .ok_or(LadderError::NoModel);
        }
        match fetch().await {
            Ok(recommendation) => {
                self.set_zero_retention(tenant, recommendation.zero_retention);
                if !recommendation.chain.is_empty() {
                    self.chains
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .insert(
                            (tenant.to_owned(), workload.to_owned()),
                            Cached {
                                chain: recommendation.chain.clone(),
                                stored_at: Instant::now(),
                            },
                        );
                }
                Ok(Plan::Recommended(recommendation))
            }
            Err(RecommendError::Engine(failure)) => {
                self.fall_open(tenant, workload, original_model, failure)
            }
            Err(terminal) => {
                // Only zero-retention tenants get the privacy errors.
                self.set_zero_retention(tenant, true);
                self.purge(tenant);
                tracing::warn!(error = %terminal, "maverick refused the workload; not failing open");
                Err(LadderError::Terminal(terminal))
            }
        }
    }

    fn fall_open(
        &self,
        tenant: &str,
        workload: &str,
        original_model: Option<&str>,
        failure: EngineFailure,
    ) -> Result<Plan, LadderError> {
        let zero_retention = self.is_zero_retention(tenant);
        let chain: Vec<ChainEntry> = self
            .cached(tenant, workload)
            .unwrap_or_default()
            .into_iter()
            .filter(|entry| !zero_retention || entry.retention_mode == RetentionMode::Zero)
            .collect();
        let plan = if chain.is_empty() {
            self.allowed(self.config.default_model.as_deref(), zero_retention)
                .map(Plan::DefaultModel)
                .or_else(|| {
                    self.allowed(original_model, zero_retention)
                        .map(Plan::OriginalModel)
                })
        } else {
            Some(Plan::LastKnownGood(chain))
        };
        match plan {
            Some(plan) => {
                tracing::warn!(error = %failure, "maverick unavailable; failing open");
                Ok(plan)
            }
            None if zero_retention => {
                tracing::warn!(error = %failure, "maverick unavailable; zero-retention tenant fails closed");
                Err(LadderError::NoCompliantColumns)
            }
            None => Err(LadderError::Unavailable(failure)),
        }
    }
}
