//! The job crash and restart acceptance tests (spec §11.2 "Acceptance"; master plan
//! §3.1): real Postgres, real graphile_worker, and the fake provider.
//!
//! The worker runs as a child process: this test binary re-executed with
//! `--exact child_worker` and a JSON spec in `MAVERICK_JOBS_CRASH_CHILD`. The parent
//! seeds the smoke fixture, enqueues the night's `nightly_smoke`, interrupts the batch,
//! and checks the end state: an empty queue (so no permanently failed job), exactly one
//! completed batch and one of each follow-up, no duplicate runs or aggregate
//! contributions, no job_failure alert, and at most one repeated provider call per
//! takeover of the batch.
//!
//! The interruptions do not depend on timing. The child hangs one chosen provider call,
//! once across restarts: a marker file records the hang, and the parent interrupts while
//! the call hangs. Calls are sequential (one provider per group, one call at a time per
//! provider), so an interruption repeats only the call in flight.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use graphile_worker::{WorkerOptions, WorkerRecoveryConfig, WorkerUtils};
use maverick_domain::graders::{GoldenAnswer, Registry};
use maverick_domain::spend::SpendCaps;
use maverick_jobs::batch::{BatchConfig, NightlySmokeJob};
use maverick_jobs::predictions::{CalibrationConfig, CalibrationScoreJob};
use maverick_jobs::rebuild::{AggregateRebuildJob, RebuildConfig};
use maverick_jobs::regression_scan::{RegressionScanConfig, RegressionScanJob};
use maverick_jobs::{
    BatchLeaseManager, BatchLeaseOutcome, JobClientConfig, JobKind, NightlySmokeArgs,
    deterministic_batch_id,
};
use maverick_providers::fake::{FakeProvider, FixtureSpec};
use maverick_providers::{CompletionRequest, ProviderClient, ProviderFuture};
use maverick_runner::{Clients, ProviderLimits, RunnerConfig};
use maverick_store::catalog::{self, NewModelColumn};
use maverick_store::corpus::{self, TaskUpsert};
use maverick_store::health::GRAPHILE_WORKER_SCHEMA;
use maverick_store::{JsonRawValue, MIGRATOR};
use maverick_testdb::Schema;
use serde::{Deserialize, Serialize};
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

const CHILD_ENV: &str = "MAVERICK_JOBS_CRASH_CHILD";
const VERSION: &str = "corpus-crash";
const WARM_MODEL: &str = "claude-crash";
const COLD_MODEL: &str = "gpt-crash";
/// Every scenario's night.
const NIGHT: &str = "2030-11-05";
/// 2 smoke tasks x the warm column + 3 tasks x the cold column, 3 repeats each.
const PLANNED_RUNS: i64 = 15;
/// 5 cells x 7 dimensions.
const AGGREGATE_ROWS: i64 = 35;
/// The call that hangs: the second call of the cold-start group, so the interruption
/// lands after one group finished and mid-way through the next.
const HANG_AT_CALL: u64 = 8;
const PROVIDER_LATENCY_MS: u64 = 50;

// Short lease and recovery settings. The startup rule holds: the lease TTL is shorter
// than graphile_worker's recovery threshold.
// Margins leave room for a loaded host.
const LEASE_TTL: Duration = Duration::from_secs(4);
const HEARTBEAT: Duration = Duration::from_millis(500);
const SWEEP_THRESHOLD: Duration = Duration::from_secs(10);
const SWEEP_INTERVAL: Duration = Duration::from_secs(1);
const RECOVERY_DELAY: Duration = Duration::from_millis(200);
/// No batch in these scenarios runs this long, except one that hangs.
const LONG_TIMEOUT: Duration = Duration::from_secs(600);
/// How long a scenario may take to settle after its interruption.
const SETTLE: Duration = Duration::from_secs(90);

/// What the child worker runs with.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChildSpec {
    database_url: String,
    /// One line per provider call, appended before the call starts.
    call_log: PathBuf,
    /// Created when the hanging call starts; while it exists no call hangs again.
    hang_marker: PathBuf,
    /// The provider call that hangs, and for how long.
    hang: Option<(u64, u64)>,
    concurrency: usize,
    batch_job_timeout_ms: u64,
}

struct SeedTask {
    id: &'static str,
    tier: i16,
    smoke: bool,
    prompt: &'static str,
    answer: &'static str,
}

const TASKS: [SeedTask; 3] = [
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

fn json(text: &str) -> Box<JsonRawValue> {
    JsonRawValue::from_string(text.to_owned()).expect("json")
}

async fn seed(pool: &PgPool) {
    corpus::upsert_corpus_version(pool, VERSION, "crash tests", 3, true)
        .await
        .expect("version");
    let tags: Vec<String> = Vec::new();
    for task in &TASKS {
        let graders = format!(
            r#"[{{"type":"golden_answer","version":1,"weight_dimensions":{{"answer_correctness":1}},"spec":{{"answers":[{{"value":"{}"}}]}}}}]"#,
            task.answer
        );
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
                grader_specs: &json(&graders),
                dimension_weights: &json(r#"{"answer_correctness":1.0}"#),
                pass_threshold: 0.7,
                smoke_member: task.smoke,
                corpus_version: VERSION,
                active: true,
            },
        )
        .await
        .expect("task");
    }
    for (id, provider, model, status) in [
        ("col_warm", "anthropic", WARM_MODEL, "active"),
        ("col_cold", "openai", COLD_MODEL, "cold_start"),
    ] {
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
}

fn count_lines(path: &Path) -> u64 {
    std::fs::read_to_string(path)
        .map(|text| u64::try_from(text.lines().count()).expect("count"))
        .unwrap_or(0)
}

// ---- the child worker ----

/// Logs each call to a file before it starts, so the count survives SIGKILL, and hangs
/// the chosen call once.
struct Scripted {
    inner: Arc<FakeProvider>,
    spec: ChildSpec,
}

impl ProviderClient for Scripted {
    fn complete<'a>(&'a self, request: CompletionRequest) -> ProviderFuture<'a> {
        Box::pin(async move {
            let mut log = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.spec.call_log)
                .expect("call log");
            log.write_all(b"call\n").expect("call log write");
            drop(log);
            if let Some((at, hang_ms)) = self.spec.hang
                && count_lines(&self.spec.call_log) == at
                && std::fs::File::create_new(&self.spec.hang_marker).is_ok()
            {
                tokio::time::sleep(Duration::from_millis(hang_ms)).await;
            }
            self.inner.complete(request).await
        })
    }

    fn name(&self) -> &str {
        self.inner.name()
    }
}

fn fake() -> Arc<FakeProvider> {
    let mut specs = Vec::new();
    for model in [WARM_MODEL, COLD_MODEL] {
        for task in &TASKS {
            let spec = serde_json::json!({
                "model": model,
                "prompt": task.prompt,
                "output": task.answer,
                "tokens_in": 100,
                "tokens_out": 10,
                "latency_ms": PROVIDER_LATENCY_MS,
            });
            specs.push(serde_json::from_value::<FixtureSpec>(spec).expect("fixture"));
        }
    }
    Arc::new(FakeProvider::from_fixtures(&specs).expect("fake"))
}

/// The worker process of the scenarios. Without `MAVERICK_JOBS_CRASH_CHILD` it is an
/// empty test.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn child_worker() {
    let Ok(spec) = std::env::var(CHILD_ENV) else {
        return;
    };
    let spec: ChildSpec = serde_json::from_str(&spec).expect("child spec");
    let pool = PgPoolOptions::new()
        .max_connections(20)
        .connect(&spec.database_url)
        .await
        .expect("connect");
    let jobs = JobClientConfig {
        batch_job_timeout: Duration::from_millis(spec.batch_job_timeout_ms),
        batch_lease_ttl: LEASE_TTL,
        batch_heartbeat_interval: HEARTBEAT,
        recovery_sweep_threshold: SWEEP_THRESHOLD,
        ..JobClientConfig::default()
    };
    let lease = BatchLeaseManager::from_job_client_config(&jobs);
    let provider: Arc<dyn ProviderClient> = Arc::new(Scripted {
        inner: fake(),
        spec: spec.clone(),
    });
    let mut registry = Registry::new();
    registry.must_register(Arc::new(GoldenAnswer));
    let one_at_a_time = ProviderLimits {
        max_concurrency: 1,
        rpm_limit: 0,
    };
    let batch = BatchConfig {
        repeats: 3,
        caps: SpendCaps::default(),
        lease: lease.clone(),
        runner: RunnerConfig {
            timeout: Duration::from_secs(300),
            limits: [
                ("anthropic".to_owned(), one_at_a_time),
                ("openai".to_owned(), one_at_a_time),
            ]
            .into_iter()
            .collect(),
            ..RunnerConfig::default()
        },
        clients: Clients::new()
            .with("anthropic", Arc::clone(&provider))
            .with("openai", provider),
        graders: Arc::new(registry),
        notifier: None,
    };
    let worker = WorkerOptions::default()
        .schema(GRAPHILE_WORKER_SCHEMA)
        .concurrency(spec.concurrency)
        .pg_pool(pool.clone())
        .poll_interval(Duration::from_millis(100))
        .worker_recovery(
            WorkerRecoveryConfig::default()
                .heartbeat_interval(HEARTBEAT)
                .sweep_interval(SWEEP_INTERVAL)
                .sweep_threshold(SWEEP_THRESHOLD)
                .recovery_delay(RECOVERY_DELAY),
        )
        .listen_os_shutdown_signals(true)
        .shutdown_grace_period(Duration::from_millis(300))
        .shutdown_interrupted_job_retry_delay(RECOVERY_DELAY)
        .add_extension(jobs)
        .add_extension(batch)
        .add_extension(RebuildConfig {
            alpha: 0.3,
            lambda_var: 0.5,
            lambda_amb: 0.25,
            min_nights: 3,
            runner_timeout: Duration::from_secs(300),
            lease: lease.clone(),
        })
        .add_extension(RegressionScanConfig {
            alpha: 0.3,
            lease: lease.clone(),
            notifier: None,
        })
        .add_extension(CalibrationConfig { lease })
        .define_job::<NightlySmokeJob>()
        .define_job::<AggregateRebuildJob>()
        .define_job::<RegressionScanJob>()
        .define_job::<CalibrationScoreJob>()
        .init()
        .await
        .expect("worker");
    worker.run().await.expect("worker run");
}

// ---- the parent ----

struct Scenario {
    db: maverick_testdb::TestDatabase,
    pool: PgPool,
    call_log: PathBuf,
    hang_marker: PathBuf,
    children: Vec<Child>,
}

impl Scenario {
    async fn start(name: &str) -> Option<Self> {
        let db = maverick_testdb::database(&SCHEMA).await?;
        let pool = db.pool().await;
        seed(&pool).await;
        let stem = format!("maverick-crash-{name}-{}-{}", std::process::id(), db.name());
        let call_log = std::env::temp_dir().join(format!("{stem}.calls"));
        let hang_marker = std::env::temp_dir().join(format!("{stem}.hung"));
        let _ = std::fs::remove_file(&call_log);
        let _ = std::fs::remove_file(&hang_marker);
        Some(Scenario {
            db,
            pool,
            call_log,
            hang_marker,
            children: Vec::new(),
        })
    }

    /// Starts a worker process and returns its index.
    fn spawn(&mut self, hang: Option<(u64, u64)>, batch_job_timeout: Duration) -> usize {
        let spec = ChildSpec {
            database_url: self.db.url().to_owned(),
            call_log: self.call_log.clone(),
            hang_marker: self.hang_marker.clone(),
            hang,
            concurrency: 4,
            batch_job_timeout_ms: u64::try_from(batch_job_timeout.as_millis()).expect("ms"),
        };
        let child = Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "child_worker",
                "--exact",
                "--nocapture",
                "--test-threads",
                "1",
            ])
            .env(CHILD_ENV, serde_json::to_string(&spec).expect("spec"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the worker");
        self.children.push(child);
        self.children.len() - 1
    }

    /// A worker whose [`HANG_AT_CALL`] call hangs for `hang_ms`, once.
    fn spawn_hanging(&mut self, hang_ms: u64) -> usize {
        self.spawn(Some((HANG_AT_CALL, hang_ms)), LONG_TIMEOUT)
    }

    fn spawn_plain(&mut self) -> usize {
        self.spawn(None, LONG_TIMEOUT)
    }

    /// Waits until the chosen call hangs.
    async fn wait_for_hang(&self) {
        let started = Instant::now();
        while !self.hang_marker.exists() {
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "the batch never reached call {HANG_AT_CALL}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(self.runs().await < PLANNED_RUNS, "the hang is mid-batch");
    }

    /// SIGKILL, then reap.
    fn kill9(&mut self, index: usize) {
        let child = &mut self.children[index];
        child.kill().expect("SIGKILL");
        child.wait().expect("reap");
    }

    /// SIGTERM, then wait for the graceful exit.
    async fn terminate(&mut self, index: usize) {
        let pid = self.children[index].id().to_string();
        let status = Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .expect("kill -TERM");
        assert!(status.success(), "kill -TERM {pid}");
        let started = Instant::now();
        loop {
            if let Some(exit) = self.children[index].try_wait().expect("try_wait") {
                assert!(
                    exit.success(),
                    "the worker exits cleanly on SIGTERM: {exit}"
                );
                return;
            }
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "the worker did not exit after SIGTERM"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn enqueue(&self) {
        let job = NightlySmokeJob(NightlySmokeArgs {
            night: NIGHT.to_owned(),
        });
        WorkerUtils::new(self.pool.clone(), GRAPHILE_WORKER_SCHEMA)
            .add_job(job, JobKind::NightlySmoke.job_spec(NIGHT))
            .await
            .expect("enqueue");
    }

    async fn count(&self, sql: &'static str) -> i64 {
        sqlx::query_scalar::<_, i64>(sql)
            .fetch_one(&self.pool)
            .await
            .expect("count")
    }

    async fn runs(&self) -> i64 {
        self.count("select count(*) from runs").await
    }

    async fn queued(&self) -> i64 {
        self.count("select count(*) from graphile_worker._private_jobs")
            .await
    }

    fn calls(&self) -> u64 {
        count_lines(&self.call_log)
    }

    async fn batch_status(&self) -> (String, i32) {
        sqlx::query_as("select status, attempt from batches where kind = 'nightly_smoke'")
            .fetch_one(&self.pool)
            .await
            .expect("batch")
    }

    /// Waits until the queue is empty and the whole chain completed.
    async fn settle(&self) {
        let started = Instant::now();
        loop {
            let completed = self
                .count(
                    "select count(*) from batches
                     where status = 'completed'
                       and kind in ('nightly_smoke', 'aggregate_rebuild', 'regression_scan',
                                    'calibration_score')",
                )
                .await;
            if self.queued().await == 0 && completed == 4 {
                return;
            }
            if started.elapsed() > SETTLE {
                let jobs: Vec<(String, i16, i16, Option<String>)> = sqlx::query_as(
                    "select t.identifier, j.attempts, j.max_attempts, j.last_error
                     from graphile_worker._private_jobs j
                     join graphile_worker._private_tasks t on t.id = j.task_id",
                )
                .fetch_all(&self.pool)
                .await
                .expect("jobs");
                let batches: Vec<(String, String, i32)> =
                    sqlx::query_as("select kind, status, attempt from batches order by kind")
                        .fetch_all(&self.pool)
                        .await
                        .expect("batches");
                panic!(
                    "not settled after {SETTLE:?}: jobs {jobs:?}, batches {batches:?}, runs {}, calls {}",
                    self.runs().await,
                    self.calls()
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// The acceptance end state. `interrupted`: the batch was cut mid-call at least
    /// once. Each extra delivery of the batch (a takeover) repeats at most the one call
    /// that was in flight.
    async fn assert_clean_end(&self, interrupted: bool) {
        self.settle().await;
        let batch_id = deterministic_batch_id(JobKind::NightlySmoke, NIGHT);
        let batches: Vec<(String, String, String)> = sqlx::query_as(
            "select kind, batch_window, status from batches order by kind, batch_window",
        )
        .fetch_all(&self.pool)
        .await
        .expect("batches");
        assert_eq!(
            batches,
            [
                ("aggregate_rebuild", batch_id.as_str(), "completed"),
                ("calibration_score", batch_id.as_str(), "completed"),
                ("nightly_smoke", NIGHT, "completed"),
                ("regression_scan", batch_id.as_str(), "completed"),
            ]
            .map(|(k, w, s)| (k.to_owned(), w.to_owned(), s.to_owned())),
            "exactly one completed batch and one of each follow-up"
        );
        assert_eq!(self.runs().await, PLANNED_RUNS);
        assert_eq!(
            self.count("select count(distinct (task_id, model_column_id, repeat_index)) from runs")
                .await,
            PLANNED_RUNS
        );
        assert_eq!(
            self.count("select count(*) from runs where status = 'ok'")
                .await,
            PLANNED_RUNS
        );
        assert_eq!(
            self.count("select count(*) from scores").await,
            PLANNED_RUNS
        );
        assert_eq!(
            self.count("select count(*) from aggregates").await,
            AGGREGATE_ROWS
        );
        assert_eq!(
            self.count("select count(*) from aggregates where n <> 1")
                .await,
            0,
            "each cell folded the night once"
        );
        assert_eq!(
            self.count("select count(*) from alerts where kind = 'job_failure'")
                .await,
            0,
            "the batch completed every run"
        );
        let (_, attempt) = self.batch_status().await;
        let takeovers = u64::try_from(attempt - 1).expect("attempt");
        let planned = u64::try_from(PLANNED_RUNS).expect("runs");
        let calls = self.calls();
        assert!(
            calls <= planned + takeovers,
            "{calls} provider calls for {planned} runs and {takeovers} takeovers"
        );
        if interrupted {
            assert!(takeovers >= 1, "the interrupted batch was taken over");
            assert!(calls > planned, "the hanging call was made again");
        }
    }

    async fn finish(mut self) {
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_file(&self.call_log);
        let _ = std::fs::remove_file(&self.hang_marker);
        self.pool.close().await;
        self.db.close().await;
    }
}

/// 1. `kill -9` of the worker mid-batch, then a restart. graphile_worker's recovery
/// sweep releases the job from the dead worker. By then the lease has expired, so the
/// new worker takes it over and resumes from the persisted runs.
#[tokio::test(flavor = "multi_thread")]
async fn kill9_mid_batch_then_restart() {
    let Some(mut s) = Scenario::start("kill9").await else {
        return;
    };
    let _ = parity_support::divergence!("D10");
    let first = s.spawn_hanging(600_000);
    s.enqueue().await;
    s.wait_for_hang().await;
    s.kill9(first);
    s.spawn_plain();
    s.assert_clean_end(true).await;
    s.finish().await;
}

/// 2. `kill -9` where graphile_worker's recovery fires while the application lease is
/// still live. The sweep removes the dead worker from the registry, so its lease is
/// taken over before `lease_expires_at`.
#[tokio::test(flavor = "multi_thread")]
async fn kill9_recovery_while_the_lease_is_live() {
    let Some(mut s) = Scenario::start("kill9live").await else {
        return;
    };
    let _ = parity_support::divergence!("D10");
    let first = s.spawn_hanging(600_000);
    s.enqueue().await;
    s.wait_for_hang().await;
    s.kill9(first);
    // Hold the lease open far past the recovery sweep.
    sqlx::query(
        "update batches set lease_expires_at = now() + interval '1 hour'
         where kind = 'nightly_smoke'",
    )
    .execute(&s.pool)
    .await
    .expect("extend the lease");
    s.spawn_plain();
    s.assert_clean_end(true).await;
    s.finish().await;
}

/// 3. A graceful restart (SIGTERM) mid-batch. graphile_worker aborts the handler after
/// its grace period, requeues the job, and deregisters the worker. The next worker takes
/// the lease over and resumes.
#[tokio::test(flavor = "multi_thread")]
async fn graceful_restart_mid_batch() {
    let Some(mut s) = Scenario::start("sigterm").await else {
        return;
    };
    let _ = parity_support::divergence!("D10");
    let first = s.spawn_hanging(600_000);
    s.enqueue().await;
    s.wait_for_hang().await;
    s.terminate(first).await;
    assert_eq!(
        s.queued().await,
        1,
        "the interrupted job is back in the queue"
    );
    s.spawn_plain();
    s.assert_clean_end(true).await;
    s.finish().await;
}

/// 4. An enqueue of the same (kind, window) while the batch is running. graphile_worker
/// adds a second job because the first is locked. Every job shares the one named queue,
/// which graphile_worker runs serially, so the duplicate starts after the batch and finds
/// its completed lease.
#[tokio::test(flavor = "multi_thread")]
async fn enqueue_while_running() {
    let Some(mut s) = Scenario::start("dup-running").await else {
        return;
    };
    let _ = parity_support::divergence!("D10");
    s.spawn_hanging(1_500);
    s.enqueue().await;
    s.wait_for_hang().await;
    assert_eq!(s.batch_status().await, ("running".to_owned(), 1));
    s.enqueue().await;
    assert_eq!(
        s.queued().await,
        2,
        "a second job while the first is locked"
    );
    s.assert_clean_end(false).await;
    s.finish().await;
}

/// 5. An enqueue of the same (kind, window) after it completed. graphile_worker has
/// deleted the completed job, so the new job runs and finds the completed lease.
#[tokio::test(flavor = "multi_thread")]
async fn enqueue_after_completed() {
    let Some(mut s) = Scenario::start("dup-done").await else {
        return;
    };
    let _ = parity_support::divergence!("D10");
    s.spawn_plain();
    s.enqueue().await;
    s.assert_clean_end(false).await;
    s.enqueue().await;
    let started = Instant::now();
    while s.queued().await > 0 {
        assert!(started.elapsed() < SETTLE, "the duplicate job never ran");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    s.assert_clean_end(false).await;
    s.finish().await;
}

/// 6. A handler timeout. The batch's per-kind `tokio::time::timeout` fires while a call
/// hangs. The delivery is dropped and fails, and graphile_worker retries it on the same
/// worker, which resumes under its own lease. The failure tail is never detached (D11),
/// so the dropped delivery writes no alert.
#[tokio::test(flavor = "multi_thread")]
async fn handler_timeout() {
    let Some(mut s) = Scenario::start("timeout").await else {
        return;
    };
    let _ = parity_support::divergence!("D10");
    let _ = parity_support::divergence!("D11");
    s.spawn(Some((HANG_AT_CALL, 600_000)), Duration::from_secs(5));
    s.enqueue().await;
    s.wait_for_hang().await;
    s.assert_clean_end(true).await;
    s.finish().await;
}

/// Two deliveries of one window on the same worker id: while the first handler holds
/// the lease, the second is a no-op. After the first handler drops (a timeout, a panic),
/// the next delivery on that worker takes the lease over.
#[tokio::test]
async fn same_worker_duplicate_waits_for_the_live_handler() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let _ = parity_support::divergence!("D10");
    let lease = BatchLeaseManager::from_job_client_config(&JobClientConfig::default());
    let first = lease
        .claim(&pool, JobKind::NightlySmoke, "2030-11-06", "worker-a")
        .await
        .expect("first");
    assert!(matches!(first, BatchLeaseOutcome::Claimed(_)), "{first:?}");
    let duplicate = lease
        .claim(&pool, JobKind::NightlySmoke, "2030-11-06", "worker-a")
        .await
        .expect("duplicate");
    assert!(
        matches!(duplicate, BatchLeaseOutcome::HeldByOther(_)),
        "{duplicate:?}"
    );
    drop(first);
    let retry = lease
        .claim(&pool, JobKind::NightlySmoke, "2030-11-06", "worker-a")
        .await
        .expect("retry");
    assert!(matches!(retry, BatchLeaseOutcome::TookOver(_)), "{retry:?}");
    drop(retry);
    pool.close().await;
    db.close().await;
}
