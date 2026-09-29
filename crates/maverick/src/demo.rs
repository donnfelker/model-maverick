//! `maverick demo-run` (hidden; bead 3.13; reference Part 1 §7.6): the M0 check that a
//! runner executes a task against a column and stores the run, with no real provider.
//!
//! Load the config, then migrate (`migrate failed`, exit 1). Then, under a signal
//! context (SIGINT or SIGTERM cancels the batch):
//!
//! 1. execute the seed file as one multi-statement exec;
//! 2. build the fake provider from the fixture dir and register it for all four providers;
//! 3. read the active tasks and the runnable columns (none of either is an error that
//!    names the seed file);
//! 4. run one batch with the configured repeats, timeout, and provider limits, under the
//!    first task's corpus version;
//! 5. read the batch's runs back (none is an error).
//!
//! Any error logs `demo-run failed` and exits 1. Success prints `batch <id>: <total> runs
//! (<ok> ok)` and then the runs through the tabwriter: `RUN ID`, `STATUS`, `LATENCY_MS`,
//! `TOKENS_IN`, `TOKENS_OUT`, `COST_USD`. A NULL prints empty, and the cost is `%.6f`.

use std::io::Write as _;
use std::sync::Arc;

use maverick_config::Config;
use maverick_jobs::batch::PgRunSink;
use maverick_jobs::worker::runner_config;
use maverick_providers::ProviderClient;
use maverick_providers::fake::FakeProvider;
use maverick_runner::cancel::Cancellation;
use maverick_runner::run::{Batch, BatchColumn, BatchTask, Clients, RUNNABLE_STATUSES, Runner};
use maverick_store::catalog::{ModelColumn, list_model_columns_by_statuses};
use maverick_store::corpus::{Task, list_active_tasks};
use maverick_store::measurement::{Run, list_runs_by_batch};
use sqlx::AssertSqlSafe;
use sqlx::postgres::PgPoolOptions;

use crate::cli::{block_on, load_config};
use crate::corpus::tabwrite;
use crate::logging::{Output, with_logger};

const EXIT_SUCCESS: i32 = 0;
const EXIT_FAILURE: i32 = 1;

/// The providers the fake stands in for.
const PROVIDERS: [&str; 4] = ["anthropic", "openai", "google", "xai"];

/// `demo-run --seed <path> --fake-fixtures <dir>`.
pub(crate) fn run(
    seed: &str,
    fixtures: &str,
    lookup: &dyn Fn(&str) -> Option<String>,
    stdout: Output,
) -> i32 {
    let mut plain = stdout.clone();
    with_logger(stdout, || {
        let Ok(config) = load_config(lookup) else {
            return EXIT_FAILURE;
        };
        block_on(async {
            if let Err(error) = maverick_store::migrate::migrate(&config.database_url).await {
                tracing::error!(error = %error, "migrate failed");
                return EXIT_FAILURE;
            }
            let cancel = Cancellation::new();
            let on_signal = cancel.clone();
            let watcher = tokio::spawn(async move {
                signalled().await;
                on_signal.cancel();
            });
            let result = demo(&config, seed, fixtures, &cancel).await;
            watcher.abort();
            match result {
                Ok(report) => {
                    let _ = plain.write_all(report.as_bytes());
                    EXIT_SUCCESS
                }
                Err(error) => {
                    tracing::error!(error = %error, "demo-run failed");
                    EXIT_FAILURE
                }
            }
        })
        .unwrap_or(EXIT_FAILURE)
    })
}

/// SIGINT or SIGTERM.
async fn signalled() {
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        () = terminate => {}
    }
}

/// Steps 1 to 5; the report on success.
async fn demo(
    config: &Config,
    seed: &str,
    fixtures: &str,
    cancel: &Cancellation,
) -> Result<String, String> {
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&config.database_url)
        .await
        .map_err(|error| format!("connecting to database: {error}"))?;
    let sql = std::fs::read_to_string(seed)
        .map_err(|error| format!("reading seed fixture {seed}: {error}"))?;
    sqlx::raw_sql(AssertSqlSafe(sql))
        .execute(&pool)
        .await
        .map_err(|error| format!("executing seed fixture {seed}: {error}"))?;

    let fake: Arc<dyn ProviderClient> =
        Arc::new(FakeProvider::from_dir(fixtures).map_err(|error| error.to_string())?);
    let clients = PROVIDERS.iter().fold(Clients::new(), |clients, provider| {
        clients.with(provider, Arc::clone(&fake))
    });

    let tasks = list_active_tasks(&pool)
        .await
        .map_err(|error| format!("listing active tasks: {error}"))?;
    let Some(first) = tasks.first() else {
        return Err(format!("seed fixture {seed} produced no active tasks"));
    };
    let statuses: Vec<String> = RUNNABLE_STATUSES.iter().map(|s| (*s).to_owned()).collect();
    let columns = list_model_columns_by_statuses(&pool, &statuses)
        .await
        .map_err(|error| format!("listing runnable columns: {error}"))?;
    if columns.is_empty() {
        return Err(format!("seed fixture {seed} produced no runnable columns"));
    }

    let runner = Runner::new(
        clients,
        Arc::new(PgRunSink(pool.clone())),
        runner_config(config),
    );
    let batch = Batch {
        matrix_version: first.corpus_version.clone(),
        tasks: tasks.iter().map(batch_task).collect(),
        columns: columns.iter().map(batch_column).collect(),
        ..Batch::default()
    };
    let summary = runner.run(&batch, cancel).await;
    if let Some(error) = summary.error() {
        return Err(error);
    }
    let runs = list_runs_by_batch(&pool, &summary.batch_id)
        .await
        .map_err(|error| format!("listing runs: {error}"))?;
    if runs.is_empty() {
        return Err(format!("batch {} persisted no runs", summary.batch_id));
    }
    Ok(report(&summary.batch_id, &runs))
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

/// The summary line and the run table.
fn report(batch_id: &str, runs: &[Run]) -> String {
    let ok = runs.iter().filter(|run| run.status == "ok").count();
    let mut out = format!("batch {batch_id}: {} runs ({ok} ok)\n", runs.len());
    let optional = |value: Option<i32>| value.map(|n| n.to_string()).unwrap_or_default();
    let mut rows = vec![
        [
            "RUN ID",
            "STATUS",
            "LATENCY_MS",
            "TOKENS_IN",
            "TOKENS_OUT",
            "COST_USD",
        ]
        .map(str::to_owned)
        .to_vec(),
    ];
    rows.extend(runs.iter().map(|run| {
        vec![
            run.id.clone(),
            run.status.clone(),
            optional(run.latency_ms),
            optional(run.tokens_in),
            optional(run.tokens_out),
            run.cost
                .map(|cost| format!("{cost:.6}"))
                .unwrap_or_default(),
        ]
    }));
    out.push_str(&tabwrite(&rows, ""));
    out
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use maverick_store::Decimal;

    use super::*;

    fn run(id: &str, status: &str, latency: Option<i32>, cost: Option<&str>) -> Run {
        Run {
            id: id.to_owned(),
            batch_id: "bat_1".to_owned(),
            task_id: "task_1".to_owned(),
            model_column_id: "col_1".to_owned(),
            repeat_index: 0,
            output: None,
            tokens_in: latency.map(|_| 120),
            tokens_out: latency.map(|_| 18),
            latency_ms: latency,
            cost: cost.map(|cost| cost.parse::<Decimal>().unwrap()),
            temperature: 0.0,
            status: status.to_owned(),
            error: None,
            matrix_version: "2026-07-01".to_owned(),
            ran_at: Utc::now(),
        }
    }

    #[test]
    fn the_report_is_a_summary_line_and_a_tabwriter_table() {
        let runs = [
            run("run_a", "ok", Some(25), Some("0.000630")),
            run("run_b", "provider_error", None, None),
            run("run_c", "ok", Some(7), Some("1.5")),
        ];
        assert_eq!(
            report("bat_1", &runs),
            "batch bat_1: 3 runs (2 ok)\n\
             RUN ID  STATUS          LATENCY_MS  TOKENS_IN  TOKENS_OUT  COST_USD\n\
             run_a   ok              25          120        18          0.000630\n\
             run_b   provider_error                                     \n\
             run_c   ok              7           120        18          1.500000\n"
        );
    }
}
