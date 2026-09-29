//! The benchmark batches `nightly_smoke` and `weekly_sweep` (reference Part 3 §2.4,
//! §2.5, §2.7; spec §11).
//!
//! A delivery claims the batch lease for its window (D10), composes the groups, projects
//! spend, checks the prediction lock and the G8 guard, runs the groups through the runner
//! under the deterministic batch id with resume, grades the new runs, and then, in one
//! transaction with the lease completion (D11), writes the failure-tail alerts and
//! enqueues `aggregate_rebuild`. Alert fan-out happens after the commit.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::Arc;

use chrono::NaiveDate;
use graphile_worker::{IntoTaskHandlerResult, TaskHandler, WorkerContext};
use maverick_alerts::{AlertError, AlertKind, AlertNotifier, Severity, write_alert};
use maverick_domain::graders::{
    GradeInput, GradeTask, GraderKind, Registry, RunMeta, parse_envelope,
};
use maverick_domain::spend::{Projection, SpendCaps, SpendColumn, SpendTask, project_spend};
use maverick_runner::{
    Batch, BatchColumn, BatchTask, Cancellation, Clients, RunKey, RunRow, RunSink, Runner,
    RunnerConfig, SinkFuture,
};
use maverick_store::alerts::Alert;
use maverick_store::catalog::{self, ModelColumn};
use maverick_store::corpus::{self, Task};
use maverick_store::measurement::{self, NewRun, Run};
use maverick_store::{Decimal, batches};
use serde::{Deserialize, Serialize};
use serde_json::json;
use serde_json::value::RawValue;
use sqlx::PgPool;

use crate::fold::save_scores;
use crate::rebuild::{enqueue_in, with_heartbeat};
use crate::spend_guard::{SpendGuardError, check_spend};
use crate::{
    BatchEnd, BatchFollowupArgs, BatchLeaseError, BatchLeaseManager, BatchLeaseOutcome, JobArgs,
    JobKind, JobsError, NightlySmokeArgs, WeeklySweepArgs, with_job_timeout,
};

/// Below this completion rate the batch writes a `job_failure` warning (strict).
pub const COMPLETION_ALERT_THRESHOLD: f64 = 0.9;
/// At or above this judge failure rate the batch writes a `job_failure` warning.
pub const JUDGE_FAILURE_THRESHOLD: f64 = 0.1;

/// What a batch needs besides the pool.
#[derive(Clone)]
pub struct BatchConfig {
    /// `MAVERICK_RUNNER_REPEATS` (3 when 0).
    pub repeats: u32,
    pub caps: SpendCaps,
    pub lease: BatchLeaseManager,
    pub runner: RunnerConfig,
    pub clients: Clients,
    pub graders: Arc<Registry>,
    pub notifier: Option<Arc<dyn AlertNotifier>>,
}

impl std::fmt::Debug for BatchConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BatchConfig")
            .field("repeats", &self.repeats)
            .field("caps", &self.caps)
            .field("lease", &self.lease)
            .field("runner", &self.runner)
            .field("clients", &self.clients)
            .field("graders", &self.graders)
            .field("notifier", &self.notifier.is_some())
            .finish()
    }
}

impl BatchConfig {
    fn repeats(&self) -> u32 {
        if self.repeats == 0 { 3 } else { self.repeats }
    }
}

/// The batch output (`batchOutput`); false, zero, and empty optional keys are omitted.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BatchOutput {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub batch_id: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub refused: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub lock_missing: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub spend_warned: bool,
    pub projected_usd: f64,
    pub actual_usd: f64,
    pub planned_runs: i64,
    pub completed_runs: i64,
    pub completion_pct: f64,
    pub graded_runs: usize,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub judge_attempts: usize,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub judge_failures: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// How a delivery ended.
#[derive(Debug, Clone, PartialEq)]
pub enum BatchOutcome {
    /// This delivery ran (or refused, or found nothing to run).
    Ran(BatchOutput),
    /// The window's lease was already completed or skipped: a duplicate delivery (D10).
    AlreadyFinished,
    /// Another live worker holds the window's lease (D10).
    HeldByOther,
}

#[derive(Debug, thiserror::Error)]
pub enum BatchError {
    #[error("jobs: batch: {0}")]
    Store(#[from] sqlx::Error),
    #[error(transparent)]
    Lease(#[from] BatchLeaseError),
    #[error(transparent)]
    Args(#[from] JobsError),
    #[error("{0}")]
    Projection(String),
    #[error(transparent)]
    Spend(#[from] SpendGuardError),
    #[error(transparent)]
    Alert(#[from] AlertError),
    #[error("jobs: batch lease {0} was lost before completion")]
    LeaseLost(String),
    #[error("jobs: run_batch takes nightly_smoke or weekly_sweep, not {0}")]
    WrongKind(JobKind),
    #[error(transparent)]
    Enqueue(#[from] crate::rebuild::RebuildError),
}

/// `InsertRun` through the store, one row per repeat.
#[derive(Clone, Debug)]
pub struct PgRunSink(pub PgPool);

impl RunSink for PgRunSink {
    fn insert_run<'a>(&'a self, row: &'a RunRow) -> SinkFuture<'a> {
        Box::pin(async move {
            let cost = row.cost.as_deref().map(Decimal::from_str).transpose()?;
            measurement::insert_run(
                &self.0,
                &NewRun {
                    id: &row.id,
                    batch_id: &row.batch_id,
                    task_id: &row.task_id,
                    model_column_id: &row.model_column_id,
                    repeat_index: row.repeat_index,
                    output: row.output.as_deref(),
                    tokens_in: row.tokens_in,
                    tokens_out: row.tokens_out,
                    latency_ms: row.latency_ms,
                    cost,
                    temperature: row.temperature,
                    status: row.status,
                    error: row.error.as_deref(),
                    matrix_version: &row.matrix_version,
                },
            )
            .await?;
            Ok(())
        })
    }
}

/// One (tasks × columns) group.
struct Group<'a> {
    tasks: Vec<&'a Task>,
    columns: Vec<&'a ModelColumn>,
}

fn batch_task(task: &Task) -> BatchTask {
    BatchTask {
        id: task.id.clone(),
        prompt: task.prompt.clone(),
        attachments_json: task.attachments.get().to_owned(),
        temperature: task.temperature,
        max_tokens: task.max_tokens,
    }
}

fn batch_column(column: &ModelColumn) -> BatchColumn {
    BatchColumn {
        id: column.id.clone(),
        provider: column.provider.clone(),
        model_id: column.model_id.clone(),
        status: column.status.clone(),
        settings_json: column.settings.get().to_owned(),
        pricing_json: column.pricing.get().to_owned(),
    }
}

/// The spend projection over every group. An error (bad pricing, a negative price,
/// attachment JSON) fails the job so it retries.
fn project(groups: &[Group<'_>], repeats: u32) -> Result<Projection, BatchError> {
    let mut total = Projection::default();
    for group in groups {
        let tasks: Vec<SpendTask<'_>> = group
            .tasks
            .iter()
            .map(|t| SpendTask {
                id: &t.id,
                prompt: &t.prompt,
                attachments: t.attachments.get(),
                max_tokens: i64::from(t.max_tokens),
            })
            .collect();
        let columns: Vec<SpendColumn<'_>> = group
            .columns
            .iter()
            .map(|c| SpendColumn {
                id: &c.id,
                pricing: c.pricing.get(),
            })
            .collect();
        let projection =
            project_spend(&tasks, &columns, i64::from(repeats)).map_err(BatchError::Projection)?;
        total.projected_usd += projection.projected_usd;
        total.runs += projection.runs;
    }
    Ok(total)
}

/// The grading tally of one pass.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Grading {
    graded: usize,
    judge_attempts: usize,
    judge_failures: usize,
}

/// The grading pass (reference §2.7): every run with output, in `ListRunsByBatch` order.
/// Runs that already have scores were graded by an earlier delivery of this batch and
/// are skipped, so a resumed batch never grades twice (their judge counts are not
/// repeated).
async fn grade_runs(
    pool: &PgPool,
    graders: &Registry,
    runs: &[Run],
    tasks: &HashMap<&str, &Task>,
    already_scored: &HashSet<String>,
) -> Grading {
    let mut tally = Grading::default();
    for run in runs {
        let Some(output) = run.output.as_deref() else {
            continue;
        };
        if already_scored.contains(&run.id) {
            continue;
        }
        let Some(task) = tasks.get(run.task_id.as_str()) else {
            tracing::error!(run_id = %run.id, task_id = %run.task_id, "grading: unknown task");
            continue;
        };
        let specs = match serde_json::from_str::<Vec<&RawValue>>(task.grader_specs.get()) {
            Ok(specs) => specs,
            Err(error) => {
                tracing::error!(task_id = %task.id, error = %error, "grading: grader_specs do not decode");
                continue;
            }
        };
        let grade_task = GradeTask {
            id: task.id.clone(),
            capability: task.capability.clone(),
            prompt: task.prompt.clone(),
            attachments_json: task.attachments.get().to_owned(),
        };
        let meta = RunMeta {
            run_id: run.id.clone(),
            model_column_id: run.model_column_id.clone(),
            status: run.status.clone(),
            tokens_in: run.tokens_in.map_or(0, i64::from),
            tokens_out: run.tokens_out.map_or(0, i64::from),
            latency_ms: run.latency_ms.map_or(0, i64::from),
        };
        let mut saved = false;
        for (idx, spec) in specs.iter().enumerate() {
            let envelope = match parse_envelope(spec.get()) {
                Ok(envelope) => envelope,
                Err(error) => {
                    tracing::warn!(task_id = %task.id, grader_idx = idx, error = %error, "grading: envelope does not parse");
                    continue;
                }
            };
            let judge = envelope.kind == GraderKind::JudgeRubric;
            if judge {
                tally.judge_attempts += 1;
            }
            let input = GradeInput {
                task: Some(&grade_task),
                output,
                run: Some(&meta),
            };
            let result = match graders
                .grade(envelope.kind.as_str(), &envelope.spec, input)
                .await
            {
                Ok(result) => result,
                Err(error) => {
                    if judge {
                        tally.judge_failures += 1;
                    }
                    tracing::warn!(run_id = %run.id, grader_idx = idx, error = %error, "grading: grader failed");
                    continue;
                }
            };
            let Ok(mapped) = envelope.map_dimensions(Some(&result.dimensions)) else {
                continue;
            };
            if mapped.is_empty() {
                continue;
            }
            let Ok(detail) = RawValue::from_string(result.detail) else {
                continue;
            };
            let Ok(idx) = i16::try_from(idx) else {
                continue;
            };
            let mut conn = match pool.acquire().await {
                Ok(conn) => conn,
                Err(error) => {
                    tracing::error!(error = %error, "grading: no connection");
                    continue;
                }
            };
            match save_scores(&mut conn, &run.id, idx, &mapped, Some(&detail)).await {
                Ok(()) => saved = true,
                Err(error) => tracing::error!(error = %error, "grading: saving scores failed"),
            }
        }
        if saved {
            tally.graded += 1;
        }
    }
    tally
}

/// One delivery of `nightly_smoke` or `weekly_sweep`.
pub async fn run_batch(
    pool: &PgPool,
    job: &JobArgs,
    config: &BatchConfig,
    worker_id: &str,
    cancel: &Cancellation,
) -> Result<BatchOutcome, BatchError> {
    let (kind, night_text) = match job {
        JobArgs::NightlySmoke(args) => (JobKind::NightlySmoke, args.night.clone()),
        JobArgs::WeeklySweep(args) => (JobKind::WeeklySweep, args.night.clone()),
        other => return Err(BatchError::WrongKind(other.kind())),
    };
    let window = job.window()?;
    // `window()` validated the night.
    let night = NaiveDate::parse_from_str(&night_text, "%Y-%m-%d").unwrap_or_default();

    let mut lease = match config.lease.claim(pool, kind, &window, worker_id).await? {
        BatchLeaseOutcome::Claimed(lease) | BatchLeaseOutcome::TookOver(lease) => lease,
        BatchLeaseOutcome::Finished(_) => return Ok(BatchOutcome::AlreadyFinished),
        BatchLeaseOutcome::HeldByOther(_) => return Ok(BatchOutcome::HeldByOther),
    };
    let batch_id = lease.id().to_owned();
    let started = tokio::time::Instant::now();

    // Composition: the active corpus's active tasks and the runnable columns, by id.
    let (matrix_version, tasks) = match corpus::get_active_corpus_version(pool).await {
        Ok(version) => {
            let tasks = corpus::list_active_tasks_by_corpus_version(pool, &version.version).await?;
            (version.version, tasks)
        }
        Err(sqlx::Error::RowNotFound) => (String::new(), Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let columns: Vec<ModelColumn> = catalog::list_model_columns(pool)
        .await?
        .into_iter()
        .filter(|c| maverick_domain::catalog::runnable(&c.status))
        .collect();
    let groups: Vec<Group<'_>> = match kind {
        JobKind::NightlySmoke => vec![
            Group {
                tasks: tasks.iter().filter(|t| t.smoke_member).collect(),
                columns: columns
                    .iter()
                    .filter(|c| c.status != "cold_start")
                    .collect(),
            },
            Group {
                tasks: tasks.iter().collect(),
                columns: columns
                    .iter()
                    .filter(|c| c.status == "cold_start")
                    .collect(),
            },
        ],
        _ => vec![Group {
            tasks: tasks.iter().collect(),
            columns: columns.iter().collect(),
        }],
    };
    let repeats = config.repeats();
    let projection = project(&groups, repeats)?;
    let mut output = BatchOutput {
        projected_usd: projection.projected_usd,
        planned_runs: projection.runs,
        ..BatchOutput::default()
    };

    if projection.runs == 0 {
        tracing::info!(job = kind.as_str(), night = %night, "batch has nothing to run");
        lease.finish(pool, BatchEnd::Completed).await?;
        return Ok(BatchOutcome::Ran(BatchOutput {
            completion_pct: 1.0,
            ..BatchOutput::default()
        }));
    }

    if measurement::count_predictions_by_night(pool, &night_text).await? == 0 {
        tracing::warn!(
            job = kind.as_str(),
            night = %night,
            "no predictions locked for tonight; batch proceeds unscored (spec 11 prediction_lock)"
        );
        output.lock_missing = true;
    }

    let notifier = config.notifier.as_deref();
    let spend = check_spend(pool, notifier, kind, night, &projection, config.caps).await?;
    if !spend.allowed {
        lease.finish(pool, BatchEnd::Skipped).await?;
        output.refused = true;
        return Ok(BatchOutcome::Ran(output));
    }
    output.spend_warned = spend.warned;
    output.batch_id.clone_from(&batch_id);

    // Execute each group under the deterministic batch id, resuming past earlier runs.
    let runner = Runner::new(
        config.clients.clone(),
        Arc::new(PgRunSink(pool.clone())),
        config.runner.clone(),
    );
    let interval = config.lease.heartbeat_interval();
    for group in groups
        .iter()
        .filter(|g| !g.tasks.is_empty() && !g.columns.is_empty())
    {
        let done: HashSet<RunKey> = measurement::list_run_triples_by_batch(pool, &batch_id)
            .await?
            .into_iter()
            .map(|t| RunKey {
                task_id: t.task_id,
                model_column_id: t.model_column_id,
                repeat_index: t.repeat_index,
            })
            .collect();
        let batch = Batch {
            id: batch_id.clone(),
            matrix_version: matrix_version.clone(),
            repeats,
            tasks: group.tasks.iter().map(|t| batch_task(t)).collect(),
            columns: group.columns.iter().map(|c| batch_column(c)).collect(),
            done,
        };
        let summary =
            with_heartbeat(pool, &mut lease, interval, runner.run(&batch, cancel)).await?;
        if let Some(error) = summary.error() {
            tracing::error!(batch_id = %batch_id, error = %error, "persisting batch runs");
        }
        output.actual_usd += summary.cost_usd;
    }

    // Grade the new runs.
    let runs = match measurement::list_runs_by_batch(pool, &batch_id).await {
        Ok(runs) => runs,
        Err(error) => {
            tracing::error!(batch_id = %batch_id, error = %error, "listing batch runs");
            Vec::new()
        }
    };
    let already_scored: HashSet<String> = measurement::list_scores_by_batch(pool, &batch_id)
        .await?
        .into_iter()
        .map(|s| s.run_id)
        .collect();
    let by_id: HashMap<&str, &Task> = tasks.iter().map(|t| (t.id.as_str(), t)).collect();
    let grading = with_heartbeat(
        pool,
        &mut lease,
        interval,
        grade_runs(pool, &config.graders, &runs, &by_id, &already_scored),
    )
    .await?;
    let prior_graded = runs
        .iter()
        .filter(|r| already_scored.contains(&r.id))
        .count();
    output.graded_runs = grading.graded + prior_graded;
    output.judge_attempts = grading.judge_attempts;
    output.judge_failures = grading.judge_failures;
    output.completed_runs = runs
        .iter()
        .filter(|r| r.status == "ok" || r.status == "parse_failure")
        .count() as i64;
    output.completion_pct = output.completed_runs as f64 / output.planned_runs as f64;

    // The failure tail and the outcome commit together (D11).
    let mut tx = pool.begin().await?;
    batches::finish_batch(&mut *tx, &batch_id, lease.worker_id(), BatchEnd::Completed)
        .await?
        .ok_or_else(|| BatchError::LeaseLost(batch_id.clone()))?;
    // A duplicate enqueue on this batch's locked key, followed by the running worker dying,
    // leaves a permanently failed orphan in the queue (m000019). This job is the duplicate
    // that rescued the window, so it removes that orphan in the same transaction (spec 11.2).
    batches::delete_orphaned_failed_jobs(&mut *tx, kind.as_str(), &night_text).await?;
    let mut alerts: Vec<Alert> = Vec::new();
    if output.completion_pct < COMPLETION_ALERT_THRESHOLD {
        let subject = json!({
            "batch_id": batch_id,
            "completed_runs": output.completed_runs,
            "completion_pct": output.completion_pct,
            "job": kind.as_str(),
            "night": night_text,
            "planned_runs": output.planned_runs,
        });
        let message = format!(
            "{} {} completed {}/{} runs ({:.1}%)",
            kind.as_str(),
            night_text,
            output.completed_runs,
            output.planned_runs,
            output.completion_pct * 100.0
        );
        alerts.push(
            write_alert(
                &mut *tx,
                AlertKind::JobFailure,
                Severity::Warning,
                &subject,
                &message,
            )
            .await?,
        );
    }
    if output.judge_attempts > 0 {
        let rate = output.judge_failures as f64 / output.judge_attempts as f64;
        if rate >= JUDGE_FAILURE_THRESHOLD {
            let subject = json!({
                "batch_id": batch_id,
                "job": kind.as_str(),
                "judge_attempts": output.judge_attempts,
                "judge_failure_rate": rate,
                "judge_failures": output.judge_failures,
                "night": night_text,
            });
            let message = format!(
                "{} {}: judge failed {}/{} gradings ({:.1}%, threshold {:.0}%; spec 5.5)",
                kind.as_str(),
                night_text,
                output.judge_failures,
                output.judge_attempts,
                rate * 100.0,
                JUDGE_FAILURE_THRESHOLD * 100.0
            );
            alerts.push(
                write_alert(
                    &mut *tx,
                    AlertKind::JobFailure,
                    Severity::Warning,
                    &subject,
                    &message,
                )
                .await?,
            );
        }
    }
    enqueue_in(
        &mut tx,
        &JobArgs::AggregateRebuild(BatchFollowupArgs {
            night: night_text.clone(),
            batch_id: batch_id.clone(),
        }),
    )
    .await?;
    tx.commit().await?;
    if let Some(notifier) = notifier {
        for alert in &alerts {
            notifier.notify(alert);
        }
    }
    tracing::info!(
        job = kind.as_str(),
        batch_id = %batch_id,
        projected_usd = output.projected_usd,
        actual_usd = output.actual_usd,
        planned_runs = output.planned_runs,
        completed_runs = output.completed_runs,
        completion_pct = output.completion_pct,
        graded_runs = output.graded_runs,
        duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        "batch complete"
    );
    crate::metrics::batch_spend(kind, output.actual_usd);
    Ok(BatchOutcome::Ran(output))
}

async fn run_handler(ctx: &WorkerContext, job: JobArgs) -> Result<(), String> {
    let Some(config) = ctx.extensions().get::<BatchConfig>() else {
        return Err(format!(
            "jobs: {}: the worker has no BatchConfig",
            job.kind()
        ));
    };
    let kind = job.kind();
    with_job_timeout(ctx, kind, async {
        run_batch(
            ctx.pg_pool(),
            &job,
            config,
            ctx.worker_id(),
            &Cancellation::new(),
        )
        .await
        .map(|_| ())
    })
    .await
}

/// The `nightly_smoke` graphile task. The worker must carry a [`BatchConfig`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NightlySmokeJob(pub NightlySmokeArgs);

impl TaskHandler for NightlySmokeJob {
    const IDENTIFIER: &'static str = "nightly_smoke";

    async fn run(self, ctx: WorkerContext) -> impl IntoTaskHandlerResult {
        run_handler(&ctx, JobArgs::NightlySmoke(self.0)).await
    }
}

/// The `weekly_sweep` graphile task. The worker must carry a [`BatchConfig`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WeeklySweepJob(pub WeeklySweepArgs);

impl TaskHandler for WeeklySweepJob {
    const IDENTIFIER: &'static str = "weekly_sweep";

    async fn run(self, ctx: WorkerContext) -> impl IntoTaskHandlerResult {
        run_handler(&ctx, JobArgs::WeeklySweep(self.0)).await
    }
}
