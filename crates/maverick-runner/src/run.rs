//! `Run(ctx, batch)`: executes a batch of (task × column × repeats) and persists one
//! `runs` row per repeat (reference Part 2 §5.2 to §5.6).
//!
//! Providers run concurrently. Per provider, a bounded set of workers takes pairs from a
//! FIFO; all repeats of a pair run in order on one worker, because repeats measure
//! variance, not throughput. The runner never writes `parse_failure`.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::error::Error as StdError;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use maverick_domain::ids;
use maverick_domain::pricing::{Pricing, cost, cost_numeric, parse_pricing};
use maverick_domain::scoring::status;
use maverick_providers::{
    CompletionRequest, ContextCanceled, DeadlineExceeded, ProviderClient, ProviderError,
    ProviderErrorKind, kind_of,
};
use tokio::task::JoinSet;
use tokio::time::Instant;

use crate::assemble::{BenchColumn, BenchTask, assemble};
use crate::cancel::Cancellation;
use crate::pacer::Pacer;

/// Repeats when neither the batch nor the config sets them.
pub const DEFAULT_REPEATS: u32 = 3;
/// The per-run timeout when the config sets none.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
/// Concurrent calls per provider when the config sets none.
pub const DEFAULT_MAX_CONCURRENCY: usize = 8;
/// Column statuses the runner executes; any other status is skipped.
pub const RUNNABLE_STATUSES: [&str; 3] = ["cold_start", "active", "deprecated"];

/// One provider's limits. Zero values take the defaults.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProviderLimits {
    /// `0` means [`DEFAULT_MAX_CONCURRENCY`].
    pub max_concurrency: usize,
    /// Requests per minute; `<= 0` is unthrottled.
    pub rpm_limit: i64,
}

/// The runner configuration (`MAVERICK_RUNNER_*`, `MAVERICK_PROVIDERS_*`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunnerConfig {
    /// `0` means [`DEFAULT_REPEATS`].
    pub repeats: u32,
    /// Zero means [`DEFAULT_TIMEOUT`].
    pub timeout: Duration,
    /// Keyed by provider (`anthropic`, `openai`, `google`, `xai`).
    pub limits: HashMap<String, ProviderLimits>,
}

/// A task of the batch (a `tasks` row).
#[derive(Debug, Clone, PartialEq)]
pub struct BatchTask {
    pub id: String,
    pub prompt: String,
    /// The `attachments` column as JSON text.
    pub attachments_json: String,
    pub temperature: f32,
    pub max_tokens: i32,
}

/// A column of the batch (a `model_columns` row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchColumn {
    pub id: String,
    pub provider: String,
    pub model_id: String,
    pub status: String,
    /// The `settings` column as JSON text.
    pub settings_json: String,
    /// The pricing snapshot as JSON text.
    pub pricing_json: String,
}

/// The (task, column, repeat) triple of a run within its batch.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RunKey {
    pub task_id: String,
    pub model_column_id: String,
    pub repeat_index: i16,
}

/// A batch to execute.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Batch {
    /// Empty mints a fresh `bat_<ULID>`. The jobs package passes the deterministic id of
    /// the `batches` lease (D10).
    pub id: String,
    pub matrix_version: String,
    /// `0` falls back to the config.
    pub repeats: u32,
    pub tasks: Vec<BatchTask>,
    pub columns: Vec<BatchColumn>,
    /// Runs that already have a row for this batch id: resumed batches skip them (D10).
    /// Empty reproduces the Go runner.
    pub done: HashSet<RunKey>,
}

/// One `runs` row, ready for `InsertRun`.
#[derive(Debug, Clone, PartialEq)]
pub struct RunRow {
    pub id: String,
    pub batch_id: String,
    pub task_id: String,
    pub model_column_id: String,
    pub repeat_index: i16,
    pub output: Option<String>,
    pub tokens_in: Option<i32>,
    pub tokens_out: Option<i32>,
    pub latency_ms: Option<i32>,
    /// `numeric(12,6)` text, for example `0.000450`.
    pub cost: Option<String>,
    pub temperature: f32,
    pub status: &'static str,
    pub error: Option<String>,
    pub matrix_version: String,
}

pub type SinkError = Box<dyn StdError + Send + Sync + 'static>;
pub type SinkFuture<'a> = Pin<Box<dyn Future<Output = Result<(), SinkError>> + Send + 'a>>;

/// Where rows go: `InsertRun`, one call per repeat as soon as it finishes (no batching,
/// no transaction).
pub trait RunSink: Send + Sync {
    fn insert_run<'a>(&'a self, row: &'a RunRow) -> SinkFuture<'a>;
}

/// The provider clients by provider name (`Clients.For`).
#[derive(Clone, Default)]
pub struct Clients(HashMap<String, Arc<dyn ProviderClient>>);

impl std::fmt::Debug for Clients {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut providers: Vec<&String> = self.0.keys().collect();
        providers.sort();
        f.debug_tuple("Clients").field(&providers).finish()
    }
}

impl Clients {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, provider: &str, client: Arc<dyn ProviderClient>) -> Self {
        self.0.insert(provider.to_owned(), client);
        self
    }

    /// The column's client, or `no provider client configured for "<provider>" (column
    /// <id>)`.
    pub fn for_column(&self, column: &BatchColumn) -> Result<Arc<dyn ProviderClient>, String> {
        self.0.get(&column.provider).cloned().ok_or_else(|| {
            format!(
                "no provider client configured for {:?} (column {})",
                column.provider, column.id
            )
        })
    }
}

/// The result of a batch.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Summary {
    pub batch_id: String,
    /// Persisted rows.
    pub total: usize,
    pub by_status: BTreeMap<String, usize>,
    /// The sum of the persisted rows' rounded costs, in completion order.
    pub cost_usd: f64,
    /// `persisting run <id> (<task> × <column> repeat <n>): <err>` for every failed
    /// insert. Provider failures are never errors.
    pub persistence_errors: Vec<String>,
}

impl Summary {
    /// Go's `errors.Join` of the persistence failures, or `None`.
    pub fn error(&self) -> Option<String> {
        (!self.persistence_errors.is_empty()).then(|| self.persistence_errors.join("\n"))
    }
}

/// `statusForError`: `timeout` when the error's kind is `timeout` or its chain holds a
/// deadline; otherwise `provider_error` (rate limits, 5xx after retries, invalid
/// requests, and unknown errors such as a missing fake fixture).
pub fn status_for_error(error: &(dyn StdError + 'static)) -> &'static str {
    let mut deadline = false;
    let mut current = Some(error);
    while let Some(e) = current {
        deadline |= e.is::<DeadlineExceeded>();
        current = e.source();
    }
    if deadline || kind_of(error) == ProviderErrorKind::Timeout {
        status::TIMEOUT
    } else {
        status::PROVIDER_ERROR
    }
}

/// The error text Go stored: `err.Error()`. A Rust [`ProviderError`] of kind `unknown`
/// stands for a raw non-provider error in Go, whose text is the wrapped error's own.
pub fn error_text(error: &ProviderError) -> String {
    match error.source() {
        Some(inner) if error.kind() == ProviderErrorKind::Unknown => inner.to_string(),
        _ => error.to_string(),
    }
}

/// Elapsed whole milliseconds, truncated, as `int32`.
fn elapsed_ms(started: Instant) -> i32 {
    i32::try_from(started.elapsed().as_millis()).unwrap_or(i32::MAX)
}

fn tokens(n: usize) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

/// The benchmark runner.
#[derive(Clone)]
pub struct Runner {
    clients: Clients,
    sink: Arc<dyn RunSink>,
    config: RunnerConfig,
}

impl std::fmt::Debug for Runner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runner")
            .field("clients", &self.clients)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

struct Pair {
    task: Arc<BatchTask>,
    column: Arc<BatchColumn>,
}

struct Shared {
    batch_id: String,
    matrix_version: String,
    repeats: u32,
    timeout: Duration,
    done: HashSet<RunKey>,
    sink: Arc<dyn RunSink>,
    clients: Clients,
    cancel: Cancellation,
    tally: Mutex<Summary>,
}

impl Runner {
    pub fn new(clients: Clients, sink: Arc<dyn RunSink>, config: RunnerConfig) -> Self {
        Runner {
            clients,
            sink,
            config,
        }
    }

    fn repeats(&self, batch: &Batch) -> u32 {
        if batch.repeats > 0 {
            batch.repeats
        } else if self.config.repeats > 0 {
            self.config.repeats
        } else {
            DEFAULT_REPEATS
        }
    }

    fn timeout(&self) -> Duration {
        if self.config.timeout.is_zero() {
            DEFAULT_TIMEOUT
        } else {
            self.config.timeout
        }
    }

    fn limits(&self, provider: &str) -> (usize, i64) {
        let limits = self
            .config
            .limits
            .get(provider)
            .copied()
            .unwrap_or_default();
        let max_concurrency = if limits.max_concurrency > 0 {
            limits.max_concurrency
        } else {
            DEFAULT_MAX_CONCURRENCY
        };
        (max_concurrency, limits.rpm_limit)
    }

    /// `Run(ctx, batch)`. Returns the summary; persistence failures are in
    /// [`Summary::persistence_errors`].
    pub async fn run(&self, batch: &Batch, cancel: &Cancellation) -> Summary {
        let started = Instant::now();
        let batch_id = if batch.id.is_empty() {
            ids::new(ids::prefix::BATCH)
        } else {
            batch.id.clone()
        };
        let shared = Arc::new(Shared {
            batch_id: batch_id.clone(),
            matrix_version: batch.matrix_version.clone(),
            repeats: self.repeats(batch),
            timeout: self.timeout(),
            done: batch.done.clone(),
            sink: Arc::clone(&self.sink),
            clients: self.clients.clone(),
            cancel: cancel.clone(),
            tally: Mutex::new(Summary {
                batch_id: batch_id.clone(),
                ..Summary::default()
            }),
        });

        let tasks: Vec<Arc<BatchTask>> = batch.tasks.iter().cloned().map(Arc::new).collect();
        let mut by_provider: Vec<(String, VecDeque<Pair>)> = Vec::new();
        for column in &batch.columns {
            if !RUNNABLE_STATUSES.contains(&column.status.as_str()) {
                tracing::warn!(
                    batch_id = %batch_id,
                    column_id = %column.id,
                    status = %column.status,
                    "skipping non-runnable column"
                );
                continue;
            }
            let column = Arc::new(column.clone());
            let index = match by_provider.iter().position(|(p, _)| *p == column.provider) {
                Some(index) => index,
                None => {
                    by_provider.push((column.provider.clone(), VecDeque::new()));
                    by_provider.len() - 1
                }
            };
            for task in &tasks {
                by_provider[index].1.push_back(Pair {
                    task: Arc::clone(task),
                    column: Arc::clone(&column),
                });
            }
        }

        let mut workers = JoinSet::new();
        for (provider, pairs) in by_provider {
            let (max_concurrency, rpm) = self.limits(&provider);
            let count = max_concurrency.min(pairs.len());
            let queue = Arc::new(Mutex::new(pairs));
            let pacer = Arc::new(Pacer::new(rpm));
            for _ in 0..count {
                let queue = Arc::clone(&queue);
                let pacer = Arc::clone(&pacer);
                let shared = Arc::clone(&shared);
                workers.spawn(async move {
                    loop {
                        let pair = queue
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .pop_front();
                        let Some(pair) = pair else { return };
                        if shared.cancel.is_cancelled() {
                            return;
                        }
                        run_pair(&shared, &pacer, &pair).await;
                    }
                });
            }
        }
        while let Some(joined) = workers.join_next().await {
            if let Err(error) = joined
                && error.is_panic()
            {
                std::panic::resume_unwind(error.into_panic());
            }
        }

        let summary = shared
            .tally
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let ok = summary.by_status.get(status::OK).copied().unwrap_or(0);
        tracing::info!(
            batch_id = %summary.batch_id,
            runs_total = summary.total,
            runs_ok = ok,
            runs_error = summary.total - ok,
            spend_usd = summary.cost_usd,
            duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "batch complete"
        );
        summary
    }
}

/// The setup of a pair: the request, the pricing, and the client. The first failure
/// wins, in that order.
fn setup(
    shared: &Shared,
    pair: &Pair,
) -> Result<(CompletionRequest, Pricing, Arc<dyn ProviderClient>), String> {
    let task = &pair.task;
    let column = &pair.column;
    let request = assemble(
        &BenchTask {
            id: &task.id,
            prompt: &task.prompt,
            attachments_json: &task.attachments_json,
            temperature: task.temperature,
            max_tokens: task.max_tokens,
        },
        &BenchColumn {
            id: &column.id,
            model_id: &column.model_id,
            settings_json: &column.settings_json,
        },
    )
    .map_err(|e| e.to_string())?;
    let pricing = parse_pricing(&column.pricing_json)?;
    let client = shared.clients.for_column(column)?;
    Ok((request, pricing, client))
}

/// `runPair`: every repeat of one pair, in order, on this worker.
async fn run_pair(shared: &Shared, pacer: &Pacer, pair: &Pair) {
    let setup = setup(shared, pair);
    for repeat in 0..shared.repeats {
        let repeat_index = i16::try_from(repeat).unwrap_or(i16::MAX);
        if shared.cancel.is_cancelled() {
            return;
        }
        let key = RunKey {
            task_id: pair.task.id.clone(),
            model_column_id: pair.column.id.clone(),
            repeat_index,
        };
        if shared.done.contains(&key) {
            continue;
        }
        let mut row = RunRow {
            id: ids::new(ids::prefix::RUN),
            batch_id: shared.batch_id.clone(),
            task_id: key.task_id,
            model_column_id: key.model_column_id,
            repeat_index,
            output: None,
            tokens_in: None,
            tokens_out: None,
            latency_ms: None,
            cost: None,
            temperature: pair.task.temperature,
            status: status::PROVIDER_ERROR,
            error: None,
            matrix_version: shared.matrix_version.clone(),
        };
        let mut run_cost = 0.0;
        match &setup {
            Err(error) => row.error = Some(error.clone()),
            Ok((request, pricing, client)) => {
                if pacer.wait(&shared.cancel).await.is_err() {
                    return;
                }
                run_cost = execute(shared, client.as_ref(), request, pricing, &mut row).await;
            }
        }
        match shared.sink.insert_run(&row).await {
            Err(error) => {
                let message = format!(
                    "persisting run {} ({} \u{d7} {} repeat {}): {error}",
                    row.id, row.task_id, row.model_column_id, row.repeat_index
                );
                shared
                    .tally
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .persistence_errors
                    .push(message);
            }
            Ok(()) => {
                let mut tally = shared.tally.lock().unwrap_or_else(PoisonError::into_inner);
                tally.total += 1;
                *tally.by_status.entry(row.status.to_owned()).or_insert(0) += 1;
                tally.cost_usd += run_cost;
            }
        }
    }
}

/// One provider call under the per-run timeout. Fills the row and returns the run's
/// rounded cost (0 on failure). The latency is set on every executed row.
async fn execute(
    shared: &Shared,
    client: &dyn ProviderClient,
    request: &CompletionRequest,
    pricing: &Pricing,
    row: &mut RunRow,
) -> f64 {
    let started = Instant::now();
    let result = tokio::select! {
        result = tokio::time::timeout(shared.timeout, client.complete(request.clone())) => {
            result.unwrap_or_else(|_| {
                Err(ProviderError::new(client.name(), ProviderErrorKind::Timeout)
                    .with_source(DeadlineExceeded))
            })
        }
        () = shared.cancel.cancelled() => {
            Err(ProviderError::new(client.name(), ProviderErrorKind::Timeout)
                .with_source(ContextCanceled))
        }
    };
    row.latency_ms = Some(elapsed_ms(started));
    match result {
        Err(error) => {
            row.status = status_for_error(&error);
            row.error = Some(error_text(&error));
            0.0
        }
        Ok(response) => {
            let run_cost = cost(
                i64::try_from(response.tokens_in).unwrap_or(i64::MAX),
                i64::try_from(response.tokens_out).unwrap_or(i64::MAX),
                pricing,
            );
            row.status = status::OK;
            row.tokens_in = Some(tokens(response.tokens_in));
            row.tokens_out = Some(tokens(response.tokens_out));
            row.output = Some(response.text);
            row.cost = Some(cost_numeric(run_cost));
            run_cost
        }
    }
}
