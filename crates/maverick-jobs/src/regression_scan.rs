//! `regression_scan` (reference Part 3 §2.10; spec §6.5 and §11): quality regressions
//! for every cell folded on the night, with dedup and a severity from the tenants'
//! quality bars, then the parse-failure watch for the batch.
//!
//! The alerts and the lease completion commit in one transaction; notifications go out
//! after the commit.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::{Days, NaiveDate, NaiveTime, TimeZone, Utc};
use graphile_worker::{IntoTaskHandlerResult, TaskHandler, WorkerContext};
use maverick_alerts::{AlertError, AlertKind, AlertNotifier, Severity, write_alert};
use maverick_domain::scoring::{
    AlertMark, DEFAULT_QUALITY_BAR, PARSE_FAILURE_RATE_FLOOR, PARSE_FAILURE_TRAILING_MULTIPLE,
    RegressionParams, SeriesPoint, detect_regressions, filter_alertable, parse_failure_alert,
    parse_failure_rate, regression_severity,
};
use maverick_store::alerts::{self, Alert};
use maverick_store::measurement::{self, CellDimension};
use maverick_store::{batches, tenancy};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgPool;

use crate::{
    BatchEnd, BatchFollowupArgs, BatchLeaseError, BatchLeaseManager, BatchLeaseOutcome, JobArgs,
    JobKind, JobsError, with_job_timeout,
};

/// What `regression_scan` needs besides the pool.
#[derive(Clone)]
pub struct RegressionScanConfig {
    /// The EWMA weight the aggregates were folded with.
    pub alpha: f64,
    pub lease: BatchLeaseManager,
    pub notifier: Option<Arc<dyn AlertNotifier>>,
}

impl std::fmt::Debug for RegressionScanConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegressionScanConfig")
            .field("alpha", &self.alpha)
            .field("lease", &self.lease)
            .field("notifier", &self.notifier.is_some())
            .finish()
    }
}

/// The job output: `{"batch_id","cells_scanned","regression_alerts","parse_failure_alerts"}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegressionScanOutput {
    pub batch_id: String,
    pub cells_scanned: usize,
    pub regression_alerts: usize,
    pub parse_failure_alerts: usize,
}

/// How a delivery ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegressionScanOutcome {
    Scanned(RegressionScanOutput),
    /// The scan for this batch already completed (D10).
    AlreadyFinished,
    /// Another live worker holds the scan's lease (D10).
    HeldByOther,
}

#[derive(Debug, thiserror::Error)]
pub enum RegressionScanError {
    #[error("jobs: regression_scan: {0}")]
    Store(#[from] sqlx::Error),
    #[error(transparent)]
    Lease(#[from] BatchLeaseError),
    #[error(transparent)]
    Args(#[from] JobsError),
    #[error(transparent)]
    Alert(#[from] AlertError),
    #[error("jobs: regression_scan: batch lease {0} was lost before completion")]
    LeaseLost(String),
}

/// An alert to write: kind, severity, subject, message.
struct Pending {
    kind: AlertKind,
    severity: Severity,
    subject: serde_json::Value,
    message: String,
}

/// The newest earlier regression for the cell: the latest `as_of` among the parseable
/// subjects, the earlier row on ties.
fn dedup_state(existing: &[Alert]) -> Option<AlertMark> {
    let mut state: Option<AlertMark> = None;
    for alert in existing {
        let Ok(subject) = serde_json::from_str::<serde_json::Value>(alert.subject.get()) else {
            continue;
        };
        let (Some(as_of), Some(current)) = (
            subject["as_of"]
                .as_str()
                .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()),
            subject["current"].as_f64(),
        ) else {
            continue;
        };
        if state.is_none_or(|s| as_of > s.as_of) {
            state = Some(AlertMark { as_of, current });
        }
    }
    state
}

/// The quality bars of the tenants whose pools hold the column, each tenant once.
async fn tenant_bars(pool: &PgPool, column: &str, capability: &str) -> sqlx::Result<Vec<f64>> {
    let mut seen = HashSet::new();
    let mut bars = Vec::new();
    for pool_row in tenancy::list_pools_by_column(pool, column).await? {
        if !seen.insert(pool_row.tenant_id.clone()) {
            continue;
        }
        let weights = tenancy::list_tenant_weights_by_tenant(pool, &pool_row.tenant_id).await?;
        let bar = weights
            .iter()
            .find(|w| w.capability == capability)
            .map_or(f64::from(DEFAULT_QUALITY_BAR as f32), |w| {
                f64::from(w.quality_bar)
            });
        bars.push(bar);
    }
    Ok(bars)
}

/// One delivery.
pub async fn regression_scan(
    pool: &PgPool,
    args: &BatchFollowupArgs,
    config: &RegressionScanConfig,
    worker_id: &str,
) -> Result<RegressionScanOutcome, RegressionScanError> {
    let window = JobArgs::RegressionScan(args.clone()).window()?;
    // `window()` validated the night.
    let night = NaiveDate::parse_from_str(&args.night, "%Y-%m-%d").unwrap_or_default();
    let lease = match config
        .lease
        .claim(pool, JobKind::RegressionScan, &window, worker_id)
        .await?
    {
        BatchLeaseOutcome::Claimed(lease) | BatchLeaseOutcome::TookOver(lease) => lease,
        BatchLeaseOutcome::Finished(_) => return Ok(RegressionScanOutcome::AlreadyFinished),
        BatchLeaseOutcome::HeldByOther(_) => return Ok(RegressionScanOutcome::HeldByOther),
    };

    let params = RegressionParams {
        alpha: config.alpha,
        ..RegressionParams::default()
    };
    let mut pending = Vec::new();
    let mut output = RegressionScanOutput {
        batch_id: args.batch_id.clone(),
        ..RegressionScanOutput::default()
    };

    // Every quality cell folded on the night (QUIRK J4: not only this batch's).
    let cells = measurement::list_aggregates_at_as_of(pool, "quality", night).await?;
    for cell in &cells {
        output.cells_scanned += 1;
        let key = CellDimension {
            model_column_id: &cell.model_column_id,
            capability: &cell.capability,
            difficulty_tier: cell.difficulty_tier,
            dimension: "quality",
        };
        let series: Vec<SeriesPoint> =
            measurement::list_aggregate_series_before(pool, &key, night + Days::new(1))
                .await?
                .into_iter()
                .map(|row| SeriesPoint {
                    as_of: row.as_of,
                    ewma: f64::from(row.ewma),
                    variance: f64::from(row.variance),
                })
                .collect();
        let tonight: Vec<_> = detect_regressions(&series, &params)
            .into_iter()
            .filter(|f| f.as_of == night)
            .collect();
        if tonight.is_empty() {
            continue;
        }
        let existing = alerts::list_regression_alerts_for_cell(
            pool,
            &cell.model_column_id,
            &cell.capability,
            &cell.difficulty_tier.to_string(),
        )
        .await?;
        let alertable = filter_alertable(&tonight, dedup_state(&existing), &params);
        if alertable.is_empty() {
            continue;
        }
        let bars = tenant_bars(pool, &cell.model_column_id, &cell.capability).await?;
        for finding in alertable {
            let severity = if regression_severity(&finding, &bars) == "critical" {
                Severity::Critical
            } else {
                Severity::Warning
            };
            let as_of = finding.as_of.format("%Y-%m-%d").to_string();
            pending.push(Pending {
                kind: AlertKind::Regression,
                severity,
                subject: json!({
                    "as_of": as_of,
                    "baseline": finding.baseline,
                    "batch_id": args.batch_id,
                    "capability": cell.capability,
                    "current": finding.current,
                    "delta": finding.delta,
                    "difficulty_tier": cell.difficulty_tier,
                    "dimension": finding.dimension,
                    "model_column_id": cell.model_column_id,
                    "regression_kind": finding.kind.as_str(),
                }),
                message: format!(
                    "quality regression ({}) on {} {} tier {}: {:.1} \u{2192} {:.1} (\u{2212}{:.1} pts) as of {}",
                    finding.kind.as_str(),
                    cell.model_column_id,
                    cell.capability,
                    cell.difficulty_tier,
                    finding.baseline,
                    finding.current,
                    finding.delta,
                    as_of
                ),
            });
            output.regression_alerts += 1;
        }
    }

    // The parse-failure watch over this batch's runs.
    let night_start = Utc.from_utc_datetime(&night.and_time(NaiveTime::MIN));
    let trailing: HashMap<String, f64> = measurement::list_parse_failure_rates_between(
        pool,
        night_start - Days::new(7),
        night_start,
    )
    .await?
    .into_iter()
    .map(|r| {
        (
            r.model_column_id,
            parse_failure_rate(r.parse_failures, r.total),
        )
    })
    .collect();
    let night_text = night.format("%Y-%m-%d").to_string();
    let already: HashSet<String> = alerts::list_alerts_by_kind(pool, "parse_failure_rate")
        .await?
        .into_iter()
        .filter_map(|a| {
            let subject: serde_json::Value = serde_json::from_str(a.subject.get()).ok()?;
            (subject["night"] == night_text.as_str())
                .then(|| subject["model_column_id"].as_str().map(str::to_owned))
                .flatten()
        })
        .collect();
    for rate in measurement::list_batch_parse_failure_rates(pool, &args.batch_id).await? {
        if rate.total <= 0 || already.contains(&rate.model_column_id) {
            continue;
        }
        let trailing_rate = trailing.get(&rate.model_column_id).copied().unwrap_or(0.0);
        let Some(value) = parse_failure_alert(rate.parse_failures, rate.total, trailing_rate)
        else {
            continue;
        };
        pending.push(Pending {
            kind: AlertKind::ParseFailureRate,
            severity: Severity::Warning,
            subject: json!({
                "batch_id": args.batch_id,
                "model_column_id": rate.model_column_id,
                "night": night_text,
                "parse_failure_rate": value,
                "parse_failures": rate.parse_failures,
                "total_runs": rate.total,
                "trailing_rate": trailing_rate,
            }),
            message: format!(
                "{} parse-failure rate {:.1}% over {} runs on {} (> {:.0}% and > {:.0}\u{d7} trailing 7-day rate {:.1}%; spec 11)",
                rate.model_column_id,
                value * 100.0,
                rate.total,
                night_text,
                PARSE_FAILURE_RATE_FLOOR * 100.0,
                PARSE_FAILURE_TRAILING_MULTIPLE,
                trailing_rate * 100.0
            ),
        });
        output.parse_failure_alerts += 1;
    }

    let mut tx = pool.begin().await?;
    let mut written = Vec::with_capacity(pending.len());
    for alert in &pending {
        written.push(
            write_alert(
                &mut *tx,
                alert.kind,
                alert.severity,
                &alert.subject,
                &alert.message,
            )
            .await?,
        );
    }
    batches::finish_batch(&mut *tx, lease.id(), lease.worker_id(), BatchEnd::Completed)
        .await?
        .ok_or_else(|| RegressionScanError::LeaseLost(lease.id().to_owned()))?;
    tx.commit().await?;
    if let Some(notifier) = config.notifier.as_deref() {
        for alert in &written {
            notifier.notify(alert);
        }
    }
    tracing::info!(
        batch_id = %output.batch_id,
        cells_scanned = output.cells_scanned,
        regression_alerts = output.regression_alerts,
        parse_failure_alerts = output.parse_failure_alerts,
        "regression_scan complete"
    );
    Ok(RegressionScanOutcome::Scanned(output))
}

/// The graphile task. The worker must carry a [`RegressionScanConfig`] extension.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RegressionScanJob(pub BatchFollowupArgs);

impl TaskHandler for RegressionScanJob {
    const IDENTIFIER: &'static str = "regression_scan";

    async fn run(self, ctx: WorkerContext) -> impl IntoTaskHandlerResult {
        let Some(config) = ctx.extensions().get::<RegressionScanConfig>() else {
            return Err("jobs: regression_scan: the worker has no RegressionScanConfig".to_owned());
        };
        with_job_timeout(&ctx, JobKind::RegressionScan, async {
            regression_scan(ctx.pg_pool(), &self.0, config, ctx.worker_id())
                .await
                .map(|_| ())
        })
        .await
    }
}
