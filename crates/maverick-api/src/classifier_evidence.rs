//! The zero-retention classifier column's evidence, watched like a pool member's (spec
//! §8.3, D2).
//!
//! A zero-retention tenant's prompt is classified only on the column that
//! `classifier.zero_retention_column` names, and only while that column holds valid zero
//! evidence. Without it, every such prompt-form request answers `422
//! zero_retention_requires_declared`. That fails closed, so it is safe, but nothing else
//! would tell the operator. So when the called column resolves but its effective
//! retention is not `zero`, this writes one critical `classifier_degraded` alert (reason
//! `zero_retention_evidence`). When the evidence is valid again, it writes one `info`
//! alert (reason `zero_retention_evidence_restored`).
//!
//! The newest of those alerts for the column ref is the state, so the column write path
//! and the D41 re-resolution loop can both call [`alert_on_change`] without alerting twice
//! for one change, and a restart does not repeat an alert. A ref that matches no catalog
//! column is not judged here: D41's `unresolved` alert covers it.

use chrono::{DateTime, Utc};
use maverick_alerts::{AlertError, AlertKind, AlertNotifier, Severity, insert_alert};
use maverick_domain::policy::RetentionMode;
use maverick_store::alerts::{self, Alert};
use maverick_store::catalog::ModelColumn;
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::classify::{ResolverSettings, find_column};
use crate::recommend::effective_retention;

/// The `classifier_degraded` reason when the called column has no valid zero evidence.
pub const LOST: &str = "zero_retention_evidence";
/// The `classifier_degraded` reason when its evidence is valid again.
pub const RESTORED: &str = "zero_retention_evidence_restored";

/// Why [`alert_on_change`] failed.
#[derive(Debug, thiserror::Error)]
pub enum EvidenceError {
    #[error("classifier evidence: reading alerts: {0}")]
    Read(#[from] sqlx::Error),
    #[error(transparent)]
    Write(#[from] AlertError),
}

/// Checks the column the zero-retention classifier calls against `columns` at `now`, and
/// writes the alert a change calls for. Returns the alert it wrote, if any. Does nothing
/// when `classifier.zero_retention_column` is unset or matches no column.
pub async fn alert_on_change(
    db: &PgPool,
    notifier: Option<&dyn AlertNotifier>,
    settings: &ResolverSettings,
    columns: &[ModelColumn],
    now: DateTime<Utc>,
) -> Result<Option<Alert>, EvidenceError> {
    let Some(zero_column) = settings.zero_retention_column.as_deref() else {
        return Ok(None);
    };
    let Ok(Some(column)) = find_column(columns, zero_column) else {
        return Ok(None);
    };
    let retention = effective_retention(column, now);
    let lost = retention != RetentionMode::Zero;
    let (severity, reason, message) = match (lost, latest_is_lost(db, zero_column).await?) {
        (true, false) => (
            Severity::Critical,
            LOST,
            format!(
                "zero-retention classifier column {zero_column} (column {}) has no valid \
                 zero-retention evidence (effective retention {}); zero-retention tenants' \
                 prompt-form requests get 422 zero_retention_requires_declared",
                column.id,
                retention.as_str()
            ),
        ),
        (false, true) => (
            Severity::Info,
            RESTORED,
            format!(
                "zero-retention classifier column {zero_column} (column {}) has valid \
                 zero-retention evidence again; zero-retention tenants' prompt-form requests \
                 are classified",
                column.id
            ),
        ),
        _ => return Ok(None),
    };
    let subject = json!({
        "classifier_column": settings.column,
        "column_id": column.id,
        "reason": reason,
        "zero_retention_column": zero_column,
    });
    let alert = insert_alert(
        db,
        notifier,
        AlertKind::ClassifierDegraded,
        severity,
        &subject,
        &message,
    )
    .await?;
    Ok(Some(alert))
}

/// True when the newest evidence alert for `zero_column` says the evidence is lost.
async fn latest_is_lost(db: &PgPool, zero_column: &str) -> Result<bool, sqlx::Error> {
    let rows = alerts::list_alerts_by_kind(db, AlertKind::ClassifierDegraded.as_str()).await?;
    Ok(rows
        .iter()
        .rev()
        .find_map(|row| {
            let subject: Value = serde_json::from_str(row.subject.get()).ok()?;
            if subject.get("zero_retention_column")?.as_str()? != zero_column {
                return None;
            }
            match subject.get("reason")?.as_str()? {
                LOST => Some(true),
                RESTORED => Some(false),
                _ => None,
            }
        })
        .unwrap_or(false))
}
