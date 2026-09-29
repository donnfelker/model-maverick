//! `aggregate_rebuild` (reference Part 3 §2.9, spec §6.1 to §6.4): the choke point that
//! turns a batch's runs and scores into folded cell aggregates, then chains
//! `regression_scan` and `calibration_score`.
//!
//! The reads and the scoring run under the batch lease with heartbeats. The folds, the
//! lease completion, and the two chained jobs commit in one transaction (D11), so a
//! rebuild never completes without its follow-ups and a retry redoes the whole night.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::time::Duration;

use chrono::NaiveDate;
use graphile_worker::{IntoTaskHandlerResult, TaskHandler, WorkerContext};
use maverick_domain::graders::{Envelope, parse_envelope};
use maverick_domain::scoring::{
    RunStats, TaskOutcome, ambiguity, ambiguity_penalty, apply_autofail, grader_observations,
    high_stakes, observe, overlay_weights, run_quality, shared_dimension_spread, variance_penalty,
};
use maverick_store::batches;
use maverick_store::corpus;
use maverick_store::measurement::{self, Run, Score};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use sqlx::{PgConnection, PgPool};

use crate::fold::{CellKey, FoldParams, fold_observations};
use crate::{
    BatchEnd, BatchFollowupArgs, BatchLease, BatchLeaseError, BatchLeaseManager, JobArgs, JobKind,
    JobsError,
};

/// What `aggregate_rebuild` needs besides the pool.
#[derive(Clone, Debug)]
pub struct RebuildConfig {
    pub alpha: f64,
    pub lambda_var: f64,
    pub lambda_amb: f64,
    pub min_nights: u64,
    /// The runner's per-run timeout: timeouts enter the latency percentiles censored at
    /// this value (D7).
    pub runner_timeout: Duration,
    pub lease: BatchLeaseManager,
}

impl RebuildConfig {
    pub fn from_config(config: &maverick_config::Config, lease: BatchLeaseManager) -> Self {
        RebuildConfig {
            alpha: config.scoring_alpha,
            lambda_var: config.scoring_lambda_var,
            lambda_amb: config.scoring_lambda_amb,
            min_nights: u64::from(config.scoring_min_nights),
            runner_timeout: config.runner_timeout,
            lease,
        }
    }

    fn timeout_ms(&self) -> i32 {
        i32::try_from(self.runner_timeout.as_millis()).unwrap_or(i32::MAX)
    }
}

/// The job output: `{"batch_id","runs","cells","aggregate_rows"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebuildOutput {
    pub batch_id: String,
    pub runs: usize,
    pub cells: usize,
    pub aggregate_rows: usize,
}

/// How a delivery of the job ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebuildOutcome {
    /// This delivery rebuilt the night and chained the follow-ups.
    Rebuilt(RebuildOutput),
    /// The lease was already completed: a duplicate delivery is a no-op (D10).
    AlreadyFinished,
    /// Another live worker holds the lease: a duplicate delivery is a no-op (D10).
    HeldByOther,
}

#[derive(Debug, thiserror::Error)]
pub enum RebuildError {
    #[error("jobs: aggregate_rebuild: {0}")]
    Store(#[from] sqlx::Error),
    #[error(transparent)]
    Lease(#[from] BatchLeaseError),
    #[error(transparent)]
    Args(#[from] JobsError),
    #[error("jobs: aggregate_rebuild: task {task_id}: {source}")]
    Task {
        task_id: String,
        #[source]
        source: sqlx::Error,
    },
    #[error("task {task_id} dimension_weights: {message}")]
    DimensionWeights { task_id: String, message: String },
    #[error("jobs: aggregate_rebuild: batch lease {batch_id} was lost before completion")]
    LeaseLost { batch_id: String },
}

/// One grader's persisted scores for a run.
#[derive(Debug, Clone, PartialEq)]
struct GraderEntry {
    idx: i16,
    /// Widened from the stored float32 values.
    dims: BTreeMap<String, f64>,
    /// The `detail` of the grader's first row.
    detail: Option<String>,
}

/// Groups `ListScoresByBatch` (ordered by run, grader index, dimension) into per-run
/// grader entries in grader order.
fn group_scores(scores: Vec<Score>) -> HashMap<String, Vec<GraderEntry>> {
    let mut by_run: HashMap<String, Vec<GraderEntry>> = HashMap::new();
    for score in scores {
        let entries = by_run.entry(score.run_id).or_default();
        let value = f64::from(score.value);
        match entries.last_mut() {
            Some(entry) if entry.idx == score.grader_idx => {
                entry.dims.insert(score.dimension, value);
            }
            _ => entries.push(GraderEntry {
                idx: score.grader_idx,
                dims: BTreeMap::from([(score.dimension, value)]),
                detail: score.detail.map(|d| d.get().to_owned()),
            }),
        }
    }
    by_run
}

/// What scoring needs from a task.
struct TaskInfo {
    capability: String,
    tier: i16,
    weights: BTreeMap<String, f64>,
    high_stakes: bool,
    pass_threshold: f64,
    /// One slot per grader spec; `None` where the envelope does not parse.
    envelopes: Vec<Option<Envelope>>,
}

async fn load_task(pool: &PgPool, task_id: &str) -> Result<TaskInfo, RebuildError> {
    let task = corpus::get_task(pool, task_id)
        .await
        .map_err(|source| RebuildError::Task {
            task_id: task_id.to_owned(),
            source,
        })?;
    let weights: BTreeMap<String, f64> = serde_json::from_str(task.dimension_weights.get())
        .map_err(|e| RebuildError::DimensionWeights {
            task_id: task_id.to_owned(),
            message: e.to_string(),
        })?;
    let envelopes = serde_json::from_str::<Vec<&RawValue>>(task.grader_specs.get())
        .map(|specs| {
            specs
                .into_iter()
                .map(|spec| parse_envelope(spec.get()).ok())
                .collect()
        })
        .unwrap_or_default();
    Ok(TaskInfo {
        capability: task.capability,
        tier: task.difficulty_tier,
        weights,
        high_stakes: high_stakes(&task.domain_tags),
        pass_threshold: f64::from(task.pass_threshold),
        envelopes,
    })
}

#[derive(Default)]
struct PairAcc {
    q: Vec<f64>,
    spreads: Vec<f64>,
}

#[derive(Default)]
struct CellAcc {
    matrix_version: String,
    runs: Vec<RunStats>,
    /// Keyed by (task id, column id).
    pairs: BTreeMap<(String, String), PairAcc>,
}

/// One cell's observations for the night.
#[derive(Debug, Clone, PartialEq)]
pub struct CellPlan {
    pub cell: CellKey,
    pub matrix_version: String,
    pub observations: BTreeMap<&'static str, f64>,
}

fn decimal_to_f64(cost: Option<maverick_store::Decimal>) -> Option<f64> {
    cost.and_then(|c| c.to_string().parse().ok())
}

/// Steps 2 to 7: reads the batch's runs and scores and computes each cell's
/// observation, cells ordered by (column, capability, tier). No writes.
pub async fn plan_rebuild(
    pool: &PgPool,
    batch_id: &str,
    config: &RebuildConfig,
) -> Result<(usize, Vec<CellPlan>), RebuildError> {
    let runs: Vec<Run> = measurement::list_runs_by_batch(pool, batch_id).await?;
    if runs.is_empty() {
        tracing::info!(batch_id = %batch_id, "aggregate_rebuild: batch has no runs");
        return Ok((0, Vec::new()));
    }
    let mut scores = group_scores(measurement::list_scores_by_batch(pool, batch_id).await?);
    let mut tasks: HashMap<String, TaskInfo> = HashMap::new();
    let mut cells: BTreeMap<CellKey, CellAcc> = BTreeMap::new();
    for run in &runs {
        if !tasks.contains_key(&run.task_id) {
            let info = load_task(pool, &run.task_id).await?;
            tasks.insert(run.task_id.clone(), info);
        }
        let task = &tasks[&run.task_id];
        let entries = scores.remove(&run.id).unwrap_or_default();
        let grader_dims: Vec<Option<BTreeMap<String, f64>>> =
            entries.iter().map(|e| Some(e.dims.clone())).collect();
        let weights = if task.high_stakes {
            overlay_weights(&task.weights)
        } else {
            task.weights.clone()
        };
        let mut score = run_quality(&weights, &grader_dims, &run.status, task.pass_threshold);
        if task.high_stakes {
            let details: Vec<Option<&str>> = entries.iter().map(|e| e.detail.as_deref()).collect();
            apply_autofail(&mut score, &details);
        }

        let key = CellKey {
            model_column_id: run.model_column_id.clone(),
            capability: task.capability.clone(),
            difficulty_tier: task.tier,
        };
        let cell = cells.entry(key).or_insert_with(|| CellAcc {
            matrix_version: run.matrix_version.clone(),
            ..CellAcc::default()
        });
        cell.runs.push(RunStats::from_run(
            &run.status,
            score.pass,
            run.latency_ms,
            decimal_to_f64(run.cost),
            config.timeout_ms(),
        ));
        let pair = cell
            .pairs
            .entry((run.task_id.clone(), run.model_column_id.clone()))
            .or_default();
        if score.gradable
            && let Some(q) = score.q
        {
            pair.q.push(q);
        }
        let observations: Vec<Option<BTreeMap<String, f64>>> = entries
            .iter()
            .flat_map(|e| {
                let envelope = usize::try_from(e.idx)
                    .ok()
                    .and_then(|i| task.envelopes.get(i))
                    .and_then(Option::as_ref);
                grader_observations(envelope, e.detail.as_deref(), Some(&e.dims))
            })
            .map(Some)
            .collect();
        if let Some(spread) = shared_dimension_spread(&observations) {
            pair.spreads.push(spread);
        }
    }

    let plans = cells
        .into_iter()
        .map(|(cell, acc)| {
            let outcomes: Vec<TaskOutcome> = acc
                .pairs
                .values()
                .map(|pair| {
                    let a = ambiguity(&pair.spreads);
                    TaskOutcome {
                        score: ambiguity_penalty(
                            variance_penalty(&pair.q, config.lambda_var).score,
                            a,
                            config.lambda_amb,
                        ),
                        ambiguity: a,
                        gradable: !pair.q.is_empty(),
                    }
                })
                .collect();
            CellPlan {
                cell,
                matrix_version: acc.matrix_version,
                observations: observe(&outcomes, &acc.runs),
            }
        })
        .collect();
    Ok((runs.len(), plans))
}

/// Inserts a job with graphile_worker's `add_job` on this connection, so it commits or
/// rolls back with the caller's transaction.
pub async fn enqueue_in(conn: &mut PgConnection, args: &JobArgs) -> Result<(), RebuildError> {
    let spec = args.job_spec()?;
    // The schema is `GRAPHILE_WORKER_SCHEMA`; sqlx only takes literal SQL.
    const ADD_JOB: &str = "select id from graphile_worker.add_job(
           identifier => $1::text,
           payload => $2::json,
           queue_name => $3::text,
           run_at => $4::timestamptz,
           max_attempts => $5::int,
           job_key => $6::text,
           priority => $7::int,
           flags => $8::text[],
           job_key_mode => $9::text
         )";
    sqlx::query(ADD_JOB)
        .bind(args.kind().as_str())
        .bind(sqlx::types::Json(args.to_value()))
        .bind(spec.queue_name)
        .bind(spec.run_at)
        .bind(spec.max_attempts.map(i32::from))
        .bind(spec.job_key)
        .bind(spec.priority.map(i32::from))
        .bind(spec.flags)
        .bind(spec.job_key_mode.map(|mode| mode.to_string()))
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Runs `work` while renewing the lease every heartbeat interval.
async fn with_heartbeat<T>(
    pool: &PgPool,
    lease: &mut BatchLease,
    interval: Duration,
    work: impl Future<Output = T>,
) -> Result<T, BatchLeaseError> {
    tokio::pin!(work);
    let mut ticker = tokio::time::interval(interval);
    ticker.tick().await;
    loop {
        tokio::select! {
            out = &mut work => return Ok(out),
            _ = ticker.tick() => {
                lease.heartbeat(pool).await?;
            }
        }
    }
}

/// The whole job for one delivery: claim the lease keyed by the batch id, plan the
/// night, then fold every cell, complete the lease, and chain `regression_scan` and
/// `calibration_score` in one transaction. A batch with no runs still completes and
/// chains.
pub async fn aggregate_rebuild(
    pool: &PgPool,
    args: &BatchFollowupArgs,
    config: &RebuildConfig,
    worker_id: &str,
) -> Result<RebuildOutcome, RebuildError> {
    // `window()` validates the night, so the parse below cannot fail.
    let window = JobArgs::AggregateRebuild(args.clone()).window()?;
    let night = NaiveDate::parse_from_str(&args.night, "%Y-%m-%d").unwrap_or_default();
    let outcome = config
        .lease
        .claim(pool, JobKind::AggregateRebuild, &window, worker_id)
        .await?;
    let already_finished = matches!(outcome, crate::BatchLeaseOutcome::Finished(_));
    let Some(mut lease) = outcome.lease() else {
        return Ok(if already_finished {
            RebuildOutcome::AlreadyFinished
        } else {
            RebuildOutcome::HeldByOther
        });
    };
    let interval = config.lease.heartbeat_interval();
    let (runs, plans) = with_heartbeat(
        pool,
        &mut lease,
        interval,
        plan_rebuild(pool, &args.batch_id, config),
    )
    .await??;

    let params = FoldParams {
        alpha: config.alpha,
        min_nights: config.min_nights,
    };
    let mut tx = pool.begin().await?;
    let mut aggregate_rows = 0;
    for plan in &plans {
        aggregate_rows += fold_observations(
            &mut tx,
            &plan.cell,
            &plan.observations,
            night,
            &plan.matrix_version,
            &params,
        )
        .await?;
    }
    batches::finish_batch(&mut *tx, lease.id(), lease.worker_id(), BatchEnd::Completed)
        .await?
        .ok_or_else(|| RebuildError::LeaseLost {
            batch_id: lease.id().to_owned(),
        })?;
    for next in [
        JobArgs::RegressionScan(args.clone()),
        JobArgs::CalibrationScore(args.clone()),
    ] {
        enqueue_in(&mut tx, &next).await?;
    }
    tx.commit().await?;
    let output = RebuildOutput {
        batch_id: args.batch_id.clone(),
        runs,
        cells: plans.len(),
        aggregate_rows,
    };
    tracing::info!(
        batch_id = %output.batch_id,
        runs = output.runs,
        cells = output.cells,
        aggregate_rows = output.aggregate_rows,
        "aggregate_rebuild complete"
    );
    Ok(RebuildOutcome::Rebuilt(output))
}

/// The graphile_worker task. The worker must carry a [`RebuildConfig`] extension.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AggregateRebuildJob(pub BatchFollowupArgs);

impl TaskHandler for AggregateRebuildJob {
    const IDENTIFIER: &'static str = "aggregate_rebuild";

    async fn run(self, ctx: WorkerContext) -> impl IntoTaskHandlerResult {
        let Some(config) = ctx.extensions().get::<RebuildConfig>() else {
            return Err("jobs: aggregate_rebuild: the worker has no RebuildConfig".to_owned());
        };
        aggregate_rebuild(ctx.pg_pool(), &self.0, config, ctx.worker_id())
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn add_job_uses_the_graphile_schema() {
        assert_eq!(
            maverick_store::health::GRAPHILE_WORKER_SCHEMA,
            "graphile_worker"
        );
    }
}
