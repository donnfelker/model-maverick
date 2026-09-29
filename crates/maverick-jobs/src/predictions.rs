//! `prediction_lock` and `calibration_score` (reference Part 3 §2.8, §2.11, and §3;
//! master plan §3.1 "Calibration").
//!
//! `prediction_lock` snapshots the pass probability that each scheduled cell's pass-rate
//! EWMA implies, before the night's batch runs. The first lock wins, so hindsight can
//! never overwrite it. After the rebuild, `calibration_score` compares the locked
//! probabilities with the batch's run outcomes, per capability.
//!
//! Each job has a lease-free core ([`lock_night`], [`score_calibration`]) with the Go
//! semantics, and a leased delivery: `prediction_lock` is keyed by the night and
//! `calibration_score` by the batch id (D10). The rows and the lease completion commit
//! in one transaction.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use chrono::{NaiveDate, Utc};
use graphile_worker::{IntoTaskHandlerResult, TaskHandler, WorkerContext};
use maverick_domain::predictions::{self, Cell, Pair, SOURCE_EWMA};
use maverick_store::measurement::{self, Prediction};
use maverick_store::{batches, catalog, corpus};
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, PgPool};

use crate::rebuild::{TaskError, TaskInfo, group_scores, load_task, score_run};
use crate::{
    BatchEnd, BatchFollowupArgs, BatchLeaseError, BatchLeaseManager, BatchLeaseOutcome, CronError,
    CronExpr, JobArgs, JobKind, JobsError, PredictionLockArgs,
};

/// What `prediction_lock` needs besides the pool.
#[derive(Debug, Clone)]
pub struct PredictionLockConfig {
    /// `MAVERICK_CRON_WEEKLY`: a night it fires on is a sweep night.
    pub cron_weekly: String,
    pub lease: BatchLeaseManager,
}

impl PredictionLockConfig {
    pub fn from_config(config: &maverick_config::Config, lease: BatchLeaseManager) -> Self {
        PredictionLockConfig {
            cron_weekly: config.cron_weekly.clone(),
            lease,
        }
    }
}

/// The job output: `{"night","sweep_night","scheduled_cells","locked_cells","inserted_rows"}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PredictionLockOutput {
    pub night: String,
    pub sweep_night: bool,
    pub scheduled_cells: usize,
    pub locked_cells: usize,
    pub inserted_rows: u64,
}

/// How a `prediction_lock` delivery ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PredictionLockOutcome {
    Locked(PredictionLockOutput),
    /// The night's lock already completed (D10).
    AlreadyFinished,
    /// Another live worker holds the night's lease (D10).
    HeldByOther,
}

#[derive(Debug, thiserror::Error)]
pub enum PredictionLockError {
    #[error("jobs: prediction_lock: {0}")]
    Store(#[from] sqlx::Error),
    #[error(transparent)]
    Lease(#[from] BatchLeaseError),
    #[error(transparent)]
    Args(#[from] JobsError),
    #[error("jobs: prediction_lock: MAVERICK_CRON_WEEKLY: {0}")]
    Cron(#[from] CronError),
    #[error("jobs: prediction_lock: batch lease {0} was lost before completion")]
    LeaseLost(String),
}

fn cell(column: &str, capability: &str, tier: i16) -> Cell {
    Cell {
        model_column_id: column.to_owned(),
        capability: capability.to_owned(),
        difficulty_tier: i64::from(tier),
    }
}

/// Locks the night's predictions: reads on `pool`, inserts on `conn`. A second lock of
/// the same night inserts nothing (the first lock wins).
pub async fn lock_night(
    pool: &PgPool,
    conn: &mut PgConnection,
    night: NaiveDate,
    cron_weekly: &str,
) -> Result<PredictionLockOutput, PredictionLockError> {
    let weekly = CronExpr::parse(cron_weekly)?;
    let night_tag = night.format("%Y-%m-%d").to_string();
    let mut output = PredictionLockOutput {
        night: night_tag.clone(),
        sweep_night: weekly.fires_on(night),
        ..PredictionLockOutput::default()
    };

    // The batch composition: the active corpus's active tasks and the runnable columns.
    let (matrix_version, tasks) = match corpus::get_active_corpus_version(pool).await {
        Ok(version) => {
            let tasks = corpus::list_active_tasks_by_corpus_version(pool, &version.version).await?;
            (version.version, tasks)
        }
        Err(sqlx::Error::RowNotFound) => (String::new(), Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let columns: Vec<catalog::ModelColumn> = catalog::list_model_columns(pool)
        .await?
        .into_iter()
        .filter(|c| maverick_domain::catalog::runnable(&c.status))
        .collect();

    let mut scheduled = Vec::new();
    for column in &columns {
        let cold = column.status == "cold_start";
        for task in &tasks {
            if cold || task.smoke_member || output.sweep_night {
                scheduled.push(cell(&column.id, &task.capability, task.difficulty_tier));
            }
        }
    }
    output.scheduled_cells = scheduled.iter().collect::<BTreeSet<_>>().len();

    // The latest pass_rate row per cell, any date (QUIRK J11).
    let column_ids: Vec<String> = columns.iter().map(|c| c.id.clone()).collect();
    let ewma: BTreeMap<Cell, f64> = measurement::list_latest_aggregates_by_columns(
        pool,
        &column_ids,
        &["pass_rate".to_owned()],
    )
    .await?
    .into_iter()
    .map(|row| {
        (
            cell(&row.model_column_id, &row.capability, row.difficulty_tier),
            f64::from(row.ewma),
        )
    })
    .collect();

    let derived = predictions::derive(&scheduled, &ewma);
    output.locked_cells = derived.len();
    let locked_at = Utc::now();
    for prediction in &derived {
        output.inserted_rows += measurement::insert_prediction_lock(
            &mut *conn,
            &Prediction {
                batch_id: night_tag.clone(),
                model_column_id: prediction.cell.model_column_id.clone(),
                capability: prediction.cell.capability.clone(),
                difficulty_tier: i16::try_from(prediction.cell.difficulty_tier).unwrap_or_default(),
                #[allow(clippy::cast_possible_truncation)]
                predicted_pass_prob: prediction.pass_prob as f32,
                source: SOURCE_EWMA.to_owned(),
                matrix_version: matrix_version.clone(),
                locked_at,
            },
        )
        .await?;
    }
    Ok(output)
}

/// One `prediction_lock` delivery.
pub async fn prediction_lock(
    pool: &PgPool,
    args: &PredictionLockArgs,
    config: &PredictionLockConfig,
    worker_id: &str,
) -> Result<PredictionLockOutcome, PredictionLockError> {
    let window = JobArgs::PredictionLock(args.clone()).window()?;
    // `window()` validated the night.
    let night = NaiveDate::parse_from_str(&args.night, "%Y-%m-%d").unwrap_or_default();
    let lease = match config
        .lease
        .claim(pool, JobKind::PredictionLock, &window, worker_id)
        .await?
    {
        BatchLeaseOutcome::Claimed(lease) | BatchLeaseOutcome::TookOver(lease) => lease,
        BatchLeaseOutcome::Finished(_) => return Ok(PredictionLockOutcome::AlreadyFinished),
        BatchLeaseOutcome::HeldByOther(_) => return Ok(PredictionLockOutcome::HeldByOther),
    };
    let mut tx = pool.begin().await?;
    let output = lock_night(pool, &mut tx, night, &config.cron_weekly).await?;
    batches::finish_batch(&mut *tx, lease.id(), lease.worker_id(), BatchEnd::Completed)
        .await?
        .ok_or_else(|| PredictionLockError::LeaseLost(lease.id().to_owned()))?;
    tx.commit().await?;
    tracing::info!(
        night = %output.night,
        sweep_night = output.sweep_night,
        scheduled_cells = output.scheduled_cells,
        locked_cells = output.locked_cells,
        inserted_rows = output.inserted_rows,
        "prediction_lock complete"
    );
    Ok(PredictionLockOutcome::Locked(output))
}

/// The `prediction_lock` graphile task. The worker must carry a
/// [`PredictionLockConfig`] extension.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PredictionLockJob(pub PredictionLockArgs);

impl TaskHandler for PredictionLockJob {
    const IDENTIFIER: &'static str = "prediction_lock";

    async fn run(self, ctx: WorkerContext) -> impl IntoTaskHandlerResult {
        let Some(config) = ctx.extensions().get::<PredictionLockConfig>() else {
            return Err("jobs: prediction_lock: the worker has no PredictionLockConfig".to_owned());
        };
        prediction_lock(ctx.pg_pool(), &self.0, config, ctx.worker_id())
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// What `calibration_score` needs besides the pool.
#[derive(Debug, Clone)]
pub struct CalibrationConfig {
    pub lease: BatchLeaseManager,
}

/// The job output:
/// `{"batch_id","locked_cells","pairs","unlocked_runs","calibration_rows","skipped"?}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalibrationOutput {
    pub batch_id: String,
    pub locked_cells: usize,
    pub pairs: usize,
    pub unlocked_runs: usize,
    pub calibration_rows: usize,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub skipped: bool,
}

/// How a `calibration_score` delivery ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CalibrationOutcome {
    Scored(CalibrationOutput),
    /// The batch's calibration already completed (D10).
    AlreadyFinished,
    /// Another live worker holds the lease (D10).
    HeldByOther,
}

#[derive(Debug, thiserror::Error)]
pub enum CalibrationError {
    #[error("jobs: calibration_score: {0}")]
    Store(#[from] sqlx::Error),
    #[error(transparent)]
    Lease(#[from] BatchLeaseError),
    #[error(transparent)]
    Args(#[from] JobsError),
    #[error("jobs: calibration_score: task {task_id}: {source}")]
    Task {
        task_id: String,
        #[source]
        source: sqlx::Error,
    },
    #[error("task {task_id} dimension_weights: {message}")]
    DimensionWeights { task_id: String, message: String },
    #[error("jobs: calibration_score: batch lease {0} was lost before completion")]
    LeaseLost(String),
}

impl From<TaskError> for CalibrationError {
    fn from(error: TaskError) -> Self {
        match error {
            TaskError::Load { task_id, source } => CalibrationError::Task { task_id, source },
            TaskError::DimensionWeights { task_id, message } => {
                CalibrationError::DimensionWeights { task_id, message }
            }
        }
    }
}

/// Scores the batch against the night's locked predictions: reads on `pool`, upserts
/// on `conn`. A retry replaces its own rows.
pub async fn score_calibration(
    pool: &PgPool,
    conn: &mut PgConnection,
    night: &str,
    batch_id: &str,
) -> Result<CalibrationOutput, CalibrationError> {
    let mut output = CalibrationOutput {
        batch_id: batch_id.to_owned(),
        ..CalibrationOutput::default()
    };
    let locked: HashMap<Cell, f64> = measurement::list_predictions_by_night(pool, night)
        .await?
        .into_iter()
        .map(|p| {
            (
                cell(&p.model_column_id, &p.capability, p.difficulty_tier),
                f64::from(p.predicted_pass_prob),
            )
        })
        .collect();
    output.locked_cells = locked.len();
    if locked.is_empty() {
        tracing::warn!(
            night = %night,
            batch_id = %batch_id,
            "no predictions locked for the night; calibration skipped"
        );
        output.skipped = true;
        return Ok(output);
    }
    let runs = measurement::list_runs_by_batch(pool, batch_id).await?;
    if runs.is_empty() {
        tracing::info!(batch_id = %batch_id, "calibration_score: batch has no runs");
        output.skipped = true;
        return Ok(output);
    }

    let mut scores = group_scores(measurement::list_scores_by_batch(pool, batch_id).await?);
    let mut tasks: HashMap<String, TaskInfo> = HashMap::new();
    let mut pairs: BTreeMap<String, Vec<Pair>> = BTreeMap::new();
    for run in &runs {
        if !tasks.contains_key(&run.task_id) {
            let info = load_task(pool, &run.task_id).await?;
            tasks.insert(run.task_id.clone(), info);
        }
        let task = &tasks[&run.task_id];
        let entries = scores.remove(&run.id).unwrap_or_default();
        let score = score_run(task, &entries, &run.status);
        let Some(&p) = locked.get(&cell(&run.model_column_id, &task.capability, task.tier)) else {
            output.unlocked_runs += 1;
            continue;
        };
        pairs
            .entry(task.capability.clone())
            .or_default()
            .push(Pair { p, o: score.pass });
        output.pairs += 1;
    }
    for (capability, capability_pairs) in &pairs {
        #[allow(clippy::cast_possible_truncation)]
        measurement::upsert_calibration(
            &mut *conn,
            batch_id,
            capability,
            predictions::brier(capability_pairs) as f32,
            predictions::overconfidence(capability_pairs) as f32,
            i32::try_from(capability_pairs.len()).unwrap_or(i32::MAX),
        )
        .await?;
        output.calibration_rows += 1;
    }
    Ok(output)
}

/// One `calibration_score` delivery.
pub async fn calibration_score(
    pool: &PgPool,
    args: &BatchFollowupArgs,
    config: &CalibrationConfig,
    worker_id: &str,
) -> Result<CalibrationOutcome, CalibrationError> {
    let window = JobArgs::CalibrationScore(args.clone()).window()?;
    let lease = match config
        .lease
        .claim(pool, JobKind::CalibrationScore, &window, worker_id)
        .await?
    {
        BatchLeaseOutcome::Claimed(lease) | BatchLeaseOutcome::TookOver(lease) => lease,
        BatchLeaseOutcome::Finished(_) => return Ok(CalibrationOutcome::AlreadyFinished),
        BatchLeaseOutcome::HeldByOther(_) => return Ok(CalibrationOutcome::HeldByOther),
    };
    let mut tx = pool.begin().await?;
    let output = score_calibration(pool, &mut tx, &args.night, &args.batch_id).await?;
    batches::finish_batch(&mut *tx, lease.id(), lease.worker_id(), BatchEnd::Completed)
        .await?
        .ok_or_else(|| CalibrationError::LeaseLost(lease.id().to_owned()))?;
    tx.commit().await?;
    tracing::info!(
        batch_id = %output.batch_id,
        locked_cells = output.locked_cells,
        pairs = output.pairs,
        unlocked_runs = output.unlocked_runs,
        calibration_rows = output.calibration_rows,
        skipped = output.skipped,
        "calibration_score complete"
    );
    Ok(CalibrationOutcome::Scored(output))
}

/// The `calibration_score` graphile task. The worker must carry a
/// [`CalibrationConfig`] extension.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CalibrationScoreJob(pub BatchFollowupArgs);

impl TaskHandler for CalibrationScoreJob {
    const IDENTIFIER: &'static str = "calibration_score";

    async fn run(self, ctx: WorkerContext) -> impl IntoTaskHandlerResult {
        let Some(config) = ctx.extensions().get::<CalibrationConfig>() else {
            return Err("jobs: calibration_score: the worker has no CalibrationConfig".to_owned());
        };
        calibration_score(ctx.pg_pool(), &self.0, config, ctx.worker_id())
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calibration_output_omits_skipped_when_false() {
        let scored = CalibrationOutput {
            batch_id: "bat_1".to_owned(),
            pairs: 3,
            ..CalibrationOutput::default()
        };
        assert_eq!(
            serde_json::to_string(&scored).expect("json"),
            r#"{"batch_id":"bat_1","locked_cells":0,"pairs":3,"unlocked_runs":0,"calibration_rows":0}"#
        );
        let skipped = CalibrationOutput {
            skipped: true,
            ..scored
        };
        assert!(
            serde_json::to_string(&skipped)
                .expect("json")
                .ends_with(r#""skipped":true}"#)
        );
    }

    #[test]
    fn job_identifiers_match_the_kinds() {
        assert_eq!(
            <PredictionLockJob as TaskHandler>::IDENTIFIER,
            JobKind::PredictionLock.as_str()
        );
        assert_eq!(
            <CalibrationScoreJob as TaskHandler>::IDENTIFIER,
            JobKind::CalibrationScore.as_str()
        );
    }
}
