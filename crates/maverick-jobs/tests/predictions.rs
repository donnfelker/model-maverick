//! `prediction_lock` and `calibration_score` on real Postgres with the fake provider and
//! the graphile_worker schema (reference Part 3 §2.8, §2.11, and §6).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use chrono::{NaiveDate, Utc};
use graphile_worker::{JobSpec, WorkerOptions, WorkerUtils};
use maverick_alerts::AlertNotifier;
use maverick_domain::graders::{GoldenAnswer, Registry};
use maverick_domain::spend::SpendCaps;
use maverick_jobs::batch::{BatchConfig, BatchOutcome, BatchOutput, NightlySmokeJob, run_batch};
use maverick_jobs::fold::save_scores;
use maverick_jobs::predictions::{
    CalibrationConfig, CalibrationOutcome, CalibrationOutput, CalibrationScoreJob,
    PredictionLockConfig, PredictionLockJob, PredictionLockOutcome, PredictionLockOutput,
    calibration_score, lock_night, prediction_lock, score_calibration,
};
use maverick_jobs::rebuild::{AggregateRebuildJob, RebuildConfig};
use maverick_jobs::regression_scan::{RegressionScanConfig, RegressionScanJob};
use maverick_jobs::{
    BatchFollowupArgs, BatchLeaseConfig, BatchLeaseManager, DEFAULT_QUEUE_NAME, JobArgs,
    JobClientConfig, JobKind, NightlySmokeArgs, PredictionLockArgs,
};
use maverick_providers::fake::{FakeProvider, FixtureSpec};
use maverick_runner::{Cancellation, Clients, RunnerConfig};
use maverick_store::alerts::Alert;
use maverick_store::catalog::{self, NewModelColumn};
use maverick_store::corpus::{self, TaskUpsert};
use maverick_store::health::GRAPHILE_WORKER_SCHEMA;
use maverick_store::measurement::{self, AggregateUpsert, NewRun};
use maverick_store::{JsonRawValue, MIGRATOR};
use maverick_testdb::Schema;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

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

const WEEKLY: &str = "0 4 * * 0";
const WARM_MODEL: &str = "claude-predict";
const COLD_MODEL: &str = "gpt-predict";

fn json(text: &str) -> Box<JsonRawValue> {
    JsonRawValue::from_string(text.to_owned()).expect("json")
}

fn day(s: &str) -> NaiveDate {
    s.parse().expect("date")
}

#[derive(Default)]
struct Counter(AtomicUsize);

impl AlertNotifier for Counter {
    fn notify(&self, _alert: &Alert) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn lease() -> BatchLeaseManager {
    BatchLeaseManager::new(BatchLeaseConfig {
        lease_ttl: Duration::from_secs(600),
        heartbeat_interval: Duration::from_secs(200),
        owner_stale_after: Duration::from_secs(60),
        graphile_schema: GRAPHILE_WORKER_SCHEMA.to_string(),
    })
}

fn lock_config() -> PredictionLockConfig {
    PredictionLockConfig {
        cron_weekly: WEEKLY.to_owned(),
        lease: lease(),
    }
}

fn calibration_config() -> CalibrationConfig {
    CalibrationConfig { lease: lease() }
}

struct SeedTask {
    id: &'static str,
    capability: &'static str,
    tier: i16,
    smoke: bool,
    prompt: &'static str,
    answer: &'static str,
}

/// The smoke corpus of the batch tests: t1 and t4 are smoke members, t3 is not.
const SMOKE_TASKS: [SeedTask; 3] = [
    SeedTask {
        id: "task_t1",
        capability: "structured_extraction",
        tier: 1,
        smoke: true,
        prompt: "What is six times seven? Answer with a number.",
        answer: "42",
    },
    SeedTask {
        id: "task_t3",
        capability: "structured_extraction",
        tier: 3,
        smoke: false,
        prompt: "How many days are in a week? Answer with a number.",
        answer: "7",
    },
    SeedTask {
        id: "task_t4",
        capability: "structured_extraction",
        tier: 4,
        smoke: true,
        prompt: "What color is a clear daytime sky? Answer with one word.",
        answer: "blue",
    },
];

fn golden(answer: &str) -> String {
    format!(
        r#"[{{"type":"golden_answer","version":1,"weight_dimensions":{{"answer_correctness":1}},"spec":{{"answers":[{{"value":"{answer}"}}]}}}}]"#
    )
}

async fn seed_version(pool: &PgPool, version: &str) {
    corpus::upsert_corpus_version(pool, version, "prediction tests", 3, true)
        .await
        .expect("version");
}

async fn seed_task(pool: &PgPool, version: &str, task: &SeedTask) {
    let tags: Vec<String> = Vec::new();
    corpus::upsert_task(
        pool,
        &TaskUpsert {
            id: task.id,
            capability: task.capability,
            domain_tags: &tags,
            difficulty_tier: task.tier,
            difficulty_knobs: &json("{}"),
            prompt: task.prompt,
            attachments: &json("[]"),
            temperature: 0.0,
            max_tokens: 200,
            grader_specs: &json(&golden(task.answer)),
            dimension_weights: &json(r#"{"answer_correctness":1.0}"#),
            pass_threshold: 0.7,
            smoke_member: task.smoke,
            corpus_version: version,
            active: true,
        },
    )
    .await
    .expect("task");
}

async fn seed_column(pool: &PgPool, id: &str, provider: &str, model: &str, status: &str) {
    catalog::create_model_column(
        pool,
        &NewModelColumn {
            id,
            provider,
            model_id: model,
            endpoint: "direct",
            settings: &json("{}"),
            status,
            pricing: &json(r#"{"input_per_mtok": 3.0, "output_per_mtok": 15.0}"#),
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

/// The smoke fixture: t1, t3, t4, a warm anthropic column, a cold-start openai column.
async fn seed_smoke(pool: &PgPool, version: &str) {
    seed_version(pool, version).await;
    for task in &SMOKE_TASKS {
        seed_task(pool, version, task).await;
    }
    seed_column(pool, "col_warm", "anthropic", WARM_MODEL, "active").await;
    seed_column(pool, "col_cold", "openai", COLD_MODEL, "cold_start").await;
}

async fn seed_pass_rate(
    pool: &PgPool,
    column: &str,
    capability: &str,
    tier: i16,
    ewma: f32,
    as_of: &str,
) {
    measurement::upsert_aggregate(
        pool,
        &AggregateUpsert {
            model_column_id: column,
            capability,
            difficulty_tier: tier,
            dimension: "pass_rate",
            ewma,
            variance: 0.0,
            n: 1,
            measured: false,
            matrix_version: "corpus-seed",
            as_of: day(as_of),
        },
    )
    .await
    .expect("aggregate");
}

fn fake_answers() -> Arc<FakeProvider> {
    let mut specs = Vec::new();
    for model in [WARM_MODEL, COLD_MODEL] {
        for task in &SMOKE_TASKS {
            let spec = serde_json::json!({"model": model, "prompt": task.prompt, "output": task.answer, "tokens_in": 100, "tokens_out": 10});
            specs.push(serde_json::from_value::<FixtureSpec>(spec).expect("fixture"));
        }
    }
    Arc::new(FakeProvider::from_fixtures(&specs).expect("fake"))
}

fn batch_config(counter: &Arc<Counter>) -> BatchConfig {
    let fake = fake_answers();
    let mut registry = Registry::new();
    registry.must_register(Arc::new(GoldenAnswer));
    BatchConfig {
        repeats: 3,
        caps: SpendCaps::default(),
        lease: lease(),
        runner: RunnerConfig {
            timeout: Duration::from_secs(30),
            ..RunnerConfig::default()
        },
        clients: Clients::new()
            .with("anthropic", Arc::clone(&fake) as _)
            .with("openai", fake),
        graders: Arc::new(registry),
        notifier: Some(Arc::clone(counter) as Arc<dyn AlertNotifier>),
    }
}

fn smoke(night: &str) -> JobArgs {
    JobArgs::NightlySmoke(NightlySmokeArgs {
        night: night.to_owned(),
    })
}

async fn ran(pool: &PgPool, night: &str, config: &BatchConfig) -> BatchOutput {
    match run_batch(
        pool,
        &smoke(night),
        config,
        "worker-test",
        &Cancellation::new(),
    )
    .await
    .expect("batch")
    {
        BatchOutcome::Ran(output) => output,
        other => panic!("not run: {other:?}"),
    }
}

async fn locked(pool: &PgPool, night: &str) -> PredictionLockOutput {
    match prediction_lock(
        pool,
        &PredictionLockArgs {
            night: night.to_owned(),
        },
        &lock_config(),
        "worker-test",
    )
    .await
    .expect("lock")
    {
        PredictionLockOutcome::Locked(output) => output,
        other => panic!("not locked: {other:?}"),
    }
}

async fn relock(pool: &PgPool, night: &str) -> PredictionLockOutput {
    let mut conn = pool.acquire().await.expect("conn");
    lock_night(pool, &mut conn, day(night), WEEKLY)
        .await
        .expect("relock")
}

async fn calibrated(pool: &PgPool, night: &str, batch: &str) -> CalibrationOutput {
    match calibration_score(
        pool,
        &BatchFollowupArgs {
            night: night.to_owned(),
            batch_id: batch.to_owned(),
        },
        &calibration_config(),
        "worker-test",
    )
    .await
    .expect("calibration")
    {
        CalibrationOutcome::Scored(output) => output,
        other => panic!("not scored: {other:?}"),
    }
}

/// `(batch_id, capability, brier, overconfidence, n)` ordered by batch and capability.
async fn calibration_rows(pool: &PgPool) -> Vec<(String, String, f32, f32, i32)> {
    sqlx::query_as::<_, (String, String, f32, f32, i32)>(
        "select batch_id, capability, brier_score, overconfidence, n
         from calibration order by batch_id, capability",
    )
    .fetch_all(pool)
    .await
    .expect("calibration rows")
}

fn close(actual: f32, expected: f64) -> bool {
    (f64::from(actual) - expected).abs() < 1e-6
}

/// parity: jobs.TestPredictionLockDerivesAndIsImmutable
#[tokio::test]
async fn prediction_lock_derives_and_is_immutable() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed_smoke(&pool, "corpus-predlock").await;
    for (column, tier, ewma) in [
        ("col_warm", 1, 0.8),
        ("col_warm", 4, 0.4),
        ("col_warm", 3, 0.9),
        ("col_cold", 1, 0.7),
    ] {
        seed_pass_rate(
            &pool,
            column,
            "structured_extraction",
            tier,
            ewma,
            "2030-05-14",
        )
        .await;
    }
    let before = Utc::now();
    let out = locked(&pool, "2030-05-15").await;
    assert_eq!(
        out,
        PredictionLockOutput {
            night: "2030-05-15".to_owned(),
            sweep_night: false,
            scheduled_cells: 5,
            locked_cells: 3,
            inserted_rows: 3,
        }
    );
    let rows = measurement::list_predictions_by_night(&pool, "2030-05-15")
        .await
        .expect("predictions");
    let cells: Vec<(&str, i16, f32)> = rows
        .iter()
        .map(|p| {
            (
                p.model_column_id.as_str(),
                p.difficulty_tier,
                p.predicted_pass_prob,
            )
        })
        .collect();
    assert_eq!(
        cells,
        [
            ("col_cold", 1, 0.7),
            ("col_warm", 1, 0.8),
            ("col_warm", 4, 0.4)
        ]
    );
    for row in &rows {
        assert_eq!(row.source, "ewma");
        assert_eq!(row.matrix_version, "corpus-predlock");
        assert_eq!(row.capability, "structured_extraction");
        assert!(row.locked_at >= before - chrono::Duration::seconds(5));
    }

    // The leased re-delivery is a no-op (D10); the Go re-run inserts nothing.
    let _ = parity_support::divergence!("D10");
    let again = prediction_lock(
        &pool,
        &PredictionLockArgs {
            night: "2030-05-15".to_owned(),
        },
        &lock_config(),
        "worker-2",
    )
    .await
    .expect("again");
    assert_eq!(again, PredictionLockOutcome::AlreadyFinished);
    let rerun = relock(&pool, "2030-05-15").await;
    assert_eq!((rerun.locked_cells, rerun.inserted_rows), (3, 0));

    // Hindsight cannot move a locked prediction.
    seed_pass_rate(
        &pool,
        "col_warm",
        "structured_extraction",
        1,
        0.95,
        "2030-05-15",
    )
    .await;
    assert_eq!(relock(&pool, "2030-05-15").await.inserted_rows, 0);
    let warm_t1 = measurement::list_predictions_by_night(&pool, "2030-05-15")
        .await
        .expect("predictions")
        .into_iter()
        .find(|p| p.model_column_id == "col_warm" && p.difficulty_tier == 1)
        .expect("warm t1");
    assert_eq!(warm_t1.predicted_pass_prob, 0.8);
    db.close().await;
}

/// parity: jobs.TestPredictionLockSweepNightWidens
#[tokio::test]
async fn prediction_lock_sweep_night_widens() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed_smoke(&pool, "corpus-predlock").await;
    seed_pass_rate(
        &pool,
        "col_warm",
        "structured_extraction",
        3,
        0.9,
        "2030-06-01",
    )
    .await;
    let out = locked(&pool, "2030-06-02").await;
    assert!(out.sweep_night, "2030-06-02 is a Sunday");
    assert_eq!(
        (out.scheduled_cells, out.locked_cells, out.inserted_rows),
        (6, 1, 1)
    );
    let rows = measurement::list_predictions_by_night(&pool, "2030-06-02")
        .await
        .expect("predictions");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].model_column_id.as_str(), rows[0].difficulty_tier),
        ("col_warm", 3)
    );
    assert_eq!(rows[0].predicted_pass_prob, 0.9);
    db.close().await;
}

/// parity: jobs.TestLockBeforeRunOrdering
#[tokio::test]
async fn lock_before_run_ordering() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed_smoke(&pool, "corpus-order").await;
    let counter = Arc::new(Counter::default());
    let config = batch_config(&counter);

    // Night 1 has no lock: the batch runs unscored and calibration skips.
    let first = ran(&pool, "2030-07-08", &config).await;
    assert!(first.lock_missing);
    let skipped = calibrated(&pool, "2030-07-08", &first.batch_id).await;
    assert!(skipped.skipped);
    assert_eq!((skipped.pairs, skipped.calibration_rows), (0, 0));
    assert!(calibration_rows(&pool).await.is_empty());

    // Night 2 locks before it runs.
    for (column, tier, ewma) in [
        ("col_warm", 1, 0.8),
        ("col_warm", 4, 0.4),
        ("col_cold", 1, 0.7),
        ("col_cold", 3, 0.6),
        ("col_cold", 4, 0.5),
    ] {
        seed_pass_rate(
            &pool,
            column,
            "structured_extraction",
            tier,
            ewma,
            "2030-07-08",
        )
        .await;
    }
    assert_eq!(locked(&pool, "2030-07-09").await.locked_cells, 5);
    let second = ran(&pool, "2030-07-09", &config).await;
    assert!(!second.lock_missing);
    let scored = calibrated(&pool, "2030-07-09", &second.batch_id).await;
    assert_eq!(
        (
            scored.calibration_rows,
            scored.pairs,
            scored.unlocked_runs,
            scored.skipped
        ),
        (1, 15, 0, false)
    );
    let rows = calibration_rows(&pool).await;
    assert_eq!(rows.len(), 1);
    let (batch, capability, brier, overconfidence, n) = &rows[0];
    assert_eq!(batch, &second.batch_id);
    assert_eq!(capability, "structured_extraction");
    assert!(close(*brier, 0.18), "brier {brier}");
    assert!(
        close(*overconfidence, -0.4),
        "overconfidence {overconfidence}"
    );
    assert_eq!(*n, 15);
    db.close().await;
}

async fn seed_scored_run(pool: &PgPool, batch: &str, task: &str, repeat: i16, correct: f64) {
    let id = format!("run_{task}_{repeat}");
    measurement::insert_run(
        pool,
        &NewRun {
            id: &id,
            batch_id: batch,
            task_id: task,
            model_column_id: "col_cal",
            repeat_index: repeat,
            output: Some(if correct > 0.5 { "42" } else { "wrong" }),
            tokens_in: Some(10),
            tokens_out: Some(2),
            latency_ms: Some(100),
            cost: None,
            temperature: 0.0,
            status: "ok",
            error: None,
            matrix_version: "corpus-cal",
        },
    )
    .await
    .expect("run");
    let mut conn = pool.acquire().await.expect("conn");
    save_scores(
        &mut conn,
        &id,
        0,
        &BTreeMap::from([("answer_correctness".to_owned(), correct)]),
        Some(&json("{}")),
    )
    .await
    .expect("scores");
}

/// parity: jobs.TestCalibrationRowsPerCapability
#[tokio::test]
async fn calibration_rows_per_capability() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed_version(&pool, "corpus-cal").await;
    let tasks = [
        ("task_a1", "structured_extraction", 1, 1.0),
        ("task_a2", "structured_extraction", 1, 0.0),
        ("task_b1", "factual_qa", 2, 1.0),
        ("task_c1", "code_generation", 1, 1.0),
    ];
    for (id, capability, tier, _) in tasks {
        seed_task(
            &pool,
            "corpus-cal",
            &SeedTask {
                id,
                capability,
                tier,
                smoke: true,
                prompt: "p",
                answer: "42",
            },
        )
        .await;
    }
    seed_column(&pool, "col_cal", "anthropic", "claude-cal", "active").await;
    seed_pass_rate(
        &pool,
        "col_cal",
        "structured_extraction",
        1,
        0.8,
        "2030-09-09",
    )
    .await;
    seed_pass_rate(&pool, "col_cal", "factual_qa", 2, 0.5, "2030-09-09").await;
    let lock = relock(&pool, "2030-09-10").await;
    assert_eq!((lock.scheduled_cells, lock.locked_cells), (3, 2));
    for (id, _, _, correct) in tasks {
        for repeat in 0..3 {
            seed_scored_run(&pool, "bat_cal", id, repeat, correct).await;
        }
    }

    let out = calibrated(&pool, "2030-09-10", "bat_cal").await;
    assert_eq!(
        (
            out.calibration_rows,
            out.pairs,
            out.unlocked_runs,
            out.locked_cells
        ),
        (2, 9, 3, 2)
    );
    let expect = |rows: &[(String, String, f32, f32, i32)]| {
        assert_eq!(rows.len(), 2);
        let (_, capability, brier, overconfidence, n) = &rows[0];
        assert_eq!(capability, "factual_qa");
        assert!(
            close(*brier, 0.25) && close(*overconfidence, -0.5) && *n == 3,
            "{rows:?}"
        );
        let (_, capability, brier, overconfidence, n) = &rows[1];
        assert_eq!(capability, "structured_extraction");
        assert!(
            close(*brier, 0.34) && close(*overconfidence, 0.3) && *n == 6,
            "{rows:?}"
        );
    };
    expect(&calibration_rows(&pool).await);

    // The leased re-delivery is a no-op (D10); the Go re-run upserts the same two rows.
    let _ = parity_support::divergence!("D10");
    let again = calibration_score(
        &pool,
        &BatchFollowupArgs {
            night: "2030-09-10".to_owned(),
            batch_id: "bat_cal".to_owned(),
        },
        &calibration_config(),
        "worker-2",
    )
    .await
    .expect("again");
    assert_eq!(again, CalibrationOutcome::AlreadyFinished);
    let mut conn = pool.acquire().await.expect("conn");
    let rerun = score_calibration(&pool, &mut conn, "2030-09-10", "bat_cal")
        .await
        .expect("rerun");
    drop(conn);
    assert_eq!(rerun, out);
    expect(&calibration_rows(&pool).await);
    db.close().await;
}

fn spec(kind: JobKind, window: &str) -> JobSpec {
    JobSpec::builder()
        .queue_name(DEFAULT_QUEUE_NAME)
        .job_key(kind.job_key(window))
        .max_attempts(kind.max_attempts())
        .build()
}

/// parity: jobs.TestPredictionCalibrationChain
#[tokio::test]
async fn prediction_calibration_chain() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed_smoke(&pool, "corpus-chain").await;
    let counter = Arc::new(Counter::default());
    let worker = WorkerOptions::default()
        .concurrency(1)
        .schema(GRAPHILE_WORKER_SCHEMA)
        .pg_pool(pool.clone())
        .add_extension(batch_config(&counter))
        .add_extension(RebuildConfig {
            matrix_export_enabled: false,
            alpha: 0.3,
            lambda_var: 0.5,
            lambda_amb: 0.25,
            min_nights: 3,
            runner_timeout: Duration::from_secs(120),
            lease: lease(),
        })
        .add_extension(RegressionScanConfig {
            alpha: 0.3,
            lease: lease(),
            notifier: None,
        })
        .add_extension(lock_config())
        .add_extension(calibration_config())
        .define_job::<NightlySmokeJob>()
        .define_job::<AggregateRebuildJob>()
        .define_job::<RegressionScanJob>()
        .define_job::<PredictionLockJob>()
        .define_job::<CalibrationScoreJob>()
        .init()
        .await
        .expect("worker");
    let utils = worker.create_utils();
    let completed = |kind: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>(
                "select count(*) from batches where kind = $1 and status = 'completed'",
            )
            .bind(kind)
            .fetch_one(&pool)
            .await
            .expect("count")
        }
    };

    // Night 1: no lock; the chain's calibration skips.
    let night1 = NightlySmokeArgs {
        night: "2030-08-12".to_owned(),
    };
    utils
        .add_job(
            NightlySmokeJob(night1),
            spec(JobKind::NightlySmoke, "2030-08-12"),
        )
        .await
        .expect("enqueue");
    for _ in 0..3 {
        worker.run_once().await.expect("run");
    }
    assert_eq!(completed("calibration_score").await, 1);
    assert!(calibration_rows(&pool).await.is_empty());

    // Night 2: the lock precedes the batch; the chain scores it.
    utils
        .add_job(
            PredictionLockJob(PredictionLockArgs {
                night: "2030-08-13".to_owned(),
            }),
            spec(JobKind::PredictionLock, "2030-08-13"),
        )
        .await
        .expect("enqueue lock");
    worker.run_once().await.expect("lock");
    assert_eq!(
        measurement::count_predictions_by_night(&pool, "2030-08-13")
            .await
            .expect("count"),
        5
    );
    utils
        .add_job(
            NightlySmokeJob(NightlySmokeArgs {
                night: "2030-08-13".to_owned(),
            }),
            spec(JobKind::NightlySmoke, "2030-08-13"),
        )
        .await
        .expect("enqueue smoke");
    for _ in 0..3 {
        worker.run_once().await.expect("run");
    }
    assert_eq!(completed("calibration_score").await, 2);
    let rows = calibration_rows(&pool).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    let (batch, capability, brier, overconfidence, n) = &rows[0];
    assert_eq!(
        batch,
        &maverick_jobs::deterministic_batch_id(JobKind::NightlySmoke, "2030-08-13")
    );
    assert_eq!(capability, "structured_extraction");
    assert!(
        close(*brier, 0.0) && close(*overconfidence, 0.0),
        "{rows:?}"
    );
    assert_eq!(*n, 15);
    db.close().await;
}

/// parity: jobs.TestCalibrationScoreTimeoutUsesAnalyticsJobTimeout
#[test]
fn calibration_timeout_uses_the_analytics_job_timeout() {
    assert_eq!(
        JobKind::CalibrationScore.timeout(&JobClientConfig::default()),
        Duration::from_secs(15 * 60)
    );
}
