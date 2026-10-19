//! The `model_discovery` job (06:20 daily; spec §4.6, §11, D51): list the text models of
//! every provider with an operator key, compare them with the catalog, and raise
//! `new_model_available` and `model_withdrawn`. It never changes the catalog.
//!
//! [`discover`] is the core (list, then diff), so the on-demand refresh endpoint runs the
//! same code. [`model_discovery`] adds the day's `batches` lease (D10), the alerts, and
//! one INFO summary line. A list call sends no prompt and costs nothing, so G8 does not
//! apply. A failed call is logged with the provider, the error class, and the HTTP status
//! only, never a response body or a key (G5).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use maverick_alerts::{AlertError, AlertKind, AlertNotifier, Severity, insert_alert};
use maverick_domain::discovery::{self, CatalogColumn, ProviderListing, WithdrawnModel};
use maverick_domain::gofmt::quote;
use maverick_providers::ProviderError;
use maverick_providers::models::{ListedModel, ModelLister};
use maverick_store::alerts::list_alerts_by_kind;
use maverick_store::batches::{self, BatchEnd};
use maverick_store::catalog::list_model_columns;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::{BatchLeaseError, BatchLeaseManager, BatchLeaseOutcome, DayArgs, JobArgs, JobsError};

/// The providers discovery reports on, in report order.
pub const PROVIDERS: [&str; 4] = ["anthropic", "google", "openai", "xai"];

/// How long one provider's list call (every page and retry) may take. The calls run one
/// after another inside the job's 60 s limit, so a slow provider is `failed` and the
/// others still run.
pub const LIST_TIMEOUT: Duration = Duration::from_secs(12);

/// How one provider's list call went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderStatus {
    Ok,
    SkippedNoKey,
    Failed,
}

impl ProviderStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::SkippedNoKey => "skipped_no_key",
            Self::Failed => "failed",
        }
    }
}

/// One provider's line in the report: `listed` (the kept text models) when `ok`, and
/// `error` (the class and the HTTP status, never a body) when `failed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderReport {
    pub provider: String,
    pub status: ProviderStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub listed: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A new model, with what its provider said about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NewEntry {
    pub provider: String,
    pub model_id: String,
    pub base_model_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
}

/// What one discovery run found.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Discovery {
    pub providers: Vec<ProviderReport>,
    pub new: Vec<NewEntry>,
    pub withdrawn: Vec<WithdrawnModel>,
    /// Every kept model of every provider that answered, in list order. The refresh
    /// endpoint checks enrollment entries against it.
    #[serde(skip)]
    pub listed: Vec<ListedModel>,
}

impl Discovery {
    /// The providers with `status`, comma-separated, in report order.
    fn providers_with(&self, status: ProviderStatus) -> String {
        self.providers
            .iter()
            .filter(|report| report.status == status)
            .map(|report| report.provider.as_str())
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// How a `model_discovery` delivery ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryOutcome {
    Discovered(DiscoveryOutput),
    /// The day's discovery already completed (D10).
    AlreadyFinished,
    /// Another live worker holds the lease (D10).
    HeldByOther,
}

/// The counts of one completed run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DiscoveryOutput {
    pub new: usize,
    pub withdrawn: usize,
    pub new_alerts: usize,
    pub withdrawn_alerts: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    #[error("jobs: model_discovery: {0}")]
    Store(#[from] sqlx::Error),
    #[error(transparent)]
    Lease(#[from] BatchLeaseError),
    #[error(transparent)]
    Args(#[from] JobsError),
    #[error("jobs: model_discovery: {0}")]
    Alert(#[from] AlertError),
    #[error("jobs: model_discovery: batch lease {0} was lost before completion")]
    LeaseLost(String),
}

/// The error class and HTTP status of a failed list call, never its source text.
fn failure(error: &ProviderError) -> String {
    match error.status() {
        0 => error.kind().as_str().to_owned(),
        status => format!("{} (status {status})", error.kind()),
    }
}

/// Lists every provider in [`PROVIDERS`] order, then compares the lists with the catalog
/// (spec §4.6). A provider without a lister is `skipped_no_key`. A failed call, or one
/// that takes longer than [`LIST_TIMEOUT`], is `failed`, and it reports nothing new and
/// withdraws nothing.
pub async fn discover(
    pool: &PgPool,
    listers: &[Arc<dyn ModelLister>],
) -> Result<Discovery, DiscoveryError> {
    discover_with_timeout(pool, listers, LIST_TIMEOUT).await
}

/// [`discover`] with each provider's list call limited to `list_timeout`.
pub async fn discover_with_timeout(
    pool: &PgPool,
    listers: &[Arc<dyn ModelLister>],
    list_timeout: Duration,
) -> Result<Discovery, DiscoveryError> {
    let mut reports = Vec::with_capacity(PROVIDERS.len());
    let mut listings = Vec::with_capacity(PROVIDERS.len());
    let mut details: HashMap<(String, String), ListedModel> = HashMap::new();
    let mut listed = Vec::new();
    for provider in PROVIDERS {
        let Some(lister) = listers.iter().find(|lister| lister.provider() == provider) else {
            reports.push(ProviderReport {
                provider: provider.to_owned(),
                status: ProviderStatus::SkippedNoKey,
                listed: None,
                error: None,
            });
            listings.push(ProviderListing {
                provider: provider.to_owned(),
                model_ids: None,
            });
            continue;
        };
        let outcome = match tokio::time::timeout(list_timeout, lister.list_models()).await {
            Ok(Ok(models)) => Ok(models),
            Ok(Err(error)) => {
                tracing::warn!(
                    provider,
                    error_kind = error.kind().as_str(),
                    status = error.status(),
                    "model discovery: list call failed"
                );
                Err(failure(&error))
            }
            Err(_) => {
                tracing::warn!(provider, "model discovery: list call timed out");
                Err("timeout".to_owned())
            }
        };
        match outcome {
            Ok(models) => {
                reports.push(ProviderReport {
                    provider: provider.to_owned(),
                    status: ProviderStatus::Ok,
                    listed: Some(models.len()),
                    error: None,
                });
                listings.push(ProviderListing {
                    provider: provider.to_owned(),
                    model_ids: Some(models.iter().map(|model| model.model_id.clone()).collect()),
                });
                listed.extend(models.iter().cloned());
                for model in models {
                    details.insert((provider.to_owned(), model.model_id.clone()), model);
                }
            }
            Err(error) => {
                reports.push(ProviderReport {
                    provider: provider.to_owned(),
                    status: ProviderStatus::Failed,
                    listed: None,
                    error: Some(error),
                });
                listings.push(ProviderListing {
                    provider: provider.to_owned(),
                    model_ids: None,
                });
            }
        }
    }

    let catalog: Vec<CatalogColumn> = list_model_columns(pool)
        .await?
        .into_iter()
        .map(|column| CatalogColumn {
            id: column.id,
            provider: column.provider,
            model_id: column.model_id,
            endpoint: column.endpoint,
            status: column.status,
        })
        .collect();
    let diff = discovery::diff(&listings, &catalog);
    let new = diff
        .new
        .into_iter()
        .map(|model| {
            let listed = details.remove(&(model.provider.clone(), model.model_id.clone()));
            NewEntry {
                display_name: listed.as_ref().and_then(|l| l.display_name.clone()),
                created_at: listed.and_then(|l| l.created_at),
                provider: model.provider,
                model_id: model.model_id,
                base_model_id: model.base_model_id,
            }
        })
        .collect();
    Ok(Discovery {
        providers: reports,
        new,
        withdrawn: diff.withdrawn,
        listed,
    })
}

/// The `new_model_available` subject.
#[derive(Debug, Serialize, Deserialize)]
struct NewSubject {
    provider: String,
    base_model_id: String,
    model_id: String,
}

/// The `model_withdrawn` subject.
#[derive(Debug, Serialize, Deserialize)]
struct WithdrawnSubject {
    column_id: String,
    provider: String,
    model_id: String,
}

/// The subjects of every stored `kind` alert that parse as `T`.
async fn stored_subjects<T: for<'de> Deserialize<'de>>(
    pool: &PgPool,
    kind: AlertKind,
) -> Result<Vec<T>, sqlx::Error> {
    Ok(list_alerts_by_kind(pool, kind.as_str())
        .await?
        .iter()
        .filter_map(|alert| serde_json::from_str(alert.subject.get()).ok())
        .collect())
}

/// Writes the run's alerts (spec §4.6): one `new_model_available` (info) per provider and
/// base id, and one `model_withdrawn` (warning) per column. No second row is written while
/// an earlier row for the same pair or column exists. Messages name ids only. Returns the
/// counts written. The refresh endpoint calls it too.
pub async fn write_alerts(
    pool: &PgPool,
    notifier: Option<&dyn AlertNotifier>,
    run: &Discovery,
) -> Result<(usize, usize), DiscoveryError> {
    let announced: HashSet<(String, String)> =
        stored_subjects::<NewSubject>(pool, AlertKind::NewModelAvailable)
            .await?
            .into_iter()
            .map(|subject| (subject.provider, subject.base_model_id))
            .collect();
    let mut new_alerts = 0;
    for entry in &run.new {
        if announced.contains(&(entry.provider.clone(), entry.base_model_id.clone())) {
            continue;
        }
        let subject = NewSubject {
            provider: entry.provider.clone(),
            base_model_id: entry.base_model_id.clone(),
            model_id: entry.model_id.clone(),
        };
        let message = format!(
            "provider {} lists model {} that no catalog column covers; enroll it with the models refresh or the column API (spec 4.6)",
            entry.provider,
            quote(&entry.model_id)
        );
        insert_alert(
            pool,
            notifier,
            AlertKind::NewModelAvailable,
            Severity::Info,
            &subject,
            &message,
        )
        .await?;
        new_alerts += 1;
    }

    let reported: HashSet<String> =
        stored_subjects::<WithdrawnSubject>(pool, AlertKind::ModelWithdrawn)
            .await?
            .into_iter()
            .map(|subject| subject.column_id)
            .collect();
    let mut withdrawn_alerts = 0;
    for column in &run.withdrawn {
        if reported.contains(&column.column_id) {
            continue;
        }
        let subject = WithdrawnSubject {
            column_id: column.column_id.clone(),
            provider: column.provider.clone(),
            model_id: column.model_id.clone(),
        };
        let message = format!(
            "provider {} no longer lists model {} of column {}; deprecate the column when it is gone (spec 4.6)",
            column.provider,
            quote(&column.model_id),
            column.column_id
        );
        insert_alert(
            pool,
            notifier,
            AlertKind::ModelWithdrawn,
            Severity::Warning,
            &subject,
            &message,
        )
        .await?;
        withdrawn_alerts += 1;
    }
    Ok((new_alerts, withdrawn_alerts))
}

/// What `model_discovery` needs besides the pool.
#[derive(Clone)]
pub struct DiscoveryConfig {
    pub lease: BatchLeaseManager,
    pub notifier: Option<Arc<dyn AlertNotifier>>,
    /// One lister per provider with an operator key. Empty: every provider is skipped.
    pub listers: Vec<Arc<dyn ModelLister>>,
}

impl std::fmt::Debug for DiscoveryConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscoveryConfig")
            .field("lease", &self.lease)
            .field("notifier", &self.notifier.is_some())
            .field(
                "listers",
                &self
                    .listers
                    .iter()
                    .map(|lister| lister.provider().to_owned())
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// One `model_discovery` delivery for `args.day`: claims the day's lease, discovers,
/// writes the alerts, completes the lease, and logs one INFO line.
pub async fn model_discovery(
    pool: &PgPool,
    args: &DayArgs,
    config: &DiscoveryConfig,
    worker_id: &str,
) -> Result<DiscoveryOutcome, DiscoveryError> {
    let window = JobArgs::ModelDiscovery(args.clone()).window()?;
    let lease = match config
        .lease
        .claim(pool, crate::JobKind::ModelDiscovery, &window, worker_id)
        .await?
    {
        BatchLeaseOutcome::Claimed(lease) | BatchLeaseOutcome::TookOver(lease) => lease,
        BatchLeaseOutcome::Finished(_) => return Ok(DiscoveryOutcome::AlreadyFinished),
        BatchLeaseOutcome::HeldByOther(_) => return Ok(DiscoveryOutcome::HeldByOther),
    };
    let run = discover(pool, &config.listers).await?;
    let (new_alerts, withdrawn_alerts) =
        write_alerts(pool, config.notifier.as_deref(), &run).await?;
    batches::finish_batch(pool, lease.id(), lease.worker_id(), BatchEnd::Completed)
        .await?
        .ok_or_else(|| DiscoveryError::LeaseLost(lease.id().to_owned()))?;
    let output = DiscoveryOutput {
        new: run.new.len(),
        withdrawn: run.withdrawn.len(),
        new_alerts,
        withdrawn_alerts,
    };
    tracing::info!(
        day = %window,
        ok = %run.providers_with(ProviderStatus::Ok),
        skipped_no_key = %run.providers_with(ProviderStatus::SkippedNoKey),
        failed = %run.providers_with(ProviderStatus::Failed),
        new = output.new,
        withdrawn = output.withdrawn,
        new_alerts = output.new_alerts,
        withdrawn_alerts = output.withdrawn_alerts,
        "model discovery finished"
    );
    Ok(DiscoveryOutcome::Discovered(output))
}

#[cfg(test)]
mod tests {
    use super::*;
    use maverick_providers::ProviderErrorKind;

    #[test]
    fn a_failure_names_the_class_and_status_only() {
        let error = ProviderError::new("openai", ProviderErrorKind::ProviderError)
            .with_status(503)
            .with_source("body text that must not appear");
        assert_eq!(failure(&error), "provider_error (status 503)");
        let error = ProviderError::new("xai", ProviderErrorKind::Timeout);
        assert_eq!(failure(&error), "timeout");
    }

    #[test]
    fn statuses_have_their_report_names() {
        for (status, name) in [
            (ProviderStatus::Ok, "ok"),
            (ProviderStatus::SkippedNoKey, "skipped_no_key"),
            (ProviderStatus::Failed, "failed"),
        ] {
            assert_eq!(status.as_str(), name);
            assert_eq!(
                serde_json::to_value(status).expect("encodes"),
                serde_json::Value::String(name.to_owned())
            );
        }
    }
}
