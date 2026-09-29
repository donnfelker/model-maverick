//! The catalog jobs: `price_sync` (06:00) and `catalog_sync` (06:10).
//!
//! Reference Part 3 §2.13 and §5; spec §4.1 to §4.5, §7.2, §11. `price_sync` refreshes each
//! runnable column's pricing snapshot from the configured price source. `catalog_sync`
//! moves columns through the lifecycle (`cold_start -> active`, `deprecated -> retired`
//! behind the retirement guard), runs the pool-drift check, and raises `retention_expiring`
//! 14 days before any column's or alias's retention evidence expires (spec §4.5).
//! Historical `runs.cost` is never restated.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::error::Error as StdError;
use std::future::Future;
use std::pin::Pin;

use chrono::{DateTime, Duration, NaiveDate, SecondsFormat, Utc};
use maverick_alerts::{AlertError, AlertKind, AlertNotifier, Severity, insert_alert};
use maverick_domain::catalog::{self as lifecycle, RetirementBlocked, TenantPools};
use maverick_domain::gofmt::quote;
use maverick_domain::gojson;
use maverick_domain::policy::{self, Pool, SimulationState, StandingConstraints};
use maverick_domain::pricing::{Pricing, parse_pricing};
use maverick_store::catalog::{
    CatalogChange, ModelColumn, ModelColumnUpdate, list_model_columns,
    list_model_columns_by_statuses, log_change, update_model_column,
};
use maverick_store::{
    JsonRawValue, is_not_found, measurement, numeric_to_f64, simulation, tenancy,
};
use serde::Serialize;
use sqlx::{PgConnection, PgPool};

use crate::{DayArgs, JobArgs, JobKind, JobsError};

/// How long before `retention_valid_until` the `retention_expiring` warning starts.
pub const RETENTION_WARNING_DAYS: i64 = 14;

/// A boxed error from a price source.
pub type PriceSourceError = Box<dyn StdError + Send + Sync>;

/// The key a price source is looked up by. Settings are not part of it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PriceKey {
    pub provider: String,
    pub model_id: String,
    pub endpoint: String,
}

impl PriceKey {
    pub fn new(provider: &str, model_id: &str, endpoint: &str) -> Self {
        PriceKey {
            provider: provider.to_owned(),
            model_id: model_id.to_owned(),
            endpoint: endpoint.to_owned(),
        }
    }
}

/// The future a price source returns.
pub type PricesFuture<'a> =
    Pin<Box<dyn Future<Output = Result<HashMap<PriceKey, Pricing>, PriceSourceError>> + Send + 'a>>;

/// Where `price_sync` reads list prices. The models.dev source is bead 7.15; no source
/// configured is a valid state (Go behavior).
pub trait PriceSource: Send + Sync {
    fn prices(&self) -> PricesFuture<'_>;
}

/// A fixed price list (tests and operator overrides).
#[derive(Debug, Clone, Default)]
pub struct StaticPrices(pub HashMap<PriceKey, Pricing>);

impl PriceSource for StaticPrices {
    fn prices(&self) -> PricesFuture<'_> {
        let prices = self.0.clone();
        Box::pin(async move { Ok(prices) })
    }
}

/// What the catalog jobs need.
#[derive(Clone, Copy)]
pub struct CatalogDeps<'a> {
    pub pool: &'a PgPool,
    /// Delivery for the alerts. `None` still writes the alert rows.
    pub notifier: Option<&'a dyn AlertNotifier>,
    /// `None`: `price_sync` is a no-op.
    pub price_source: Option<&'a dyn PriceSource>,
}

/// The output of `price_sync`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PriceSyncOutput {
    pub columns: u64,
    pub updated: u64,
    pub missing_from_source: u64,
}

/// The output of `catalog_sync`. `retention_alerts` is new in Rust (spec §4.5).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CatalogSyncOutput {
    pub activated: u64,
    pub retired: u64,
    pub retirement_blocked: u64,
    pub drift_alerts: u64,
    pub retention_alerts: u64,
}

/// An error from a catalog job.
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    /// The day argument did not parse.
    #[error(transparent)]
    InvalidArgument(#[from] JobsError),
    /// A database step failed.
    #[error("jobs: {job}: {step}: {source}")]
    Store {
        job: JobKind,
        step: &'static str,
        source: sqlx::Error,
    },
    /// The price source failed.
    #[error("jobs: price_sync: fetching prices: {0}")]
    PriceSource(PriceSourceError),
    /// A source price could not be encoded as JSON (NaN or infinite).
    #[error("jobs: price_sync: encoding the pricing snapshot for {column_id}: {message}")]
    Snapshot { column_id: String, message: String },
    /// The retirement guard could not simulate.
    #[error("catalog: retirement guard for {column_id}: {source}")]
    Guard {
        column_id: String,
        source: sqlx::Error,
    },
    /// The retirement guard could not write its alert.
    #[error("catalog: writing retirement alert for {column_id}: {source}")]
    GuardAlert {
        column_id: String,
        source: AlertError,
    },
    /// A drift or retention alert could not be written.
    #[error("jobs: catalog_sync: {0}")]
    Alert(AlertError),
}

fn store_error(job: JobKind, step: &'static str) -> impl FnOnce(sqlx::Error) -> CatalogError {
    move |source| CatalogError::Store { job, step, source }
}

/// `price_sync` for `day` (reference §2.13). Each runnable column, in id order, whose list
/// price, output price, or currency differs from the source gets a new snapshot (`as_of`
/// defaults to `day`); every other field is kept.
pub async fn price_sync(
    deps: &CatalogDeps<'_>,
    day: NaiveDate,
) -> Result<PriceSyncOutput, CatalogError> {
    let job = JobKind::PriceSync;
    let Some(source) = deps.price_source else {
        tracing::info!(
            day = %day,
            "price_sync: no price source configured; skipping"
        );
        return Ok(PriceSyncOutput::default());
    };
    let prices = source.prices().await.map_err(CatalogError::PriceSource)?;
    let mut columns = list_model_columns(deps.pool)
        .await
        .map_err(store_error(job, "listing columns"))?;
    columns.sort_by(|a, b| a.id.cmp(&b.id));

    let mut output = PriceSyncOutput::default();
    for column in columns
        .iter()
        .filter(|column| lifecycle::runnable(&column.status))
    {
        output.columns += 1;
        let key = PriceKey::new(&column.provider, &column.model_id, &column.endpoint);
        let Some(entry) = prices.get(&key) else {
            output.missing_from_source += 1;
            continue;
        };
        match parse_pricing(column.pricing.get()) {
            Ok(current)
                if current.input_per_mtok == entry.input_per_mtok
                    && current.output_per_mtok == entry.output_per_mtok
                    && current.currency == entry.currency =>
            {
                continue;
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(
                column_id = %column.id,
                error = %error,
                "price_sync: stored pricing does not parse; replacing it"
            ),
        }
        let mut snapshot = entry.clone();
        if snapshot.as_of.is_empty() {
            snapshot.as_of = day.format("%Y-%m-%d").to_string();
        }
        let new = snapshot_json(&snapshot).map_err(|message| CatalogError::Snapshot {
            column_id: column.id.clone(),
            message,
        })?;
        let pricing =
            JsonRawValue::from_string(new.clone()).map_err(|error| CatalogError::Snapshot {
                column_id: column.id.clone(),
                message: error.to_string(),
            })?;
        let update = ModelColumnUpdate {
            pricing: &pricing,
            ..keep(column)
        };
        update_model_column(deps.pool, &column.id, &update)
            .await
            .map_err(store_error(job, "updating pricing"))?;
        log_change(
            CatalogChange::Pricing,
            &column.id,
            column.pricing.get(),
            &new,
        );
        output.updated += 1;
    }
    Ok(output)
}

/// The snapshot in Go `json.Marshal` form, keys in order: `input_per_mtok`,
/// `output_per_mtok`, `currency`, `as_of`, `source`.
fn snapshot_json(pricing: &Pricing) -> Result<String, String> {
    let mut out = String::from("{\"input_per_mtok\":");
    out.push_str(&gojson::encode_f64(pricing.input_per_mtok)?);
    out.push_str(",\"output_per_mtok\":");
    out.push_str(&gojson::encode_f64(pricing.output_per_mtok)?);
    out.push_str(",\"currency\":");
    gojson::write_string(&mut out, &pricing.currency);
    out.push_str(",\"as_of\":");
    gojson::write_string(&mut out, &pricing.as_of);
    out.push_str(",\"source\":");
    gojson::write_string(&mut out, &pricing.source);
    out.push('}');
    Ok(out)
}

/// An update that writes every field of `column` back unchanged (the jsonb maps are kept
/// by `coalesce`).
fn keep(column: &ModelColumn) -> ModelColumnUpdate<'_> {
    ModelColumnUpdate {
        settings: &column.settings,
        status: &column.status,
        pricing: &column.pricing,
        quality_source_column_id: column.quality_source_column_id.as_deref(),
        cold_start_until: column.cold_start_until,
        deprecation_date: column.deprecation_date,
        execution_aliases: None,
        retention_mode: &column.retention_mode,
        retention_verified_at: column.retention_verified_at,
        retention_valid_until: column.retention_valid_until,
        retention_source: column.retention_source.as_deref(),
        execution_alias_retention: None,
        param_constraints: None,
    }
}

async fn set_status(pool: &PgPool, column: &ModelColumn, status: &str) -> Result<(), CatalogError> {
    let update = ModelColumnUpdate {
        status,
        ..keep(column)
    };
    update_model_column(pool, &column.id, &update)
        .await
        .map_err(store_error(JobKind::CatalogSync, "updating status"))?;
    log_change(CatalogChange::Status, &column.id, &column.status, status);
    Ok(())
}

/// One tenant's pools, constraints, and simulation state.
struct TenantSurvey {
    tenant_id: String,
    pools: Vec<Pool>,
    constraints: StandingConstraints,
    state: SimulationState,
}

/// Groups stored pools by tenant in first-seen order and loads each tenant's standing
/// constraints (no row: unconstrained) and the simulation state for its pools' columns.
async fn survey(
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
        tenant.constraints =
            match tenancy::get_tenant_constraints(&mut *conn, &tenant.tenant_id).await {
                Ok(row) => StandingConstraints {
                    max_cost_per_run: row.max_cost_per_run.map(numeric_to_f64),
                    max_latency_ms: row.max_latency_ms,
                    zero_retention: row.zero_retention,
                },
                Err(error) if is_not_found(&error) => StandingConstraints::default(),
                Err(error) => return Err(error),
            };
        let mut ids: Vec<String> = tenant
            .pools
            .iter()
            .flat_map(|pool| pool.model_column_ids.iter().cloned())
            .collect();
        ids.sort();
        ids.dedup();
        tenant.state = simulation::load_state(&mut *conn, &ids).await?;
    }
    Ok(tenants)
}

/// The retirement guard (reference §5.2): re-simulates every stored pool that contains
/// `column_id`, per tenant, with the column forced `retired`. `Ok(None)` allows the
/// retirement. `Ok(Some(_))` blocks it, after writing one critical `pool_would_be_empty`
/// alert with subject `{column_id, pools}` and the blocked message.
pub async fn guard_retirement(
    deps: &CatalogDeps<'_>,
    column_id: &str,
    now: DateTime<Utc>,
) -> Result<Option<RetirementBlocked>, CatalogError> {
    let guard_error = |source| CatalogError::Guard {
        column_id: column_id.to_owned(),
        source,
    };
    let mut conn = deps.pool.acquire().await.map_err(guard_error)?;
    let pools = tenancy::list_pools_by_column(&mut *conn, column_id)
        .await
        .map_err(guard_error)?;
    let tenants = survey(&mut conn, pools).await.map_err(guard_error)?;
    let views: Vec<TenantPools<'_>> = tenants
        .iter()
        .map(|tenant| TenantPools {
            tenant_id: &tenant.tenant_id,
            pools: &tenant.pools,
            constraints: &tenant.constraints,
            state: &tenant.state,
        })
        .collect();
    let blocked = lifecycle::simulate_retirement(column_id, &views, now);
    if blocked.is_empty() {
        return Ok(None);
    }
    let blocked = RetirementBlocked {
        column_id: column_id.to_owned(),
        pools: blocked,
    };
    insert_alert(
        deps.pool,
        deps.notifier,
        AlertKind::PoolWouldBeEmpty,
        Severity::Critical,
        &blocked,
        &blocked.message(),
    )
    .await
    .map_err(|source| CatalogError::GuardAlert {
        column_id: column_id.to_owned(),
        source,
    })?;
    Ok(Some(blocked))
}

/// `catalog_sync` for `day` (reference §2.13): the lifecycle transitions, the drift check,
/// and `retention_expiring`. `now` is the clock for retention evidence.
pub async fn catalog_sync(
    deps: &CatalogDeps<'_>,
    day: NaiveDate,
    now: DateTime<Utc>,
) -> Result<CatalogSyncOutput, CatalogError> {
    let job = JobKind::CatalogSync;
    let mut output = CatalogSyncOutput::default();

    let measured: HashSet<String> = measurement::list_measured_column_ids(deps.pool)
        .await
        .map_err(store_error(job, "listing measured columns"))?
        .into_iter()
        .collect();
    let statuses = ["cold_start".to_owned(), "deprecated".to_owned()];
    let columns = list_model_columns_by_statuses(deps.pool, &statuses)
        .await
        .map_err(store_error(job, "listing lifecycle columns"))?;
    for column in &columns {
        if column.status == "cold_start" {
            if lifecycle::should_activate(
                column.cold_start_until,
                day,
                measured.contains(&column.id),
            ) {
                set_status(deps.pool, column, "active").await?;
                output.activated += 1;
            }
        } else if matches!(column.deprecation_date, Some(date) if day >= date) {
            if let Some(blocked) = guard_retirement(deps, &column.id, now).await? {
                tracing::warn!(
                    column_id = %column.id,
                    error = %blocked.message(),
                    "catalog_sync: retirement blocked"
                );
                output.retirement_blocked += 1;
            } else {
                set_status(deps.pool, column, "retired").await?;
                output.retired += 1;
            }
        }
    }

    output.drift_alerts = drift_check(deps, day, now).await?;
    output.retention_alerts = retention_expiring(deps, day, now).await?;
    Ok(output)
}

/// The drift check (spec §7.2, reference §2.13 step 5): every stored pool, per tenant with
/// its standing constraints (D1 zero retention included), whose floor leaves 1 or fewer
/// columns gets a `pool_would_be_empty` alert: critical at 0, warning at 1. No dedup: it
/// re-alerts every day (QUIRK J8).
async fn drift_check(
    deps: &CatalogDeps<'_>,
    day: NaiveDate,
    now: DateTime<Utc>,
) -> Result<u64, CatalogError> {
    let job = JobKind::CatalogSync;
    let mut conn = deps
        .pool
        .acquire()
        .await
        .map_err(store_error(job, "drift check"))?;
    let pools = tenancy::list_pools(&mut *conn)
        .await
        .map_err(store_error(job, "drift check"))?;
    let tenants = survey(&mut conn, pools)
        .await
        .map_err(store_error(job, "drift check"))?;
    drop(conn);

    let mut alerts = 0;
    for tenant in &tenants {
        for pool in &tenant.pools {
            let floor = policy::pool_floor(
                &pool.model_column_ids,
                &tenant.constraints,
                &tenant.state,
                now,
            );
            if floor.survivors > 1 {
                continue;
            }
            let (severity, detail) = if floor.survivors == 0 {
                (Severity::Critical, "has no usable columns")
            } else {
                (Severity::Warning, "is within one column of empty")
            };
            let (capability, tier) = floor.cell.map_or((String::new(), 0), |cell| {
                (cell.capability, cell.difficulty_tier)
            });
            let subject = DriftSubject {
                capability,
                constraint: floor.constraint,
                day: day.format("%Y-%m-%d").to_string(),
                difficulty_tier: tier,
                pool: &pool.name,
                survivors: floor.survivors,
                tenant_id: &tenant.tenant_id,
            };
            let message = format!(
                "pool {} of tenant {} {detail} under current aggregates ({} surviving; spec 7.2 drift check)",
                quote(&pool.name),
                tenant.tenant_id,
                floor.survivors
            );
            insert_alert(
                deps.pool,
                deps.notifier,
                AlertKind::PoolWouldBeEmpty,
                severity,
                &subject,
                &message,
            )
            .await
            .map_err(CatalogError::Alert)?;
            alerts += 1;
        }
    }
    Ok(alerts)
}

/// The drift alert subject. The keys are in Go's (sorted map) order.
#[derive(Serialize)]
struct DriftSubject<'a> {
    capability: String,
    constraint: &'static str,
    day: String,
    difficulty_tier: i32,
    pool: &'a str,
    survivors: usize,
    tenant_id: &'a str,
}

/// The `retention_expiring` alert subject (spec §4.5, Rust-only). `alias` is absent for the
/// column's own evidence.
#[derive(Serialize)]
struct RetentionSubject<'a> {
    column_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    alias: Option<&'a str>,
    retention_mode: &'a str,
    retention_valid_until: String,
    day: String,
}

/// A `retention_expiring` warning for every runnable column, and every alias of one, whose
/// evidence expires after `now` and within [`RETENTION_WARNING_DAYS`] (spec §4.5). It
/// repeats every day of the window, like the drift check. Expired evidence already reads as
/// `unknown`; the drift check reports what that empties.
async fn retention_expiring(
    deps: &CatalogDeps<'_>,
    day: NaiveDate,
    now: DateTime<Utc>,
) -> Result<u64, CatalogError> {
    let mut columns = list_model_columns(deps.pool)
        .await
        .map_err(store_error(JobKind::CatalogSync, "listing columns"))?;
    columns.sort_by(|a, b| a.id.cmp(&b.id));
    let horizon = now + Duration::days(RETENTION_WARNING_DAYS);
    let expiring = |until: DateTime<Utc>| until > now && until <= horizon;
    let day = day.format("%Y-%m-%d").to_string();

    let mut alerts = 0;
    for column in columns
        .iter()
        .filter(|column| lifecycle::runnable(&column.status))
    {
        let mut evidence: Vec<(Option<&str>, String, DateTime<Utc>)> = Vec::new();
        if let Some(until) = column.retention_valid_until.filter(|u| expiring(*u)) {
            evidence.push((None, column.retention_mode.clone(), until));
        }
        let aliases: BTreeMap<String, serde_json::Value> =
            serde_json::from_str(column.execution_alias_retention.get()).unwrap_or_default();
        for (alias, entry) in &aliases {
            let until = entry
                .get("retention_valid_until")
                .and_then(serde_json::Value::as_str)
                .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
                .map(|until| until.with_timezone(&Utc));
            if let Some(until) = until.filter(|u| expiring(*u)) {
                let mode = entry
                    .get("retention_mode")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown")
                    .to_owned();
                evidence.push((Some(alias.as_str()), mode, until));
            }
        }
        for (alias, mode, until) in evidence {
            let until_text = until.to_rfc3339_opts(SecondsFormat::AutoSi, true);
            let message = match alias {
                None => format!(
                    "retention evidence for column {} expires at {until_text}; revalidate it through the internal column API (spec 4.5)",
                    column.id
                ),
                Some(alias) => format!(
                    "retention evidence for alias {} of column {} expires at {until_text}; revalidate it through the internal column API (spec 4.5)",
                    quote(alias),
                    column.id
                ),
            };
            let subject = RetentionSubject {
                column_id: &column.id,
                alias,
                retention_mode: &mode,
                retention_valid_until: until_text,
                day: day.clone(),
            };
            insert_alert(
                deps.pool,
                deps.notifier,
                AlertKind::RetentionExpiring,
                Severity::Warning,
                &subject,
                &message,
            )
            .await
            .map_err(CatalogError::Alert)?;
            alerts += 1;
        }
    }
    Ok(alerts)
}

fn job_day(args: JobArgs) -> Result<NaiveDate, CatalogError> {
    let day = args.window()?;
    // `window` already validated the day.
    NaiveDate::parse_from_str(&day, "%Y-%m-%d").map_err(|source| {
        CatalogError::InvalidArgument(JobsError::InvalidDay {
            kind: args.kind(),
            value: day,
            source: source.to_string(),
        })
    })
}

/// The `price_sync` job: validates the day argument, then runs [`price_sync`].
pub async fn run_price_sync(
    deps: &CatalogDeps<'_>,
    args: &DayArgs,
) -> Result<PriceSyncOutput, CatalogError> {
    let day = job_day(JobArgs::PriceSync(args.clone()))?;
    price_sync(deps, day).await
}

/// The `catalog_sync` job: validates the day argument, then runs [`catalog_sync`] at `now`.
pub async fn run_catalog_sync(
    deps: &CatalogDeps<'_>,
    args: &DayArgs,
    now: DateTime<Utc>,
) -> Result<CatalogSyncOutput, CatalogError> {
    let day = job_day(JobArgs::CatalogSync(args.clone()))?;
    catalog_sync(deps, day, now).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_json_uses_go_numbers_and_key_order() {
        let pricing = Pricing {
            input_per_mtok: 4.0,
            output_per_mtok: 0.25,
            currency: "USD".to_owned(),
            as_of: "2031-07-01".to_owned(),
            source: "list <a&b>".to_owned(),
        };
        assert_eq!(
            snapshot_json(&pricing).unwrap(),
            "{\"input_per_mtok\":4,\"output_per_mtok\":0.25,\"currency\":\"USD\",\"as_of\":\"2031-07-01\",\"source\":\"list \\u003ca\\u0026b\\u003e\"}"
        );
        let nan = Pricing {
            input_per_mtok: f64::NAN,
            ..pricing
        };
        assert!(snapshot_json(&nan).is_err());
    }
}
