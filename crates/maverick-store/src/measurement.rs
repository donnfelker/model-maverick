//! Measurement: runs, scores, aggregates, predictions, and calibration.
//!
//! Go sources: `parity/schema/queries/runs.sql`, `scores.sql`, `aggregates.sql`,
//! `predictions.sql`, and `calibration.sql`. `InsertRun`, `InsertScore`, and
//! `UpsertAggregate` are single autocommit statements in Go (reference Part 1 §3).

use chrono::{DateTime, NaiveDate, Utc};
use sqlx::PgExecutor;

use crate::{Decimal, Json, JsonRawValue};

/// A `runs` row.
#[derive(Debug, Clone)]
pub struct Run {
    pub id: String,
    pub batch_id: String,
    pub task_id: String,
    pub model_column_id: String,
    pub repeat_index: i16,
    pub output: Option<String>,
    pub tokens_in: Option<i32>,
    pub tokens_out: Option<i32>,
    pub latency_ms: Option<i32>,
    pub cost: Option<Decimal>,
    pub temperature: f32,
    pub status: String,
    pub error: Option<String>,
    pub matrix_version: String,
    pub ran_at: DateTime<Utc>,
}

/// The values of `InsertRun`.
#[derive(Debug, Clone, Copy)]
pub struct NewRun<'a> {
    pub id: &'a str,
    pub batch_id: &'a str,
    pub task_id: &'a str,
    pub model_column_id: &'a str,
    pub repeat_index: i16,
    pub output: Option<&'a str>,
    pub tokens_in: Option<i32>,
    pub tokens_out: Option<i32>,
    pub latency_ms: Option<i32>,
    pub cost: Option<Decimal>,
    pub temperature: f32,
    pub status: &'a str,
    pub error: Option<&'a str>,
    pub matrix_version: &'a str,
}

/// The (task, column, repeat) triple that identifies a run within its batch.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RunTriple {
    pub task_id: String,
    pub model_column_id: String,
    pub repeat_index: i16,
}

/// Parse-failure counts for one column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseFailureRate {
    pub model_column_id: String,
    pub total: i64,
    pub parse_failures: i64,
}

/// A `scores` row. `value` is `real`, so it is `f32`.
#[derive(Debug, Clone)]
pub struct Score {
    pub run_id: String,
    pub grader_idx: i16,
    pub dimension: String,
    pub value: f32,
    pub detail: Option<Json>,
}

/// An `aggregates` row. The table is append-only per `as_of`; "current" is the newest.
#[derive(Debug, Clone, PartialEq)]
pub struct Aggregate {
    pub model_column_id: String,
    pub capability: String,
    pub difficulty_tier: i16,
    pub dimension: String,
    pub ewma: f32,
    pub variance: f32,
    pub n: i32,
    pub measured: bool,
    pub matrix_version: String,
    pub as_of: NaiveDate,
}

/// The values of `UpsertAggregate`.
#[derive(Debug, Clone, Copy)]
pub struct AggregateUpsert<'a> {
    pub model_column_id: &'a str,
    pub capability: &'a str,
    pub difficulty_tier: i16,
    pub dimension: &'a str,
    pub ewma: f32,
    pub variance: f32,
    pub n: i32,
    pub measured: bool,
    pub matrix_version: &'a str,
    pub as_of: NaiveDate,
}

/// The key of one aggregate cell and dimension.
#[derive(Debug, Clone, Copy)]
pub struct CellDimension<'a> {
    pub model_column_id: &'a str,
    pub capability: &'a str,
    pub difficulty_tier: i16,
    pub dimension: &'a str,
}

/// The keyset position after the last row of an aggregate history page.
#[derive(Debug, Clone)]
pub struct AggregateCursor {
    pub as_of: NaiveDate,
    pub capability: String,
    pub difficulty_tier: i16,
}

/// The filters of `ListAggregateHistoryPage`.
#[derive(Debug, Clone, Copy)]
pub struct AggregateHistoryQuery<'a> {
    pub model_column_id: &'a str,
    pub dimension: &'a str,
    /// Empty means every capability.
    pub capability: &'a str,
    pub difficulty: Option<i16>,
    pub since: Option<NaiveDate>,
    /// `None` is the first page.
    pub cursor: Option<&'a AggregateCursor>,
    pub page_limit: i32,
}

/// A `predictions` row. `batch_id` is the UTC night, for example `2026-07-06`.
#[derive(Debug, Clone, PartialEq)]
pub struct Prediction {
    pub batch_id: String,
    pub model_column_id: String,
    pub capability: String,
    pub difficulty_tier: i16,
    pub predicted_pass_prob: f32,
    pub source: String,
    pub matrix_version: String,
    pub locked_at: DateTime<Utc>,
}

/// A `calibration` row.
#[derive(Debug, Clone, PartialEq)]
pub struct Calibration {
    pub batch_id: String,
    pub capability: String,
    pub brier_score: f32,
    pub overconfidence: f32,
    pub n: i32,
    pub scored_at: DateTime<Utc>,
}

/// The filters of `ListCalibrationPage`.
#[derive(Debug, Clone, Copy)]
pub struct CalibrationPageQuery<'a> {
    /// Empty means every capability.
    pub capability: &'a str,
    pub since: Option<DateTime<Utc>>,
    /// Empty is the first page.
    pub cursor_batch: &'a str,
    pub cursor_capability: &'a str,
    pub page_limit: i32,
}

// ---- runs.sql ----

/// `InsertRun`: `ran_at = now()`. A duplicate (batch, task, column, repeat) fails with
/// SQLSTATE 23505.
pub async fn insert_run<'e>(db: impl PgExecutor<'e>, run: &NewRun<'_>) -> sqlx::Result<Run> {
    sqlx::query_as!(
        Run,
        "insert into runs (
           id, batch_id, task_id, model_column_id, repeat_index, output, tokens_in,
           tokens_out, latency_ms, cost, temperature, status, error, matrix_version
         )
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
         returning id, batch_id, task_id, model_column_id, repeat_index, output, tokens_in,
                   tokens_out, latency_ms, cost, temperature, status, error,
                   matrix_version, ran_at",
        run.id,
        run.batch_id,
        run.task_id,
        run.model_column_id,
        run.repeat_index,
        run.output,
        run.tokens_in,
        run.tokens_out,
        run.latency_ms,
        run.cost,
        run.temperature,
        run.status,
        run.error,
        run.matrix_version,
    )
    .fetch_one(db)
    .await
}

/// Like [`insert_run`], but a duplicate (batch, task, column, repeat) returns `None`
/// instead of failing. For a resumed batch (spec §11.2). Rust only.
pub async fn insert_run_if_absent<'e>(
    db: impl PgExecutor<'e>,
    run: &NewRun<'_>,
) -> sqlx::Result<Option<Run>> {
    sqlx::query_as!(
        Run,
        "insert into runs (
           id, batch_id, task_id, model_column_id, repeat_index, output, tokens_in,
           tokens_out, latency_ms, cost, temperature, status, error, matrix_version
         )
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
         on conflict (batch_id, task_id, model_column_id, repeat_index) do nothing
         returning id, batch_id, task_id, model_column_id, repeat_index, output, tokens_in,
                   tokens_out, latency_ms, cost, temperature, status, error,
                   matrix_version, ran_at",
        run.id,
        run.batch_id,
        run.task_id,
        run.model_column_id,
        run.repeat_index,
        run.output,
        run.tokens_in,
        run.tokens_out,
        run.latency_ms,
        run.cost,
        run.temperature,
        run.status,
        run.error,
        run.matrix_version,
    )
    .fetch_optional(db)
    .await
}

/// `GetRun`.
pub async fn get_run<'e>(db: impl PgExecutor<'e>, id: &str) -> sqlx::Result<Run> {
    sqlx::query_as!(
        Run,
        "select id, batch_id, task_id, model_column_id, repeat_index, output, tokens_in,
                tokens_out, latency_ms, cost, temperature, status, error, matrix_version,
                ran_at
         from runs
         where id = $1",
        id
    )
    .fetch_one(db)
    .await
}

/// `ListRunsByBatch`: ordered by task, column, repeat.
pub async fn list_runs_by_batch<'e>(
    db: impl PgExecutor<'e>,
    batch_id: &str,
) -> sqlx::Result<Vec<Run>> {
    sqlx::query_as!(
        Run,
        "select id, batch_id, task_id, model_column_id, repeat_index, output, tokens_in,
                tokens_out, latency_ms, cost, temperature, status, error, matrix_version,
                ran_at
         from runs
         where batch_id = $1
         order by task_id, model_column_id, repeat_index",
        batch_id
    )
    .fetch_all(db)
    .await
}

/// The (task, column, repeat) triples that already have a run in the batch, ordered like
/// [`list_runs_by_batch`]. A resumed batch skips them (spec §11.2). Rust only.
pub async fn list_run_triples_by_batch<'e>(
    db: impl PgExecutor<'e>,
    batch_id: &str,
) -> sqlx::Result<Vec<RunTriple>> {
    sqlx::query_as!(
        RunTriple,
        "select task_id, model_column_id, repeat_index
         from runs
         where batch_id = $1
         order by task_id, model_column_id, repeat_index",
        batch_id
    )
    .fetch_all(db)
    .await
}

/// `SumRunCostBetween`: the summed cost of runs with `start_at <= ran_at < end_at`, as
/// `numeric(14,6)`. NULL costs are ignored, and no runs is 0.
pub async fn sum_run_cost_between<'e>(
    db: impl PgExecutor<'e>,
    start_at: DateTime<Utc>,
    end_at: DateTime<Utc>,
) -> sqlx::Result<Decimal> {
    sqlx::query_scalar!(
        r#"select coalesce(sum(cost), 0)::numeric(14,6) as "total!" from runs
           where ran_at >= $1 and ran_at < $2"#,
        start_at,
        end_at
    )
    .fetch_one(db)
    .await
}

/// `ListBatchParseFailureRates`: per-column counts for one batch, ordered by column.
pub async fn list_batch_parse_failure_rates<'e>(
    db: impl PgExecutor<'e>,
    batch_id: &str,
) -> sqlx::Result<Vec<ParseFailureRate>> {
    sqlx::query_as!(
        ParseFailureRate,
        r#"select model_column_id,
                  count(*) as "total!",
                  count(*) filter (where status = 'parse_failure') as "parse_failures!"
           from runs
           where batch_id = $1
           group by model_column_id
           order by model_column_id"#,
        batch_id
    )
    .fetch_all(db)
    .await
}

/// `ListParseFailureRatesBetween`: per-column counts over `start_at <= ran_at < end_at`,
/// ordered by column.
pub async fn list_parse_failure_rates_between<'e>(
    db: impl PgExecutor<'e>,
    start_at: DateTime<Utc>,
    end_at: DateTime<Utc>,
) -> sqlx::Result<Vec<ParseFailureRate>> {
    sqlx::query_as!(
        ParseFailureRate,
        r#"select model_column_id,
                  count(*) as "total!",
                  count(*) filter (where status = 'parse_failure') as "parse_failures!"
           from runs
           where ran_at >= $1 and ran_at < $2
           group by model_column_id
           order by model_column_id"#,
        start_at,
        end_at
    )
    .fetch_all(db)
    .await
}

// ---- scores.sql ----

/// `InsertScore`.
pub async fn insert_score<'e>(
    db: impl PgExecutor<'e>,
    run_id: &str,
    grader_idx: i16,
    dimension: &str,
    value: f32,
    detail: Option<&JsonRawValue>,
) -> sqlx::Result<Score> {
    sqlx::query_as!(
        Score,
        r#"insert into scores (run_id, grader_idx, dimension, value, detail)
           values ($1, $2, $3, $4, $5)
           returning run_id, grader_idx, dimension, value, detail as "detail: _""#,
        run_id,
        grader_idx,
        dimension,
        value,
        detail as Option<&JsonRawValue>
    )
    .fetch_one(db)
    .await
}

/// `ListScoresByRun`: ordered by grader index, then dimension.
pub async fn list_scores_by_run<'e>(
    db: impl PgExecutor<'e>,
    run_id: &str,
) -> sqlx::Result<Vec<Score>> {
    sqlx::query_as!(
        Score,
        r#"select run_id, grader_idx, dimension, value, detail as "detail: _"
           from scores
           where run_id = $1
           order by grader_idx, dimension"#,
        run_id
    )
    .fetch_all(db)
    .await
}

/// `ListScoresByBatch`: the scores of the batch's runs, ordered by run, grader index,
/// then dimension.
pub async fn list_scores_by_batch<'e>(
    db: impl PgExecutor<'e>,
    batch_id: &str,
) -> sqlx::Result<Vec<Score>> {
    sqlx::query_as!(
        Score,
        r#"select scores.run_id, scores.grader_idx, scores.dimension, scores.value,
                  scores.detail as "detail: _"
           from scores
           join runs on runs.id = scores.run_id
           where runs.batch_id = $1
           order by scores.run_id, scores.grader_idx, scores.dimension"#,
        batch_id
    )
    .fetch_all(db)
    .await
}

// ---- aggregates.sql ----

/// `UpsertAggregate`: a conflict on the key and `as_of` replaces the values, not the key.
pub async fn upsert_aggregate<'e>(
    db: impl PgExecutor<'e>,
    row: &AggregateUpsert<'_>,
) -> sqlx::Result<Aggregate> {
    sqlx::query_as!(
        Aggregate,
        "insert into aggregates (
           model_column_id, capability, difficulty_tier, dimension,
           ewma, variance, n, measured, matrix_version, as_of
         )
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
         on conflict (model_column_id, capability, difficulty_tier, dimension, as_of)
         do update set
           ewma = excluded.ewma,
           variance = excluded.variance,
           n = excluded.n,
           measured = excluded.measured,
           matrix_version = excluded.matrix_version
         returning model_column_id, capability, difficulty_tier, dimension, ewma, variance,
                   n, measured, matrix_version, as_of",
        row.model_column_id,
        row.capability,
        row.difficulty_tier,
        row.dimension,
        row.ewma,
        row.variance,
        row.n,
        row.measured,
        row.matrix_version,
        row.as_of,
    )
    .fetch_one(db)
    .await
}

/// `ListAggregateSeriesBefore`: the cell's rows with `as_of < before`, oldest first.
pub async fn list_aggregate_series_before<'e>(
    db: impl PgExecutor<'e>,
    cell: &CellDimension<'_>,
    before: NaiveDate,
) -> sqlx::Result<Vec<Aggregate>> {
    sqlx::query_as!(
        Aggregate,
        "select model_column_id, capability, difficulty_tier, dimension, ewma, variance, n,
                measured, matrix_version, as_of
         from aggregates
         where model_column_id = $1
           and capability = $2
           and difficulty_tier = $3
           and dimension = $4
           and as_of < $5
         order by as_of",
        cell.model_column_id,
        cell.capability,
        cell.difficulty_tier,
        cell.dimension,
        before
    )
    .fetch_all(db)
    .await
}

/// `GetLatestAggregate`: the cell's newest row.
pub async fn get_latest_aggregate<'e>(
    db: impl PgExecutor<'e>,
    cell: &CellDimension<'_>,
) -> sqlx::Result<Aggregate> {
    sqlx::query_as!(
        Aggregate,
        "select model_column_id, capability, difficulty_tier, dimension, ewma, variance, n,
                measured, matrix_version, as_of
         from aggregates
         where model_column_id = $1
           and capability = $2
           and difficulty_tier = $3
           and dimension = $4
         order by as_of desc
         limit 1",
        cell.model_column_id,
        cell.capability,
        cell.difficulty_tier,
        cell.dimension
    )
    .fetch_one(db)
    .await
}

/// `ListLatestAggregatesByColumns`: the newest row per (column, capability, tier,
/// dimension), ordered by that key.
pub async fn list_latest_aggregates_by_columns<'e>(
    db: impl PgExecutor<'e>,
    column_ids: &[String],
    dimensions: &[String],
) -> sqlx::Result<Vec<Aggregate>> {
    sqlx::query_as!(
        Aggregate,
        r#"select distinct on (model_column_id, capability, difficulty_tier, dimension)
                  model_column_id as "model_column_id!", capability as "capability!",
                  difficulty_tier as "difficulty_tier!", dimension as "dimension!",
                  ewma as "ewma!", variance as "variance!", n as "n!",
                  measured as "measured!", matrix_version as "matrix_version!",
                  as_of as "as_of!"
           from aggregates
           where model_column_id = any($1::text[])
             and dimension = any($2::text[])
           order by model_column_id, capability, difficulty_tier, dimension, as_of desc"#,
        column_ids,
        dimensions
    )
    .fetch_all(db)
    .await
}

/// `ListLatestAggregatesByDimension`: the newest row per (column, capability, tier) for
/// one dimension, ordered by that key.
pub async fn list_latest_aggregates_by_dimension<'e>(
    db: impl PgExecutor<'e>,
    dimension: &str,
) -> sqlx::Result<Vec<Aggregate>> {
    sqlx::query_as!(
        Aggregate,
        r#"select distinct on (model_column_id, capability, difficulty_tier)
                  model_column_id as "model_column_id!", capability as "capability!",
                  difficulty_tier as "difficulty_tier!", dimension as "dimension!",
                  ewma as "ewma!", variance as "variance!", n as "n!",
                  measured as "measured!", matrix_version as "matrix_version!",
                  as_of as "as_of!"
           from aggregates
           where dimension = $1
           order by model_column_id, capability, difficulty_tier, as_of desc"#,
        dimension
    )
    .fetch_all(db)
    .await
}

/// `ListAggregateHistoryPage`: one column's series for a dimension, ascending by
/// (as_of, capability, tier), after the cursor, at most `page_limit` rows.
pub async fn list_aggregate_history_page<'e>(
    db: impl PgExecutor<'e>,
    query: &AggregateHistoryQuery<'_>,
) -> sqlx::Result<Vec<Aggregate>> {
    let cursor_set = query.cursor.is_some();
    let cursor_as_of = query.cursor.map(|c| c.as_of);
    let cursor_capability = query.cursor.map_or("", |c| c.capability.as_str());
    let cursor_tier = query.cursor.map_or(0, |c| c.difficulty_tier);
    sqlx::query_as!(
        Aggregate,
        "select model_column_id, capability, difficulty_tier, dimension, ewma, variance, n,
                measured, matrix_version, as_of
         from aggregates
         where model_column_id = $1
           and dimension = $2
           and ($3::text = '' or capability = $3)
           and ($4::smallint is null or difficulty_tier = $4)
           and ($5::date is null or as_of >= $5)
           and ($6::boolean = false
                or (as_of, capability, difficulty_tier) > ($7::date, $8::text, $9::smallint))
         order by as_of, capability, difficulty_tier
         limit $10",
        query.model_column_id,
        query.dimension,
        query.capability,
        query.difficulty,
        query.since,
        cursor_set,
        cursor_as_of,
        cursor_capability,
        cursor_tier,
        i64::from(query.page_limit),
    )
    .fetch_all(db)
    .await
}

/// `ListAggregatesAtAsOf`: every row of one dimension and date, ordered by column,
/// capability, tier.
pub async fn list_aggregates_at_as_of<'e>(
    db: impl PgExecutor<'e>,
    dimension: &str,
    as_of: NaiveDate,
) -> sqlx::Result<Vec<Aggregate>> {
    sqlx::query_as!(
        Aggregate,
        "select model_column_id, capability, difficulty_tier, dimension, ewma, variance, n,
                measured, matrix_version, as_of
         from aggregates
         where dimension = $1
           and as_of = $2
         order by model_column_id, capability, difficulty_tier",
        dimension,
        as_of
    )
    .fetch_all(db)
    .await
}

/// `ListMeasuredColumnIDs`: the columns with any measured row, ascending.
pub async fn list_measured_column_ids<'e>(db: impl PgExecutor<'e>) -> sqlx::Result<Vec<String>> {
    sqlx::query_scalar!(
        "select distinct model_column_id from aggregates
         where measured
         order by model_column_id"
    )
    .fetch_all(db)
    .await
}

/// `ListLatestCellAggregates`: for one capability, the newest row per (column, tier,
/// dimension), ordered by that key.
pub async fn list_latest_cell_aggregates<'e>(
    db: impl PgExecutor<'e>,
    column_ids: &[String],
    capability: &str,
    dimensions: &[String],
) -> sqlx::Result<Vec<Aggregate>> {
    sqlx::query_as!(
        Aggregate,
        r#"select distinct on (model_column_id, difficulty_tier, dimension)
                  model_column_id as "model_column_id!", capability as "capability!",
                  difficulty_tier as "difficulty_tier!", dimension as "dimension!",
                  ewma as "ewma!", variance as "variance!", n as "n!",
                  measured as "measured!", matrix_version as "matrix_version!",
                  as_of as "as_of!"
           from aggregates
           where model_column_id = any($1::text[])
             and capability = $2
             and dimension = any($3::text[])
           order by model_column_id, difficulty_tier, dimension, as_of desc"#,
        column_ids,
        capability,
        dimensions
    )
    .fetch_all(db)
    .await
}

/// One column's mean availability over a window of nights (D8).
#[derive(Debug, Clone, PartialEq)]
pub struct TrailingAvailability {
    pub model_column_id: String,
    pub availability: f64,
}

/// D8 (Rust only): for each column, the mean of its nightly `availability` rows for one
/// workload cell with `as_of` on or after `since`, ascending by column. A column with no
/// row in the window is absent. Each night's row holds that night's folded value, so this
/// is the mean over the trailing nights (spec §7.3).
pub async fn list_trailing_availability<'e>(
    db: impl PgExecutor<'e>,
    column_ids: &[String],
    capability: &str,
    difficulty_tier: i16,
    since: NaiveDate,
) -> sqlx::Result<Vec<TrailingAvailability>> {
    sqlx::query_as!(
        TrailingAvailability,
        r#"select model_column_id as "model_column_id!",
                  avg(ewma)::float8 as "availability!"
           from aggregates
           where model_column_id = any($1::text[])
             and capability = $2
             and difficulty_tier = $3
             and dimension = 'availability'
             and as_of >= $4
           group by model_column_id
           order by model_column_id"#,
        column_ids,
        capability,
        difficulty_tier,
        since
    )
    .fetch_all(db)
    .await
}

// ---- predictions.sql ----

/// `InsertPredictionLock`: returns 1 when inserted, 0 when the key exists. The first lock
/// wins, so hindsight can never overwrite it.
pub async fn insert_prediction_lock<'e>(
    db: impl PgExecutor<'e>,
    prediction: &Prediction,
) -> sqlx::Result<u64> {
    sqlx::query!(
        "insert into predictions (
           batch_id, model_column_id, capability, difficulty_tier,
           predicted_pass_prob, source, matrix_version, locked_at
         )
         values ($1, $2, $3, $4, $5, $6, $7, $8)
         on conflict (batch_id, model_column_id, capability, difficulty_tier) do nothing",
        prediction.batch_id,
        prediction.model_column_id,
        prediction.capability,
        prediction.difficulty_tier,
        prediction.predicted_pass_prob,
        prediction.source,
        prediction.matrix_version,
        prediction.locked_at,
    )
    .execute(db)
    .await
    .map(|done| done.rows_affected())
}

/// `ListPredictionsByNight`: ordered by column, capability, tier.
pub async fn list_predictions_by_night<'e>(
    db: impl PgExecutor<'e>,
    batch_id: &str,
) -> sqlx::Result<Vec<Prediction>> {
    sqlx::query_as!(
        Prediction,
        "select batch_id, model_column_id, capability, difficulty_tier, predicted_pass_prob,
                source, matrix_version, locked_at
         from predictions
         where batch_id = $1
         order by model_column_id, capability, difficulty_tier",
        batch_id
    )
    .fetch_all(db)
    .await
}

/// `CountPredictionsByNight`.
pub async fn count_predictions_by_night<'e>(
    db: impl PgExecutor<'e>,
    batch_id: &str,
) -> sqlx::Result<i64> {
    sqlx::query_scalar!(
        r#"select count(*) as "count!" from predictions where batch_id = $1"#,
        batch_id
    )
    .fetch_one(db)
    .await
}

// ---- calibration.sql ----

/// `UpsertCalibration`: a retry replaces its own row and sets `scored_at = now()`.
pub async fn upsert_calibration<'e>(
    db: impl PgExecutor<'e>,
    batch_id: &str,
    capability: &str,
    brier_score: f32,
    overconfidence: f32,
    n: i32,
) -> sqlx::Result<Calibration> {
    sqlx::query_as!(
        Calibration,
        "insert into calibration (batch_id, capability, brier_score, overconfidence, n)
         values ($1, $2, $3, $4, $5)
         on conflict (batch_id, capability) do update set
           brier_score = excluded.brier_score,
           overconfidence = excluded.overconfidence,
           n = excluded.n,
           scored_at = now()
         returning batch_id, capability, brier_score, overconfidence, n, scored_at",
        batch_id,
        capability,
        brier_score,
        overconfidence,
        n
    )
    .fetch_one(db)
    .await
}

/// `ListCalibrationPage`: newest batch first, capabilities ascending within a batch,
/// after the cursor, at most `page_limit` rows.
pub async fn list_calibration_page<'e>(
    db: impl PgExecutor<'e>,
    query: &CalibrationPageQuery<'_>,
) -> sqlx::Result<Vec<Calibration>> {
    sqlx::query_as!(
        Calibration,
        "select batch_id, capability, brier_score, overconfidence, n, scored_at
         from calibration
         where ($1::text = '' or capability = $1)
           and ($2::timestamptz is null or scored_at >= $2)
           and ($3::text = ''
                or batch_id < $3
                or (batch_id = $3 and capability > $4::text))
         order by batch_id desc, capability
         limit $5",
        query.capability,
        query.since,
        query.cursor_batch,
        query.cursor_capability,
        i64::from(query.page_limit),
    )
    .fetch_all(db)
    .await
}
