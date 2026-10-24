//! Config-write validation (reference Part 4 §1.4). Pure functions the store and API layer
//! (bead 5.3) run inside the config-write transaction before a full replace. No SQL here.
//!
//! `validate_weights`, `validate_constraint_values`, `validate_pool_shapes`, and
//! `validate_pool_names` each return the violations they find, in order; the caller
//! concatenates them (weights, then constraints, then pools) and, when the shape is valid,
//! runs [`simulate`](super::simulate).

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};

use super::{Pool, StandingConstraints};
use crate::gofmt::quote;
use crate::tasks::CAPABILITIES;

/// The largest deviation from a weight sum of 1 that still validates.
const WEIGHT_SUM_TOLERANCE: f64 = 0.001;

/// Whether a weight sum's deviation from 1 is too large. A deviation of exactly the tolerance is
/// allowed (Go uses a strict `>`); this is split out so the boundary is unit-testable.
fn deviation_exceeds_tolerance(deviation: f64) -> bool {
    deviation > WEIGHT_SUM_TOLERANCE
}

/// A config-write validation or G4 simulation error: `{code, message, details}`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ConfigError {
    /// The error code (`invalid_request`, `unknown_model`, or `pool_would_be_empty`).
    pub code: ConfigErrorCode,
    /// The engine-authored message.
    pub message: String,
    /// The structured details for the wire body.
    pub details: JsonValue,
}

/// The config-write error codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigErrorCode {
    /// A malformed weights, constraints, or pools proposal.
    InvalidRequest,
    /// A pool names a column that is not in the catalog.
    UnknownModel,
    /// The proposed config would leave a pool with no usable column (G4).
    PoolWouldBeEmpty,
}

impl ConfigError {
    pub(super) fn invalid(message: String, details: JsonValue) -> Self {
        Self {
            code: ConfigErrorCode::InvalidRequest,
            message,
            details,
        }
    }
}

/// One proposed per-capability weight row. The weights and bar are `real` (f32) columns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WeightRow {
    /// The capability the weights apply to.
    pub capability: String,
    /// The quality weight.
    pub w_quality: f32,
    /// The cost weight.
    pub w_cost: f32,
    /// The latency weight.
    pub w_latency: f32,
    /// The per-capability quality bar.
    pub quality_bar: f32,
}

/// Validate proposed tenant weights (reference §1.4). Checks each row in order.
#[must_use]
pub fn validate_weights(rows: &[WeightRow]) -> Vec<ConfigError> {
    let mut errors = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();

    for row in rows {
        let capability = row.capability.as_str();
        if !CAPABILITIES.contains(&capability) {
            errors.push(ConfigError::invalid(
                format!("capability {capability:?} is not a v1 capability"),
                json!({ "capability": capability }),
            ));
            continue;
        }
        if !seen.insert(capability) {
            errors.push(ConfigError::invalid(
                format!("capability {capability:?} appears more than once"),
                json!({ "capability": capability }),
            ));
            continue;
        }

        for (field, value) in [
            ("w_quality", row.w_quality),
            ("w_cost", row.w_cost),
            ("w_latency", row.w_latency),
        ] {
            if !(0.0..=1.0).contains(&value) {
                errors.push(ConfigError::invalid(
                    format!("{field} for {capability:?} must be between 0 and 1"),
                    json!({ "capability": capability, "field": field }),
                ));
            }
        }

        let sum = f64::from(row.w_quality) + f64::from(row.w_cost) + f64::from(row.w_latency);
        if deviation_exceeds_tolerance((sum - 1.0).abs()) {
            errors.push(ConfigError::invalid(
                format!("weights for {capability:?} must sum to 1 (got {sum:.4})"),
                json!({ "capability": capability, "sum": sum }),
            ));
        }

        if row.quality_bar <= 0.0 || row.quality_bar > 1.0 {
            errors.push(ConfigError::invalid(
                format!("quality_bar for {capability:?} must be greater than 0 and at most 1"),
                json!({ "capability": capability, "field": "quality_bar" }),
            ));
        }
    }

    errors
}

/// Validate proposed standing constraint values (reference §1.4). A set limit must be positive.
#[must_use]
pub fn validate_constraint_values(constraints: &StandingConstraints) -> Vec<ConfigError> {
    let mut errors = Vec::new();
    if let Some(cost) = constraints.max_cost_per_run
        && cost <= 0.0
    {
        errors.push(ConfigError::invalid(
            "max_cost_per_run must be greater than 0".to_string(),
            json!({ "field": "max_cost_per_run" }),
        ));
    }
    if let Some(latency) = constraints.max_latency_ms
        && latency <= 0
    {
        errors.push(ConfigError::invalid(
            "max_latency_ms must be greater than 0".to_string(),
            json!({ "field": "max_latency_ms" }),
        ));
    }
    errors
}

/// Validate proposed pool shapes (reference §1.4). Checks each pool in order, then that exactly
/// one pool is the default.
#[must_use]
pub fn validate_pool_shapes(pools: &[Pool]) -> Vec<ConfigError> {
    let mut errors = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    let mut defaults = 0;

    for pool in pools {
        // Counted before the name checks, so pools skipped below still count.
        if pool.is_default {
            defaults += 1;
        }
        let name = pool.name.as_str();
        if name.is_empty() {
            errors.push(ConfigError::invalid(
                "pool name must not be empty".to_string(),
                json!({ "pool": "" }),
            ));
            continue;
        }
        if !seen.insert(name) {
            errors.push(ConfigError::invalid(
                format!("pool name {name:?} appears more than once"),
                json!({ "pool": name }),
            ));
            continue;
        }
        if pool.model_column_ids.is_empty() {
            errors.push(ConfigError::invalid(
                format!("pool {name:?} has no model_column_ids"),
                json!({ "pool": name }),
            ));
        }
        let mut columns: HashSet<&str> = HashSet::new();
        for id in &pool.model_column_ids {
            if !columns.insert(id.as_str()) {
                errors.push(ConfigError::invalid(
                    format!("pool {name:?} lists column {id:?} more than once"),
                    json!({ "pool": name, "entry": id }),
                ));
            }
        }
    }

    if defaults != 1 {
        errors.push(ConfigError::invalid(
            format!("exactly one pool must be is_default: true (got {defaults})"),
            json!({ "default_count": defaults }),
        ));
    }

    errors
}

/// Pool names that the model grammar cannot route to (D45, spec §9.7): `auto` is the
/// default-pool route `maverick/auto`, and a `:` would start a variant. Checks each pool in
/// order. The caller runs it after [`validate_pool_shapes`] and before the catalog and G4
/// checks, so a request that Go also rejects keeps Go's error.
#[must_use]
pub fn validate_pool_names(pools: &[Pool]) -> Vec<ConfigError> {
    pools
        .iter()
        .filter_map(|pool| {
            let name = pool.name.as_str();
            let message = if name == "auto" {
                format!("pool name {} is reserved", quote(name))
            } else if name.contains(':') {
                format!("pool name {} must not contain ':'", quote(name))
            } else {
                return None;
            };
            Some(ConfigError::invalid(message, json!({ "pool": name })))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(name: &str) -> Pool {
        Pool {
            name: name.to_owned(),
            model_column_ids: vec!["col_a".to_owned()],
            is_default: false,
        }
    }

    /// DIVERGENCE D45: `auto` and names with `:` are refused, in pool order; Go accepts both.
    #[test]
    fn pool_names_the_grammar_cannot_route_are_refused() {
        let pools = [
            named("team/a"),
            named("Auto"),
            named("a:b"),
            named("auto"),
            named(":"),
        ];
        let errors = validate_pool_names(&pools);
        let got: Vec<(&str, &JsonValue)> = errors
            .iter()
            .map(|error| (error.message.as_str(), &error.details))
            .collect();
        assert_eq!(
            got,
            [
                (
                    "pool name \"a:b\" must not contain ':'",
                    &json!({"pool": "a:b"})
                ),
                ("pool name \"auto\" is reserved", &json!({"pool": "auto"})),
                (
                    "pool name \":\" must not contain ':'",
                    &json!({"pool": ":"})
                ),
            ]
        );
        assert!(
            errors
                .iter()
                .all(|error| error.code == ConfigErrorCode::InvalidRequest)
        );
    }

    /// D45: names with `/`, other case, and `auto` inside a longer name stay valid.
    #[test]
    fn pool_names_the_grammar_can_route_are_accepted() {
        let pools = [
            named("default"),
            named("team/a"),
            named("AUTO"),
            named("auto-pool"),
            named("my auto"),
        ];
        assert_eq!(validate_pool_names(&pools), []);
    }

    /// D45: the name is quoted as Go `%q` quotes it.
    #[test]
    fn pool_name_messages_quote_like_go() {
        let errors = validate_pool_names(&[named("a:\"b\"\n")]);
        assert_eq!(
            errors[0].message,
            "pool name \"a:\\\"b\\\"\\n\" must not contain ':'"
        );
    }

    #[test]
    fn weight_sum_tolerance_boundary_is_inclusive() {
        // A deviation of exactly the tolerance validates; a strict `>` keeps the boundary in.
        assert!(!deviation_exceeds_tolerance(WEIGHT_SUM_TOLERANCE));
        assert!(!deviation_exceeds_tolerance(0.0));
        assert!(deviation_exceeds_tolerance(WEIGHT_SUM_TOLERANCE * 2.0));
    }
}
