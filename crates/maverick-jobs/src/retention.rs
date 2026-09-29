//! The retention jobs: `capture_expiry` (hourly) and `history_prune` (daily).
//!
//! Spec §10.3 and §10.4; reference Part 3 §2.14, Part 4 §4.7, Part 1 §3. Each job runs
//! in one transaction. Aggregates, calibration, and the capture access log are never
//! pruned: they are the product and the audit trail. Both jobs validate their window
//! argument but run against the clock they are given, not the argument (as Go did).

use std::time::Duration;

use chrono::{DateTime, Days, SecondsFormat, Utc};
use maverick_alerts::{AlertKind, AlertNotifier, Severity, insert_alert};
use maverick_domain::ids;
use maverick_store::{capture, decisions};
use serde::Serialize;
use sqlx::{PgConnection, PgPool};

use crate::{DayArgs, HourArgs, JobArgs, JobKind, JobsError};

/// The access-log actor of a capture that `capture_expiry` deleted.
pub const CAPTURE_EXPIRY_ACTOR: &str = "capture_expiry";
/// The access-log actor of a capture that `history_prune` deleted.
pub const HISTORY_PRUNE_ACTOR: &str = "history_prune";

/// The output of `capture_expiry`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CaptureExpiryOutput {
    pub deleted_captures: u64,
    pub deleted_windows: u64,
}

/// The output of `history_prune`. `cutoff` is RFC 3339.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HistoryPruneOutput {
    pub cutoff: String,
    pub deleted_recommendations: u64,
    pub deleted_outcomes: u64,
    pub deleted_captures: u64,
}

/// An error from a retention job.
#[derive(Debug, thiserror::Error)]
pub enum RetentionError {
    /// The window argument did not parse.
    #[error(transparent)]
    InvalidArgument(#[from] JobsError),
    /// A database step failed. The transaction rolled back.
    #[error("jobs: {job}: {step}: {source}")]
    Store {
        job: JobKind,
        step: &'static str,
        source: sqlx::Error,
    },
}

/// What the retention jobs need.
#[derive(Clone)]
pub struct RetentionDeps<'a> {
    pub pool: &'a PgPool,
    /// Delivery for the `job_failure` alert. `None` still writes the alert row.
    pub notifier: Option<&'a dyn AlertNotifier>,
    /// `capture.retention_hours`: a window is swept once it ended this long ago.
    pub capture_retention: Duration,
    /// `history.retention_days` (`MAVERICK_HISTORY_RETENTION_DAYS`).
    pub history_retention_days: u32,
}

/// Deletes every capture with `expires_at <= now` and logs a `delete` row for each,
/// then deletes the windows that ended `capture_retention` or more before `now`. One
/// transaction.
pub async fn capture_expiry(
    pool: &PgPool,
    now: DateTime<Utc>,
    capture_retention: Duration,
) -> Result<CaptureExpiryOutput, RetentionError> {
    let job = JobKind::CaptureExpiry;
    let err = |step| move |source| RetentionError::Store { job, step, source };
    let mut tx = pool.begin().await.map_err(err("beginning"))?;
    let deleted = capture::delete_expired_debug_captures(&mut *tx, now)
        .await
        .map_err(err("deleting expired captures"))?;
    log_deletes(&mut tx, &deleted, CAPTURE_EXPIRY_ACTOR)
        .await
        .map_err(err("logging capture deletes"))?;
    let retention = chrono::Duration::from_std(capture_retention).unwrap_or(chrono::Duration::MAX);
    let window_cutoff = now
        .checked_sub_signed(retention)
        .unwrap_or(DateTime::<Utc>::MIN_UTC);
    let deleted_windows = capture::delete_expired_capture_windows(&mut *tx, window_cutoff)
        .await
        .map_err(err("deleting spent windows"))?;
    tx.commit().await.map_err(err("committing"))?;
    Ok(CaptureExpiryOutput {
        deleted_captures: deleted.len() as u64,
        deleted_windows,
    })
}

/// Deletes the recommendations created before `now - retention_days` (calendar days),
/// after their captures (each logged) and their outcomes, in that order. One
/// transaction.
pub async fn history_prune(
    pool: &PgPool,
    now: DateTime<Utc>,
    retention_days: u32,
) -> Result<HistoryPruneOutput, RetentionError> {
    let job = JobKind::HistoryPrune;
    let err = |step| move |source| RetentionError::Store { job, step, source };
    let cutoff = now
        .checked_sub_days(Days::new(u64::from(retention_days)))
        .unwrap_or(DateTime::<Utc>::MIN_UTC);
    let mut tx = pool.begin().await.map_err(err("beginning"))?;
    let captures = capture::delete_captures_for_recommendations_before(&mut *tx, cutoff)
        .await
        .map_err(err("deleting captures"))?;
    log_deletes(&mut tx, &captures, HISTORY_PRUNE_ACTOR)
        .await
        .map_err(err("logging capture deletes"))?;
    let deleted_outcomes = decisions::delete_outcomes_for_recommendations_before(&mut *tx, cutoff)
        .await
        .map_err(err("deleting outcomes"))?;
    let deleted_recommendations = decisions::delete_recommendations_before(&mut *tx, cutoff)
        .await
        .map_err(err("deleting recommendations"))?;
    tx.commit().await.map_err(err("committing"))?;
    Ok(HistoryPruneOutput {
        cutoff: cutoff.to_rfc3339_opts(SecondsFormat::AutoSi, true),
        deleted_recommendations,
        deleted_outcomes,
        deleted_captures: captures.len() as u64,
    })
}

/// Appends one `delete` access-log row per deleted capture. The log has no FK, so the
/// rows outlive the captures and turn a later replay into `410 capture_expired`.
async fn log_deletes(
    conn: &mut PgConnection,
    deleted: &[capture::DeletedCapture],
    actor: &str,
) -> sqlx::Result<()> {
    for capture in deleted {
        let id = ids::new(ids::prefix::CAPTURE_ACCESS);
        capture::insert_capture_access_log(
            &mut *conn,
            &id,
            &capture.id,
            &capture.tenant_id,
            actor,
            "delete",
        )
        .await?;
    }
    Ok(())
}

/// The `capture_expiry` job: validates the hour argument, runs [`capture_expiry`] at
/// `now`, and on failure writes a `job_failure` warning before returning the error (the
/// queue retries).
pub async fn run_capture_expiry(
    deps: &RetentionDeps<'_>,
    args: &HourArgs,
    now: DateTime<Utc>,
) -> Result<CaptureExpiryOutput, RetentionError> {
    JobArgs::CaptureExpiry(args.clone()).window()?;
    let result = capture_expiry(deps.pool, now, deps.capture_retention).await;
    if let Err(error) = &result {
        let subject = serde_json::json!({"error": error.to_string(), "job": "capture_expiry", "hour": args.hour});
        report_failure(deps, JobKind::CaptureExpiry, &subject, error).await;
    }
    result
}

/// The `history_prune` job: validates the day argument, runs [`history_prune`] at
/// `now`, and on failure writes a `job_failure` warning before returning the error.
pub async fn run_history_prune(
    deps: &RetentionDeps<'_>,
    args: &DayArgs,
    now: DateTime<Utc>,
) -> Result<HistoryPruneOutput, RetentionError> {
    JobArgs::HistoryPrune(args.clone()).window()?;
    let result = history_prune(deps.pool, now, deps.history_retention_days).await;
    if let Err(error) = &result {
        let subject = serde_json::json!({"error": error.to_string(), "job": "history_prune", "day": args.day});
        report_failure(deps, JobKind::HistoryPrune, &subject, error).await;
    }
    result
}

/// Writes the `job_failure` alert in its own statement, after the failed transaction
/// rolled back. A failure to write it is logged; the job error still wins.
async fn report_failure(
    deps: &RetentionDeps<'_>,
    job: JobKind,
    subject: &serde_json::Value,
    error: &RetentionError,
) {
    let message = format!("{} failed: {error}", job.as_str());
    if let Err(alert_error) = insert_alert(
        deps.pool,
        deps.notifier,
        AlertKind::JobFailure,
        Severity::Warning,
        subject,
        &message,
    )
    .await
    {
        tracing::warn!(job = job.as_str(), error = %alert_error, "writing the job_failure alert failed");
    }
}
