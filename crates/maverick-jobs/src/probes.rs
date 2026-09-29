//! `latency_probes` (hourly; reference Part 3 §2.12, spec §11): thin pings for
//! secondary-endpoint columns, feeding their latency cells only. Quality is a property of
//! the model; latency and price are properties of the path to it, so a column with
//! `quality_source_column_id` set borrows quality from its source and measures its own
//! latency here. These calls use operator keys (benchmark use); CI uses the fake provider.

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::NaiveDate;
use graphile_worker::{IntoTaskHandlerResult, TaskHandler, WorkerContext};
use maverick_domain::catalog::runnable;
use maverick_domain::scoring::percentile;
use maverick_runner::{BatchColumn, BenchColumn, BenchTask, Cancellation, Clients, assemble};
use maverick_store::catalog::{self, ModelColumn};
use maverick_store::measurement;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use tokio::time::Instant;

use crate::fold::{CellKey, FoldParams, fold_observations};
use crate::{HourArgs, JobArgs, JobKind, JobsError, with_job_timeout};

/// The 60-character probe line; the prompt is this line 67 times (4020 characters).
pub const PROBE_LINE: &str = "Reply with the single word pong to this latency probe line. ";
/// How many times the probe line repeats.
pub const PROBE_REPEATS: usize = 67;
/// The probe call's `max_tokens`.
pub const PROBE_MAX_TOKENS: i32 = 100;
/// Sequential pings per column.
pub const PINGS_PER_COLUMN: usize = 5;

/// The probe prompt.
pub fn probe_prompt() -> String {
    PROBE_LINE.repeat(PROBE_REPEATS)
}

/// What `latency_probes` needs besides the pool.
#[derive(Clone, Debug)]
pub struct ProbeConfig {
    pub clients: Clients,
    /// Each ping's timeout (`MAVERICK_RUNNER_TIMEOUT`; zero means 120 s).
    pub runner_timeout: Duration,
    pub alpha: f64,
    pub min_nights: u64,
}

impl ProbeConfig {
    fn ping_timeout(&self) -> Duration {
        if self.runner_timeout.is_zero() {
            Duration::from_secs(120)
        } else {
            self.runner_timeout
        }
    }
}

/// The job output: `{"columns","probes_ok","probes_failed","cells_updated"}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeOutput {
    pub columns: usize,
    pub probes_ok: usize,
    pub probes_failed: usize,
    pub cells_updated: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error("jobs: latency_probes: {0}")]
    Store(#[from] sqlx::Error),
    #[error(transparent)]
    Args(#[from] JobsError),
}

/// A source cell: (capability, tier) and the matrix version of its first row in query
/// order (`pass_rate` sorts before `quality`, so its version wins).
struct SourceCell {
    capability: String,
    tier: i16,
    matrix_version: String,
}

async fn source_cells(pool: &PgPool, source: &str) -> sqlx::Result<Vec<SourceCell>> {
    let rows = measurement::list_latest_aggregates_by_columns(
        pool,
        &[source.to_owned()],
        &["quality".to_owned(), "pass_rate".to_owned()],
    )
    .await?;
    let mut cells: BTreeMap<(String, i16), String> = BTreeMap::new();
    for row in rows {
        cells
            .entry((row.capability, row.difficulty_tier))
            .or_insert(row.matrix_version);
    }
    Ok(cells
        .into_iter()
        .map(|((capability, tier), matrix_version)| SourceCell {
            capability,
            tier,
            matrix_version,
        })
        .collect())
}

/// Five sequential pings. Returns the successful latencies in milliseconds and the
/// failure count. A missing client or unusable settings is five failures and no ping;
/// a cancel stops early without counting the remaining pings.
async fn probe(
    column: &ModelColumn,
    config: &ProbeConfig,
    cancel: &Cancellation,
) -> (Vec<f64>, usize) {
    let batch_column = BatchColumn {
        id: column.id.clone(),
        provider: column.provider.clone(),
        model_id: column.model_id.clone(),
        status: column.status.clone(),
        settings_json: column.settings.get().to_owned(),
        pricing_json: String::new(),
    };
    let prompt = probe_prompt();
    let request = assemble(
        &BenchTask {
            id: "latency_probe",
            prompt: &prompt,
            attachments_json: "",
            temperature: 0.0,
            max_tokens: PROBE_MAX_TOKENS,
        },
        &BenchColumn {
            id: &column.id,
            model_id: &column.model_id,
            settings_json: &batch_column.settings_json,
        },
    );
    let (request, client) = match (request, config.clients.for_column(&batch_column)) {
        (Ok(request), Ok(client)) => (request, client),
        _ => return (Vec::new(), PINGS_PER_COLUMN),
    };
    let mut latencies = Vec::new();
    let mut failed = 0;
    for _ in 0..PINGS_PER_COLUMN {
        if cancel.is_cancelled() {
            break;
        }
        let started = Instant::now();
        let outcome = tokio::select! {
            outcome = tokio::time::timeout(config.ping_timeout(), client.complete(request.clone())) => outcome,
            () = cancel.cancelled() => break,
        };
        match outcome {
            Ok(Ok(_)) => {
                let ms = started.elapsed().as_millis();
                latencies.push(ms as f64);
            }
            _ => failed += 1,
        }
    }
    (latencies, failed)
}

/// The whole job for one delivery.
pub async fn latency_probes(
    pool: &PgPool,
    args: &HourArgs,
    config: &ProbeConfig,
    cancel: &Cancellation,
) -> Result<ProbeOutput, ProbeError> {
    JobArgs::LatencyProbes(args.clone()).window()?;
    // `window()` validated `YYYY-MM-DDTHH`, so the date parses.
    let as_of = NaiveDate::parse_from_str(&args.hour[..10], "%Y-%m-%d").unwrap_or_default();
    let params = FoldParams {
        alpha: config.alpha,
        min_nights: config.min_nights,
    };
    let mut output = ProbeOutput::default();
    for column in catalog::list_model_columns(pool).await? {
        if !runnable(&column.status) {
            continue;
        }
        let Some(source) = column.quality_source_column_id.clone() else {
            continue;
        };
        output.columns += 1;
        let cells = source_cells(pool, &source).await?;
        if cells.is_empty() {
            continue;
        }
        let (mut latencies, failed) = probe(&column, config, cancel).await;
        output.probes_ok += latencies.len();
        output.probes_failed += failed;
        if latencies.is_empty() {
            continue;
        }
        latencies.sort_by(f64::total_cmp);
        let observations = BTreeMap::from([
            ("latency_p50_ms", percentile(&latencies, 0.50)),
            ("latency_p95_ms", percentile(&latencies, 0.95)),
        ]);
        let mut conn = pool.acquire().await?;
        for cell in &cells {
            fold_observations(
                &mut conn,
                &CellKey {
                    model_column_id: column.id.clone(),
                    capability: cell.capability.clone(),
                    difficulty_tier: cell.tier,
                },
                &observations,
                as_of,
                &cell.matrix_version,
                &params,
            )
            .await?;
            output.cells_updated += 1;
        }
    }
    tracing::info!(
        columns = output.columns,
        probes_ok = output.probes_ok,
        probes_failed = output.probes_failed,
        cells_updated = output.cells_updated,
        "latency_probes complete"
    );
    Ok(output)
}

/// The graphile_worker task. The worker must carry a [`ProbeConfig`] extension.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LatencyProbesJob(pub HourArgs);

impl TaskHandler for LatencyProbesJob {
    const IDENTIFIER: &'static str = "latency_probes";

    async fn run(self, ctx: WorkerContext) -> impl IntoTaskHandlerResult {
        let Some(config) = ctx.extensions().get::<ProbeConfig>() else {
            return Err("jobs: latency_probes: the worker has no ProbeConfig".to_owned());
        };
        with_job_timeout(&ctx, JobKind::LatencyProbes, async {
            latency_probes(ctx.pg_pool(), &self.0, config, &Cancellation::new())
                .await
                .map(|_| ())
        })
        .await
    }
}
