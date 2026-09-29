//! Operator alerts: the row-first insert helper and the process-wide delivery fan-out
//! to SMTP email and an HMAC-signed webhook (spec §11.1; reference Part 3 §4).
//!
//! An alert is always written to the `alerts` table first. Delivery then starts with the
//! row as Postgres returned it, so the subject is jsonb text. Delivery never blocks or
//! fails the writer. A write inside a transaction uses [`write_alert`] and notifies after
//! the commit.

mod email;
mod fanout;
mod webhook;
mod wire;

use maverick_store::alerts::{self, Alert};
use serde::Serialize;
use sqlx::PgExecutor;

pub use email::Email;
pub use fanout::{ATTEMPT_TIMEOUT, AlertNotifier, DELIVERY_RETRIES, Destination, Fanout};
pub use webhook::{
    SIGNATURE_HEADER, Webhook, body as webhook_body, signature as webhook_signature,
};

/// The alert kinds (closed set; reference Part 3 §4.1, plus Rust `retention_expiring`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertKind {
    Regression,
    ParseFailureRate,
    JobFailure,
    SpendWarning,
    SpendRefusal,
    PoolWouldBeEmpty,
    ClassifierDegraded,
    /// Spec §4.5: 14 days before a column's or alias's retention evidence expires.
    RetentionExpiring,
}

impl AlertKind {
    /// The stored `alerts.kind` text.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Regression => "regression",
            Self::ParseFailureRate => "parse_failure_rate",
            Self::JobFailure => "job_failure",
            Self::SpendWarning => "spend_warning",
            Self::SpendRefusal => "spend_refusal",
            Self::PoolWouldBeEmpty => "pool_would_be_empty",
            Self::ClassifierDegraded => "classifier_degraded",
            Self::RetentionExpiring => "retention_expiring",
        }
    }
}

impl std::fmt::Display for AlertKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The alert severities (the `alerts.severity` CHECK set).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

impl Severity {
    /// The stored `alerts.severity` text.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Critical => "critical",
        }
    }
}

/// An error writing an alert row.
#[derive(Debug, thiserror::Error)]
pub enum AlertError {
    #[error("jobs: encoding {kind} alert subject: {source}")]
    Encode {
        kind: AlertKind,
        source: serde_json::Error,
    },
    #[error("jobs: writing {kind} alert: {source}")]
    Write {
        kind: AlertKind,
        source: sqlx::Error,
    },
}

/// Writes an alert row (id `alr_<ulid>`) and returns it as Postgres stored it. Does not
/// notify: use it inside a transaction and notify after the commit.
pub async fn write_alert<'e>(
    db: impl PgExecutor<'e>,
    kind: AlertKind,
    severity: Severity,
    subject: &impl Serialize,
    message: &str,
) -> Result<Alert, AlertError> {
    let subject = serde_json::value::to_raw_value(subject)
        .map_err(|source| AlertError::Encode { kind, source })?;
    let id = maverick_domain::ids::new(maverick_domain::ids::prefix::ALERT);
    alerts::insert_alert(db, &id, kind.as_str(), severity.as_str(), &subject, message)
        .await
        .map_err(|source| AlertError::Write { kind, source })
}

/// Writes an alert row, then notifies with the returned row. A failed write returns the
/// error and notifies nobody. `notifier` may be `None` (Go's nil notifier): the row is
/// still written.
pub async fn insert_alert<'e>(
    db: impl PgExecutor<'e>,
    notifier: Option<&dyn AlertNotifier>,
    kind: AlertKind,
    severity: Severity,
    subject: &impl Serialize,
    message: &str,
) -> Result<Alert, AlertError> {
    let alert = write_alert(db, kind, severity, subject, message).await?;
    if let Some(notifier) = notifier {
        notifier.notify(&alert);
    }
    Ok(alert)
}
