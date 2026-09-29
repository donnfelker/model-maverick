//! The G4 simulation state read from the store (reference Part 4 §1.5): the status and
//! retention evidence of every referenced column, and the latest cost and latency EWMA per
//! (column, capability, tier). Shared by the config write endpoints, the nightly drift
//! check, and the retirement guard.

use std::collections::BTreeSet;

use maverick_domain::policy::{
    CellEwma, ColumnRetention, ColumnStatus, RetentionMode, SimCell, SimulationState,
};
use sqlx::PgConnection;

use crate::{catalog, measurement};

/// The aggregate dimension the cost constraint reads.
pub const COST_DIMENSION: &str = "cost_per_run_usd";
/// The aggregate dimension the latency constraint reads.
pub const LATENCY_DIMENSION: &str = "latency_p95_ms";

/// The simulation state for pools over `column_ids` (`ListModelColumnsByIDs`, then
/// `ListLatestAggregatesByColumns` for the two constraint dimensions). The cells are every
/// (capability, tier) with any of those aggregates, sorted by capability then tier. A column
/// with no row, or with a status outside the lifecycle, has no status entry and is usable.
pub async fn load_state(
    conn: &mut PgConnection,
    column_ids: &[String],
) -> sqlx::Result<SimulationState> {
    let mut state = SimulationState::default();
    for column in catalog::list_model_columns_by_ids(&mut *conn, column_ids).await? {
        if let Some(status) = column_status(&column.status) {
            state.column_status.insert(column.id.clone(), status);
        }
        state.column_retention.insert(
            column.id,
            ColumnRetention {
                mode: retention_mode(&column.retention_mode),
                valid_until: column.retention_valid_until,
            },
        );
    }

    let dimensions = [COST_DIMENSION.to_owned(), LATENCY_DIMENSION.to_owned()];
    let aggregates =
        measurement::list_latest_aggregates_by_columns(&mut *conn, column_ids, &dimensions).await?;
    let mut cells = BTreeSet::new();
    for aggregate in aggregates {
        let tier = i32::from(aggregate.difficulty_tier);
        cells.insert((aggregate.capability.clone(), tier));
        let ewma = CellEwma {
            column_id: aggregate.model_column_id,
            capability: aggregate.capability,
            difficulty_tier: tier,
            ewma: f64::from(aggregate.ewma),
        };
        if aggregate.dimension == COST_DIMENSION {
            state.cost_ewmas.push(ewma);
        } else {
            state.latency_ewmas.push(ewma);
        }
    }
    state.cells_sorted = cells
        .into_iter()
        .map(|(capability, difficulty_tier)| SimCell {
            capability,
            difficulty_tier,
        })
        .collect();
    Ok(state)
}

/// A stored `model_columns.status`.
pub fn column_status(status: &str) -> Option<ColumnStatus> {
    match status {
        "cold_start" => Some(ColumnStatus::ColdStart),
        "active" => Some(ColumnStatus::Active),
        "deprecated" => Some(ColumnStatus::Deprecated),
        "retired" => Some(ColumnStatus::Retired),
        _ => None,
    }
}

/// A stored `retention_mode`. Anything but `standard` or `zero` reads as `unknown`.
pub fn retention_mode(mode: &str) -> RetentionMode {
    match mode {
        "standard" => RetentionMode::Standard,
        "zero" => RetentionMode::Zero,
        _ => RetentionMode::Unknown,
    }
}
