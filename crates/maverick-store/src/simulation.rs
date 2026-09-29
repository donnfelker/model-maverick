//! The G4 simulation state read from the store (reference Part 4 §1.5): the status and
//! retention evidence of every referenced column, and the latest cost and latency EWMA per
//! (column, capability, tier). Shared by the config write endpoints, the nightly drift
//! check, and the retirement guard.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use maverick_domain::catalog::{BlockedPool, TenantPools, simulate_retirement};
use maverick_domain::policy::{
    self, CellEwma, ColumnRetention, ColumnStatus, RetentionMode, SimCell, SimulationState,
    StandingConstraints,
};
use sqlx::PgConnection;

use crate::{catalog, is_not_found, measurement, numeric_to_f64, tenancy};

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

/// One tenant's stored pools, standing constraints, and the simulation state for the
/// columns those pools hold.
#[derive(Debug, Clone)]
pub struct TenantSurvey {
    pub tenant_id: String,
    pub pools: Vec<policy::Pool>,
    pub constraints: StandingConstraints,
    pub state: SimulationState,
}

/// Groups stored pools by tenant in first-seen order and loads each tenant's standing
/// constraints (no row: unconstrained) and the simulation state for its pools' columns
/// (reference Part 4 §1.6).
pub async fn survey(
    conn: &mut PgConnection,
    pools: Vec<tenancy::Pool>,
) -> sqlx::Result<Vec<TenantSurvey>> {
    let mut tenants: Vec<TenantSurvey> = Vec::new();
    for pool in pools {
        let entry = policy::Pool {
            name: pool.name,
            model_column_ids: pool.model_column_ids,
            is_default: pool.is_default,
        };
        match tenants.iter_mut().find(|t| t.tenant_id == pool.tenant_id) {
            Some(tenant) => tenant.pools.push(entry),
            None => tenants.push(TenantSurvey {
                tenant_id: pool.tenant_id,
                pools: vec![entry],
                constraints: StandingConstraints::default(),
                state: SimulationState::default(),
            }),
        }
    }
    for tenant in &mut tenants {
        tenant.constraints = standing_constraints(&mut *conn, &tenant.tenant_id).await?;
        let mut ids: Vec<String> = tenant
            .pools
            .iter()
            .flat_map(|pool| pool.model_column_ids.iter().cloned())
            .collect();
        ids.sort();
        ids.dedup();
        tenant.state = load_state(&mut *conn, &ids).await?;
    }
    Ok(tenants)
}

/// A tenant's standing constraints; no row is unconstrained.
pub async fn standing_constraints(
    conn: &mut PgConnection,
    tenant_id: &str,
) -> sqlx::Result<StandingConstraints> {
    match tenancy::get_tenant_constraints(&mut *conn, tenant_id).await {
        Ok(row) => Ok(StandingConstraints {
            max_cost_per_run: row.max_cost_per_run.map(numeric_to_f64),
            max_latency_ms: row.max_latency_ms,
            zero_retention: row.zero_retention,
        }),
        Err(error) if is_not_found(&error) => Ok(StandingConstraints::default()),
        Err(error) => Err(error),
    }
}

/// The retirement guard's simulation (reference Part 3 §5.2, Part 4 §1.6
/// `SimulateRetirement`): every stored pool that holds `column_id`, per tenant, with the
/// column forced `retired`. Empty means the retirement is allowed. The caller writes the
/// alert.
pub async fn retirement_blockers(
    conn: &mut PgConnection,
    column_id: &str,
    now: DateTime<Utc>,
) -> sqlx::Result<Vec<BlockedPool>> {
    let pools = tenancy::list_pools_by_column(&mut *conn, column_id).await?;
    let tenants = survey(conn, pools).await?;
    let views: Vec<TenantPools<'_>> = tenants
        .iter()
        .map(|tenant| TenantPools {
            tenant_id: &tenant.tenant_id,
            pools: &tenant.pools,
            constraints: &tenant.constraints,
            state: &tenant.state,
        })
        .collect();
    Ok(simulate_retirement(column_id, &views, now))
}
