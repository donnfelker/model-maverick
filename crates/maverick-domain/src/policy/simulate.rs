//! G4 write-time simulation (reference Part 4 §1.5). `simulate` checks that every pool, under
//! the constraints as they would exist after a write and against current aggregates, keeps at
//! least one usable column. No SQL here; the caller (bead 5.3) loads the state.
//!
//! D1 (Rust-only, privacy): when the tenant's `zero_retention` constraint is set, a usable
//! column must also hold valid (unexpired) zero-retention evidence. This fails closed: a column
//! with unknown, standard, or expired evidence is dropped, and a pool left empty is a
//! `pool_would_be_empty` violation naming the `zero_retention` constraint. Go ignored the flag
//! here, so the parity fixtures never set it; the D1 cases are new Rust tests.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::validate::{ConfigError, ConfigErrorCode};
use super::{ColumnStatus, Pool, RetentionMode, StandingConstraints};

/// A (capability, tier) cell to simulate, pre-sorted by capability then tier.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimCell {
    /// The capability.
    pub capability: String,
    /// The difficulty tier.
    pub difficulty_tier: i32,
}

/// One column's cost or latency EWMA in a (capability, tier) cell.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CellEwma {
    /// The column id.
    pub column_id: String,
    /// The capability.
    pub capability: String,
    /// The difficulty tier.
    pub difficulty_tier: i32,
    /// The EWMA value.
    pub ewma: f64,
}

/// A column's retention evidence, for the D1 zero-retention simulation.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ColumnRetention {
    /// The recorded retention mode.
    #[serde(default)]
    pub mode: RetentionMode,
    /// The evidence expiry, if any.
    #[serde(default)]
    pub valid_until: Option<DateTime<Utc>>,
}

/// The simulation state: column statuses, the sorted cells, and the per-cell cost and latency
/// EWMAs, plus optional retention evidence for D1.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SimulationState {
    /// Every (capability, tier) with any aggregate, sorted by capability then tier.
    #[serde(default)]
    pub cells_sorted: Vec<SimCell>,
    /// `column_id -> status`; an id with no entry is usable.
    #[serde(default)]
    pub column_status: BTreeMap<String, ColumnStatus>,
    /// The cost EWMAs.
    #[serde(default)]
    pub cost_ewmas: Vec<CellEwma>,
    /// The latency EWMAs.
    #[serde(default)]
    pub latency_ewmas: Vec<CellEwma>,
    /// Per-column retention evidence (D1); empty for the Go parity fixtures.
    #[serde(default)]
    pub column_retention: BTreeMap<String, ColumnRetention>,
}

/// The outcome of simulating one pool.
enum FloorResult {
    /// The pool keeps at least one column in every cell.
    Ok,
    /// Every column is retired.
    Retired,
    /// D1: no column holds valid, unexpired zero-retention evidence.
    ZeroRetention,
    /// The first cell (in capability, tier order) a constraint empties, and which constraint.
    Emptied {
        constraint: &'static str,
        capability: String,
        tier: i32,
    },
}

/// The constraint that empties a cell for these columns, or `None` when at least one column
/// survives. Cost is applied, then latency to the cost-survivors; a column is kept iff it has no
/// EWMA in the cell or its EWMA is within the limit (equal passes). When both filters run and
/// latency removes the last survivor, latency is the reported constraint (reference §1.5 QUIRK).
fn cell_emptied_by(
    usable: &[&String],
    cell: &SimCell,
    constraints: &StandingConstraints,
    state: &SimulationState,
) -> Option<&'static str> {
    let mut survivors: Vec<&String> = usable.to_vec();
    if let Some(limit) = constraints.max_cost_per_run {
        survivors.retain(|id| ewma_for(&state.cost_ewmas, id, cell).is_none_or(|e| e <= limit));
        if survivors.is_empty() {
            return Some("max_cost_per_run");
        }
    }
    if let Some(limit) = constraints.max_latency_ms {
        let limit = f64::from(limit);
        survivors.retain(|id| ewma_for(&state.latency_ewmas, id, cell).is_none_or(|e| e <= limit));
        if survivors.is_empty() {
            return Some("max_latency_ms");
        }
    }
    None
}

fn ewma_for(ewmas: &[CellEwma], column_id: &str, cell: &SimCell) -> Option<f64> {
    ewmas
        .iter()
        .find(|ewma| {
            ewma.column_id == column_id
                && ewma.capability == cell.capability
                && ewma.difficulty_tier == cell.difficulty_tier
        })
        .map(|ewma| ewma.ewma)
}

/// Whether a pool keeps at least one usable column in every cell, or the first cell (and
/// constraint) that empties it (reference §1.5 `poolFloor`). Retired columns are never usable;
/// under D1 zero retention, only columns with valid, unexpired zero-retention evidence are.
fn pool_floor(
    ids: &[String],
    constraints: &StandingConstraints,
    state: &SimulationState,
    now: DateTime<Utc>,
) -> FloorResult {
    let mut usable: Vec<&String> = ids
        .iter()
        .filter(|id| {
            state
                .column_status
                .get(id.as_str())
                .is_none_or(|status| *status != ColumnStatus::Retired)
        })
        .collect();
    if usable.is_empty() {
        return FloorResult::Retired;
    }

    if constraints.zero_retention {
        usable.retain(|id| {
            state
                .column_retention
                .get(id.as_str())
                .map_or(RetentionMode::Unknown, |retention| {
                    retention.mode.effective(retention.valid_until, now)
                })
                == RetentionMode::Zero
        });
        if usable.is_empty() {
            return FloorResult::ZeroRetention;
        }
    }

    for cell in &state.cells_sorted {
        if let Some(constraint) = cell_emptied_by(&usable, cell, constraints, state) {
            return FloorResult::Emptied {
                constraint,
                capability: cell.capability.clone(),
                tier: cell.difficulty_tier,
            };
        }
    }
    FloorResult::Ok
}

/// Simulate a proposed config: return a `pool_would_be_empty` violation for every pool the
/// constraints would leave with no usable column (reference §1.5). Empty output means the write
/// is safe under G4.
#[must_use]
pub fn simulate(
    pools: &[Pool],
    constraints: &StandingConstraints,
    state: &SimulationState,
    now: DateTime<Utc>,
) -> Vec<ConfigError> {
    let mut errors = Vec::new();
    for pool in pools {
        let name = pool.name.as_str();
        let error = match pool_floor(&pool.model_column_ids, constraints, state, now) {
            FloorResult::Ok => continue,
            FloorResult::Retired => ConfigError {
                code: ConfigErrorCode::PoolWouldBeEmpty,
                message: format!(
                    "pool {name:?} would have no usable columns: every column is retired"
                ),
                details: json!({ "pool": name, "constraint": "status_retired" }),
            },
            FloorResult::ZeroRetention => ConfigError {
                code: ConfigErrorCode::PoolWouldBeEmpty,
                message: format!(
                    "pool {name:?} would have no columns with valid zero-retention evidence"
                ),
                details: json!({ "pool": name, "constraint": "zero_retention" }),
            },
            FloorResult::Emptied {
                constraint,
                capability,
                tier,
            } => ConfigError {
                code: ConfigErrorCode::PoolWouldBeEmpty,
                message: format!(
                    "constraint {constraint:?} would leave pool {name:?} with zero candidates for {capability} tier {tier}"
                ),
                details: json!({
                    "pool": name,
                    "constraint": constraint,
                    "capability": capability,
                    "difficulty_tier": tier,
                }),
            },
        };
        errors.push(error);
    }
    errors
}
