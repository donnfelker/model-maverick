//! `aggregate_rebuild`, `FoldObservations`, and `SaveScores` on real Postgres with the
//! graphile_worker schema (reference Part 3 §1.8, §2.9, and §6).

use std::collections::{BTreeMap, HashSet};
use std::str::FromStr;
use std::sync::LazyLock;
use std::time::Duration;

use chrono::NaiveDate;
use graphile_worker::{JobSpec, WorkerOptions, WorkerUtils};
use maverick_jobs::fold::{CellKey, FoldParams, fold_observations, save_scores};
use maverick_jobs::rebuild::{
    AggregateRebuildJob, RebuildConfig, RebuildOutcome, RebuildOutput, aggregate_rebuild,
};
use maverick_jobs::{
    BatchFollowupArgs, BatchLeaseConfig, BatchLeaseManager, BatchLeaseOutcome, DEFAULT_QUEUE_NAME,
    JobClientConfig, JobKind,
};
use maverick_store::catalog::{self, NewModelColumn};
use maverick_store::corpus::{self, TaskUpsert};
use maverick_store::health::GRAPHILE_WORKER_SCHEMA;
use maverick_store::measurement::{self, CellDimension, NewRun};
use maverick_store::{Decimal, JsonRawValue, MIGRATOR};
use maverick_testdb::Schema;
use serde_json::Value;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

/// The store migrations plus the graphile_worker schema.
///
/// parity: scoring.TestMain
/// parity: jobs.TestMain
static SCHEMA: LazyLock<Schema> = LazyLock::new(|| {
    Schema::sqlx(&MIGRATOR).then(Schema::custom("graphile_worker 0.13", |url| async move {
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await?;
        WorkerUtils::new(pool.clone(), GRAPHILE_WORKER_SCHEMA)
            .migrate()
            .await?;
        pool.close().await;
        Ok(())
    }))
});

const VERSION: &str = "corpus-rebuild";

fn json(text: &str) -> Box<JsonRawValue> {
    JsonRawValue::from_string(text.to_owned()).expect("json")
}

fn day(s: &str) -> NaiveDate {
    s.parse().expect("date")
}

fn config() -> RebuildConfig {
    RebuildConfig {
        matrix_export_enabled: false,
        alpha: 0.3,
        lambda_var: 0.5,
        lambda_amb: 0.25,
        min_nights: 3,
        runner_timeout: Duration::from_secs(120),
        lease: BatchLeaseManager::new(BatchLeaseConfig {
            lease_ttl: Duration::from_secs(600),
            heartbeat_interval: Duration::from_secs(200),
            owner_stale_after: Duration::from_secs(60),
            graphile_schema: GRAPHILE_WORKER_SCHEMA.to_string(),
        }),
    }
}

async fn seed_version(pool: &PgPool) {
    corpus::upsert_corpus_version(pool, VERSION, "", 4, true)
        .await
        .expect("version");
}

struct TaskSeed<'a> {
    id: &'a str,
    tier: i16,
    tags: &'a [&'a str],
    graders: &'a str,
    weights: &'a str,
    pass: f32,
}

async fn seed_task(pool: &PgPool, t: &TaskSeed<'_>) {
    let tags: Vec<String> = t.tags.iter().map(|s| (*s).to_owned()).collect();
    corpus::upsert_task(
        pool,
        &TaskUpsert {
            id: t.id,
            capability: "structured_extraction",
            domain_tags: &tags,
            difficulty_tier: t.tier,
            difficulty_knobs: &json("{}"),
            prompt: "p",
            attachments: &json("[]"),
            temperature: 0.0,
            max_tokens: 64,
            grader_specs: &json(t.graders),
            dimension_weights: &json(t.weights),
            pass_threshold: t.pass,
            smoke_member: false,
            corpus_version: VERSION,
            active: true,
        },
    )
    .await
    .expect("task");
}

const GOLDEN: &str = r#"[{"type":"golden_answer","version":1,"weight_dimensions":{"answer_correctness":1},"spec":{"answers":[{"value":"42"}]}}]"#;

async fn seed_simple_task(pool: &PgPool, id: &str, tier: i16) {
    seed_task(
        pool,
        &TaskSeed {
            id,
            tier,
            tags: &[],
            graders: GOLDEN,
            weights: r#"{"answer_correctness":1.0}"#,
            pass: 0.7,
        },
    )
    .await;
}

async fn seed_column(pool: &PgPool, id: &str) {
    catalog::create_model_column(
        pool,
        &NewModelColumn {
            id,
            provider: "anthropic",
            model_id: &format!("model-{id}"),
            endpoint: "direct",
            settings: &json("{}"),
            status: "active",
            pricing: &json(r#"{"input_per_mtok":3,"output_per_mtok":15}"#),
            quality_source_column_id: None,
            cold_start_until: None,
            deprecation_date: None,
            execution_aliases: None,
            retention_mode: None,
            retention_verified_at: None,
            retention_valid_until: None,
            retention_source: None,
            execution_alias_retention: None,
            param_constraints: None,
        },
    )
    .await
    .expect("column");
}

struct RunSeed<'a> {
    id: &'a str,
    batch: &'a str,
    task: &'a str,
    column: &'a str,
    repeat: i16,
    status: &'a str,
    latency: Option<i32>,
    cost: Option<&'a str>,
}

async fn seed_run(pool: &PgPool, r: &RunSeed<'_>) {
    let ok = r.status == "ok";
    measurement::insert_run(
        pool,
        &NewRun {
            id: r.id,
            batch_id: r.batch,
            task_id: r.task,
            model_column_id: r.column,
            repeat_index: r.repeat,
            output: ok.then_some("42"),
            tokens_in: ok.then_some(10),
            tokens_out: ok.then_some(2),
            latency_ms: r.latency,
            cost: r.cost.map(|c| Decimal::from_str(c).expect("decimal")),
            temperature: 0.0,
            status: r.status,
            error: (!ok).then_some("boom"),
            matrix_version: VERSION,
        },
    )
    .await
    .expect("run");
}

async fn score(pool: &PgPool, run: &str, grader: i16, dims: &[(&str, f64)], detail: &str) {
    let mut conn = pool.acquire().await.expect("conn");
    let dims: BTreeMap<String, f64> = dims.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect();
    save_scores(&mut conn, run, grader, &dims, Some(&json(detail)))
        .await
        .expect("scores");
}

fn args(night: &str, batch: &str) -> BatchFollowupArgs {
    BatchFollowupArgs {
        night: night.to_owned(),
        batch_id: batch.to_owned(),
    }
}

async fn rebuild(pool: &PgPool, night: &str, batch: &str) -> RebuildOutput {
    match aggregate_rebuild(pool, &args(night, batch), &config(), "worker-test")
        .await
        .expect("rebuild")
    {
        RebuildOutcome::Rebuilt(output) => output,
        other => panic!("not rebuilt: {other:?}"),
    }
}

async fn series(
    pool: &PgPool,
    column: &str,
    tier: i16,
    dimension: &str,
    before: &str,
) -> Vec<measurement::Aggregate> {
    measurement::list_aggregate_series_before(
        pool,
        &CellDimension {
            model_column_id: column,
            capability: "structured_extraction",
            difficulty_tier: tier,
            dimension,
        },
        day(before),
    )
    .await
    .expect("series")
}

async fn latest(pool: &PgPool, column: &str, tier: i16, dimension: &str) -> measurement::Aggregate {
    series(pool, column, tier, dimension, "2100-01-01")
        .await
        .pop()
        .unwrap_or_else(|| panic!("no {dimension} row"))
}

fn fold_fixture(id: &str) -> Value {
    let doc = parity_support::fixture("measurement-core", "ewma.json").expect("fixture");
    doc["fold_observations_via_float32_storage"]
        .as_array()
        .expect("rows")
        .iter()
        .find(|r| r["id"] == id)
        .expect("row")
        .clone()
}

fn cell(column: &str, tier: i16) -> CellKey {
    CellKey {
        model_column_id: column.to_owned(),
        capability: "structured_extraction".to_owned(),
        difficulty_tier: tier,
    }
}

/// Bit-exact float32 storage: resume from stored EWMAs, fold, store as `real`.
///
/// parity: scoring.TestFoldObservationsThreeNights
#[tokio::test]
async fn fold_observations_three_nights() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed_column(&pool, "col_fold").await;
    let case = fold_fixture("three_nights");
    let params = FoldParams {
        alpha: case["alpha"].as_f64().expect("alpha"),
        min_nights: case["min_nights"].as_u64().expect("min"),
    };
    let dimension = case["dimension"].as_str().expect("dimension");
    let mut conn = pool.acquire().await.expect("conn");
    let nights = case["nights"].as_array().expect("nights");
    for (i, night) in nights.iter().enumerate() {
        let x = night["x"].as_f64().expect("x");
        let as_of = day(night["as_of"].as_str().expect("as_of"));
        let written = fold_observations(
            &mut conn,
            &cell("col_fold", 3),
            &BTreeMap::from([(dimension, x)]),
            as_of,
            VERSION,
            &params,
        )
        .await
        .expect("fold");
        assert_eq!(written, 1);
        let row = latest(&pool, "col_fold", 3, dimension).await;
        assert_eq!(f64::from(row.ewma), night["stored_ewma_f32"], "night {i}");
        assert_eq!(
            f64::from(row.variance),
            night["stored_variance_f32"],
            "night {i}"
        );
        assert_eq!(row.n, night["n"], "night {i}");
        assert_eq!(row.measured, night["measured"], "night {i}");
        if i == 1 {
            // The approximate Go assertions.
            assert!((f64::from(row.ewma) - 0.74).abs() < 1e-5);
            assert!((f64::from(row.variance) - 0.01).abs() < 1e-5);
        }
    }
    // Re-folding the last night replaces its row.
    let last = nights.last().expect("last");
    fold_observations(
        &mut conn,
        &cell("col_fold", 3),
        &BTreeMap::from([(dimension, last["x"].as_f64().expect("x"))]),
        day(last["as_of"].as_str().expect("as_of")),
        VERSION,
        &params,
    )
    .await
    .expect("refold");
    let refold = &case["refold_last_night"];
    let rows = series(&pool, "col_fold", 3, dimension, "2100-01-01").await;
    assert_eq!(
        rows.len() as u64,
        refold["rows_total"].as_u64().expect("rows")
    );
    let row = rows.last().expect("row");
    assert_eq!(f64::from(row.ewma), refold["stored_ewma_f32"]);
    assert_eq!(f64::from(row.variance), refold["stored_variance_f32"]);
    assert_eq!(row.n, refold["n"]);
    assert!((f64::from(row.variance) - 0.0155556).abs() < 1e-5);
    drop(conn);
    db.close().await;
}

/// parity: scoring.TestFoldObservationsMultipleDimensions
#[tokio::test]
async fn fold_observations_multiple_dimensions() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed_column(&pool, "col_multi").await;
    let mut conn = pool.acquire().await.expect("conn");
    let observations = BTreeMap::from([
        ("quality", 0.8),
        ("pass_rate", 0.5),
        ("latency_p95_ms", 1450.0),
    ]);
    let written = fold_observations(
        &mut conn,
        &cell("col_multi", 2),
        &observations,
        day("2026-07-01"),
        VERSION,
        &FoldParams {
            alpha: 0.3,
            min_nights: 3,
        },
    )
    .await
    .expect("fold");
    assert_eq!(written, 3);
    let latency = fold_fixture("latency_single");
    for (dimension, x) in observations {
        let row = latest(&pool, "col_multi", 2, dimension).await;
        assert_eq!(row.n, 1);
        assert_eq!(row.variance, 0.0);
        #[allow(clippy::cast_possible_truncation)]
        let stored = x as f32;
        assert_eq!(row.ewma, stored, "{dimension}");
        assert!(!row.measured);
    }
    assert_eq!(latency["nights"][0]["stored_ewma_f32"], 1450.0);
    drop(conn);
    db.close().await;
}

/// parity: scoring.TestSaveScoresRoundTrip
#[tokio::test]
async fn save_scores_round_trip() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed_version(&pool).await;
    seed_simple_task(&pool, "task_s", 1).await;
    seed_column(&pool, "col_s").await;
    seed_run(
        &pool,
        &RunSeed {
            id: "run_s",
            batch: "bat_s",
            task: "task_s",
            column: "col_s",
            repeat: 0,
            status: "ok",
            latency: Some(10),
            cost: None,
        },
    )
    .await;
    let detail = r#"{"missing":["effective_date"]}"#;
    score(
        &pool,
        "run_s",
        1,
        &[("schema_validity", 0.9), ("field_accuracy", 0.4)],
        detail,
    )
    .await;
    let rows = measurement::list_scores_by_run(&pool, "run_s")
        .await
        .expect("scores");
    let got: Vec<(i16, &str, f32)> = rows
        .iter()
        .map(|s| (s.grader_idx, s.dimension.as_str(), s.value))
        .collect();
    assert_eq!(got.len(), 2);
    assert_eq!((got[0].0, got[0].1), (1, "field_accuracy"));
    assert!((f64::from(got[0].2) - 0.4).abs() < 1e-6);
    assert_eq!((got[1].0, got[1].1), (1, "schema_validity"));
    assert!((f64::from(got[1].2) - 0.9).abs() < 1e-6);
    for row in &rows {
        let stored: Value =
            serde_json::from_str(row.detail.as_ref().expect("detail").get()).expect("json");
        assert_eq!(stored, serde_json::from_str::<Value>(detail).expect("json"));
    }
    // Values clamp to [0, 1].
    score(&pool, "run_s", 2, &[("a", 1.5), ("b", -0.5)], "{}").await;
    let clamped: Vec<f32> = measurement::list_scores_by_run(&pool, "run_s")
        .await
        .expect("scores")
        .iter()
        .filter(|s| s.grader_idx == 2)
        .map(|s| s.value)
        .collect();
    assert_eq!(clamped, [1.0, 0.0]);
    db.close().await;
}

async fn enqueued(pool: &PgPool) -> Vec<(String, Value, Option<String>)> {
    sqlx::query_as::<_, (String, sqlx::types::Json<Value>, Option<String>)>(
        "select tasks.identifier, jobs.payload, jobs.key
         from graphile_worker._private_jobs as jobs
         join graphile_worker._private_tasks as tasks on tasks.id = jobs.task_id
         order by jobs.id",
    )
    .fetch_all(pool)
    .await
    .expect("jobs")
    .into_iter()
    .map(|(task, payload, key)| (task, payload.0, key))
    .collect()
}

/// D6 adds the availability row (Go wrote 6 rows per cell). D10 makes a second delivery
/// of the same job a no-op; each night has its own batch id.
///
/// parity: jobs.TestAggregateRebuildFoldsAndStaysIdempotent
#[tokio::test]
async fn folds_and_stays_idempotent() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed_version(&pool).await;
    seed_simple_task(&pool, "task_r", 2).await;
    seed_column(&pool, "col_r").await;
    for (batch, prefix) in [("bat_night1", "a"), ("bat_night2", "b")] {
        for repeat in 0..3 {
            let id = format!("run_{prefix}{repeat}");
            seed_run(
                &pool,
                &RunSeed {
                    id: &id,
                    batch,
                    task: "task_r",
                    column: "col_r",
                    repeat,
                    status: "ok",
                    latency: Some(120),
                    cost: Some("0.000450"),
                },
            )
            .await;
            score(&pool, &id, 0, &[("answer_correctness", 1.0)], "{}").await;
        }
    }
    let output = rebuild(&pool, "2030-06-10", "bat_night1").await;
    let _ = parity_support::divergence!("D6");
    assert_eq!(
        output,
        RebuildOutput {
            batch_id: "bat_night1".to_owned(),
            runs: 3,
            cells: 1,
            aggregate_rows: 7,
        }
    );
    let quality = latest(&pool, "col_r", 2, "quality").await;
    assert_eq!((quality.ewma, quality.n, quality.measured), (1.0, 1, false));
    let availability = latest(&pool, "col_r", 2, "availability").await;
    assert_eq!(availability.ewma, 1.0);
    assert_eq!(
        latest(&pool, "col_r", 2, "latency_p95_ms").await.ewma,
        120.0
    );
    assert!(
        (f64::from(latest(&pool, "col_r", 2, "cost_per_run_usd").await.ewma) - 0.00045).abs()
            < 1e-9
    );

    // A second delivery finds the lease completed and does nothing.
    let _ = parity_support::divergence!("D10");
    let again = aggregate_rebuild(
        &pool,
        &args("2030-06-10", "bat_night1"),
        &config(),
        "worker-2",
    )
    .await
    .expect("again");
    assert_eq!(again, RebuildOutcome::AlreadyFinished);
    assert_eq!(
        series(&pool, "col_r", 2, "quality", "2100-01-01")
            .await
            .len(),
        1
    );

    // The next night folds on top.
    rebuild(&pool, "2030-06-11", "bat_night2").await;
    let quality = latest(&pool, "col_r", 2, "quality").await;
    assert_eq!((quality.ewma, quality.n), (1.0, 2));
    assert_eq!(latest(&pool, "col_r", 2, "pass_rate").await.ewma, 1.0);

    // Each rebuild chained regression_scan then calibration_score (D11).
    let jobs = enqueued(&pool).await;
    let expected: Vec<(String, Value, Option<String>)> = ["bat_night1", "bat_night2"]
        .iter()
        .zip(["2030-06-10", "2030-06-11"])
        .flat_map(|(batch, night)| {
            [JobKind::RegressionScan, JobKind::CalibrationScore].map(|kind| {
                (
                    kind.as_str().to_owned(),
                    serde_json::json!({"night": night, "batch_id": batch}),
                    Some(kind.job_key(batch)),
                )
            })
        })
        .collect();
    assert_eq!(jobs, expected);
    db.close().await;
}

/// parity: jobs.TestAggregateRebuildHighStakesOverlay
#[tokio::test]
async fn high_stakes_overlay() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed_version(&pool).await;
    seed_column(&pool, "col_hs").await;
    let graders = r#"[{"type":"golden_answer","version":1,"weight_dimensions":{"answer_correctness":1},"spec":{"answers":[{"value":"42"}]}},{"type":"rule_scan","version":1,"weight_dimensions":{"constraint_compliance":1},"spec":{"rules":[{"must_not_include":"x","weight":1,"critical":true}]}}]"#;
    let critical = r#"{"rules":[{"index":0,"kind":"must_not_include","pass":false,"weight":1,"critical":true}],"score":0,"critical_failed":true}"#;
    let tags: [&[&str]; 4] = [&["legal"], &["real_estate"], &["medical"], &[]];
    for (i, tags) in tags.iter().enumerate() {
        let tier = i16::try_from(i + 1).expect("tier");
        let task = format!("task_hs{tier}");
        seed_task(
            &pool,
            &TaskSeed {
                id: &task,
                tier,
                tags,
                graders,
                weights: r#"{"answer_correctness":0.5,"constraint_compliance":0.5}"#,
                pass: 0.75,
            },
        )
        .await;
        let run = format!("run_hs{tier}");
        seed_run(
            &pool,
            &RunSeed {
                id: &run,
                batch: "bat_hs",
                task: &task,
                column: "col_hs",
                repeat: 0,
                status: "ok",
                latency: Some(50),
                cost: None,
            },
        )
        .await;
        if tier <= 2 {
            score(&pool, &run, 0, &[("answer_correctness", 1.0)], "{}").await;
            score(&pool, &run, 1, &[("constraint_compliance", 0.4)], "{}").await;
        } else {
            score(&pool, &run, 0, &[("answer_correctness", 1.0)], critical).await;
            score(&pool, &run, 1, &[("constraint_compliance", 1.0)], critical).await;
        }
    }
    rebuild(&pool, "2030-07-01", "bat_hs").await;
    for (tier, quality, pass) in [
        (1, 0.5 / 0.65, 1.0),
        (2, 0.70, 0.0),
        (3, 0.0, 0.0),
        (4, 1.0, 1.0),
    ] {
        let q = f64::from(latest(&pool, "col_hs", tier, "quality").await.ewma);
        let p = f64::from(latest(&pool, "col_hs", tier, "pass_rate").await.ewma);
        assert!((q - quality).abs() < 1e-6, "tier {tier}: quality {q}");
        assert!((p - pass).abs() < 1e-6, "tier {tier}: pass {p}");
    }
    db.close().await;
}

async fn seed_runs(
    pool: &PgPool,
    batch: &str,
    task: &str,
    column: &str,
    runs: &[(&str, Option<i32>)],
) {
    for (repeat, (status, latency)) in runs.iter().enumerate() {
        let repeat = i16::try_from(repeat).expect("repeat");
        let id = format!("run_{batch}_{repeat}");
        seed_run(
            pool,
            &RunSeed {
                id: &id,
                batch,
                task,
                column,
                repeat,
                status,
                latency: *latency,
                cost: None,
            },
        )
        .await;
        if *status == "ok" {
            score(pool, &id, 0, &[("answer_correctness", 1.0)], "{}").await;
        }
    }
}

/// Errors are pass failures but not quality evidence; D6 counts them in availability.
///
/// parity: jobs.TestAggregateRebuildZeroesFailedRuns
#[tokio::test]
async fn zeroes_failed_runs() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed_version(&pool).await;
    seed_simple_task(&pool, "task_z", 1).await;
    seed_column(&pool, "col_z").await;
    seed_runs(
        &pool,
        "bat_z",
        "task_z",
        "col_z",
        &[("ok", Some(90)), ("provider_error", None)],
    )
    .await;
    rebuild(&pool, "2030-08-01", "bat_z").await;
    assert_eq!(latest(&pool, "col_z", 1, "quality").await.ewma, 1.0);
    assert_eq!(latest(&pool, "col_z", 1, "pass_rate").await.ewma, 0.5);
    let _ = parity_support::divergence!("D6");
    assert_eq!(latest(&pool, "col_z", 1, "availability").await.ewma, 0.5);
    assert_eq!(latest(&pool, "col_z", 1, "latency_p95_ms").await.ewma, 90.0);
    db.close().await;
}

/// D7: a timeout's latency enters the percentiles censored at the runner timeout.
#[tokio::test]
async fn timeouts_censor_latency() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed_version(&pool).await;
    seed_simple_task(&pool, "task_t", 5).await;
    seed_column(&pool, "col_t").await;
    seed_runs(
        &pool,
        "bat_t",
        "task_t",
        "col_t",
        &[("ok", Some(90)), ("timeout", Some(4000)), ("timeout", None)],
    )
    .await;
    rebuild(&pool, "2030-08-02", "bat_t").await;
    let _ = parity_support::divergence!("D7");
    assert_eq!(
        latest(&pool, "col_t", 5, "latency_p50_ms").await.ewma,
        120_000.0
    );
    assert_eq!(
        latest(&pool, "col_t", 5, "latency_p95_ms").await.ewma,
        120_000.0
    );
    let availability = f64::from(latest(&pool, "col_t", 5, "availability").await.ewma);
    assert!((availability - 1.0 / 3.0).abs() < 1e-6);
    db.close().await;
}

/// A batch with no runs completes and still chains its follow-ups.
#[tokio::test]
async fn empty_batch_still_chains() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let output = rebuild(&pool, "2030-09-01", "bat_empty").await;
    assert_eq!(
        output,
        RebuildOutput {
            batch_id: "bat_empty".to_owned(),
            runs: 0,
            cells: 0,
            aggregate_rows: 0,
        }
    );
    let kinds: HashSet<String> = enqueued(&pool).await.into_iter().map(|j| j.0).collect();
    assert_eq!(
        kinds,
        HashSet::from(["regression_scan".to_owned(), "calibration_score".to_owned()])
    );
    db.close().await;
}

/// D10: while another live worker holds the lease, a duplicate delivery does nothing.
#[tokio::test]
async fn held_lease_is_a_no_op() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let config = config();
    let held = config
        .lease
        .claim(&pool, JobKind::AggregateRebuild, "bat_held", "worker-a")
        .await
        .expect("claim");
    assert!(matches!(held, BatchLeaseOutcome::Claimed(_)));
    sqlx::query("SELECT graphile_worker.worker_heartbeat($1)")
        .bind("worker-a")
        .execute(&pool)
        .await
        .expect("register worker");
    let outcome = aggregate_rebuild(&pool, &args("2030-09-02", "bat_held"), &config, "worker-b")
        .await
        .expect("rebuild");
    assert_eq!(outcome, RebuildOutcome::HeldByOther);
    assert!(enqueued(&pool).await.is_empty());
    db.close().await;
}

/// The graphile task end to end: the worker carries the config as an extension.
#[tokio::test]
async fn task_handler_runs_through_graphile() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed_version(&pool).await;
    seed_simple_task(&pool, "task_g", 4).await;
    seed_column(&pool, "col_g").await;
    seed_run(
        &pool,
        &RunSeed {
            id: "run_g",
            batch: "bat_g",
            task: "task_g",
            column: "col_g",
            repeat: 0,
            status: "ok",
            latency: Some(33),
            cost: None,
        },
    )
    .await;
    score(&pool, "run_g", 0, &[("answer_correctness", 1.0)], "{}").await;
    assert_eq!(
        <AggregateRebuildJob as graphile_worker::TaskHandler>::IDENTIFIER,
        JobKind::AggregateRebuild.as_str()
    );
    let worker = WorkerOptions::default()
        .concurrency(1)
        .schema(GRAPHILE_WORKER_SCHEMA)
        .pg_pool(pool.clone())
        .add_extension(config())
        .define_job::<AggregateRebuildJob>()
        .init()
        .await
        .expect("worker");
    worker
        .create_utils()
        .add_job(
            AggregateRebuildJob(args("2030-10-01", "bat_g")),
            JobSpec::builder()
                .queue_name(DEFAULT_QUEUE_NAME)
                .job_key(JobKind::AggregateRebuild.job_key("bat_g"))
                .max_attempts(JobKind::AggregateRebuild.max_attempts())
                .build(),
        )
        .await
        .expect("enqueue");
    worker.run_once().await.expect("run once");
    assert_eq!(latest(&pool, "col_g", 4, "quality").await.ewma, 1.0);
    let kinds: Vec<String> = enqueued(&pool).await.into_iter().map(|j| j.0).collect();
    assert!(kinds.contains(&"regression_scan".to_owned()), "{kinds:?}");
    assert!(kinds.contains(&"calibration_score".to_owned()), "{kinds:?}");
    db.close().await;
}

/// parity: jobs.TestAggregateRebuildTimeoutUsesAnalyticsJobTimeout
#[test]
fn timeout_uses_the_analytics_job_timeout() {
    let config = JobClientConfig::default();
    assert_eq!(
        JobKind::AggregateRebuild.timeout(&config),
        Duration::from_secs(15 * 60)
    );
    assert_eq!(
        JobKind::AggregateRebuild.timeout(&config),
        config.analytics_job_timeout
    );
}

/// DIVERGENCE: D49. The export identity is committed with the rebuild, once.
#[tokio::test]
async fn enabled_origin_chains_export_with_the_rebuild_completion_time() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed_version(&pool).await;
    let mut config = config();
    config.matrix_export_enabled = true;
    let args = BatchFollowupArgs {
        night: "2030-09-01".into(),
        batch_id: "bat_export".into(),
    };
    aggregate_rebuild(&pool, &args, &config, "worker-export")
        .await
        .unwrap();
    let exports:Vec<serde_json::Value>=sqlx::query_scalar("select j.payload from graphile_worker._private_jobs j join graphile_worker._private_tasks t on t.id=j.task_id where t.identifier='matrix_export'").fetch_all(&pool).await.unwrap();
    assert_eq!(exports.len(), 1);
    let export: maverick_jobs::matrix_export::MatrixExportArgs =
        serde_json::from_value(exports[0].clone()).unwrap();
    let completion: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("select completed_at from batches where id=$1")
            .bind(&export.rebuild_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(export.published_at, completion);
    assert_eq!(export.matrix_version, VERSION);
    assert_eq!(export.night, args.night);
    assert!(matches!(
        aggregate_rebuild(&pool, &args, &config, "worker-export")
            .await
            .unwrap(),
        RebuildOutcome::AlreadyFinished
    ));
    assert_eq!(enqueued(&pool).await.len(), 3);
    pool.close().await;
    db.close().await;
}
