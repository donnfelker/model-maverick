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
//! The proxy runs the same ladder (D46, spec §8.1) around its own engine call:
//! [`FailOpen::record`] after a recommendation, [`FailOpen::refuse`] after a terminal
//! privacy error, and [`FailOpen::fall_back`] after an engine failure. It has no caller
//! model to fall back to, keeps the whole recommendation (its id goes into telemetry),
//! and bounds the store with [`FailOpen::with_capacity`].

use std::collections::{BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::num::NonZeroUsize;
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

/// Where a failed engine call falls open to, for a caller with no model of its own (the
/// proxy's route, D46).
#[derive(Debug, Clone, PartialEq)]
pub enum Fallback {
    /// The last good recommendation for the key, with its id. For a zero-retention tenant
    /// the chain holds only its `zero`-stamped entries.
    LastKnownGood(Recommendation),
    /// The default model the caller passed, allowed for this tenant.
    DefaultModel(String),
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

#[derive(Debug)]
struct Cached {
    recommendation: Recommendation,
    stored_at: Instant,
    /// The store's clock at the last read or write, for least-recently-used eviction.
    used: u64,
}

/// The last-known-good recommendations by (tenant, key), and a use counter.
#[derive(Debug, Default)]
struct Store {
    entries: HashMap<(String, String), Cached>,
    clock: u64,
}

impl Store {
    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }
}

/// The ladder and its in-memory state: last-known-good chains by (tenant, workload key)
/// and the tenants known to be zero retention.
#[derive(Debug, Default)]
pub struct FailOpen {
    config: FailOpenConfig,
    ttl: Duration,
    /// At most this many cached chains; `None` is unbounded.
    capacity: Option<NonZeroUsize>,
    chains: Mutex<Store>,
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
            capacity: None,
            chains: Mutex::default(),
            zero_retention: Mutex::default(),
        }
    }

    /// Keeps at most `capacity` cached chains: storing another evicts the least recently
    /// used one. Zero means unbounded, the default.
    #[must_use]
    pub fn with_capacity(mut self, capacity: usize) -> Self {
        self.capacity = NonZeroUsize::new(capacity);
        self
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
            .entries
            .retain(|(t, _), _| t != tenant);
    }

    /// Records a recommendation MM answered: the tenant's zero-retention state, and the
    /// recommendation as the key's last good one when its chain is not empty.
    pub fn record(&self, tenant: &str, key: &str, recommendation: &Recommendation) {
        self.set_zero_retention(tenant, recommendation.zero_retention);
        if recommendation.chain.is_empty() {
            return;
        }
        let mut store = self.chains.lock().unwrap_or_else(PoisonError::into_inner);
        let key = (tenant.to_owned(), key.to_owned());
        if let Some(capacity) = self.capacity
            && !store.entries.contains_key(&key)
            && store.entries.len() >= capacity.get()
        {
            let oldest = store
                .entries
                .iter()
                .min_by_key(|(_, cached)| cached.used)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                store.entries.remove(&oldest);
            }
        }
        let used = store.tick();
        store.entries.insert(
            key,
            Cached {
                recommendation: recommendation.clone(),
                stored_at: Instant::now(),
                used,
            },
        );
    }

    /// Records a terminal privacy error: only zero-retention tenants get one, so the
    /// tenant is zero retention from now on, and its cached chains are purged.
    pub fn refuse(&self, tenant: &str) {
        self.set_zero_retention(tenant, true);
        self.purge(tenant);
    }

    /// The fresh last good recommendation for the key, if any. For a zero-retention tenant
    /// it keeps only the `zero`-stamped entries, and none left means none.
    fn last_good(&self, tenant: &str, key: &str) -> Option<Recommendation> {
        let zero_retention = self.is_zero_retention(tenant);
        let mut store = self.chains.lock().unwrap_or_else(PoisonError::into_inner);
        let key = (tenant.to_owned(), key.to_owned());
        let fresh = store
            .entries
            .get(&key)
            .is_some_and(|cached| cached.stored_at.elapsed() < self.ttl);
        if !fresh {
            store.entries.remove(&key);
            return None;
        }
        let used = store.tick();
        let cached = store.entries.get_mut(&key)?;
        cached.used = used;
        let mut recommendation = cached.recommendation.clone();
        if zero_retention {
            recommendation
                .chain
                .retain(|entry| entry.retention_mode == RetentionMode::Zero);
        }
        (!recommendation.chain.is_empty()).then_some(recommendation)
    }

    /// A model the ladder may fall open to: any model for an ordinary tenant, only a
    /// verified zero-retention model for a zero-retention tenant.
    fn allowed(&self, model: Option<&str>, zero_retention: bool) -> Option<String> {
        model
            .filter(|m| !zero_retention || self.config.zero_retention_verified.contains(*m))
            .map(str::to_owned)
    }

    /// Where an engine failure falls open to, for a caller with no model of its own: the
    /// key's last good recommendation, then `default_model` when it is allowed for the
    /// tenant. `None` means nothing is usable, and the caller fails closed.
    pub fn fall_back(
        &self,
        tenant: &str,
        key: &str,
        default_model: Option<&str>,
    ) -> Option<Fallback> {
        self.last_good(tenant, key)
            .map(Fallback::LastKnownGood)
            .or_else(|| {
                self.allowed(default_model, self.is_zero_retention(tenant))
                    .map(Fallback::DefaultModel)
            })
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
                self.record(tenant, workload, &recommendation);
                Ok(Plan::Recommended(recommendation))
            }
            Err(RecommendError::Engine(failure)) => {
                self.fall_open(tenant, workload, original_model, failure)
            }
            Err(terminal) => {
                self.refuse(tenant);
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
        let plan = match self.last_good(tenant, workload) {
            Some(recommendation) => Some(Plan::LastKnownGood(recommendation.chain)),
            None => self
                .allowed(self.config.default_model.as_deref(), zero_retention)
                .map(Plan::DefaultModel)
                .or_else(|| {
                    self.allowed(original_model, zero_retention)
                        .map(Plan::OriginalModel)
                }),
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
