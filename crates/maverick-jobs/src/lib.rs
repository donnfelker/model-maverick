//! Background jobs on graphile_worker.
//!
//! This crate owns the queue-facing contract: job kind names, argument JSON,
//! schedule validation, queue keys, retry attempts, and per-kind timeouts.

pub mod batch;
pub mod catalog;
pub mod fold;
pub mod predictions;
pub mod probes;
pub mod rebuild;
pub mod regression_scan;
pub mod retention;
pub mod spend_guard;
pub mod worker;

use chrono::{
    DateTime, Datelike, Duration as ChronoDuration, Months, NaiveDate, NaiveDateTime, TimeZone,
    Timelike, Utc,
};
use graphile_worker::{
    CronJobKeyMode, Crontab, CrontabTimer, CrontabValue, JobKeyMode, JobSpec, WorkerContext,
    WorkerRecoveryConfig,
};
use maverick_store::{
    batches::{self, BatchClaim, ClaimOutcome},
    health::{self, GRAPHILE_WORKER_SCHEMA, WorkerHeartbeat},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fmt;
use std::future::Future;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Duration;
use ulid::Ulid;

pub use maverick_store::batches::{Batch, BatchEnd};

pub const DEFAULT_QUEUE_NAME: &str = "default";
pub const WORKER_CONCURRENCY: usize = 10;
pub const FAST_JOB_TIMEOUT: Duration = Duration::from_secs(60);
pub const DEFAULT_ANALYTICS_JOB_TIMEOUT: Duration = Duration::from_secs(15 * 60);
pub const DEFAULT_BATCH_JOB_TIMEOUT: Duration = Duration::from_secs(6 * 60 * 60);
pub const DEFAULT_BATCH_LEASE_TTL: Duration = Duration::from_secs(60);
pub const DEFAULT_BATCH_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
pub const GRAPHILE_RECOVERY_SWEEP_THRESHOLD: Duration = Duration::from_secs(5 * 60);
const BATCH_ID_DOMAIN: &[u8] = b"model-maverick/batch-id/v1";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum JobKind {
    PredictionLock,
    NightlySmoke,
    WeeklySweep,
    AggregateRebuild,
    RegressionScan,
    CalibrationScore,
    LatencyProbes,
    PriceSync,
    CatalogSync,
    CaptureExpiry,
    HistoryPrune,
}

impl JobKind {
    pub const ALL: [Self; 11] = [
        Self::PredictionLock,
        Self::NightlySmoke,
        Self::WeeklySweep,
        Self::AggregateRebuild,
        Self::RegressionScan,
        Self::CalibrationScore,
        Self::LatencyProbes,
        Self::PriceSync,
        Self::CatalogSync,
        Self::CaptureExpiry,
        Self::HistoryPrune,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::PredictionLock => "prediction_lock",
            Self::NightlySmoke => "nightly_smoke",
            Self::WeeklySweep => "weekly_sweep",
            Self::AggregateRebuild => "aggregate_rebuild",
            Self::RegressionScan => "regression_scan",
            Self::CalibrationScore => "calibration_score",
            Self::LatencyProbes => "latency_probes",
            Self::PriceSync => "price_sync",
            Self::CatalogSync => "catalog_sync",
            Self::CaptureExpiry => "capture_expiry",
            Self::HistoryPrune => "history_prune",
        }
    }

    pub fn is_batch(self) -> bool {
        matches!(self, Self::NightlySmoke | Self::WeeklySweep)
    }

    pub fn is_chained_or_hourly(self) -> bool {
        matches!(
            self,
            Self::AggregateRebuild
                | Self::RegressionScan
                | Self::CalibrationScore
                | Self::LatencyProbes
        )
    }

    pub fn max_attempts(self) -> i16 {
        if self.is_batch() { 3 } else { 5 }
    }

    pub fn timeout(self, config: &JobClientConfig) -> Duration {
        if self.is_batch() {
            config.batch_job_timeout
        } else if self.is_chained_or_hourly() {
            config.analytics_job_timeout
        } else {
            FAST_JOB_TIMEOUT
        }
    }

    pub fn job_key(self, window: &str) -> String {
        format!("{}:{window}", self.as_str())
    }

    pub fn job_spec(self, window: &str) -> JobSpec {
        JobSpec::builder()
            .queue_name(DEFAULT_QUEUE_NAME)
            .max_attempts(self.max_attempts())
            .job_key(self.job_key(window))
            .job_key_mode(JobKeyMode::PreserveRunAt)
            .build()
    }
}

impl fmt::Display for JobKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PredictionLockArgs {
    pub night: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NightlySmokeArgs {
    pub night: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WeeklySweepArgs {
    pub week: String,
    pub night: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BatchFollowupArgs {
    pub night: String,
    pub batch_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HourArgs {
    pub hour: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DayArgs {
    pub day: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum JobArgs {
    PredictionLock(PredictionLockArgs),
    NightlySmoke(NightlySmokeArgs),
    WeeklySweep(WeeklySweepArgs),
    AggregateRebuild(BatchFollowupArgs),
    RegressionScan(BatchFollowupArgs),
    CalibrationScore(BatchFollowupArgs),
    LatencyProbes(HourArgs),
    PriceSync(DayArgs),
    CatalogSync(DayArgs),
    CaptureExpiry(HourArgs),
    HistoryPrune(DayArgs),
}

impl JobArgs {
    pub fn kind(&self) -> JobKind {
        match self {
            Self::PredictionLock(_) => JobKind::PredictionLock,
            Self::NightlySmoke(_) => JobKind::NightlySmoke,
            Self::WeeklySweep(_) => JobKind::WeeklySweep,
            Self::AggregateRebuild(_) => JobKind::AggregateRebuild,
            Self::RegressionScan(_) => JobKind::RegressionScan,
            Self::CalibrationScore(_) => JobKind::CalibrationScore,
            Self::LatencyProbes(_) => JobKind::LatencyProbes,
            Self::PriceSync(_) => JobKind::PriceSync,
            Self::CatalogSync(_) => JobKind::CatalogSync,
            Self::CaptureExpiry(_) => JobKind::CaptureExpiry,
            Self::HistoryPrune(_) => JobKind::HistoryPrune,
        }
    }

    pub fn to_value(&self) -> Value {
        match self {
            Self::PredictionLock(args) => json!({ "night": args.night }),
            Self::NightlySmoke(args) => json!({ "night": args.night }),
            Self::WeeklySweep(args) => json!({ "week": args.week, "night": args.night }),
            Self::AggregateRebuild(args)
            | Self::RegressionScan(args)
            | Self::CalibrationScore(args) => {
                json!({ "night": args.night, "batch_id": args.batch_id })
            }
            Self::LatencyProbes(args) | Self::CaptureExpiry(args) => {
                json!({ "hour": args.hour })
            }
            Self::PriceSync(args) | Self::CatalogSync(args) | Self::HistoryPrune(args) => {
                json!({ "day": args.day })
            }
        }
    }

    pub fn window(&self) -> Result<String, JobsError> {
        match self {
            Self::PredictionLock(args) => {
                parse_night(self.kind(), &args.night)?;
                Ok(args.night.clone())
            }
            Self::NightlySmoke(args) => {
                parse_night(self.kind(), &args.night)?;
                Ok(args.night.clone())
            }
            Self::WeeklySweep(args) => {
                parse_night(self.kind(), &args.night)?;
                parse_week(self.kind(), &args.week)?;
                Ok(args.week.clone())
            }
            Self::AggregateRebuild(args)
            | Self::RegressionScan(args)
            | Self::CalibrationScore(args) => {
                parse_night(self.kind(), &args.night)?;
                Ok(args.batch_id.clone())
            }
            Self::LatencyProbes(args) | Self::CaptureExpiry(args) => {
                parse_hour(self.kind(), &args.hour)?;
                Ok(args.hour.clone())
            }
            Self::PriceSync(args) | Self::CatalogSync(args) | Self::HistoryPrune(args) => {
                parse_day(self.kind(), &args.day)?;
                Ok(args.day.clone())
            }
        }
    }

    pub fn job_spec(&self) -> Result<JobSpec, JobsError> {
        Ok(self.kind().job_spec(&self.window()?))
    }
}

/// Runs one delivery under the explicit per-kind `tokio::time::timeout` of master plan
/// §3.1 (graphile_worker has no job timeout). An overrun drops the handler, which stops
/// its runs and its lease heartbeat, and fails the job. graphile_worker retries it on
/// the same worker, and the retry resumes under the same lease. The limits come from
/// the worker's [`JobClientConfig`] extension, or the defaults without one.
pub async fn with_job_timeout<E: fmt::Display>(
    ctx: &WorkerContext,
    kind: JobKind,
    work: impl Future<Output = Result<(), E>>,
) -> Result<(), String> {
    let limit = match ctx.extensions().get::<JobClientConfig>() {
        Some(config) => kind.timeout(config),
        None => kind.timeout(&JobClientConfig::default()),
    };
    run_with_timeout(kind, limit, work).await
}

/// The core of [`with_job_timeout`].
pub async fn run_with_timeout<E: fmt::Display>(
    kind: JobKind,
    limit: Duration,
    work: impl Future<Output = Result<(), E>>,
) -> Result<(), String> {
    match tokio::time::timeout(limit, work).await {
        Ok(result) => result.map_err(|e| e.to_string()),
        Err(_) => {
            tracing::warn!(
                job = kind.as_str(),
                timeout_ms = u64::try_from(limit.as_millis()).unwrap_or(u64::MAX),
                "job handler timed out"
            );
            Err(format!("jobs: {kind}: handler timed out after {limit:?}"))
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JobClientConfig {
    pub batch_job_timeout: Duration,
    pub batch_lease_ttl: Duration,
    pub batch_heartbeat_interval: Duration,
    pub analytics_job_timeout: Duration,
    pub cron_prediction_lock: String,
    pub cron_nightly: String,
    pub cron_weekly: String,
    pub cron_latency_probes: String,
    pub cron_price_sync: String,
    pub cron_catalog_sync: String,
    pub cron_capture_expiry: String,
    pub cron_history_prune: String,
    pub recovery_sweep_threshold: Duration,
}

impl Default for JobClientConfig {
    fn default() -> Self {
        Self {
            batch_job_timeout: DEFAULT_BATCH_JOB_TIMEOUT,
            batch_lease_ttl: DEFAULT_BATCH_LEASE_TTL,
            batch_heartbeat_interval: DEFAULT_BATCH_HEARTBEAT_INTERVAL,
            analytics_job_timeout: DEFAULT_ANALYTICS_JOB_TIMEOUT,
            cron_prediction_lock: "50 2 * * *".to_string(),
            cron_nightly: "0 3 * * *".to_string(),
            cron_weekly: "0 4 * * 0".to_string(),
            cron_latency_probes: "0 * * * *".to_string(),
            cron_price_sync: "0 6 * * *".to_string(),
            cron_catalog_sync: "10 6 * * *".to_string(),
            cron_capture_expiry: "0 * * * *".to_string(),
            cron_history_prune: "0 5 * * *".to_string(),
            recovery_sweep_threshold: GRAPHILE_RECOVERY_SWEEP_THRESHOLD,
        }
    }
}

impl From<&maverick_config::Config> for JobClientConfig {
    fn from(config: &maverick_config::Config) -> Self {
        Self {
            batch_job_timeout: config.batch_job_timeout,
            batch_lease_ttl: config.batch_lease_ttl,
            batch_heartbeat_interval: config.batch_heartbeat_interval,
            analytics_job_timeout: config.analytics_job_timeout,
            cron_prediction_lock: config.cron_prediction_lock.clone(),
            cron_nightly: config.cron_nightly.clone(),
            cron_weekly: config.cron_weekly.clone(),
            cron_latency_probes: config.cron_latency_probes.clone(),
            cron_price_sync: config.cron_price_sync.clone(),
            cron_catalog_sync: config.cron_catalog_sync.clone(),
            cron_capture_expiry: config.cron_capture_expiry.clone(),
            cron_history_prune: config.cron_history_prune.clone(),
            recovery_sweep_threshold: GRAPHILE_RECOVERY_SWEEP_THRESHOLD,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SchedulePlan {
    pub env_var: &'static str,
    pub kind: JobKind,
    pub expression: CronExpr,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScheduledJob {
    pub env_var: &'static str,
    pub kind: JobKind,
    pub expression: String,
    pub args: JobArgs,
    pub job_key: String,
    pub max_attempts: i16,
    pub timeout: Duration,
    pub next_run: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub struct JobClient {
    config: JobClientConfig,
    schedules: Vec<SchedulePlan>,
}

impl JobClient {
    pub fn new(config: JobClientConfig) -> Result<Self, JobsError> {
        validate_batch_lease_ttl(&config)?;
        let schedules = validate_schedules(&config)?;
        Ok(Self { config, schedules })
    }

    pub fn from_config(config: &maverick_config::Config) -> Result<Self, JobsError> {
        Self::new(JobClientConfig::from(config))
    }

    pub fn config(&self) -> &JobClientConfig {
        &self.config
    }

    pub fn schedules(&self) -> &[SchedulePlan] {
        &self.schedules
    }

    pub fn recovery_config(&self) -> WorkerRecoveryConfig {
        WorkerRecoveryConfig::default()
            .heartbeat_interval(self.config.batch_heartbeat_interval)
            .sweep_threshold(self.config.recovery_sweep_threshold)
    }

    pub fn scheduled_jobs_after(
        &self,
        from: DateTime<Utc>,
    ) -> Result<Vec<ScheduledJob>, JobsError> {
        self.schedules
            .iter()
            .map(|schedule| {
                let next_run = schedule.expression.next_after(from);
                let args = scheduled_args(schedule.kind, next_run);
                let window = args.window()?;
                Ok(ScheduledJob {
                    env_var: schedule.env_var,
                    kind: schedule.kind,
                    expression: schedule.expression.as_str().to_string(),
                    job_key: schedule.kind.job_key(&window),
                    max_attempts: schedule.kind.max_attempts(),
                    timeout: schedule.kind.timeout(&self.config),
                    args,
                    next_run,
                })
            })
            .collect()
    }

    pub fn crontabs_after(&self, from: DateTime<Utc>) -> Result<Vec<Crontab>, JobsError> {
        self.scheduled_jobs_after(from)?
            .into_iter()
            .map(|job| {
                let schedule = self
                    .schedules
                    .iter()
                    .find(|schedule| schedule.env_var == job.env_var)
                    .expect("scheduled job came from an existing schedule");
                Ok(graphile_crontab(schedule, &job))
            })
            .collect()
    }
}

/// Runtime settings for the engine-owned `batches` lease.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatchLeaseConfig {
    pub lease_ttl: Duration,
    pub heartbeat_interval: Duration,
    pub owner_stale_after: Duration,
    pub graphile_schema: String,
}

impl Default for BatchLeaseConfig {
    fn default() -> Self {
        Self {
            lease_ttl: DEFAULT_BATCH_LEASE_TTL,
            heartbeat_interval: DEFAULT_BATCH_HEARTBEAT_INTERVAL,
            owner_stale_after: GRAPHILE_RECOVERY_SWEEP_THRESHOLD,
            graphile_schema: GRAPHILE_WORKER_SCHEMA.to_string(),
        }
    }
}

impl From<&JobClientConfig> for BatchLeaseConfig {
    fn from(config: &JobClientConfig) -> Self {
        Self {
            lease_ttl: config.batch_lease_ttl,
            heartbeat_interval: config.batch_heartbeat_interval,
            owner_stale_after: config.recovery_sweep_threshold,
            graphile_schema: GRAPHILE_WORKER_SCHEMA.to_string(),
        }
    }
}

impl BatchLeaseConfig {
    fn lease_ttl_secs(&self) -> f64 {
        self.lease_ttl.as_secs_f64()
    }
}

/// Graphile-aware wrapper around the store `batches` lease.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatchLeaseManager {
    config: BatchLeaseConfig,
}

impl BatchLeaseManager {
    pub fn new(config: BatchLeaseConfig) -> Self {
        Self { config }
    }

    pub fn from_job_client_config(config: &JobClientConfig) -> Self {
        Self::new(BatchLeaseConfig::from(config))
    }

    pub fn config(&self) -> &BatchLeaseConfig {
        &self.config
    }

    /// Interval a handler should use between heartbeat renewals.
    pub fn heartbeat_interval(&self) -> Duration {
        self.config.heartbeat_interval
    }

    /// Claims the lease for a job kind and window.
    ///
    /// The batch id is deterministic: `bat_` plus the Crockford base32 ULID text
    /// for the first 128 bits of SHA-256 over
    /// `model-maverick/batch-id/v1`, the job kind name, and the window. The
    /// zero byte separators keep the derivation stable and unambiguous.
    pub async fn claim(
        &self,
        pool: &sqlx::PgPool,
        kind: JobKind,
        window: &str,
        worker_id: &str,
    ) -> Result<BatchLeaseOutcome, BatchLeaseError> {
        let id = deterministic_batch_id(kind, window);
        let Some(live) = LiveLease::acquire(worker_id, &id) else {
            // A live handler in this process holds the window, so this delivery is a
            // duplicate on the same worker id.
            let batch = batches::get_batch_by_id(pool, &id).await?;
            return Ok(if batch.status == "running" {
                BatchLeaseOutcome::HeldByOther(batch)
            } else {
                BatchLeaseOutcome::Finished(batch)
            });
        };
        // Registry read failures fail closed: live foreign owners stay no-op until
        // expiry instead of burning graphile max_attempts.
        let workers = health::list_worker_heartbeats(pool, &self.config.graphile_schema)
            .await
            .ok();
        let request = BatchClaim {
            id: &id,
            kind: kind.as_str(),
            batch_window: window,
            worker_id,
            lease_ttl_secs: self.config.lease_ttl_secs(),
        };
        let stale_after = self.config.owner_stale_after;
        let mut conn = pool.acquire().await?;
        let outcome = batches::claim_batch(&mut conn, &request, |batch| {
            workers
                .as_deref()
                .is_some_and(|workers| batch_owner_is_gone(batch, workers, stale_after))
        })
        .await?;
        Ok(BatchLeaseOutcome::from_store(
            outcome,
            kind,
            worker_id,
            self.config.lease_ttl_secs(),
            live,
        ))
    }
}

/// The leases that live handlers in this process hold, as (worker id, batch id).
///
/// Every delivery on one graphile_worker worker shares its worker id. The store alone
/// cannot tell a retry after this worker dropped its handler (a timeout, a panic) from a
/// second delivery while the first still runs (a recovery sweep that took a stalled but
/// live worker for dead). The first may take the lease over; the second must not. A
/// dropped handler drops its [`BatchLease`] and so releases its entry.
static LIVE_LEASES: LazyLock<Mutex<HashSet<(String, String)>>> = LazyLock::new(Mutex::default);

#[derive(Debug, PartialEq, Eq)]
struct LiveLease {
    worker_id: String,
    batch_id: String,
}

impl LiveLease {
    fn acquire(worker_id: &str, batch_id: &str) -> Option<Self> {
        let key = (worker_id.to_owned(), batch_id.to_owned());
        let inserted = LIVE_LEASES
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key);
        inserted.then(|| LiveLease {
            worker_id: worker_id.to_owned(),
            batch_id: batch_id.to_owned(),
        })
    }
}

impl Drop for LiveLease {
    fn drop(&mut self) {
        LIVE_LEASES
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&(
                std::mem::take(&mut self.worker_id),
                std::mem::take(&mut self.batch_id),
            ));
    }
}

/// A held `batches` lease.
#[derive(Clone, Debug, PartialEq)]
pub struct BatchLease {
    batch: Batch,
    kind: JobKind,
    worker_id: String,
    lease_ttl_secs: f64,
    /// This process's claim on the lease; released when the last clone drops.
    live: Arc<LiveLease>,
}

impl BatchLease {
    fn new(
        batch: Batch,
        kind: JobKind,
        worker_id: &str,
        lease_ttl_secs: f64,
        live: LiveLease,
    ) -> Self {
        Self {
            batch,
            kind,
            worker_id: worker_id.to_string(),
            lease_ttl_secs,
            live: Arc::new(live),
        }
    }

    pub fn batch(&self) -> &Batch {
        &self.batch
    }

    pub fn id(&self) -> &str {
        &self.batch.id
    }

    pub fn kind(&self) -> JobKind {
        self.kind
    }

    pub fn worker_id(&self) -> &str {
        &self.worker_id
    }

    pub async fn heartbeat(&mut self, pool: &sqlx::PgPool) -> Result<&Batch, BatchLeaseError> {
        let Some(batch) =
            batches::heartbeat_batch(pool, &self.batch.id, &self.worker_id, self.lease_ttl_secs)
                .await?
        else {
            return Err(self.lease_lost());
        };
        self.batch = batch;
        Ok(&self.batch)
    }

    pub async fn finish(
        self,
        pool: &sqlx::PgPool,
        end: BatchEnd,
    ) -> Result<Batch, BatchLeaseError> {
        let Some(batch) = batches::finish_batch(pool, &self.batch.id, &self.worker_id, end).await?
        else {
            return Err(self.lease_lost());
        };
        Ok(batch)
    }

    fn lease_lost(&self) -> BatchLeaseError {
        BatchLeaseError::LeaseLost {
            kind: self.kind,
            batch_id: self.batch.id.clone(),
            worker_id: self.worker_id.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum BatchLeaseOutcome {
    Claimed(BatchLease),
    TookOver(BatchLease),
    Finished(Batch),
    HeldByOther(Batch),
}

impl BatchLeaseOutcome {
    pub fn batch(&self) -> &Batch {
        match self {
            Self::Claimed(lease) | Self::TookOver(lease) => lease.batch(),
            Self::Finished(batch) | Self::HeldByOther(batch) => batch,
        }
    }

    pub fn holds_lease(&self) -> bool {
        matches!(self, Self::Claimed(_) | Self::TookOver(_))
    }

    pub fn lease(self) -> Option<BatchLease> {
        match self {
            Self::Claimed(lease) | Self::TookOver(lease) => Some(lease),
            Self::Finished(_) | Self::HeldByOther(_) => None,
        }
    }

    fn from_store(
        outcome: ClaimOutcome,
        kind: JobKind,
        worker_id: &str,
        lease_ttl_secs: f64,
        live: LiveLease,
    ) -> Self {
        match outcome {
            ClaimOutcome::Claimed(batch) => Self::Claimed(BatchLease::new(
                batch,
                kind,
                worker_id,
                lease_ttl_secs,
                live,
            )),
            ClaimOutcome::TookOver(batch) => Self::TookOver(BatchLease::new(
                batch,
                kind,
                worker_id,
                lease_ttl_secs,
                live,
            )),
            ClaimOutcome::Finished(batch) => Self::Finished(batch),
            ClaimOutcome::HeldByOther(batch) => Self::HeldByOther(batch),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BatchLeaseError {
    #[error(transparent)]
    Store(#[from] sqlx::Error),
    #[error("jobs: {kind}: batch lease {batch_id} was lost by worker {worker_id}")]
    LeaseLost {
        kind: JobKind,
        batch_id: String,
        worker_id: String,
    },
}

pub fn deterministic_batch_id(kind: JobKind, window: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(BATCH_ID_DOMAIN);
    hasher.update([0]);
    hasher.update(kind.as_str().as_bytes());
    hasher.update([0]);
    hasher.update(window.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    format!("bat_{}", Ulid::from(bytes))
}

fn batch_owner_is_gone(
    batch: &Batch,
    workers: &[WorkerHeartbeat],
    owner_stale_after: Duration,
) -> bool {
    let Some(owner) = batch.lease_owner.as_deref() else {
        return true;
    };
    workers
        .iter()
        .find(|worker| worker.id == owner)
        .is_none_or(|worker| worker.age_seconds > owner_stale_after.as_secs_f64())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum JobsError {
    InvalidCron {
        env_var: &'static str,
        source: CronError,
    },
    BatchLeaseTtl {
        ttl: Duration,
        recovery_threshold: Duration,
    },
    InvalidNight {
        kind: JobKind,
        value: String,
        source: String,
    },
    InvalidDay {
        kind: JobKind,
        value: String,
        source: String,
    },
    InvalidHour {
        kind: JobKind,
        value: String,
        source: String,
    },
    InvalidWeek {
        kind: JobKind,
        value: String,
    },
}

impl fmt::Display for JobsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCron { env_var, source } => {
                write!(formatter, "jobs: {env_var}: {source}")
            }
            Self::BatchLeaseTtl {
                ttl,
                recovery_threshold,
            } => write!(
                formatter,
                "jobs: MAVERICK_BATCH_LEASE_TTL {:?} must be shorter than graphile_worker recovery threshold {:?}",
                ttl, recovery_threshold
            ),
            Self::InvalidNight {
                kind,
                value,
                source,
            } => write!(formatter, "jobs: {kind}: invalid night {value:?}: {source}"),
            Self::InvalidDay {
                kind,
                value,
                source,
            } => write!(formatter, "jobs: {kind}: invalid day {value:?}: {source}"),
            Self::InvalidHour {
                kind,
                value,
                source,
            } => write!(formatter, "jobs: {kind}: invalid hour {value:?}: {source}"),
            Self::InvalidWeek { kind, value } => {
                write!(formatter, "jobs: {kind}: invalid week {value:?}")
            }
        }
    }
}

impl std::error::Error for JobsError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CronExpr {
    source: String,
    minutes: CronField,
    hours: CronField,
    days: CronField,
    months: CronField,
    dows: CronField,
}

impl CronExpr {
    pub fn parse(expression: &str) -> Result<Self, CronError> {
        parse_cron(expression)
    }

    pub fn as_str(&self) -> &str {
        &self.source
    }

    pub fn next_after(&self, from: DateTime<Utc>) -> DateTime<Utc> {
        let start = truncate_to_minute(from) + ChronoDuration::minutes(1);
        let horizon = add_years(start, 5);
        let mut cursor = start;
        while cursor < horizon {
            if self.matches(cursor) {
                return cursor;
            }
            cursor += ChronoDuration::minutes(1);
        }
        horizon
    }

    pub fn fires_on(&self, night: NaiveDate) -> bool {
        let start = Utc.from_utc_datetime(
            &night
                .and_hms_opt(0, 0, 0)
                .expect("midnight is valid for a NaiveDate"),
        ) - ChronoDuration::minutes(1);
        let end = Utc.from_utc_datetime(
            &(night + ChronoDuration::days(1))
                .and_hms_opt(0, 0, 0)
                .expect("midnight is valid for a NaiveDate"),
        );
        self.next_after(start) < end
    }

    pub fn matches(&self, at: DateTime<Utc>) -> bool {
        self.minutes.contains(at.minute())
            && self.hours.contains(at.hour())
            && self.months.contains(at.month())
            && self.matches_day(at.date_naive())
    }

    fn matches_day(&self, day: NaiveDate) -> bool {
        let day_of_month = self.days.contains(day.day());
        let day_of_week = self.dows.contains(day.weekday().num_days_from_sunday());
        match (self.days.is_star, self.dows.is_star) {
            (true, true) => true,
            (true, false) => day_of_week,
            (false, true) => day_of_month,
            (false, false) => day_of_month || day_of_week,
        }
    }

    fn to_graphile_timer(&self) -> CrontabTimer {
        CrontabTimer {
            minutes: self.minutes.to_graphile_values(0, None),
            hours: self.hours.to_graphile_values(0, None),
            days: self.days.to_graphile_values(1, None),
            months: self.months.to_graphile_values(1, None),
            dows: self.dows.to_graphile_values(0, Some(map_dow_to_graphile)),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CronError {
    message: String,
}

impl fmt::Display for CronError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CronError {}

pub fn parse_cron(expression: &str) -> Result<CronExpr, CronError> {
    let fields = expression.split_whitespace().collect::<Vec<_>>();
    if fields.len() != 5 {
        return cron_error(format!(
            "cron {expression:?}: want 5 fields, got {}",
            fields.len()
        ));
    }

    let minutes = parse_field(expression, 1, fields[0], 0, 59, false)?;
    let hours = parse_field(expression, 2, fields[1], 0, 23, false)?;
    let days = parse_field(expression, 3, fields[2], 1, 31, false)?;
    let months = parse_field(expression, 4, fields[3], 1, 12, false)?;
    let dows = parse_field(expression, 5, fields[4], 0, 7, true)?;

    Ok(CronExpr {
        source: expression.to_string(),
        minutes,
        hours,
        days,
        months,
        dows,
    })
}

pub fn cron_fires_on(expression: &str, night: NaiveDate) -> Result<bool, CronError> {
    Ok(parse_cron(expression)?.fires_on(night))
}

pub fn iso_week(day: NaiveDate) -> String {
    let week = day.iso_week();
    format!("{:04}-W{:02}", week.year(), week.week())
}

fn validate_schedules(config: &JobClientConfig) -> Result<Vec<SchedulePlan>, JobsError> {
    [
        (
            "MAVERICK_CRON_PREDICTION_LOCK",
            JobKind::PredictionLock,
            config.cron_prediction_lock.as_str(),
        ),
        (
            "MAVERICK_CRON_NIGHTLY",
            JobKind::NightlySmoke,
            config.cron_nightly.as_str(),
        ),
        (
            "MAVERICK_CRON_WEEKLY",
            JobKind::WeeklySweep,
            config.cron_weekly.as_str(),
        ),
        (
            "MAVERICK_CRON_LATENCY_PROBES",
            JobKind::LatencyProbes,
            config.cron_latency_probes.as_str(),
        ),
        (
            "MAVERICK_CRON_PRICE_SYNC",
            JobKind::PriceSync,
            config.cron_price_sync.as_str(),
        ),
        (
            "MAVERICK_CRON_CATALOG_SYNC",
            JobKind::CatalogSync,
            config.cron_catalog_sync.as_str(),
        ),
        (
            "MAVERICK_CRON_CAPTURE_EXPIRY",
            JobKind::CaptureExpiry,
            config.cron_capture_expiry.as_str(),
        ),
        (
            "MAVERICK_CRON_HISTORY_PRUNE",
            JobKind::HistoryPrune,
            config.cron_history_prune.as_str(),
        ),
    ]
    .into_iter()
    .map(|(env_var, kind, expression)| {
        let expression =
            parse_cron(expression).map_err(|source| JobsError::InvalidCron { env_var, source })?;
        Ok(SchedulePlan {
            env_var,
            kind,
            expression,
        })
    })
    .collect()
}

fn validate_batch_lease_ttl(config: &JobClientConfig) -> Result<(), JobsError> {
    if config.batch_lease_ttl < config.recovery_sweep_threshold {
        Ok(())
    } else {
        Err(JobsError::BatchLeaseTtl {
            ttl: config.batch_lease_ttl,
            recovery_threshold: config.recovery_sweep_threshold,
        })
    }
}

fn scheduled_args(kind: JobKind, at: DateTime<Utc>) -> JobArgs {
    let day = at.date_naive();
    let night = day.format("%Y-%m-%d").to_string();
    let hour = at.format("%Y-%m-%dT%H").to_string();
    match kind {
        JobKind::PredictionLock => JobArgs::PredictionLock(PredictionLockArgs { night }),
        JobKind::NightlySmoke => JobArgs::NightlySmoke(NightlySmokeArgs { night }),
        JobKind::WeeklySweep => JobArgs::WeeklySweep(WeeklySweepArgs {
            week: iso_week(day),
            night,
        }),
        JobKind::LatencyProbes => JobArgs::LatencyProbes(HourArgs { hour }),
        JobKind::PriceSync => JobArgs::PriceSync(DayArgs { day: night }),
        JobKind::CatalogSync => JobArgs::CatalogSync(DayArgs { day: night }),
        JobKind::CaptureExpiry => JobArgs::CaptureExpiry(HourArgs { hour }),
        JobKind::HistoryPrune => JobArgs::HistoryPrune(DayArgs { day: night }),
        JobKind::AggregateRebuild | JobKind::RegressionScan | JobKind::CalibrationScore => {
            unreachable!("chained jobs are enqueued by handlers, not by cron")
        }
    }
}

fn graphile_crontab(schedule: &SchedulePlan, job: &ScheduledJob) -> Crontab {
    let mut crontab = Crontab::new(
        schedule.expression.to_graphile_timer(),
        schedule.kind.as_str(),
    );
    crontab.options.id = Some(schedule.kind.as_str().to_string());
    crontab.options.max = Some(schedule.kind.max_attempts() as u16);
    crontab.options.queue = Some(DEFAULT_QUEUE_NAME.to_string());
    crontab.options.job_key = Some(job.job_key.clone());
    crontab.options.job_key_mode = Some(CronJobKeyMode::PreserveRunAt);
    crontab.payload = Some(job.args.to_value());
    crontab
}

fn parse_night(kind: JobKind, value: &str) -> Result<NaiveDate, JobsError> {
    NaiveDate::parse_from_str(value, "%Y-%m-%d").map_err(|source| JobsError::InvalidNight {
        kind,
        value: value.to_string(),
        source: source.to_string(),
    })
}

fn parse_day(kind: JobKind, value: &str) -> Result<NaiveDate, JobsError> {
    NaiveDate::parse_from_str(value, "%Y-%m-%d").map_err(|source| JobsError::InvalidDay {
        kind,
        value: value.to_string(),
        source: source.to_string(),
    })
}

fn parse_hour(kind: JobKind, value: &str) -> Result<NaiveDateTime, JobsError> {
    let invalid = |source: &str| JobsError::InvalidHour {
        kind,
        value: value.to_string(),
        source: source.to_string(),
    };

    let (date, hour) = value
        .split_once('T')
        .ok_or_else(|| invalid("input is not enough for unique date and time"))?;
    let date = NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map_err(|source| invalid(&source.to_string()))?;
    let hour = hour.parse::<u32>().map_err(|_| invalid("invalid hour"))?;
    date.and_hms_opt(hour, 0, 0)
        .ok_or_else(|| invalid("hour out of range"))
}

fn parse_week(kind: JobKind, value: &str) -> Result<(), JobsError> {
    let (year, week) = value
        .split_once("-W")
        .ok_or_else(|| JobsError::InvalidWeek {
            kind,
            value: value.to_string(),
        })?;
    let year = year.parse::<i32>();
    let week = week.parse::<u32>();
    match (year, week) {
        (Ok(_), Ok(1..=53)) => Ok(()),
        _ => Err(JobsError::InvalidWeek {
            kind,
            value: value.to_string(),
        }),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CronField {
    min: u32,
    max: u32,
    selected: Vec<bool>,
    is_star: bool,
}

impl CronField {
    fn contains(&self, value: u32) -> bool {
        value >= self.min && value <= self.max && self.selected[(value - self.min) as usize]
    }

    fn to_graphile_values(
        &self,
        any_step_offset: u32,
        map_number: Option<fn(u32) -> u32>,
    ) -> Vec<CrontabValue> {
        if self.is_star {
            return vec![CrontabValue::Any];
        }
        self.selected
            .iter()
            .enumerate()
            .filter(|(_, selected)| **selected)
            .map(|(index, _)| {
                let value = self.min + index as u32;
                CrontabValue::Number(map_number.map_or(value, |map| map(value)))
            })
            .collect::<Vec<_>>()
            .if_empty_then_any(any_step_offset)
    }
}

trait GraphileValuesExt {
    fn if_empty_then_any(self, _step_offset: u32) -> Self;
}

impl GraphileValuesExt for Vec<CrontabValue> {
    fn if_empty_then_any(self, _step_offset: u32) -> Self {
        if self.is_empty() {
            vec![CrontabValue::Any]
        } else {
            self
        }
    }
}

fn parse_field(
    expression: &str,
    field_index: usize,
    field: &str,
    min: u32,
    max: u32,
    fold_seven_to_zero: bool,
) -> Result<CronField, CronError> {
    let mut selected = vec![false; (max - min + 1) as usize];
    let mut is_star = false;
    for part in field.split(',') {
        let part_star = match apply_field_part(part, min, max, fold_seven_to_zero, &mut selected) {
            Ok(part_star) => part_star,
            Err(message) => return field_error(expression, field_index, message),
        };
        is_star = is_star || part_star;
    }

    if !selected.iter().any(|selected| *selected) {
        return field_error(expression, field_index, format!("empty field {field:?}"));
    }

    Ok(CronField {
        min,
        max,
        selected,
        is_star,
    })
}

fn apply_field_part(
    part: &str,
    min: u32,
    max: u32,
    fold_seven_to_zero: bool,
    selected: &mut [bool],
) -> Result<bool, String> {
    let (base, step) = if let Some((base, step)) = part.split_once('/') {
        let step = step
            .parse::<u32>()
            .ok()
            .filter(|step| *step > 0)
            .ok_or_else(|| format!("invalid step {part:?}"))?;
        (base, Some(step))
    } else {
        (part, None)
    };

    let (lo, hi, star_base) = if base == "*" {
        (min, max, true)
    } else if let Some((lo, hi)) = base.split_once('-') {
        let lo = lo
            .parse::<u32>()
            .map_err(|_| format!("invalid value {part:?}"))?;
        let hi = hi
            .parse::<u32>()
            .map_err(|_| format!("invalid range {part:?}"))?;
        (lo, hi, false)
    } else {
        let lo = base
            .parse::<u32>()
            .map_err(|_| format!("invalid value {part:?}"))?;
        let hi = if step.is_some() { max } else { lo };
        (lo, hi, false)
    };

    if lo < min || hi > max || lo > hi {
        return Err(format!("value out of range {min}-{max}: {part:?}"));
    }

    let step = step.unwrap_or(1);
    let mut value = lo;
    while value <= hi {
        let folded = if fold_seven_to_zero && value == 7 {
            0
        } else {
            value
        };
        selected[(folded - min) as usize] = true;
        value = match value.checked_add(step) {
            Some(next) => next,
            None => break,
        };
    }

    Ok(star_base && step == 1)
}

fn field_error<T>(expression: &str, field_index: usize, message: String) -> Result<T, CronError> {
    cron_error(format!(
        "cron {expression:?} field {field_index}: {message}"
    ))
}

fn cron_error<T>(message: String) -> Result<T, CronError> {
    Err(CronError { message })
}

fn truncate_to_minute(at: DateTime<Utc>) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(at.year(), at.month(), at.day(), at.hour(), at.minute(), 0)
        .single()
        .expect("existing UTC date minute should round-trip")
}

fn add_years(at: DateTime<Utc>, years: u32) -> DateTime<Utc> {
    at.checked_add_months(Months::new(years * 12))
        .unwrap_or_else(|| at + ChronoDuration::days(365 * i64::from(years)))
}

fn map_dow_to_graphile(value: u32) -> u32 {
    if value == 0 { 7 } else { value }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct CronFixture {
        parse_accept: ParseAccept,
        parse_reject: ParseReject,
        next: NextCases,
        fires_on: FiresOnCases,
        iso_week: IsoWeekCases,
    }

    #[derive(Debug, Deserialize)]
    struct ParseAccept {
        cases: Vec<ParseAcceptCase>,
    }

    #[derive(Debug, Deserialize)]
    struct ParseAcceptCase {
        expr: String,
        ok: bool,
    }

    #[derive(Debug, Deserialize)]
    struct ParseReject {
        cases: Vec<ParseRejectCase>,
    }

    #[derive(Debug, Deserialize)]
    struct ParseRejectCase {
        expr: String,
        ok: bool,
        error: String,
    }

    #[derive(Debug, Deserialize)]
    struct NextCases {
        cases: Vec<NextCase>,
    }

    #[derive(Debug, Deserialize)]
    struct NextCase {
        expr: String,
        from: String,
        expected: String,
    }

    #[derive(Debug, Deserialize)]
    struct FiresOnCases {
        cases: Vec<FiresOnCase>,
        invalid_expression_error: String,
    }

    #[derive(Debug, Deserialize)]
    struct FiresOnCase {
        expr: String,
        night: String,
        expected: bool,
    }

    #[derive(Debug, Deserialize)]
    struct IsoWeekCases {
        cases: Vec<IsoWeekCase>,
    }

    #[derive(Debug, Deserialize)]
    struct IsoWeekCase {
        day: String,
        expected: String,
    }

    fn cron_fixture() -> CronFixture {
        serde_json::from_str(include_str!(
            "../../../parity/fixtures/measurement-core/cron.json"
        ))
        .expect("cron fixture must be valid JSON")
    }

    #[test]
    fn parse_cron_accepts_config_defaults() {
        for case in cron_fixture().parse_accept.cases {
            assert!(case.ok);
            parse_cron(&case.expr).unwrap_or_else(|err| panic!("{}: {err}", case.expr));
        }
    }

    #[test]
    fn parse_cron_rejects_invalid_with_exact_errors() {
        for case in cron_fixture().parse_reject.cases {
            assert!(!case.ok);
            let err = parse_cron(&case.expr).expect_err("case should reject");
            assert_eq!(err.to_string(), case.error, "{}", case.expr);
        }
    }

    #[test]
    fn cron_next_matches_reference() {
        for case in cron_fixture().next.cases {
            let cron = parse_cron(&case.expr).expect("fixture cron should parse");
            let from = DateTime::parse_from_rfc3339(&case.from)
                .expect("fixture from timestamp")
                .with_timezone(&Utc);
            let expected = DateTime::parse_from_rfc3339(&case.expected)
                .expect("fixture expected timestamp")
                .with_timezone(&Utc);
            assert_eq!(cron.next_after(from), expected, "{}", case.expr);
        }
    }

    #[test]
    fn cron_fires_on_matches_reference() {
        let fixture = cron_fixture().fires_on;
        for case in fixture.cases {
            let night = NaiveDate::parse_from_str(&case.night, "%Y-%m-%d").unwrap();
            assert_eq!(
                cron_fires_on(&case.expr, night).expect("fixture cron should parse"),
                case.expected,
                "{} on {}",
                case.expr,
                case.night
            );
        }

        let err = cron_fires_on("not a cron", NaiveDate::from_ymd_opt(2030, 6, 2).unwrap())
            .expect_err("invalid cron should reject");
        assert_eq!(err.to_string(), fixture.invalid_expression_error);
    }

    #[test]
    fn iso_week_matches_reference() {
        for case in cron_fixture().iso_week.cases {
            let day = NaiveDate::parse_from_str(&case.day, "%Y-%m-%d").unwrap();
            assert_eq!(iso_week(day), case.expected, "{}", case.day);
        }
    }

    #[test]
    fn job_kinds_have_reference_names_attempts_and_timeouts() {
        let config = JobClientConfig::default();
        assert_eq!(JobKind::PredictionLock.as_str(), "prediction_lock");
        assert_eq!(JobKind::NightlySmoke.as_str(), "nightly_smoke");
        assert_eq!(JobKind::WeeklySweep.as_str(), "weekly_sweep");
        assert_eq!(JobKind::AggregateRebuild.as_str(), "aggregate_rebuild");
        assert_eq!(JobKind::RegressionScan.as_str(), "regression_scan");
        assert_eq!(JobKind::CalibrationScore.as_str(), "calibration_score");
        assert_eq!(JobKind::LatencyProbes.as_str(), "latency_probes");
        assert_eq!(JobKind::PriceSync.as_str(), "price_sync");
        assert_eq!(JobKind::CatalogSync.as_str(), "catalog_sync");
        assert_eq!(JobKind::CaptureExpiry.as_str(), "capture_expiry");
        assert_eq!(JobKind::HistoryPrune.as_str(), "history_prune");

        assert_eq!(JobKind::NightlySmoke.max_attempts(), 3);
        assert_eq!(JobKind::WeeklySweep.max_attempts(), 3);
        for kind in JobKind::ALL.into_iter().filter(|kind| !kind.is_batch()) {
            assert_eq!(kind.max_attempts(), 5, "{kind}");
        }

        assert_eq!(
            JobKind::NightlySmoke.timeout(&config),
            DEFAULT_BATCH_JOB_TIMEOUT
        );
        assert_eq!(
            JobKind::WeeklySweep.timeout(&config),
            DEFAULT_BATCH_JOB_TIMEOUT
        );
        assert_eq!(
            JobKind::AggregateRebuild.timeout(&config),
            DEFAULT_ANALYTICS_JOB_TIMEOUT
        );
        assert_eq!(
            JobKind::LatencyProbes.timeout(&config),
            DEFAULT_ANALYTICS_JOB_TIMEOUT
        );
        assert_eq!(JobKind::PredictionLock.timeout(&config), FAST_JOB_TIMEOUT);
        assert_eq!(JobKind::HistoryPrune.timeout(&config), FAST_JOB_TIMEOUT);
    }

    #[test]
    fn job_args_emit_exact_json_and_windows() {
        let weekly = JobArgs::WeeklySweep(WeeklySweepArgs {
            week: "2031-W10".to_string(),
            night: "2031-03-09".to_string(),
        });
        assert_eq!(
            weekly.to_value(),
            json!({ "week": "2031-W10", "night": "2031-03-09" })
        );
        assert_eq!(weekly.window().unwrap(), "2031-W10");
        assert_eq!(
            weekly.job_spec().unwrap().job_key(),
            &Some("weekly_sweep:2031-W10".to_string())
        );

        let rebuild = JobArgs::AggregateRebuild(BatchFollowupArgs {
            night: "2031-03-09".to_string(),
            batch_id: "bat_01H".to_string(),
        });
        assert_eq!(
            rebuild.to_value(),
            json!({ "night": "2031-03-09", "batch_id": "bat_01H" })
        );
        assert_eq!(rebuild.window().unwrap(), "bat_01H");
        assert_eq!(rebuild.job_spec().unwrap().max_attempts(), &Some(5));
    }

    #[test]
    fn bad_job_args_keep_reference_prefixes() {
        let args = JobArgs::NightlySmoke(NightlySmokeArgs {
            night: "not-a-day".to_string(),
        });
        assert!(
            args.window()
                .expect_err("bad night should reject")
                .to_string()
                .starts_with("jobs: nightly_smoke: invalid night \"not-a-day\":"),
        );

        let args = JobArgs::LatencyProbes(HourArgs {
            hour: "2030-06-02Txx".to_string(),
        });
        assert!(
            args.window()
                .expect_err("bad hour should reject")
                .to_string()
                .starts_with("jobs: latency_probes: invalid hour \"2030-06-02Txx\":"),
        );
    }

    #[test]
    fn job_client_validates_all_crons_and_lease_ttl() {
        let client = JobClient::new(JobClientConfig::default()).expect("default config is valid");
        assert_eq!(client.schedules().len(), 8);

        let mut config = JobClientConfig {
            cron_nightly: "99 99 * * *".to_string(),
            ..JobClientConfig::default()
        };
        let err = JobClient::new(config.clone()).expect_err("bad nightly cron should reject");
        assert_eq!(
            err.to_string(),
            "jobs: MAVERICK_CRON_NIGHTLY: cron \"99 99 * * *\" field 1: value out of range 0-59: \"99\""
        );

        config.cron_nightly = "0 3 * * *".to_string();
        config.batch_lease_ttl = GRAPHILE_RECOVERY_SWEEP_THRESHOLD;
        let err = JobClient::new(config).expect_err("ttl at threshold should reject");
        assert!(
            err.to_string()
                .starts_with("jobs: MAVERICK_BATCH_LEASE_TTL")
        );
    }

    #[test]
    fn scheduled_jobs_pin_args_keys_attempts_and_timeouts() {
        let client = JobClient::new(JobClientConfig::default()).expect("default config is valid");
        let from = Utc.with_ymd_and_hms(2030, 6, 1, 23, 0, 0).unwrap();
        let jobs = client.scheduled_jobs_after(from).unwrap();

        let weekly = jobs
            .iter()
            .find(|job| job.kind == JobKind::WeeklySweep)
            .expect("weekly schedule exists");
        assert_eq!(
            weekly.next_run,
            Utc.with_ymd_and_hms(2030, 6, 2, 4, 0, 0).unwrap()
        );
        assert_eq!(
            weekly.args.to_value(),
            json!({ "week": "2030-W22", "night": "2030-06-02" })
        );
        assert_eq!(weekly.job_key, "weekly_sweep:2030-W22");
        assert_eq!(weekly.max_attempts, 3);
        assert_eq!(weekly.timeout, DEFAULT_BATCH_JOB_TIMEOUT);

        let latency = jobs
            .iter()
            .find(|job| job.kind == JobKind::LatencyProbes)
            .expect("latency schedule exists");
        assert_eq!(latency.args.to_value(), json!({ "hour": "2030-06-02T00" }));
        assert_eq!(latency.job_key, "latency_probes:2030-06-02T00");
        assert_eq!(latency.max_attempts, 5);
        assert_eq!(latency.timeout, DEFAULT_ANALYTICS_JOB_TIMEOUT);
    }

    #[test]
    fn crontabs_are_configured_for_graphile_worker() {
        let client = JobClient::new(JobClientConfig::default()).expect("default config is valid");
        let from = Utc.with_ymd_and_hms(2030, 6, 1, 23, 0, 0).unwrap();
        let crontabs = client.crontabs_after(from).unwrap();
        let nightly = crontabs
            .iter()
            .find(|crontab| crontab.task_identifier == "nightly_smoke")
            .expect("nightly crontab exists");

        assert_eq!(nightly.options.id(), &Some("nightly_smoke".to_string()));
        assert_eq!(nightly.options.max(), &Some(3));
        assert_eq!(
            nightly.options.queue(),
            &Some(DEFAULT_QUEUE_NAME.to_string())
        );
        assert_eq!(
            nightly.options.job_key(),
            &Some("nightly_smoke:2030-06-02".to_string())
        );
        assert_eq!(
            nightly.options.job_key_mode(),
            &Some(CronJobKeyMode::PreserveRunAt)
        );
        assert_eq!(nightly.payload(), &Some(json!({ "night": "2030-06-02" })));
    }

    #[tokio::test]
    async fn run_with_timeout_fails_an_overrunning_handler() {
        let limit = Duration::from_millis(50);
        let slow = async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok::<(), String>(())
        };
        assert_eq!(
            run_with_timeout(JobKind::AggregateRebuild, limit, slow).await,
            Err("jobs: aggregate_rebuild: handler timed out after 50ms".to_owned())
        );
        assert_eq!(
            run_with_timeout(JobKind::AggregateRebuild, limit, async {
                Err::<(), _>("boom")
            })
            .await,
            Err("boom".to_owned())
        );
        assert_eq!(
            run_with_timeout(JobKind::AggregateRebuild, limit, async {
                Ok::<(), String>(())
            })
            .await,
            Ok(())
        );
    }
}
