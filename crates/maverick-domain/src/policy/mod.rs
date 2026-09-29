//! Tenant policy (spec §7; reference Part 4 §1): the inputs policy reads and the
//! request-time evaluation of spec §7.1 steps 1 to 4.
//!
//! No SQL here. Callers load the tenant config, the catalog, and the cell's latest
//! aggregates, then call the pure functions in this module. G4 write-time simulation
//! (spec §7.2) and ranking (spec §7.4) build on the same types.

mod evaluate;

pub use evaluate::{evaluate, resolve_pool};

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::scoring::regression::severity;

/// The alert kind raised when policy finds (or would leave) a pool with no usable
/// column.
pub const POOL_WOULD_BE_EMPTY: &str = "pool_would_be_empty";

/// A catalog column's lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColumnStatus {
    ColdStart,
    Active,
    Deprecated,
    Retired,
}

/// A column's data-retention status (spec §4.5).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionMode {
    #[default]
    Unknown,
    Standard,
    Zero,
}

impl RetentionMode {
    /// The exact wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            RetentionMode::Unknown => "unknown",
            RetentionMode::Standard => "standard",
            RetentionMode::Zero => "zero",
        }
    }

    /// The mode as policy, classifier selection, and responses read it at `now`
    /// (spec §4.5). Evidence whose `valid_until` has passed reads as `unknown`. `zero`
    /// counts only with an unexpired `valid_until`, so `zero` without one also reads as
    /// `unknown`: stale or missing evidence never yields compliance.
    pub fn effective(self, valid_until: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Self {
        match valid_until {
            Some(until) if now < until => self,
            Some(_) => RetentionMode::Unknown,
            None if self == RetentionMode::Zero => RetentionMode::Unknown,
            None => self,
        }
    }
}

/// One `model_columns` row as policy reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogColumn {
    pub id: String,
    pub model_id: String,
    pub status: ColumnStatus,
    #[serde(default)]
    pub retention_mode: RetentionMode,
    #[serde(default)]
    pub retention_valid_until: Option<DateTime<Utc>>,
}

impl CatalogColumn {
    /// The column's retention mode at `now` (see [`RetentionMode::effective`]).
    pub fn effective_retention_mode(&self, now: DateTime<Utc>) -> RetentionMode {
        self.retention_mode
            .effective(self.retention_valid_until, now)
    }

    /// An `allowed_models` entry matches a column when it equals the column `id`
    /// (exact) or its `model_id` (every endpoint and settings variant).
    pub fn matches_entry(&self, entry: &str) -> bool {
        entry == self.id || entry == self.model_id
    }
}

/// A named tenant pool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pool {
    pub name: String,
    pub model_column_ids: Vec<String>,
    pub is_default: bool,
}

/// A tenant's stored (standing) constraints. No row means unconstrained.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct StandingConstraints {
    #[serde(default)]
    pub max_cost_per_run: Option<f64>,
    #[serde(default)]
    pub max_latency_ms: Option<i32>,
    #[serde(default)]
    pub zero_retention: bool,
}

/// The tenant configuration policy evaluates against.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TenantConfig {
    pub tenant_id: String,
    pub pools: Vec<Pool>,
    #[serde(default)]
    pub constraints: StandingConstraints,
}

/// The latest cost and latency EWMAs of one column in the request's
/// (capability, tier) cell. A missing value cannot violate a constraint.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct CellAggregate {
    #[serde(default)]
    pub cost_per_run_usd: Option<f64>,
    #[serde(default)]
    pub latency_p95_ms: Option<f64>,
}

/// `column_id -> aggregate` for the request's cell. A column without an entry is
/// unmeasured in that cell.
pub type CellAggregates = HashMap<String, CellAggregate>;

/// The per-request preferences (`constraints` in the request body). They deprioritize
/// and never drop.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct RequestConstraints {
    #[serde(default)]
    pub max_cost_per_run: Option<f64>,
    #[serde(default)]
    pub max_latency_ms: Option<i32>,
}

/// The parts of a recommend request policy reads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyRequest {
    /// The pool name; empty means the tenant's default pool.
    #[serde(default)]
    pub pool: String,
    /// `None` or an empty list is no filter.
    #[serde(default)]
    pub allowed_models: Option<Vec<String>>,
    pub capability: String,
    pub difficulty_tier: i32,
    #[serde(default)]
    pub constraints: RequestConstraints,
}

/// A cost or latency constraint name (exact wire spelling).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Constraint {
    MaxCostPerRun,
    MaxLatencyMs,
}

impl Constraint {
    pub fn as_str(self) -> &'static str {
        match self {
            Constraint::MaxCostPerRun => "max_cost_per_run",
            Constraint::MaxLatencyMs => "max_latency_ms",
        }
    }

    /// The aggregate dimension the constraint limits.
    pub fn dimension(self) -> &'static str {
        match self {
            Constraint::MaxCostPerRun => "cost_per_run_usd",
            Constraint::MaxLatencyMs => "latency_p95_ms",
        }
    }

    /// The exclusion reason for a standing violation of this constraint.
    pub fn standing_reason(self) -> ExclusionReason {
        match self {
            Constraint::MaxCostPerRun => ExclusionReason::StandingMaxCostPerRun,
            Constraint::MaxLatencyMs => ExclusionReason::StandingMaxLatencyMs,
        }
    }
}

/// The closed exclusion-reason enum (spec §10.2). `no_data` is emitted by ranking,
/// never by [`evaluate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ExclusionReason {
    #[serde(rename = "not_in_pool")]
    NotInPool,
    #[serde(rename = "not_in_allowlist")]
    NotInAllowlist,
    #[serde(rename = "standing_constraint:max_cost_per_run")]
    StandingMaxCostPerRun,
    #[serde(rename = "standing_constraint:max_latency_ms")]
    StandingMaxLatencyMs,
    /// D1: new in the Rust port.
    #[serde(rename = "standing_constraint:zero_retention")]
    StandingZeroRetention,
    #[serde(rename = "status_retired")]
    StatusRetired,
    #[serde(rename = "no_data")]
    NoData,
}

impl ExclusionReason {
    /// Every reason, in the spec's order (`Reasons()`).
    pub const ALL: [ExclusionReason; 7] = [
        ExclusionReason::NotInPool,
        ExclusionReason::NotInAllowlist,
        ExclusionReason::StandingMaxCostPerRun,
        ExclusionReason::StandingMaxLatencyMs,
        ExclusionReason::StandingZeroRetention,
        ExclusionReason::StatusRetired,
        ExclusionReason::NoData,
    ];

    /// The exact wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            ExclusionReason::NotInPool => "not_in_pool",
            ExclusionReason::NotInAllowlist => "not_in_allowlist",
            ExclusionReason::StandingMaxCostPerRun => "standing_constraint:max_cost_per_run",
            ExclusionReason::StandingMaxLatencyMs => "standing_constraint:max_latency_ms",
            ExclusionReason::StandingZeroRetention => "standing_constraint:zero_retention",
            ExclusionReason::StatusRetired => "status_retired",
            ExclusionReason::NoData => "no_data",
        }
    }
}

/// One constraint a candidate breaks: `{constraint, limit, estimated}`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Violation {
    pub constraint: Constraint,
    pub limit: f64,
    pub estimated: f64,
}

/// A column that survived policy, in output order.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Survivor {
    pub column_id: String,
    /// False when the column breaks a request preference.
    pub prefs_satisfied: bool,
    /// Standing violations carried by the G1 drift fallback, then preference
    /// violations.
    pub violations: Vec<Violation>,
}

/// A column that policy removed, with its reason and an exact detail string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Exclusion {
    pub column_id: String,
    pub reason: ExclusionReason,
    pub detail: String,
}

/// The subject of a `pool_would_be_empty` alert raised at request time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AlertSubject {
    pub tenant_id: String,
    pub pool: String,
    pub capability: String,
    pub difficulty_tier: i32,
    /// The least-violating columns kept by the drift fallback; `None` when nothing
    /// was retained.
    pub retained: Option<Vec<String>>,
    /// The constraint that emptied the pool, when policy cannot relax it (D1:
    /// `zero_retention`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub constraint: Option<&'static str>,
}

/// An alert that evaluation asks the caller to raise.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Alert {
    pub kind: &'static str,
    pub severity: &'static str,
    pub subject: AlertSubject,
    pub message: String,
}

impl Alert {
    fn pool_would_be_empty(subject: AlertSubject, message: String) -> Self {
        Alert {
            kind: POOL_WOULD_BE_EMPTY,
            severity: severity::CRITICAL,
            subject,
            message,
        }
    }
}

/// The outcome of [`evaluate`]: the ordered survivors and every exclusion.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Decision {
    pub pool: String,
    pub survivors: Vec<Survivor>,
    pub exclusions: Vec<Exclusion>,
    /// False when the G1 drift fallback kept columns that break a standing
    /// constraint. Preferences never change it.
    pub constraints_met: bool,
    pub drift_alert: Option<Alert>,
}

impl Decision {
    fn exclude(&mut self, column_id: &str, reason: ExclusionReason, detail: String) {
        self.exclusions.push(Exclusion {
            column_id: column_id.to_owned(),
            reason,
            detail,
        });
    }
}

/// A policy error code (G3 caller errors and the D1 privacy error).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyErrorCode {
    UnknownPool,
    UnknownModel,
    ModelNotInPool,
    /// D1: a zero-retention tenant whose candidates hold no valid zero-retention
    /// evidence.
    NoCompliantColumns,
}

impl PolicyErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            PolicyErrorCode::UnknownPool => "unknown_pool",
            PolicyErrorCode::UnknownModel => "unknown_model",
            PolicyErrorCode::ModelNotInPool => "model_not_in_pool",
            PolicyErrorCode::NoCompliantColumns => "no_compliant_columns",
        }
    }

    /// The HTTP status the API answers with.
    pub fn http_status(self) -> u16 {
        match self {
            PolicyErrorCode::NoCompliantColumns => 409,
            _ => 422,
        }
    }
}

/// A request that policy refuses: `{code, message, details}`, plus the alert to raise
/// for `no_compliant_columns`.
#[derive(Debug, Clone, PartialEq, Serialize, thiserror::Error)]
#[error("{message}")]
pub struct PolicyError {
    pub code: PolicyErrorCode,
    pub message: String,
    pub details: JsonValue,
    /// Boxed: only `no_compliant_columns` carries one, and it keeps the error small.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alert: Option<Box<Alert>>,
}
