//! `nightly_smoke` and `weekly_sweep` on real Postgres with the fake provider and the
//! graphile_worker schema (reference Part 3 §2.4, §2.5, §2.7, and §6).

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use graphile_worker::{JobSpec, WorkerOptions, WorkerUtils};
use maverick_alerts::AlertNotifier;
use maverick_domain::graders::judge_rubric::{FamilyResolver, Judge, JudgeClient, JudgeRequest};
use maverick_domain::graders::{
    BoxError, BoxFuture, GoldenAnswer, JsonSchema, JudgeRubric, KeyFacts, Registry, RuleScan,
};
use maverick_domain::spend::{SpendCaps, SpendColumn, SpendTask, project_spend};
use maverick_jobs::batch::{
    BatchConfig, BatchOutcome, BatchOutput, NightlySmokeJob, WeeklySweepJob, run_batch,
};
use maverick_jobs::rebuild::{
    AggregateRebuildJob, RebuildConfig, RebuildOutcome, aggregate_rebuild,
};
use maverick_jobs::{
    BatchFollowupArgs, BatchLeaseConfig, BatchLeaseManager, DEFAULT_QUEUE_NAME, JobArgs,
    JobClientConfig, JobKind, NightlySmokeArgs, WeeklySweepArgs,
};
use maverick_providers::fake::{FakeProvider, FixtureSpec};
use maverick_runner::{Cancellation, Clients, RunnerConfig};
use maverick_store::alerts::Alert;
use maverick_store::catalog::{self, NewModelColumn};
use maverick_store::corpus::{self, TaskUpsert};
use maverick_store::health::GRAPHILE_WORKER_SCHEMA;
use maverick_store::measurement::{self, CellDimension};
use maverick_store::{JsonRawValue, MIGRATOR};
use maverick_testdb::Schema;
use serde_json::Value;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

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

const VERSION: &str = "corpus-batch";
const WARM_MODEL: &str = "claude-batch";
const COLD_MODEL: &str = "gpt-batch";

fn json(text: &str) -> Box<JsonRawValue> {
    JsonRawValue::from_string(text.to_owned()).expect("json")
}

#[derive(Default)]
struct Counter(AtomicUsize);

impl AlertNotifier for Counter {
    fn notify(&self, _alert: &Alert) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

struct SeedTask {
    id: &'static str,
    tier: i16,
    smoke: bool,
    prompt: &'static str,
    answer: &'static str,
}

const SMOKE_TASKS: [SeedTask; 3] = [
    SeedTask {
        id: "task_t1",
        tier: 1,
        smoke: true,
        prompt: "What is six times seven? Answer with a number.",
        answer: "42",
    },
    SeedTask {
        id: "task_t3",
        tier: 3,
        smoke: false,
        prompt: "How many days are in a week? Answer with a number.",
        answer: "7",
    },
    SeedTask {
        id: "task_t4",
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

async fn seed_task(pool: &PgPool, task: &SeedTask, graders: &str, weights: &str) {
    let tags: Vec<String> = Vec::new();
    corpus::upsert_task(
        pool,
        &TaskUpsert {
            id: task.id,
            capability: "structured_extraction",
            domain_tags: &tags,
            difficulty_tier: task.tier,
            difficulty_knobs: &json("{}"),
            prompt: task.prompt,
            attachments: &json("[]"),
            temperature: 0.0,
            max_tokens: 200,
            grader_specs: &json(graders),
            dimension_weights: &json(weights),
            pass_threshold: 0.7,
            smoke_member: task.smoke,
            corpus_version: VERSION,
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

/// The smoke fixture: 3 tasks, a warm anthropic column, and a cold-start openai column.
async fn seed_smoke(pool: &PgPool) {
    corpus::upsert_corpus_version(pool, VERSION, "batch tests", 3, true)
        .await
        .expect("version");
    for task in &SMOKE_TASKS {
        seed_task(
            pool,
            task,
            &golden(task.answer),
            r#"{"answer_correctness":1.0}"#,
        )
        .await;
    }
    seed_column(pool, "col_warm", "anthropic", WARM_MODEL, "active").await;
    seed_column(pool, "col_cold", "openai", COLD_MODEL, "cold_start").await;
}

fn fake_answers(broken: &[&str]) -> Arc<FakeProvider> {
    let mut specs = Vec::new();
    for model in [WARM_MODEL, COLD_MODEL] {
        for task in &SMOKE_TASKS {
            let spec = if broken.contains(&task.id) {
                serde_json::json!({"model": model, "prompt": task.prompt, "error": "provider_error"})
            } else {
                serde_json::json!({"model": model, "prompt": task.prompt, "output": task.answer, "tokens_in": 100, "tokens_out": 10})
            };
            specs.push(serde_json::from_value::<FixtureSpec>(spec).expect("fixture"));
        }
    }
    Arc::new(FakeProvider::from_fixtures(&specs).expect("fake"))
}

fn lease() -> BatchLeaseManager {
    BatchLeaseManager::new(BatchLeaseConfig {
        lease_ttl: Duration::from_secs(600),
        heartbeat_interval: Duration::from_secs(200),
        owner_stale_after: Duration::from_secs(60),
        graphile_schema: GRAPHILE_WORKER_SCHEMA.to_string(),
    })
}

fn registry() -> Registry {
    let mut registry = Registry::new();
    registry.must_register(Arc::new(GoldenAnswer));
    registry.must_register(Arc::new(KeyFacts));
    registry.must_register(Arc::new(RuleScan));
    registry.must_register(Arc::new(JsonSchema));
    registry
}

fn config(fake: Arc<FakeProvider>, caps: SpendCaps, counter: &Arc<Counter>) -> BatchConfig {
    BatchConfig {
        repeats: 3,
        caps,
        lease: lease(),
        runner: RunnerConfig {
            // Scripted provider errors retry inside the runner; keep them fast.
            timeout: Duration::from_secs(30),
            ..RunnerConfig::default()
        },
        clients: Clients::new()
            .with("anthropic", Arc::clone(&fake) as _)
            .with("openai", fake),
        graders: Arc::new(registry()),
        notifier: Some(Arc::clone(counter) as Arc<dyn AlertNotifier>),
    }
}

fn rebuild_config() -> RebuildConfig {
    RebuildConfig {
        alpha: 0.3,
        lambda_var: 0.5,
        lambda_amb: 0.25,
        min_nights: 3,
        runner_timeout: Duration::from_secs(120),
        lease: lease(),
    }
}

fn smoke(night: &str) -> JobArgs {
    JobArgs::NightlySmoke(NightlySmokeArgs {
        night: night.to_owned(),
    })
}

fn sweep(week: &str, night: &str) -> JobArgs {
    JobArgs::WeeklySweep(WeeklySweepArgs {
        week: week.to_owned(),
        night: night.to_owned(),
    })
}

async fn ran(pool: &PgPool, job: &JobArgs, config: &BatchConfig) -> BatchOutput {
    match run_batch(pool, job, config, "worker-test", &Cancellation::new())
        .await
        .expect("batch")
    {
        BatchOutcome::Ran(output) => output,
        other => panic!("not run: {other:?}"),
    }
}

async fn count(pool: &PgPool, sql: &'static str) -> i64 {
    sqlx::query_scalar::<_, i64>(sql)
        .fetch_one(pool)
        .await
        .expect("count")
}

async fn runs_for(pool: &PgPool, column: &str) -> i64 {
    sqlx::query_scalar::<_, i64>("select count(*) from runs where model_column_id = $1")
        .bind(column)
        .fetch_one(pool)
        .await
        .expect("count")
}

/// `(task identifier, payload)` of every queued graphile job, in insertion order.
async fn queued(pool: &PgPool) -> Vec<(String, Value)> {
    sqlx::query_as::<_, (String, sqlx::types::Json<Value>)>(
        "select tasks.identifier, jobs.payload
         from graphile_worker._private_jobs as jobs
         join graphile_worker._private_tasks as tasks on tasks.id = jobs.task_id
         order by jobs.id",
    )
    .fetch_all(pool)
    .await
    .expect("jobs")
    .into_iter()
    .map(|(t, p)| (t, p.0))
    .collect()
}

async fn alerts(pool: &PgPool, kind: &str) -> Vec<(String, Value, String)> {
    sqlx::query_as::<_, (String, sqlx::types::Json<Value>, String)>(
        "select severity, subject, message from alerts where kind = $1 order by created_at, id",
    )
    .bind(kind)
    .fetch_all(pool)
    .await
    .expect("alerts")
    .into_iter()
    .map(|(s, subject, m)| (s, subject.0, m))
    .collect()
}

/// The planned projection for the nightly groups, computed independently.
fn view(t: &corpus::Task) -> SpendTask<'_> {
    SpendTask {
        id: "t",
        prompt: t.prompt.as_str(),
        attachments: "[]",
        max_tokens: i64::from(t.max_tokens),
    }
}

async fn nightly_projection(pool: &PgPool) -> f64 {
    let tasks = corpus::list_active_tasks_by_corpus_version(pool, VERSION)
        .await
        .expect("tasks");
    let pricing = r#"{"input_per_mtok": 3.0, "output_per_mtok": 15.0}"#;
    let smoke: Vec<SpendTask<'_>> = tasks.iter().filter(|t| t.smoke_member).map(view).collect();
    let all: Vec<SpendTask<'_>> = tasks.iter().map(view).collect();
    let column = [SpendColumn { id: "c", pricing }];
    project_spend(&smoke, &column, 3)
        .expect("smoke")
        .projected_usd
        + project_spend(&all, &column, 3).expect("all").projected_usd
}

/// parity: jobs.TestNightlySmokeRefusalRunsNothing
#[tokio::test]
async fn nightly_smoke_refusal_runs_nothing() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed_smoke(&pool).await;
    let counter = Arc::new(Counter::default());
    let config = config(
        fake_answers(&[]),
        SpendCaps {
            nightly_usd: 0.000001,
            weekly_usd: 500.0,
        },
        &counter,
    );
    let out = ran(&pool, &smoke("2030-01-10"), &config).await;
    assert!(out.refused);
    assert!(out.batch_id.is_empty());
    assert_eq!(out.planned_runs, 15);
    assert_eq!(count(&pool, "select count(*) from runs").await, 0);
    assert!(queued(&pool).await.is_empty());
    let refusals = alerts(&pool, "spend_refusal").await;
    assert_eq!(refusals.len(), 1);
    assert_eq!(refusals[0].0, "critical");
    // D10: the window is recorded as skipped, so a redelivery is a no-op.
    let _ = parity_support::divergence!("D10");
    let again = run_batch(
        &pool,
        &smoke("2030-01-10"),
        &config,
        "worker-2",
        &Cancellation::new(),
    )
    .await
    .expect("again");
    assert_eq!(again, BatchOutcome::AlreadyFinished);
    let serialized = serde_json::to_value(&out).expect("output");
    assert!(serialized.get("batch_id").is_none());
    assert_eq!(serialized["refused"], true);
    db.close().await;
}

/// parity: jobs.TestNightlySmokeWarningStillRuns
#[tokio::test]
async fn nightly_smoke_warning_still_runs() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed_smoke(&pool).await;
    let counter = Arc::new(Counter::default());
    let projected = nightly_projection(&pool).await;
    let config = config(
        fake_answers(&[]),
        SpendCaps {
            nightly_usd: projected / 0.9,
            weekly_usd: 500.0,
        },
        &counter,
    );
    let out = ran(&pool, &smoke("2030-02-10"), &config).await;
    assert!(!out.refused && out.spend_warned);
    assert!(out.lock_missing, "no predictions were locked for the night");
    assert!((out.projected_usd - projected).abs() < 1e-12);
    assert_eq!(out.planned_runs, 15);
    assert_eq!(count(&pool, "select count(*) from runs").await, 15);
    assert_eq!(out.completed_runs, 15);
    assert_eq!(out.completion_pct, 1.0);
    assert_eq!(out.graded_runs, 15);
    let chained = queued(&pool).await;
    assert_eq!(chained.len(), 1);
    assert_eq!(chained[0].0, "aggregate_rebuild");
    assert_eq!(chained[0].1["night"], "2030-02-10");
    assert_eq!(chained[0].1["batch_id"], out.batch_id);
    let warnings = alerts(&pool, "spend_warning").await;
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].1["night"], "2030-02-10");
    assert!(alerts(&pool, "job_failure").await.is_empty());
    assert!(counter.0.load(Ordering::SeqCst) >= 1);
    db.close().await;
}

/// Through graphile: the batch, then the chained rebuild. A redelivery is a no-op (D10;
/// River rejected the duplicate insert instead).
///
/// parity: jobs.TestNightlySmokeChain
#[tokio::test]
async fn nightly_smoke_chain() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed_smoke(&pool).await;
    let counter = Arc::new(Counter::default());
    let worker = WorkerOptions::default()
        .concurrency(1)
        .schema(GRAPHILE_WORKER_SCHEMA)
        .pg_pool(pool.clone())
        .add_extension(config(fake_answers(&[]), SpendCaps::default(), &counter))
        .add_extension(rebuild_config())
        .define_job::<NightlySmokeJob>()
        .define_job::<AggregateRebuildJob>()
        .init()
        .await
        .expect("worker");
    let utils = worker.create_utils();
    let spec = || {
        JobSpec::builder()
            .queue_name(DEFAULT_QUEUE_NAME)
            .job_key(JobKind::NightlySmoke.job_key("2030-03-10"))
            .max_attempts(JobKind::NightlySmoke.max_attempts())
            .build()
    };
    let job = NightlySmokeJob(NightlySmokeArgs {
        night: "2030-03-10".to_owned(),
    });
    utils.add_job(job.clone(), spec()).await.expect("enqueue");
    worker.run_once().await.expect("batch");
    worker.run_once().await.expect("rebuild");
    worker.run_once().await.expect("followups");

    assert_eq!(runs_for(&pool, "col_warm").await, 6);
    assert_eq!(runs_for(&pool, "col_cold").await, 9);
    assert_eq!(count(&pool, "select count(*) from scores").await, 15);
    let done: i64 = count(
        &pool,
        "select count(*) from batches where status = 'completed'",
    )
    .await;
    assert_eq!(done, 2, "the batch and its rebuild completed");
    let _ = parity_support::divergence!("D6");
    for (column, tier) in [
        ("col_warm", 1),
        ("col_warm", 4),
        ("col_cold", 1),
        ("col_cold", 3),
        ("col_cold", 4),
    ] {
        for dimension in [
            "quality",
            "pass_rate",
            "ambiguity",
            "latency_p50_ms",
            "latency_p95_ms",
            "cost_per_run_usd",
            "availability",
        ] {
            let row = measurement::get_latest_aggregate(
                &pool,
                &CellDimension {
                    model_column_id: column,
                    capability: "structured_extraction",
                    difficulty_tier: tier,
                    dimension,
                },
            )
            .await
            .unwrap_or_else(|e| panic!("{column} {tier} {dimension}: {e}"));
            assert_eq!(
                (row.n, row.measured),
                (1, false),
                "{column} {tier} {dimension}"
            );
            if matches!(
                dimension,
                "quality" | "pass_rate" | "ambiguity" | "availability"
            ) {
                assert!((0.0..=1.0).contains(&row.ewma));
            } else {
                assert!(row.ewma >= 0.0);
            }
        }
    }
    // Redelivering the same night is a no-op: the window's lease is completed.
    let _ = parity_support::divergence!("D10");
    utils.add_job(job, spec()).await.expect("re-enqueue");
    worker.run_once().await.expect("duplicate");
    assert_eq!(count(&pool, "select count(*) from runs").await, 15);
    db.close().await;
}

/// parity: jobs.TestNightlySmokeThreeNightsFlipMeasured
#[tokio::test]
async fn three_nights_flip_measured() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed_smoke(&pool).await;
    let counter = Arc::new(Counter::default());
    let config = config(fake_answers(&[]), SpendCaps::default(), &counter);
    for night in ["2030-04-10", "2030-04-11", "2030-04-12"] {
        let out = ran(&pool, &smoke(night), &config).await;
        let rebuilt = aggregate_rebuild(
            &pool,
            &BatchFollowupArgs {
                night: night.to_owned(),
                batch_id: out.batch_id.clone(),
            },
            &rebuild_config(),
            "worker-test",
        )
        .await
        .expect("rebuild");
        assert!(matches!(rebuilt, RebuildOutcome::Rebuilt(_)));
    }
    for (column, tier) in [("col_warm", 1), ("col_warm", 4), ("col_cold", 3)] {
        for dimension in [
            "quality",
            "pass_rate",
            "ambiguity",
            "latency_p95_ms",
            "availability",
        ] {
            let row = measurement::get_latest_aggregate(
                &pool,
                &CellDimension {
                    model_column_id: column,
                    capability: "structured_extraction",
                    difficulty_tier: tier,
                    dimension,
                },
            )
            .await
            .expect("row");
            assert_eq!(
                (row.n, row.measured),
                (3, true),
                "{column} {tier} {dimension}"
            );
        }
    }
    let quality = measurement::get_latest_aggregate(
        &pool,
        &CellDimension {
            model_column_id: "col_warm",
            capability: "structured_extraction",
            difficulty_tier: 1,
            dimension: "quality",
        },
    )
    .await
    .expect("quality");
    assert!((0.999..=1.0).contains(&f64::from(quality.ewma)));
    db.close().await;
}

/// parity: jobs.TestWeeklySweepRefusalAgainstWeeklyCap
#[tokio::test]
async fn weekly_sweep_refusal_against_weekly_cap() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed_smoke(&pool).await;
    let counter = Arc::new(Counter::default());
    let config = config(
        fake_answers(&[]),
        SpendCaps {
            nightly_usd: 100.0,
            weekly_usd: 0.000001,
        },
        &counter,
    );
    let out = ran(&pool, &sweep("2031-W02", "2031-01-12"), &config).await;
    assert!(out.refused && out.batch_id.is_empty());
    let tasks = corpus::list_active_tasks_by_corpus_version(&pool, VERSION)
        .await
        .expect("tasks");
    let all: Vec<SpendTask<'_>> = tasks
        .iter()
        .map(|t| SpendTask {
            id: &t.id,
            prompt: &t.prompt,
            attachments: "[]",
            max_tokens: i64::from(t.max_tokens),
        })
        .collect();
    let pricing = r#"{"input_per_mtok": 3.0, "output_per_mtok": 15.0}"#;
    let columns = [
        SpendColumn {
            id: "col_cold",
            pricing,
        },
        SpendColumn {
            id: "col_warm",
            pricing,
        },
    ];
    let expected = project_spend(&all, &columns, 3).expect("projection");
    assert!((out.projected_usd - expected.projected_usd).abs() < 1e-12);
    assert_eq!(out.planned_runs, expected.runs);
    assert_eq!(count(&pool, "select count(*) from runs").await, 0);
    assert!(queued(&pool).await.is_empty());
    let refusals = alerts(&pool, "spend_refusal").await;
    assert_eq!(refusals.len(), 1);
    assert_eq!(refusals[0].0, "critical");
    db.close().await;
}

/// parity: jobs.TestWeeklySweepIgnoresNightlyCap
#[tokio::test]
async fn weekly_sweep_ignores_nightly_cap() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed_smoke(&pool).await;
    let counter = Arc::new(Counter::default());
    let config = config(
        fake_answers(&[]),
        SpendCaps {
            nightly_usd: 0.000001,
            weekly_usd: 500.0,
        },
        &counter,
    );
    let out = ran(&pool, &sweep("2031-W06", "2031-02-09"), &config).await;
    assert!(!out.refused);
    assert_eq!(out.planned_runs, 18);
    assert_eq!(count(&pool, "select count(*) from runs").await, 18);
    assert_eq!(queued(&pool).await.len(), 1);
    assert!(alerts(&pool, "spend_refusal").await.is_empty());
    db.close().await;
}

/// The sweep and its rebuild; the window is the ISO week, so a second delivery in the
/// same week is a no-op (D10).
///
/// parity: jobs.TestWeeklySweepChain
#[tokio::test]
async fn weekly_sweep_chain() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed_smoke(&pool).await;
    let counter = Arc::new(Counter::default());
    let config = config(fake_answers(&[]), SpendCaps::default(), &counter);
    let worker = WorkerOptions::default()
        .concurrency(1)
        .schema(GRAPHILE_WORKER_SCHEMA)
        .pg_pool(pool.clone())
        .add_extension(config.clone())
        .add_extension(rebuild_config())
        .define_job::<WeeklySweepJob>()
        .define_job::<AggregateRebuildJob>()
        .init()
        .await
        .expect("worker");
    worker
        .create_utils()
        .add_job(
            WeeklySweepJob(WeeklySweepArgs {
                week: "2031-W10".to_owned(),
                night: "2031-03-09".to_owned(),
            }),
            JobSpec::builder()
                .queue_name(DEFAULT_QUEUE_NAME)
                .job_key(JobKind::WeeklySweep.job_key("2031-W10"))
                .build(),
        )
        .await
        .expect("enqueue");
    worker.run_once().await.expect("sweep");
    worker.run_once().await.expect("rebuild");
    assert_eq!(runs_for(&pool, "col_warm").await, 9);
    assert_eq!(runs_for(&pool, "col_cold").await, 9);
    for column in ["col_warm", "col_cold"] {
        for tier in [1, 3, 4] {
            let row = measurement::get_latest_aggregate(
                &pool,
                &CellDimension {
                    model_column_id: column,
                    capability: "structured_extraction",
                    difficulty_tier: tier,
                    dimension: "quality",
                },
            )
            .await
            .expect("quality");
            assert_eq!(row.n, 1);
        }
    }
    let _ = parity_support::divergence!("D10");
    let again = run_batch(
        &pool,
        &sweep("2031-W10", "2031-03-10"),
        &config,
        "worker-2",
        &Cancellation::new(),
    )
    .await
    .expect("again");
    assert_eq!(again, BatchOutcome::AlreadyFinished);
    assert_eq!(count(&pool, "select count(*) from runs").await, 18);
    db.close().await;
}

/// D11: the completion alert, the lease completion, and the rebuild enqueue commit in one
/// transaction. When the enqueue fails nothing of the tail is written and the job
/// retries; the retry resumes past the existing runs and grades nothing twice (D10).
///
/// parity: jobs.TestFinishBatchAlertsAndEnqueueSurviveExpiredJobContext
/// parity: jobs.TestFinishBatchAlertsWhenEnqueueFailsDespiteExpiredJobContext
#[tokio::test]
async fn failure_tail_commits_with_the_outcome() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed_smoke(&pool).await;
    let counter = Arc::new(Counter::default());
    // Tier 3 always fails: its 3 cold runs fail, so 12 of 15 runs complete.
    let config = config(fake_answers(&["task_t3"]), SpendCaps::default(), &counter);
    sqlx::query("alter schema graphile_worker rename to graphile_worker_gone")
        .execute(&pool)
        .await
        .expect("hide graphile");
    let failed = run_batch(
        &pool,
        &smoke("2030-08-01"),
        &config,
        "worker-test",
        &Cancellation::new(),
    )
    .await;
    assert!(failed.is_err(), "the enqueue fails, so the tail fails");
    let _ = parity_support::divergence!("D11");
    assert!(alerts(&pool, "job_failure").await.is_empty(), "rolled back");
    assert_eq!(
        count(
            &pool,
            "select count(*) from batches where status = 'running'"
        )
        .await,
        1
    );
    let runs_before = count(&pool, "select count(*) from runs").await;
    let scores_before = count(&pool, "select count(*) from scores").await;
    assert_eq!(runs_before, 15);
    sqlx::query("alter schema graphile_worker_gone rename to graphile_worker")
        .execute(&pool)
        .await
        .expect("restore graphile");

    let _ = parity_support::divergence!("D10");
    let out = ran(&pool, &smoke("2030-08-01"), &config).await;
    assert_eq!(
        count(&pool, "select count(*) from runs").await,
        runs_before,
        "resumed"
    );
    assert_eq!(
        count(&pool, "select count(*) from scores").await,
        scores_before,
        "not regraded"
    );
    assert_eq!((out.planned_runs, out.completed_runs), (15, 12));
    assert_eq!(out.graded_runs, 12);
    let failures = alerts(&pool, "job_failure").await;
    assert_eq!(failures.len(), 1);
    let (severity, subject, message) = &failures[0];
    assert_eq!(severity, "warning");
    assert_eq!(
        message,
        "nightly_smoke 2030-08-01 completed 12/15 runs (80.0%)"
    );
    assert_eq!(subject["completed_runs"], 12);
    assert_eq!(subject["planned_runs"], 15);
    assert_eq!(subject["completion_pct"], 0.8);
    assert_eq!(subject["job"], "nightly_smoke");
    assert_eq!(subject["batch_id"], out.batch_id);
    assert_eq!(queued(&pool).await.len(), 1);
    assert!(
        counter.0.load(Ordering::SeqCst) >= 1,
        "notified after the commit"
    );
    db.close().await;
}

/// Nothing to run: no rebuild, and the window completes.
#[tokio::test]
async fn nothing_to_run() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let counter = Arc::new(Counter::default());
    let config = config(fake_answers(&[]), SpendCaps::default(), &counter);
    let out = ran(&pool, &smoke("2030-09-01"), &config).await;
    assert_eq!(out.completion_pct, 1.0);
    assert_eq!(out.planned_runs, 0);
    assert!(queued(&pool).await.is_empty());
    assert_eq!(
        count(
            &pool,
            "select count(*) from batches where status = 'completed'"
        )
        .await,
        1
    );
    assert!(
        run_batch(
            &pool,
            &JobArgs::AggregateRebuild(BatchFollowupArgs {
                night: "2030-09-01".to_owned(),
                batch_id: "b".to_owned()
            }),
            &config,
            "w",
            &Cancellation::new()
        )
        .await
        .is_err()
    );
    db.close().await;
}

/// The judge answers 0.9 on even calls and 0.5 on odd calls, or always fails.
struct AlternatingJudge {
    calls: AtomicUsize,
    fail: bool,
}

impl JudgeClient for AlternatingJudge {
    fn complete<'a>(&'a self, _request: JudgeRequest) -> BoxFuture<'a, Result<String, BoxError>> {
        Box::pin(async move {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err("scripted judge outage".into());
            }
            let score = if n % 2 == 0 { 0.9 } else { 0.5 };
            Ok(format!(r#"{{"scores": {{"style": {score}}}}}"#))
        })
    }
}

struct Families(HashMap<String, String>);

impl FamilyResolver for Families {
    fn family<'a>(&'a self, column_id: &'a str) -> BoxFuture<'a, Result<String, BoxError>> {
        Box::pin(async move {
            self.0
                .get(column_id)
                .cloned()
                .ok_or_else(|| format!("unknown column {column_id}").into())
        })
    }
}

async fn judge_setup(
    pool: &PgPool,
    fail: bool,
) -> (BatchConfig, Arc<AlternatingJudge>, Arc<Counter>) {
    corpus::upsert_corpus_version(pool, VERSION, "judge tests", 1, true)
        .await
        .expect("version");
    let task = SeedTask {
        id: "task_judge",
        tier: 2,
        smoke: true,
        prompt: "Name the capital of France.",
        answer: "Paris",
    };
    let graders = r#"[{"type":"golden_answer","version":1,"weight_dimensions":{"answer_correctness":1},"spec":{"answers":[{"value":"Paris"}]}},{"type":"judge_rubric","version":1,"weight_dimensions":{"style":1},"spec":{"rubric":[{"dimension":"style","criterion":"The answer is clear and direct."}]}}]"#;
    seed_task(
        pool,
        &task,
        graders,
        r#"{"answer_correctness":0.5,"style":0.5}"#,
    )
    .await;
    seed_column(pool, "col_judged", "anthropic", WARM_MODEL, "active").await;
    let fake = Arc::new(
        FakeProvider::from_fixtures(&[serde_json::from_value::<FixtureSpec>(serde_json::json!({
            "model": WARM_MODEL, "prompt": task.prompt, "output": "Paris", "tokens_in": 20, "tokens_out": 2
        }))
        .expect("fixture")])
        .expect("fake"),
    );
    let judge = Arc::new(AlternatingJudge {
        calls: AtomicUsize::new(0),
        fail,
    });
    let pool_members = vec![
        Judge {
            reference: "gemini-3.1-pro/direct".to_owned(),
            family: "google".to_owned(),
            model: "gemini-3.1-pro".to_owned(),
            client: Some(Arc::clone(&judge) as Arc<dyn JudgeClient>),
        },
        Judge {
            reference: "gpt-5.5/direct".to_owned(),
            family: "openai".to_owned(),
            model: "gpt-5.5".to_owned(),
            client: Some(Arc::clone(&judge) as Arc<dyn JudgeClient>),
        },
    ];
    let resolver = Families(HashMap::from([(
        "col_judged".to_owned(),
        "anthropic".to_owned(),
    )]));
    let mut registry = registry();
    registry.must_register(Arc::new(
        JudgeRubric::new(pool_members, Some(Arc::new(resolver))).expect("judge pool"),
    ));
    let counter = Arc::new(Counter::default());
    let mut config = config(fake, SpendCaps::default(), &counter);
    config.graders = Arc::new(registry);
    (config, judge, counter)
}

/// parity: jobs.TestNightlySmokeJudgeDisagreementFeedsAmbiguity
#[tokio::test]
async fn judge_disagreement_feeds_ambiguity() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let (config, judge, _) = judge_setup(&pool, false).await;
    let out = ran(&pool, &smoke("2030-07-10"), &config).await;
    assert_eq!((out.judge_attempts, out.judge_failures), (3, 0));
    assert_eq!(judge.calls.load(Ordering::SeqCst), 6);
    assert!(alerts(&pool, "job_failure").await.is_empty());
    let styles: Vec<(f32, String)> = sqlx::query_as::<_, (f32, sqlx::types::Json<Value>)>(
        "select value, detail from scores where dimension = 'style'",
    )
    .fetch_all(&pool)
    .await
    .expect("styles")
    .into_iter()
    .map(|(v, d)| (v, d.0.to_string()))
    .collect();
    assert_eq!(styles.len(), 3);
    for (value, detail) in &styles {
        assert!((f64::from(*value) - 0.7).abs() < 1e-6);
        assert!(detail.contains("order_swap"), "{detail}");
    }
    aggregate_rebuild(
        &pool,
        &BatchFollowupArgs {
            night: "2030-07-10".to_owned(),
            batch_id: out.batch_id.clone(),
        },
        &rebuild_config(),
        "worker-test",
    )
    .await
    .expect("rebuild");
    let latest = |dimension: &'static str| {
        let pool = pool.clone();
        async move {
            f64::from(
                measurement::get_latest_aggregate(
                    &pool,
                    &CellDimension {
                        model_column_id: "col_judged",
                        capability: "structured_extraction",
                        difficulty_tier: 2,
                        dimension,
                    },
                )
                .await
                .expect("row")
                .ewma,
            )
        }
    };
    let pipeline = parity_support::fixture("measurement-core", "penalties.json").expect("fixture")
        ["task_pipeline"][0]["expected"]
        .clone();
    assert!((latest("quality").await - pipeline["task_score"].as_f64().expect("q")).abs() < 1e-6);
    assert!((latest("ambiguity").await - pipeline["ambiguity"].as_f64().expect("a")).abs() < 1e-6);
    assert!((latest("pass_rate").await - 1.0).abs() < 1e-6);
    db.close().await;
}

/// parity: jobs.TestNightlySmokeJudgeFailureAlert
#[tokio::test]
async fn judge_failure_alert() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let (config, _, counter) = judge_setup(&pool, true).await;
    let out = ran(&pool, &smoke("2030-07-20"), &config).await;
    assert_eq!((out.judge_attempts, out.judge_failures), (3, 3));
    assert_eq!((out.graded_runs, out.completion_pct), (3, 1.0));
    let failures = alerts(&pool, "job_failure").await;
    assert_eq!(failures.len(), 1);
    let (severity, subject, message) = &failures[0];
    assert_eq!(severity, "warning");
    assert!(message.contains("judge"), "{message}");
    assert_eq!(
        message,
        "nightly_smoke 2030-07-20: judge failed 3/3 gradings (100.0%, threshold 10%; spec 5.5)"
    );
    assert_eq!(subject["judge_attempts"], 3);
    assert_eq!(subject["judge_failures"], 3);
    assert_eq!(subject["judge_failure_rate"], 1.0);
    assert_eq!(
        count(
            &pool,
            "select count(*) from scores where dimension = 'style'"
        )
        .await,
        0
    );
    assert_eq!(
        count(
            &pool,
            "select count(*) from scores where dimension = 'answer_correctness'"
        )
        .await,
        3
    );
    assert!(counter.0.load(Ordering::SeqCst) >= 1);
    db.close().await;
}

/// parity: jobs.TestNightlySmokeTimeoutUsesConfiguredBatchJobTimeout
/// parity: jobs.TestWeeklySweepTimeoutUsesConfiguredBatchJobTimeout
#[test]
fn batch_timeouts_use_the_configured_batch_job_timeout() {
    let config = JobClientConfig {
        batch_job_timeout: Duration::from_secs(9 * 3600),
        ..JobClientConfig::default()
    };
    assert_eq!(
        JobKind::NightlySmoke.timeout(&config),
        Duration::from_secs(9 * 3600)
    );
    assert_eq!(
        JobKind::WeeklySweep.timeout(&config),
        Duration::from_secs(9 * 3600)
    );
    let output = BatchOutput {
        completion_pct: 1.0,
        ..BatchOutput::default()
    };
    let keys: Vec<String> = serde_json::to_value(&output)
        .expect("output")
        .as_object()
        .expect("object")
        .keys()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    assert_eq!(
        keys,
        [
            "actual_usd",
            "completed_runs",
            "completion_pct",
            "graded_runs",
            "planned_runs",
            "projected_usd"
        ]
    );
}
