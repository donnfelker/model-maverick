//! The worker process (reference Part 1 §7.3 step 9; spec §11): every implemented job
//! kind as a graphile_worker task with its configuration as a worker extension, and the
//! cron schedule.
//!
//! graphile_worker keeps a crontab's payload fixed and adds only `_cron: {ts}` to each
//! firing, so the window cannot be baked into the payload at startup (a long-running
//! process would repeat its first window forever). Each scheduled kind instead gets a
//! small `cron.<kind>` trigger task: its payload is `{}`, and it enqueues the real job
//! for the firing's own window (`scheduled_args(kind, ts)`), with the kind's job key.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use graphile_worker::{
    CronJobKeyMode, Crontab, IntoTaskHandlerResult, TaskHandler, WorkerContext, WorkerOptions,
};
use maverick_alerts::AlertNotifier;
use maverick_domain::graders::Registry;
use maverick_domain::spend::SpendCaps;
use maverick_providers::models::ModelLister;
use maverick_runner::{Clients, ProviderLimits, RunnerConfig};
use maverick_store::health::GRAPHILE_WORKER_SCHEMA;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::batch::{BatchConfig, NightlySmokeJob, WeeklySweepJob};
use crate::catalog::{self, CatalogDeps, ModelsDevPriceSource, PriceSource};
use crate::discovery::{self, DiscoveryConfig};
use crate::predictions::{
    CalibrationConfig, CalibrationScoreJob, PredictionLockConfig, PredictionLockJob,
};
use crate::probes::{LatencyProbesJob, ProbeConfig};
use crate::rebuild::{self, AggregateRebuildJob, RebuildConfig};
use crate::regression_scan::{RegressionScanConfig, RegressionScanJob};
use crate::retention::{self, RetentionDeps};
use crate::{
    BatchLeaseManager, DEFAULT_QUEUE_NAME, DayArgs, HourArgs, JobClient, JobKind,
    WORKER_CONCURRENCY, scheduled_args, with_job_timeout,
};

/// The kinds that get a cron trigger, in schedule order: every scheduled kind. The
/// chained kinds (aggregate_rebuild, regression_scan, calibration_score) are enqueued by
/// the jobs before them.
pub const SCHEDULED: [JobKind; 10] = [
    JobKind::PredictionLock,
    JobKind::NightlySmoke,
    JobKind::WeeklySweep,
    JobKind::LatencyProbes,
    JobKind::PriceSync,
    JobKind::CatalogSync,
    JobKind::ModelDiscovery,
    JobKind::CaptureExpiry,
    JobKind::AffinityPrune,
    JobKind::HistoryPrune,
];

/// What the workers share with the rest of the process.
#[derive(Clone)]
pub struct WorkerDeps {
    /// The provider clients (batches and latency probes).
    pub clients: Clients,
    /// The grader registry, judge_rubric included when the judge pool resolved.
    pub graders: Arc<Registry>,
    /// The process-wide alert delivery.
    pub notifier: Option<Arc<dyn AlertNotifier>>,
    /// `None`: `price_sync` is a no-op.
    pub price_source: Option<Arc<dyn PriceSource>>,
    /// One model lister per provider with an operator key (spec §4.6). Empty:
    /// `model_discovery` reports every provider as `skipped_no_key`.
    pub model_listers: Vec<Arc<dyn ModelLister>>,
}

/// The configuration of the catalog tasks.
#[derive(Clone)]
pub struct CatalogConfig {
    pub role: maverick_config::Role,
    pub notifier: Option<Arc<dyn AlertNotifier>>,
    pub price_source: Option<Arc<dyn PriceSource>>,
}

impl std::fmt::Debug for CatalogConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CatalogConfig")
            .field("role", &self.role)
            .field("notifier", &self.notifier.is_some())
            .field("price_source", &self.price_source.is_some())
            .finish()
    }
}

/// The configuration of the retention tasks.
#[derive(Clone)]
pub struct RetentionConfig {
    pub notifier: Option<Arc<dyn AlertNotifier>>,
    pub capture_retention: Duration,
    pub history_retention_days: u32,
}

impl std::fmt::Debug for RetentionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetentionConfig")
            .field("notifier", &self.notifier.is_some())
            .field("capture_retention", &self.capture_retention)
            .field("history_retention_days", &self.history_retention_days)
            .finish()
    }
}

/// The runner settings from `MAVERICK_RUNNER_*` and `MAVERICK_PROVIDERS_*`.
pub fn runner_config(config: &maverick_config::Config) -> RunnerConfig {
    let limits = [
        ("anthropic", config.provider_limits.anthropic),
        ("openai", config.provider_limits.openai),
        ("google", config.provider_limits.google),
        ("xai", config.provider_limits.xai),
    ]
    .into_iter()
    .map(|(provider, limit)| {
        (
            provider.to_owned(),
            ProviderLimits {
                max_concurrency: usize::try_from(limit.max_concurrency).unwrap_or(usize::MAX),
                rpm_limit: i64::from(limit.rpm_limit),
            },
        )
    })
    .collect();
    RunnerConfig {
        repeats: config.runner_repeats,
        timeout: config.runner_timeout,
        limits,
    }
}

/// The worker options for every implemented task, its extension, and the cron triggers.
/// The caller adds the pool (or database URL) and the shutdown signal, then `init`s.
pub fn worker_options(
    client: &JobClient,
    config: &maverick_config::Config,
    deps: WorkerDeps,
) -> WorkerOptions {
    let lease = BatchLeaseManager::from_job_client_config(client.config());
    let batch = BatchConfig {
        repeats: config.runner_repeats,
        caps: SpendCaps {
            nightly_usd: config.spend_nightly_cap_usd,
            weekly_usd: config.spend_weekly_cap_usd,
        },
        lease: lease.clone(),
        runner: runner_config(config),
        clients: deps.clients.clone(),
        graders: deps.graders,
        notifier: deps.notifier.clone(),
    };
    let probes = ProbeConfig {
        clients: deps.clients,
        runner_timeout: config.runner_timeout,
        alpha: config.scoring_alpha,
        min_nights: u64::from(config.scoring_min_nights),
    };
    let regression = RegressionScanConfig {
        alpha: config.scoring_alpha,
        lease: lease.clone(),
        notifier: deps.notifier.clone(),
    };
    let price_source = if client.config().registers(JobKind::PriceSync) {
        deps.price_source.or_else(|| {
            ModelsDevPriceSource::from_config(config)
                .map(|source| Arc::new(source) as Arc<dyn PriceSource>)
        })
    } else {
        None
    };
    let catalog = CatalogConfig {
        role: config.role,
        notifier: deps.notifier.clone(),
        price_source,
    };
    let discovery = DiscoveryConfig {
        lease: lease.clone(),
        notifier: deps.notifier.clone(),
        listers: deps.model_listers,
    };
    let retention = RetentionConfig {
        notifier: deps.notifier,
        capture_retention: Duration::from_secs(u64::from(config.capture_retention_hours) * 3600),
        history_retention_days: config.history_retention_days,
    };
    let options = WorkerOptions::default()
        .schema(GRAPHILE_WORKER_SCHEMA)
        .concurrency(WORKER_CONCURRENCY)
        .worker_recovery(client.recovery_config())
        .listen_os_shutdown_signals(false)
        // The per-kind handler timeouts (`with_job_timeout`) read the job client config.
        .add_extension(client.config().clone())
        .add_extension(batch)
        .add_extension(PredictionLockConfig::from_config(config, lease.clone()))
        .add_extension(CalibrationConfig {
            lease: lease.clone(),
        })
        .add_extension(regression)
        .add_extension(RebuildConfig::from_config(config, lease))
        .add_extension(probes)
        .add_extension(catalog)
        .add_extension(discovery)
        .add_extension(retention)
        .define_job::<LatencyProbesJob>()
        .define_job::<CatalogSyncJob>()
        .define_job::<CaptureExpiryJob>()
        .define_job::<AffinityPruneJob>()
        .define_job::<HistoryPruneJob>()
        .define_job::<CronLatencyProbes>()
        .define_job::<CronCatalogSync>()
        .define_job::<CronCaptureExpiry>()
        .define_job::<CronAffinityPrune>()
        .define_job::<CronHistoryPrune>();
    let options = if config.role == maverick_config::Role::Origin {
        options
            .define_job::<PredictionLockJob>()
            .define_job::<NightlySmokeJob>()
            .define_job::<WeeklySweepJob>()
            .define_job::<AggregateRebuildJob>()
            .define_job::<RegressionScanJob>()
            .define_job::<CalibrationScoreJob>()
            .define_job::<ModelDiscoveryJob>()
            .define_job::<CronPredictionLock>()
            .define_job::<CronNightlySmoke>()
            .define_job::<CronWeeklySweep>()
            .define_job::<CronModelDiscovery>()
    } else {
        options
    };
    let options = if client.config().registers(JobKind::PriceSync) {
        options
            .define_job::<PriceSyncJob>()
            .define_job::<CronPriceSync>()
    } else {
        options
    };
    options.with_crons(cron_triggers(client))
}

/// One crontab per [`SCHEDULED`] kind, firing its `cron.<kind>` trigger with payload `{}`.
pub fn cron_triggers(client: &JobClient) -> Vec<Crontab> {
    client
        .schedules()
        .iter()
        .filter(|schedule| SCHEDULED.contains(&schedule.kind))
        .map(|schedule| {
            let mut crontab = Crontab::new(
                schedule.expression.to_graphile_timer(),
                trigger_identifier(schedule.kind),
            );
            crontab.options.id = Some(schedule.kind.as_str().to_owned());
            crontab.options.queue = Some(DEFAULT_QUEUE_NAME.to_owned());
            crontab.options.max = Some(5);
            crontab.options.job_key = Some(trigger_identifier(schedule.kind));
            crontab.options.job_key_mode = Some(CronJobKeyMode::Replace);
            crontab.payload = Some(serde_json::json!({}));
            crontab
        })
        .collect()
}

/// `cron.<kind>`.
pub fn trigger_identifier(kind: JobKind) -> String {
    format!("cron.{}", kind.as_str())
}

/// The `_cron` metadata graphile_worker adds to a cron firing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CronFiring {
    #[serde(rename = "_cron")]
    pub cron: CronMeta,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CronMeta {
    /// The scheduled time, RFC 3339.
    pub ts: String,
    #[serde(default)]
    pub backfilled: bool,
}

/// Enqueues the real job of `kind` for the window of the firing at `firing.cron.ts`.
pub async fn enqueue_scheduled(
    pool: &sqlx::PgPool,
    kind: JobKind,
    firing: &CronFiring,
) -> Result<(), String> {
    let at = DateTime::parse_from_rfc3339(&firing.cron.ts)
        .map_err(|error| {
            format!(
                "jobs: cron.{kind}: firing time {:?}: {error}",
                firing.cron.ts
            )
        })?
        .with_timezone(&Utc);
    let args = scheduled_args(kind, at);
    let mut conn = pool
        .acquire()
        .await
        .map_err(|error| format!("jobs: cron.{kind}: {error}"))?;
    rebuild::enqueue_in(&mut conn, &args)
        .await
        .map_err(|error| format!("jobs: cron.{kind}: {error}"))?;
    tracing::info!(
        kind = kind.as_str(),
        window = %args.window().unwrap_or_default(),
        backfilled = firing.cron.backfilled,
        "scheduled job enqueued"
    );
    Ok(())
}

macro_rules! cron_trigger {
    ($name:ident, $kind:expr, $identifier:literal) => {
        #[doc = concat!("The `", $identifier, "` cron trigger.")]
        #[derive(Clone, Debug, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub CronFiring);

        impl TaskHandler for $name {
            const IDENTIFIER: &'static str = $identifier;

            async fn run(self, ctx: WorkerContext) -> impl IntoTaskHandlerResult {
                // A trigger only enqueues, so it runs under the fast timeout.
                crate::run_with_timeout(
                    $kind,
                    crate::FAST_JOB_TIMEOUT,
                    enqueue_scheduled(ctx.pg_pool(), $kind, &self.0),
                )
                .await
            }
        }
    };
}

cron_trigger!(
    CronPredictionLock,
    JobKind::PredictionLock,
    "cron.prediction_lock"
);
cron_trigger!(
    CronNightlySmoke,
    JobKind::NightlySmoke,
    "cron.nightly_smoke"
);
cron_trigger!(CronWeeklySweep, JobKind::WeeklySweep, "cron.weekly_sweep");
cron_trigger!(
    CronLatencyProbes,
    JobKind::LatencyProbes,
    "cron.latency_probes"
);
cron_trigger!(CronPriceSync, JobKind::PriceSync, "cron.price_sync");
cron_trigger!(CronCatalogSync, JobKind::CatalogSync, "cron.catalog_sync");
cron_trigger!(
    CronModelDiscovery,
    JobKind::ModelDiscovery,
    "cron.model_discovery"
);
cron_trigger!(
    CronCaptureExpiry,
    JobKind::CaptureExpiry,
    "cron.capture_expiry"
);
cron_trigger!(
    CronAffinityPrune,
    JobKind::AffinityPrune,
    "cron.affinity_prune"
);
cron_trigger!(
    CronHistoryPrune,
    JobKind::HistoryPrune,
    "cron.history_prune"
);

fn catalog_deps<'a>(ctx: &'a WorkerContext, config: &'a CatalogConfig) -> CatalogDeps<'a> {
    CatalogDeps {
        pool: ctx.pg_pool(),
        notifier: config.notifier.as_deref(),
        price_source: config.price_source.as_deref(),
    }
}

fn retention_deps<'a>(ctx: &'a WorkerContext, config: &'a RetentionConfig) -> RetentionDeps<'a> {
    RetentionDeps {
        pool: ctx.pg_pool(),
        notifier: config.notifier.as_deref(),
        capture_retention: config.capture_retention,
        history_retention_days: config.history_retention_days,
    }
}

fn missing(kind: JobKind, what: &str) -> String {
    format!("jobs: {kind}: the worker has no {what}")
}

/// The `price_sync` task.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PriceSyncJob(pub DayArgs);

impl TaskHandler for PriceSyncJob {
    const IDENTIFIER: &'static str = "price_sync";

    async fn run(self, ctx: WorkerContext) -> impl IntoTaskHandlerResult {
        let Some(config) = ctx.extensions().get::<CatalogConfig>() else {
            return Err(missing(JobKind::PriceSync, "CatalogConfig"));
        };
        with_job_timeout(&ctx, JobKind::PriceSync, async {
            catalog::run_price_sync(&catalog_deps(&ctx, config), &self.0)
                .await
                .map(|_| ())
        })
        .await
    }
}

/// The `catalog_sync` task.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CatalogSyncJob(pub DayArgs);

impl TaskHandler for CatalogSyncJob {
    const IDENTIFIER: &'static str = "catalog_sync";

    async fn run(self, ctx: WorkerContext) -> impl IntoTaskHandlerResult {
        let Some(config) = ctx.extensions().get::<CatalogConfig>() else {
            return Err(missing(JobKind::CatalogSync, "CatalogConfig"));
        };
        with_job_timeout(&ctx, JobKind::CatalogSync, async {
            catalog::run_catalog_sync_for_role(
                &catalog_deps(&ctx, config),
                &self.0,
                Utc::now(),
                config.role,
            )
            .await
            .map(|_| ())
        })
        .await
    }
}

/// The `model_discovery` task (spec §4.6).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelDiscoveryJob(pub DayArgs);

impl TaskHandler for ModelDiscoveryJob {
    const IDENTIFIER: &'static str = "model_discovery";

    async fn run(self, ctx: WorkerContext) -> impl IntoTaskHandlerResult {
        let Some(config) = ctx.extensions().get::<DiscoveryConfig>() else {
            return Err(missing(JobKind::ModelDiscovery, "DiscoveryConfig"));
        };
        with_job_timeout(&ctx, JobKind::ModelDiscovery, async {
            discovery::model_discovery(ctx.pg_pool(), &self.0, config, ctx.worker_id())
                .await
                .map(|_| ())
        })
        .await
    }
}

/// The `capture_expiry` task.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CaptureExpiryJob(pub HourArgs);

impl TaskHandler for CaptureExpiryJob {
    const IDENTIFIER: &'static str = "capture_expiry";

    async fn run(self, ctx: WorkerContext) -> impl IntoTaskHandlerResult {
        let Some(config) = ctx.extensions().get::<RetentionConfig>() else {
            return Err(missing(JobKind::CaptureExpiry, "RetentionConfig"));
        };
        with_job_timeout(&ctx, JobKind::CaptureExpiry, async {
            retention::run_capture_expiry(&retention_deps(&ctx, config), &self.0, Utc::now())
                .await
                .map(|_| ())
        })
        .await
    }
}

/// The `affinity_prune` task (D44).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AffinityPruneJob(pub HourArgs);

impl TaskHandler for AffinityPruneJob {
    const IDENTIFIER: &'static str = "affinity_prune";

    async fn run(self, ctx: WorkerContext) -> impl IntoTaskHandlerResult {
        let Some(config) = ctx.extensions().get::<RetentionConfig>() else {
            return Err(missing(JobKind::AffinityPrune, "RetentionConfig"));
        };
        with_job_timeout(&ctx, JobKind::AffinityPrune, async {
            retention::run_affinity_prune(&retention_deps(&ctx, config), &self.0, Utc::now())
                .await
                .map(|_| ())
        })
        .await
    }
}

/// The `history_prune` task.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HistoryPruneJob(pub DayArgs);

impl TaskHandler for HistoryPruneJob {
    const IDENTIFIER: &'static str = "history_prune";

    async fn run(self, ctx: WorkerContext) -> impl IntoTaskHandlerResult {
        let Some(config) = ctx.extensions().get::<RetentionConfig>() else {
            return Err(missing(JobKind::HistoryPrune, "RetentionConfig"));
        };
        with_job_timeout(&ctx, JobKind::HistoryPrune, async {
            retention::run_history_prune(&retention_deps(&ctx, config), &self.0, Utc::now())
                .await
                .map(|_| ())
        })
        .await
    }
}

/// A started worker. [`RunningWorker::stop`] signals it and waits for the drain.
pub struct RunningWorker {
    stop: Option<oneshot::Sender<()>>,
    handle: JoinHandle<Result<(), String>>,
}

/// Initializes the worker from `options` (the pool must already be set) and runs it on a
/// task. It stops taking jobs when [`RunningWorker::stop`] is called; in-flight jobs get
/// the options' shutdown grace period.
pub async fn start(options: WorkerOptions) -> Result<RunningWorker, String> {
    let (stop, stopped) = oneshot::channel::<()>();
    let worker = options
        .shutdown_signal(async move {
            let _ = stopped.await;
        })
        .init()
        .await
        .map_err(|error| error.to_string())?;
    let handle = tokio::spawn(async move { worker.run().await.map_err(|e| e.to_string()) });
    Ok(RunningWorker {
        stop: Some(stop),
        handle,
    })
}

impl RunningWorker {
    /// Signals shutdown and waits for the worker to drain, at most `within`. On a timeout
    /// the worker task is aborted; its unfinished jobs are retried after graphile's
    /// recovery, and an unfinished batch resumes under its lease (D10).
    pub async fn stop(mut self, within: Duration) -> Result<(), String> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        match tokio::time::timeout(within, &mut self.handle).await {
            Ok(Ok(result)) => result,
            Ok(Err(join)) => Err(join.to_string()),
            Err(_) => {
                self.handle.abort();
                Err(format!("workers did not stop within {within:?}"))
            }
        }
    }

    /// Resolves when the worker task ends on its own (a runtime error).
    pub async fn finished(&mut self) -> Result<(), String> {
        match (&mut self.handle).await {
            Ok(result) => result,
            Err(join) => Err(join.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triggers_cover_the_scheduled_kinds_with_an_empty_payload() {
        let client = JobClient::new(crate::JobClientConfig::default()).expect("client");
        let crons = cron_triggers(&client);
        assert_eq!(crons.len(), SCHEDULED.len());
        for (crontab, kind) in crons.iter().zip(SCHEDULED) {
            let trigger = trigger_identifier(kind);
            assert_eq!(crontab.task_identifier, trigger);
            // The crontab id stays the kind, so graphile's backfill bookkeeping carries over.
            assert_eq!(crontab.identifier(), kind.as_str());
            assert_eq!(crontab.payload, Some(serde_json::json!({})));
            assert_eq!(crontab.options.job_key.as_deref(), Some(trigger.as_str()));
        }
        // Every schedule gets its trigger.
        assert_eq!(crons.len(), client.schedules().len());
    }

    #[test]
    fn cron_firing_reads_graphile_metadata() {
        let firing: CronFiring = serde_json::from_value(serde_json::json!({
            "_cron": {"ts": "2031-01-02T03:00:00Z", "backfilled": false}
        }))
        .expect("firing");
        assert_eq!(firing.cron.ts, "2031-01-02T03:00:00Z");
    }
}
