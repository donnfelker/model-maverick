//! Operator alerts.
//!
//! Go source: `parity/schema/queries/alerts.sql`.

use chrono::{DateTime, Utc};
use sqlx::PgExecutor;

use crate::{Json, JsonRawValue};

/// An `alerts` row. `subject` is machine-readable: column, cell, job, or tenant refs.
#[derive(Debug, Clone)]
pub struct Alert {
    pub id: String,
    pub kind: String,
    pub severity: String,
    pub subject: Json,
    pub message: String,
    pub created_at: DateTime<Utc>,
    pub acked_at: Option<DateTime<Utc>>,
}

/// `InsertAlert`: `created_at = now()`, not acked. `severity` is `info`, `warning`, or
/// `critical`.
pub async fn insert_alert<'e>(
    db: impl PgExecutor<'e>,
    id: &str,
    kind: &str,
    severity: &str,
    subject: &JsonRawValue,
    message: &str,
) -> sqlx::Result<Alert> {
    sqlx::query_as!(
        Alert,
        r#"insert into alerts (id, kind, severity, subject, message)
           values ($1, $2, $3, $4, $5)
           returning id, kind, severity, subject as "subject: _", message, created_at,
                     acked_at"#,
        id,
        kind,
        severity,
        subject as &JsonRawValue,
        message
    )
    .fetch_one(db)
    .await
}

/// `ListAlertsByKind`: oldest first, ties by id.
pub async fn list_alerts_by_kind<'e>(
    db: impl PgExecutor<'e>,
    kind: &str,
) -> sqlx::Result<Vec<Alert>> {
    sqlx::query_as!(
        Alert,
        r#"select id, kind, severity, subject as "subject: _", message, created_at, acked_at
           from alerts
           where kind = $1
           order by created_at, id"#,
        kind
    )
    .fetch_all(db)
    .await
}

/// `ListRegressionAlertsForColumn`: regression alerts whose subject names the column.
pub async fn list_regression_alerts_for_column<'e>(
    db: impl PgExecutor<'e>,
    model_column_id: &str,
) -> sqlx::Result<Vec<Alert>> {
    sqlx::query_as!(
        Alert,
        r#"select id, kind, severity, subject as "subject: _", message, created_at, acked_at
           from alerts
           where kind = 'regression'
             and subject->>'model_column_id' = $1::text
           order by created_at, id"#,
        model_column_id
    )
    .fetch_all(db)
    .await
}

/// `ListRegressionAlertsForCell`: regression alerts for one cell. `difficulty_tier` is
/// compared as text (for example `"3"`), so a malformed subject never fails the scan.
pub async fn list_regression_alerts_for_cell<'e>(
    db: impl PgExecutor<'e>,
    model_column_id: &str,
    capability: &str,
    difficulty_tier: &str,
) -> sqlx::Result<Vec<Alert>> {
    sqlx::query_as!(
        Alert,
        r#"select id, kind, severity, subject as "subject: _", message, created_at, acked_at
           from alerts
           where kind = 'regression'
             and subject->>'model_column_id' = $1::text
             and subject->>'capability' = $2::text
             and subject->>'difficulty_tier' = $3::text
           order by created_at, id"#,
        model_column_id,
        capability,
        difficulty_tier
    )
    .fetch_all(db)
    .await
}

/// `ListAlertsFeed`: every alert, oldest first, optionally one kind (empty means all)
/// and from `since`.
pub async fn list_alerts_feed<'e>(
    db: impl PgExecutor<'e>,
    kind: &str,
    since: Option<DateTime<Utc>>,
) -> sqlx::Result<Vec<Alert>> {
    sqlx::query_as!(
        Alert,
        r#"select id, kind, severity, subject as "subject: _", message, created_at, acked_at
           from alerts
           where ($1::text = '' or kind = $1)
             and ($2::timestamptz is null or created_at >= $2)
           order by created_at, id"#,
        kind,
        since
    )
    .fetch_all(db)
    .await
}
