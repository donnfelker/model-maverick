//! Catalog lifecycle, the retirement guard decision, quality transfer, and retention evaluation
//! (spec §4.1 to §4.5; reference Part 3 §5). Pure logic: the `catalog_sync`/`price_sync` jobs and
//! the internal column endpoints that read the store and call the internal API live in 2e/2d.
//!
//! Pricing (`ParsePricing`, `Cost`) already lives in [`crate::pricing`]. Retention modes and their
//! `effective` evaluation live in [`crate::policy::RetentionMode`]; this module adds the write-time
//! validity window and the lifecycle and retirement-guard decisions.

use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::policy::{
    ColumnStatus, ConfigError, ConfigErrorCode, Pool, SimulationState, StandingConstraints,
    simulate,
};

/// The maximum span between retention verification and its expiry (spec §4.5).
pub const RETENTION_MAX_VALID_DAYS: i64 = 90;

/// Whether a column status is runnable: benchmarks run on `cold_start`, `active`, and
/// `deprecated` columns. `retired` and any unknown status are not runnable (reference §5.1).
#[must_use]
pub fn runnable(status: &str) -> bool {
    matches!(status, "cold_start" | "active" | "deprecated")
}

/// Whether a `cold_start` column may transition to `active` on `day` (reference §5.1): its
/// dense-run window has ended and it has at least one measured aggregate.
#[must_use]
pub fn should_activate(
    cold_start_until: Option<NaiveDate>,
    day: NaiveDate,
    measured: bool,
) -> bool {
    matches!(cold_start_until, Some(until) if day >= until) && measured
}

/// Whether a `deprecated` column may transition to `retired` on `day` (reference §5.1): its
/// deprecation date has arrived and the retirement guard found no pool it would empty.
#[must_use]
pub fn should_retire(
    deprecation_date: Option<NaiveDate>,
    day: NaiveDate,
    guard_allows: bool,
) -> bool {
    matches!(deprecation_date, Some(date) if day >= date) && guard_allows
}

/// A tenant pool that retiring a column would leave with no usable column. The last three fields
/// are absent when the whole pool is empty rather than a specific cell (reference §5.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockedPool {
    /// The tenant that owns the pool.
    pub tenant_id: String,
    /// The pool name.
    pub pool: String,
    /// The constraint that empties the pool, if a constraint (not retirement) did it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constraint: Option<String>,
    /// The capability of the emptied cell, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability: Option<String>,
    /// The tier of the emptied cell, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub difficulty_tier: Option<i32>,
}

/// The retirement guard's refusal: retiring `column_id` would empty `pools` (reference §5.2). Its
/// [`message`](Self::message) and [`alert_subject`](Self::alert_subject) are the exact wire forms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RetirementBlocked {
    /// The column that cannot retire.
    pub column_id: String,
    /// The pools it would empty.
    pub pools: Vec<BlockedPool>,
}

impl RetirementBlocked {
    /// The blocked error text (reference §5.2, exact format).
    #[must_use]
    pub fn message(&self) -> String {
        let list = self
            .pools
            .iter()
            .map(|pool| format!("{:?} of tenant {}", pool.pool, pool.tenant_id))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "retiring column {} would leave pool(s) {list} with zero usable columns; amend the pools first (spec 4.1)",
            self.column_id
        )
    }

    /// The `pool_would_be_empty` alert subject marshaled in field order (reference §5.2):
    /// `{column_id, pools}`. Serializing the struct directly keeps Go's key order; a
    /// `serde_json::Value` would reorder keys alphabetically. Alert writers can also pass the
    /// struct itself to the alert store, which stores it as jsonb.
    #[must_use]
    pub fn alert_subject_json(&self) -> String {
        serde_json::to_string(self).expect("RetirementBlocked serializes")
    }
}

/// One tenant's config and simulation state for the retirement guard.
pub struct TenantPools<'a> {
    /// The tenant id.
    pub tenant_id: &'a str,
    /// The tenant's pools that contain the column being retired.
    pub pools: &'a [Pool],
    /// The tenant's standing constraints.
    pub constraints: &'a StandingConstraints,
    /// The simulation state (aggregates and column statuses) for those pools.
    pub state: &'a SimulationState,
}

/// Simulate retiring `column_id`: for each tenant, force the column `retired` and re-run the G4
/// simulation over its pools, collecting the pools that drop to zero usable columns (reference
/// §5.2 `SimulateRetirement`). Empty result means the guard allows the retirement.
#[must_use]
pub fn simulate_retirement(
    column_id: &str,
    tenants: &[TenantPools<'_>],
    now: DateTime<Utc>,
) -> Vec<BlockedPool> {
    let mut blocked = Vec::new();
    for tenant in tenants {
        let mut state = tenant.state.clone();
        state
            .column_status
            .insert(column_id.to_string(), ColumnStatus::Retired);
        for error in simulate(tenant.pools, tenant.constraints, &state, now) {
            blocked.push(blocked_pool(tenant.tenant_id, &error));
        }
    }
    blocked
}

/// Map one `pool_would_be_empty` simulation error to a [`BlockedPool`] for a tenant. The
/// `status_retired` constraint (the whole pool retired) is kept and carries no cell, as
/// Go wrote it (`"constraint":"status_retired"` in the recorded 422 details).
fn blocked_pool(tenant_id: &str, error: &ConfigError) -> BlockedPool {
    let details = &error.details;
    let string_field = |key: &str| {
        details
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    let constraint = string_field("constraint");
    BlockedPool {
        tenant_id: tenant_id.to_string(),
        pool: string_field("pool").unwrap_or_default(),
        capability: string_field("capability"),
        difficulty_tier: details
            .get("difficulty_tier")
            .and_then(serde_json::Value::as_i64)
            .and_then(|tier| i32::try_from(tier).ok()),
        constraint,
    }
}

/// Whether a proposed `retention_valid_until` is within the allowed window after
/// `retention_verified_at` (spec §4.5: at most 90 days). A write beyond it is `422
/// invalid_request`. Evidence with no `valid_until` is always allowed here (its effective mode is
/// handled by [`crate::policy::RetentionMode::effective`]).
#[must_use]
pub fn retention_window_error(
    verified_at: DateTime<Utc>,
    valid_until: Option<DateTime<Utc>>,
) -> Option<ConfigError> {
    let until = valid_until?;
    if until > verified_at + Duration::days(RETENTION_MAX_VALID_DAYS) {
        Some(ConfigError {
            code: ConfigErrorCode::InvalidRequest,
            message: format!(
                "retention_valid_until must be at most {RETENTION_MAX_VALID_DAYS} days after retention_verified_at"
            ),
            details: serde_json::json!({ "field": "retention_valid_until" }),
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runnable_matches_the_lifecycle_states() {
        assert!(runnable("cold_start"));
        assert!(runnable("active"));
        assert!(runnable("deprecated"));
        assert!(!runnable("retired"));
        assert!(!runnable("bogus"));
    }

    #[test]
    fn retirement_blocked_message_and_subject_are_exact() {
        let blocked = RetirementBlocked {
            column_id: "col_x".to_string(),
            pools: vec![
                BlockedPool {
                    tenant_id: "ten_a".to_string(),
                    pool: "default".to_string(),
                    constraint: None,
                    capability: None,
                    difficulty_tier: None,
                },
                BlockedPool {
                    tenant_id: "ten_b".to_string(),
                    pool: "pool \"q\"".to_string(),
                    constraint: Some("max_cost_per_run".to_string()),
                    capability: Some("factual_qa".to_string()),
                    difficulty_tier: Some(3),
                },
            ],
        };
        assert_eq!(
            blocked.message(),
            "retiring column col_x would leave pool(s) \"default\" of tenant ten_a, \"pool \\\"q\\\"\" of tenant ten_b with zero usable columns; amend the pools first (spec 4.1)"
        );
        assert_eq!(
            blocked.alert_subject_json(),
            "{\"column_id\":\"col_x\",\"pools\":[{\"tenant_id\":\"ten_a\",\"pool\":\"default\"},{\"tenant_id\":\"ten_b\",\"pool\":\"pool \\\"q\\\"\",\"constraint\":\"max_cost_per_run\",\"capability\":\"factual_qa\",\"difficulty_tier\":3}]}"
        );
    }

    #[test]
    fn should_activate_needs_window_end_and_measured() {
        let until = NaiveDate::from_ymd_opt(2026, 7, 6).unwrap();
        let before = NaiveDate::from_ymd_opt(2026, 7, 5).unwrap();
        let on = NaiveDate::from_ymd_opt(2026, 7, 6).unwrap();
        assert!(
            !should_activate(Some(until), before, true),
            "before the window end"
        );
        assert!(!should_activate(Some(until), on, false), "not yet measured");
        assert!(!should_activate(None, on, true), "no window set");
        assert!(should_activate(Some(until), on, true));
    }

    #[test]
    fn should_retire_needs_deprecation_date_and_guard() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 6).unwrap();
        let before = NaiveDate::from_ymd_opt(2026, 7, 5).unwrap();
        assert!(!should_retire(Some(date), before, true), "before the date");
        assert!(!should_retire(Some(date), date, false), "guard blocks");
        assert!(!should_retire(None, date, true), "no deprecation date");
        assert!(should_retire(Some(date), date, true));
    }

    #[test]
    fn retention_window_rejects_evidence_beyond_ninety_days() {
        let verified = "2026-01-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let ok = verified + Duration::days(RETENTION_MAX_VALID_DAYS);
        let too_far = verified + Duration::days(RETENTION_MAX_VALID_DAYS) + Duration::seconds(1);
        assert!(retention_window_error(verified, None).is_none());
        assert!(
            retention_window_error(verified, Some(ok)).is_none(),
            "exactly 90 days is allowed"
        );
        let error = retention_window_error(verified, Some(too_far)).expect("beyond 90 days");
        assert_eq!(error.details["field"], "retention_valid_until");
    }

    #[test]
    fn simulate_retirement_reports_pools_forced_empty() {
        // A single-column pool: retiring that column empties it (status_retired).
        let pools = vec![Pool {
            name: "default".to_string(),
            model_column_ids: vec!["col_x".to_string()],
            is_default: true,
        }];
        let constraints = StandingConstraints::default();
        let state = SimulationState::default();
        let now = "2026-01-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let blocked = simulate_retirement(
            "col_x",
            &[TenantPools {
                tenant_id: "ten_a",
                pools: &pools,
                constraints: &constraints,
                state: &state,
            }],
            now,
        );
        assert_eq!(blocked.len(), 1);
        assert_eq!(blocked[0].tenant_id, "ten_a");
        assert_eq!(blocked[0].pool, "default");
        assert_eq!(
            blocked[0].constraint.as_deref(),
            Some("status_retired"),
            "a wholly retired pool names the status_retired constraint"
        );
        assert_eq!(blocked[0].capability, None);
        assert_eq!(blocked[0].difficulty_tier, None);
        assert_eq!(
            serde_json::to_string(&blocked[0]).unwrap(),
            r#"{"tenant_id":"ten_a","pool":"default","constraint":"status_retired"}"#
        );
    }
}
