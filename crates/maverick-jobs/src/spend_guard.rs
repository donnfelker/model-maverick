//! The G8 spend guard as batch jobs run it (reference Part 3 §2.6, spec §11): the batch's
//! projected cost plus the window actuals against the nightly and weekly caps.
//!
//! A refusal writes a critical `spend_refusal` alert and the batch does not run; the
//! caller records the window as `skipped` (D10). A projection at or above 80% of a cap
//! writes a `spend_warning` alert and the batch runs.

use chrono::{Days, NaiveDate, NaiveTime, TimeZone, Utc};
use maverick_alerts::{AlertError, AlertKind, AlertNotifier, Severity, insert_alert};
use maverick_domain::spend::{Projection, SpendCaps, SpendCheck, SpendDecision};
use maverick_store::measurement;
use serde_json::value::RawValue;
use sqlx::PgPool;

use crate::JobKind;

/// The guard's result for one batch.
#[derive(Debug, Clone, PartialEq)]
pub struct SpendOutcome {
    /// False when the batch must not run.
    pub allowed: bool,
    /// True when a `spend_warning` was raised.
    pub warned: bool,
    pub projected_usd: f64,
    pub planned_runs: i64,
    pub night_spent_usd: f64,
    pub week_spent_usd: f64,
}

#[derive(Debug, thiserror::Error)]
pub enum SpendGuardError {
    #[error("jobs: spend guard: {0}")]
    Store(#[from] sqlx::Error),
    #[error(transparent)]
    Alert(#[from] AlertError),
    #[error("jobs: spend guard: {0}")]
    Subject(String),
}

fn decimal_to_f64(value: maverick_store::Decimal) -> f64 {
    value.to_string().parse().unwrap_or(0.0)
}

/// `(night_spent, week_spent)`: the cost of the runs already made in the batch's windows.
/// Windows are over `runs.ran_at`, half-open: the night is `[night 00:00 UTC, +1 day)`
/// and the week `[night - 6 days, night + 1 day)`.
pub async fn window_spend(pool: &PgPool, night: NaiveDate) -> Result<(f64, f64), sqlx::Error> {
    let night_start = Utc.from_utc_datetime(&night.and_time(NaiveTime::MIN));
    let night_end = night_start + Days::new(1);
    let week_start = night_start - Days::new(6);
    let night_spent =
        decimal_to_f64(measurement::sum_run_cost_between(pool, night_start, night_end).await?);
    let week_spent =
        decimal_to_f64(measurement::sum_run_cost_between(pool, week_start, night_end).await?);
    Ok((night_spent, week_spent))
}

/// The guard's check of one batch: the nightly cap applies to every job but
/// `weekly_sweep`.
pub fn spend_check(
    job: JobKind,
    night: NaiveDate,
    projection: &Projection,
    caps: SpendCaps,
    night_spent_usd: f64,
    week_spent_usd: f64,
) -> SpendCheck<'static> {
    SpendCheck {
        job: job.as_str(),
        night,
        projected_usd: projection.projected_usd,
        planned_runs: projection.runs,
        night_spent_usd,
        week_spent_usd,
        caps,
        apply_nightly_cap: job != JobKind::WeeklySweep,
    }
}

/// `check(job, night, est, applyNightlyCap)`: [`window_spend`] and [`spend_check`], then
/// the alert for a warning or a refusal.
pub async fn check_spend(
    pool: &PgPool,
    notifier: Option<&dyn AlertNotifier>,
    job: JobKind,
    night: NaiveDate,
    projection: &Projection,
    caps: SpendCaps,
) -> Result<SpendOutcome, SpendGuardError> {
    let (night_spent, week_spent) = window_spend(pool, night).await?;
    let check = spend_check(job, night, projection, caps, night_spent, week_spent);
    let decision = check.decision();
    let outcome = SpendOutcome {
        allowed: decision != SpendDecision::Refuse,
        warned: decision == SpendDecision::Warn,
        projected_usd: projection.projected_usd,
        planned_runs: projection.runs,
        night_spent_usd: night_spent,
        week_spent_usd: week_spent,
    };
    let (kind, severity, message) = match decision {
        SpendDecision::Allow => return Ok(outcome),
        SpendDecision::Warn => (
            AlertKind::SpendWarning,
            Severity::Warning,
            check.warning_message(),
        ),
        SpendDecision::Refuse => {
            tracing::error!(
                job = job.as_str(),
                night = %night,
                projected_usd = projection.projected_usd,
                night_total_usd = check.night_total_usd(),
                week_total_usd = check.week_total_usd(),
                "spend guard refused the batch"
            );
            (
                AlertKind::SpendRefusal,
                Severity::Critical,
                check.refusal_message(),
            )
        }
    };
    let subject = check.subject().map_err(SpendGuardError::Subject)?;
    let subject =
        RawValue::from_string(subject).map_err(|e| SpendGuardError::Subject(e.to_string()))?;
    insert_alert(pool, notifier, kind, severity, &subject, &message).await?;
    Ok(outcome)
}
