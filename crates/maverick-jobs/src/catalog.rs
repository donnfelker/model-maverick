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
use maverick_config::ModelsDevPriceMapEntry;
use maverick_domain::catalog::{self as lifecycle, RetirementBlocked};
use maverick_domain::gofmt::quote;
use maverick_domain::gojson;
use maverick_domain::policy;
use maverick_domain::pricing::{Pricing, parse_pricing};
use maverick_store::catalog::{
    CatalogChange, ModelColumn, ModelColumnUpdate, list_model_columns,
    list_model_columns_by_statuses, log_change, update_model_column,
};
use maverick_store::{JsonRawValue, measurement, simulation, tenancy};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::{DayArgs, JobArgs, JobKind, JobsError};

/// How long before `retention_valid_until` the `retention_expiring` warning starts.
pub const RETENTION_WARNING_DAYS: i64 = 14;
const MODELS_DEV_SOURCE: &str = "models.dev";
const MODELS_DEV_CURRENCY: &str = "USD";
const ABSURD_DAILY_PRICE_CHANGE_RATIO: f64 = 10.0;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelsDevPriceMapping {
    pub column: PriceKey,
    pub models_dev_provider: String,
    pub models_dev_model_id: String,
}

impl ModelsDevPriceMapping {
    pub fn new(
        column_provider: &str,
        column_model_id: &str,
        column_endpoint: &str,
        models_dev_provider: &str,
        models_dev_model_id: &str,
    ) -> Self {
        ModelsDevPriceMapping {
            column: PriceKey::new(column_provider, column_model_id, column_endpoint),
            models_dev_provider: models_dev_provider.to_owned(),
            models_dev_model_id: models_dev_model_id.to_owned(),
        }
    }

    fn from_config(entry: &ModelsDevPriceMapEntry) -> Self {
        Self::new(
            &entry.column_provider,
            &entry.column_model_id,
            &entry.column_endpoint,
            &entry.models_dev_provider,
            &entry.models_dev_model_id,
        )
    }
}

/// A models.dev-backed price source. The source never guesses IDs: every catalog
/// `(provider, model_id, endpoint)` must be mapped explicitly by configuration.
#[derive(Debug, Clone)]
pub struct ModelsDevPriceSource {
    client: reqwest::Client,
    url: String,
    mappings: Vec<ModelsDevPriceMapping>,
}

impl ModelsDevPriceSource {
    pub fn new(
        url: impl Into<String>,
        mappings: impl IntoIterator<Item = ModelsDevPriceMapping>,
    ) -> Self {
        ModelsDevPriceSource {
            client: reqwest::Client::new(),
            url: url.into(),
            mappings: mappings.into_iter().collect(),
        }
    }

    pub fn from_config(config: &maverick_config::Config) -> Option<Self> {
        if config.models_dev_price_map.is_empty() {
            return None;
        }
        Some(Self::new(
            config.models_dev_price_url.clone(),
            config
                .models_dev_price_map
                .iter()
                .map(ModelsDevPriceMapping::from_config),
        ))
    }

    pub fn parse_payload(
        &self,
        payload: &str,
    ) -> Result<HashMap<PriceKey, Pricing>, ModelsDevError> {
        models_dev_prices_from_str(&self.mappings, payload)
    }
}

impl PriceSource for ModelsDevPriceSource {
    fn prices(&self) -> PricesFuture<'_> {
        Box::pin(async move {
            let payload = self
                .client
                .get(&self.url)
                .send()
                .await
                .map_err(|error| -> PriceSourceError { Box::new(ModelsDevError::Request(error)) })?
                .error_for_status()
                .map_err(|error| -> PriceSourceError { Box::new(ModelsDevError::Request(error)) })?
                .text()
                .await
                .map_err(|error| -> PriceSourceError {
                    Box::new(ModelsDevError::Request(error))
                })?;
            self.parse_payload(&payload)
                .map_err(|error| -> PriceSourceError { Box::new(error) })
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ModelsDevError {
    #[error("models.dev request failed: {0}")]
    Request(reqwest::Error),
    #[error("models.dev payload is invalid: {0}")]
    Payload(#[from] serde_json::Error),
}

#[derive(Debug, Deserialize)]
struct ModelsDevProvider {
    #[serde(default)]
    models: BTreeMap<String, ModelsDevModel>,
}

#[derive(Debug, Deserialize)]
struct ModelsDevModel {
    #[serde(default)]
    cost: Option<ModelsDevCost>,
    #[serde(default)]
    last_updated: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ModelsDevCost {
    input: Option<f64>,
    output: Option<f64>,
}

pub fn models_dev_prices_from_str(
    mappings: &[ModelsDevPriceMapping],
    payload: &str,
) -> Result<HashMap<PriceKey, Pricing>, ModelsDevError> {
    let providers: BTreeMap<String, ModelsDevProvider> = serde_json::from_str(payload)?;
    let mut prices = HashMap::new();
    for mapping in mappings {
        let Some(provider) = providers.get(&mapping.models_dev_provider) else {
            tracing::warn!(
                provider = %mapping.column.provider,
                model_id = %mapping.column.model_id,
                endpoint = %mapping.column.endpoint,
                models_dev_provider = %mapping.models_dev_provider,
                "price_sync: mapped models.dev provider is missing"
            );
            continue;
        };
        let Some(model) = provider.models.get(&mapping.models_dev_model_id) else {
            tracing::warn!(
                provider = %mapping.column.provider,
                model_id = %mapping.column.model_id,
                endpoint = %mapping.column.endpoint,
                models_dev_provider = %mapping.models_dev_provider,
                models_dev_model_id = %mapping.models_dev_model_id,
                "price_sync: mapped models.dev model is missing"
            );
            continue;
        };
        let Some(cost) = model.cost.as_ref() else {
            tracing::warn!(
                provider = %mapping.column.provider,
                model_id = %mapping.column.model_id,
                endpoint = %mapping.column.endpoint,
                models_dev_provider = %mapping.models_dev_provider,
                models_dev_model_id = %mapping.models_dev_model_id,
                "price_sync: mapped models.dev model has no token cost"
            );
            continue;
        };
        let Some(input_per_mtok) = cost.input else {
            tracing::warn!(
                provider = %mapping.column.provider,
                model_id = %mapping.column.model_id,
                endpoint = %mapping.column.endpoint,
                models_dev_provider = %mapping.models_dev_provider,
                models_dev_model_id = %mapping.models_dev_model_id,
                "price_sync: mapped models.dev model has no input price"
            );
            continue;
        };
        let Some(output_per_mtok) = cost.output else {
            tracing::warn!(
                provider = %mapping.column.provider,
                model_id = %mapping.column.model_id,
                endpoint = %mapping.column.endpoint,
                models_dev_provider = %mapping.models_dev_provider,
                models_dev_model_id = %mapping.models_dev_model_id,
                "price_sync: mapped models.dev model has no output price"
            );
            continue;
        };
        if !valid_price(input_per_mtok) || !valid_price(output_per_mtok) {
            tracing::warn!(
                provider = %mapping.column.provider,
                model_id = %mapping.column.model_id,
                endpoint = %mapping.column.endpoint,
                models_dev_provider = %mapping.models_dev_provider,
                models_dev_model_id = %mapping.models_dev_model_id,
                input_per_mtok,
                output_per_mtok,
                "price_sync: mapped models.dev price is invalid"
            );
            continue;
        }
        prices.insert(
            mapping.column.clone(),
            Pricing {
                input_per_mtok,
                output_per_mtok,
                currency: MODELS_DEV_CURRENCY.to_owned(),
                as_of: model.last_updated.clone().unwrap_or_default(),
                source: MODELS_DEV_SOURCE.to_owned(),
            },
        );
    }
    Ok(prices)
}

fn valid_price(price: f64) -> bool {
    price.is_finite() && price >= 0.0
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
            Ok(current) if same_price(&current, entry) => continue,
            Ok(current) if price_change_too_large(&current, entry) => {
                tracing::warn!(
                    column_id = %column.id,
                    provider = %column.provider,
                    model_id = %column.model_id,
                    endpoint = %column.endpoint,
                    "price_sync: source price changed by more than 10x; keeping stored price"
                );
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

fn same_price(current: &Pricing, entry: &Pricing) -> bool {
    current.input_per_mtok == entry.input_per_mtok
        && current.output_per_mtok == entry.output_per_mtok
        && current.currency == entry.currency
}

fn price_change_too_large(current: &Pricing, entry: &Pricing) -> bool {
    let empty_snapshot = current.input_per_mtok == 0.0
        && current.output_per_mtok == 0.0
        && current.currency.is_empty()
        && current.source.is_empty();
    if empty_snapshot {
        return false;
    }
    price_ratio_too_large(current.input_per_mtok, entry.input_per_mtok)
        || price_ratio_too_large(current.output_per_mtok, entry.output_per_mtok)
}

fn price_ratio_too_large(current: f64, next: f64) -> bool {
    if current <= 0.0 || next <= 0.0 {
        return false;
    }
    let ratio = current.max(next) / current.min(next);
    ratio > ABSURD_DAILY_PRICE_CHANGE_RATIO
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
    let blocked = simulation::retirement_blockers(&mut conn, column_id, now)
        .await
        .map_err(guard_error)?;
    drop(conn);
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
    let tenants = simulation::survey(&mut conn, pools)
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
