//! The corpus-dependent jobs gates (reference Part 3 §6 "Corpus-dependent gates"). They
//! run only when `MAVERICK_CORPUS_DIR` points at the private corpus checkout's `corpus/`
//! folder, and skip otherwise, so CI without the corpus stays green.
//!
//! G7: no corpus content reaches this repository, a snapshot, a log, or an assertion
//! message. The tests assert on counts, ids, shapes, and job outputs only. Every test
//! registers corpus prompt fragments with the log capture's secret-leak guard, so any
//! task text in a log line fails the test.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use chrono::NaiveDate;
use graphile_worker::{WorkerOptions, WorkerUtils};
use maverick_domain::graders::judge_rubric::{FamilyResolver, Judge, JudgeClient, JudgeRequest};
use maverick_domain::graders::{
    BoxError, BoxFuture, GoldenAnswer, GradeInput, GradeTask, GraderKind, JsonSchema, JudgeRubric,
    KeyFacts, Registry, RuleScan, ValidationRegistry, parse_envelope,
};
use maverick_domain::scoring::high_stakes;
use maverick_domain::spend::{SpendCaps, estimate_input_tokens};
use maverick_domain::tasks;
use maverick_jobs::batch::{BatchConfig, BatchOutcome, WeeklySweepJob, run_batch};
use maverick_jobs::predictions::{CalibrationConfig, CalibrationScoreJob};
use maverick_jobs::rebuild::{
    AggregateRebuildJob, RebuildConfig, RebuildOutcome, aggregate_rebuild,
};
use maverick_jobs::regression_scan::{RegressionScanConfig, RegressionScanJob};
use maverick_jobs::{
    BatchFollowupArgs, BatchLeaseConfig, BatchLeaseManager, JobArgs, JobKind, NightlySmokeArgs,
    WeeklySweepArgs,
};
use maverick_providers::{CompletionRequest, CompletionResponse, ProviderClient, ProviderFuture};
use maverick_runner::{Cancellation, Clients, RunnerConfig};
use maverick_store::catalog::{self, NewModelColumn};
use maverick_store::corpus::{self, Task};
use maverick_store::health::GRAPHILE_WORKER_SCHEMA;
use maverick_store::{JsonRawValue, MIGRATOR};
use maverick_testdb::Schema;
use serde_json::Value;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use test_support::{TestTracing, init_test_tracing, step};

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

const CORPUS_DIR_ENV: &str = "MAVERICK_CORPUS_DIR";
const GOOD_MARKER: &str = "MAVERICKCOLUMNALPHA";
const BAD_MARKER: &str = "MAVERICKCOLUMNBRAVO";

/// The corpus checkout, or `None` (the test skips).
fn corpus_dir() -> Option<PathBuf> {
    match std::env::var(CORPUS_DIR_ENV) {
        Ok(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
        _ => {
            eprintln!("skipped: {CORPUS_DIR_ENV} is unset (the private corpus is not in the repo)");
            None
        }
    }
}

fn json(text: &str) -> Box<JsonRawValue> {
    JsonRawValue::from_string(text.to_owned()).expect("json")
}

/// Loads the corpus, stores it as `version`, and activates it. `strict`: validation
/// warnings fail too. Returns the stored active tasks.
async fn load_corpus(pool: &PgPool, dir: &Path, version: &str, strict: bool) -> Vec<Task> {
    let loaded = tasks::load(dir).expect("the corpus loads");
    let report = tasks::validate(&loaded, &ValidationRegistry::v1());
    step!(
        "validated {} tasks: {} errors, {} warnings",
        loaded.tasks.len(),
        report.errors.len(),
        report.warnings.len()
    );
    assert!(
        report.errors.is_empty(),
        "{} validation errors",
        report.errors.len()
    );
    if strict {
        assert!(
            report.warnings.is_empty(),
            "{} validation warnings",
            report.warnings.len()
        );
    }
    corpus::store_corpus(pool, version, "corpus gate", &loaded)
        .await
        .expect("store");
    corpus::activate_corpus(pool, version)
        .await
        .expect("activate");
    let stored = corpus::list_active_tasks_by_corpus_version(pool, version)
        .await
        .expect("tasks");
    assert_eq!(stored.len(), loaded.tasks.len());
    step!("stored and activated {} tasks as {version}", stored.len());
    stored
}

/// A fragment of `text` long enough to be distinctive, if it has one.
fn fragment(text: &str) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    (chars.len() >= 24).then(|| chars[..24].iter().collect())
}

/// Registers fragments of several task prompts as secrets: a log line that echoes task
/// text fails the test.
fn guard_corpus_text(capture: &TestTracing, stored: &[Task]) {
    let mut registered = 0;
    for task in stored.iter().step_by(stored.len().div_ceil(8).max(1)) {
        if let Some(fragment) = fragment(&task.prompt) {
            capture.register_secret(&fragment);
            registered += 1;
        }
    }
    assert!(registered >= 4, "too few distinctive prompt fragments");
    step!("registered {registered} prompt fragments as secrets");
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

fn lease() -> BatchLeaseManager {
    BatchLeaseManager::new(BatchLeaseConfig {
        lease_ttl: Duration::from_secs(600),
        heartbeat_interval: Duration::from_secs(200),
        owner_stale_after: Duration::from_secs(300),
        graphile_schema: GRAPHILE_WORKER_SCHEMA.to_string(),
    })
}

fn rebuild_config() -> RebuildConfig {
    RebuildConfig {
        matrix_export_enabled: false,
        alpha: 0.3,
        lambda_var: 0.5,
        lambda_amb: 0.25,
        min_nights: 3,
        runner_timeout: Duration::from_secs(120),
        lease: lease(),
    }
}

/// The request's task prompt text, as the runner assembles it.
fn assembled(task: &Task) -> String {
    let attachments = tasks::parse_attachments(task.attachments.get()).expect("attachments");
    tasks::prompt_text(&task.prompt, &attachments)
}

/// Answers every request by model: the output is scripted per model, with an optional
/// prefix for chosen prompts. Tokens in follow the spend estimate, tokens out are 50.
struct ScriptedProvider {
    outputs: HashMap<String, String>,
    /// Assembled prompt text to a prefix of the output, for one model.
    prefixed: (String, HashMap<String, String>),
    calls: AtomicUsize,
}

impl ProviderClient for ScriptedProvider {
    fn complete<'a>(&'a self, request: CompletionRequest) -> ProviderFuture<'a> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let base = self
                .outputs
                .get(&request.model)
                .cloned()
                .unwrap_or_default();
            let text = match self.prefixed.1.get(&request.prompt) {
                Some(prefix) if request.model == self.prefixed.0 => format!("{prefix} {base}"),
                _ => base,
            };
            Ok(CompletionResponse {
                text,
                tokens_in: usize::try_from(estimate_input_tokens(&request.prompt)).unwrap_or(0),
                tokens_out: 50,
            })
        })
    }

    fn name(&self) -> &str {
        "fake"
    }
}

/// Scores every rubric dimension 0.9 for a good-marker output, 0.2 for a bad-marker
/// output, and 0.5 otherwise.
#[derive(Default)]
struct ScriptedJudge {
    calls: AtomicUsize,
}

impl JudgeClient for ScriptedJudge {
    fn complete<'a>(&'a self, request: JudgeRequest) -> BoxFuture<'a, Result<String, BoxError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let (_, evaluated) = request
                .prompt
                .rsplit_once("--- output under evaluation ---\n")
                .ok_or("no output section")?;
            let (_, format) = request
                .prompt
                .rsplit_once("--- response format ---\n")
                .ok_or("no response format")?;
            let score = if evaluated.contains(GOOD_MARKER) {
                "0.9"
            } else if evaluated.contains(BAD_MARKER) {
                "0.2"
            } else {
                "0.5"
            };
            Ok(format.replace("<0.0-1.0>", score))
        })
    }
}

/// The judged column's family: its provider in the catalog.
struct StoreFamilies(PgPool);

impl FamilyResolver for StoreFamilies {
    fn family<'a>(&'a self, column_id: &'a str) -> BoxFuture<'a, Result<String, BoxError>> {
        Box::pin(async move {
            Ok(catalog::get_model_column(&self.0, column_id)
                .await?
                .provider)
        })
    }
}

fn registry(pool: &PgPool, judge: &Arc<ScriptedJudge>) -> Registry {
    let mut registry = Registry::new();
    registry.must_register(Arc::new(GoldenAnswer));
    registry.must_register(Arc::new(KeyFacts));
    registry.must_register(Arc::new(RuleScan));
    registry.must_register(Arc::new(JsonSchema));
    let judges = [("gemini-3.1-pro", "google"), ("gpt-5.5", "openai")]
        .map(|(model, family)| Judge {
            reference: format!("{model}/direct"),
            family: family.to_owned(),
            model: model.to_owned(),
            client: Some(Arc::clone(judge) as Arc<dyn JudgeClient>),
        })
        .to_vec();
    registry.must_register(Arc::new(
        JudgeRubric::new(judges, Some(Arc::new(StoreFamilies(pool.clone())))).expect("judge pool"),
    ));
    registry
}

/// The grader envelopes of a stored task.
fn envelopes(task: &Task) -> Vec<maverick_domain::graders::Envelope> {
    serde_json::from_str::<Vec<&JsonRawValue>>(task.grader_specs.get())
        .expect("grader specs")
        .into_iter()
        .map(|spec| parse_envelope(spec.get()).expect("envelope"))
        .collect()
}

/// Whether a grader detail carries an autofail marker: a failed critical rule, or a
/// found forbidden fact marked autofail.
fn autofails(detail: &Value) -> bool {
    detail["critical_failed"] == Value::Bool(true)
        || detail["forbidden_facts"].as_array().is_some_and(|facts| {
            facts
                .iter()
                .any(|f| f["found"] == Value::Bool(true) && f["autofail"] == Value::Bool(true))
        })
}

/// Strings that a grader regex (Go RE2 syntax) matches: one per top-level alternative.
/// A repetition takes its minimum count, a class its first printable member, and an
/// assertion (`\b`, `^`, `$`) nothing. The caller verifies each by grading it.
fn regex_samples(pattern: &str) -> Vec<String> {
    use regex_syntax::hir::{Class, Hir, HirKind};
    fn pick(contains: impl Fn(u32) -> bool) -> Option<char> {
        (u32::from(b'a')..=u32::from(b'z'))
            .chain(u32::from(b'A')..=u32::from(b'Z'))
            .chain(u32::from(b'0')..=u32::from(b'9'))
            .chain(0x20..0x7f)
            .find(|&c| contains(c))
            .and_then(char::from_u32)
    }
    fn sample(hir: &Hir) -> Option<String> {
        Some(match hir.kind() {
            HirKind::Empty | HirKind::Look(_) => String::new(),
            HirKind::Literal(literal) => String::from_utf8(literal.0.to_vec()).ok()?,
            HirKind::Class(Class::Unicode(class)) => pick(|c| {
                class
                    .ranges()
                    .iter()
                    .any(|r| u32::from(r.start()) <= c && c <= u32::from(r.end()))
            })?
            .to_string(),
            HirKind::Class(Class::Bytes(class)) => pick(|c| {
                class
                    .ranges()
                    .iter()
                    .any(|r| u32::from(r.start()) <= c && c <= u32::from(r.end()))
            })?
            .to_string(),
            HirKind::Repetition(repetition) => {
                sample(&repetition.sub)?.repeat(usize::try_from(repetition.min).ok()?)
            }
            HirKind::Capture(capture) => sample(&capture.sub)?,
            HirKind::Concat(parts) => parts.iter().map(sample).collect::<Option<String>>()?,
            HirKind::Alternation(branches) => branches.iter().find_map(sample)?,
        })
    }
    let Ok(translated) = maverick_domain::graders::goregex::translate(pattern) else {
        return Vec::new();
    };
    let Ok(hir) = regex_syntax::parse(&translated) else {
        return Vec::new();
    };
    let mut top = &hir;
    while let HirKind::Capture(capture) = top.kind() {
        top = &capture.sub;
    }
    let branches: Vec<&Hir> = match top.kind() {
        HirKind::Alternation(branches) => branches.iter().collect(),
        _ => vec![top],
    };
    branches
        .into_iter()
        .filter_map(sample)
        .filter(|s| !s.trim().is_empty())
        .collect()
}

/// The literal forms a high-stakes task's autofail rules would flag.
fn trap_candidates(task: &Task) -> Vec<String> {
    let mut out = Vec::new();
    let specs: Vec<Value> = serde_json::from_str(task.grader_specs.get()).expect("grader specs");
    for spec in specs {
        let body = &spec["spec"];
        let forms = |regex: bool, form: &str| {
            if regex {
                regex_samples(form)
            } else {
                vec![form.to_owned()]
            }
        };
        match spec["type"].as_str() {
            Some("key_facts") => {
                for fact in body["forbidden_facts"].as_array().into_iter().flatten() {
                    if fact["autofail"] != Value::Bool(true) {
                        continue;
                    }
                    let regex = fact["regex"] == Value::Bool(true);
                    for form in fact["any_of"].as_array().into_iter().flatten() {
                        out.extend(form.as_str().map(|f| forms(regex, f)).unwrap_or_default());
                    }
                }
            }
            Some("rule_scan") => {
                for rule in body["rules"].as_array().into_iter().flatten() {
                    if rule["critical"] != Value::Bool(true) {
                        continue;
                    }
                    let regex = rule["regex"] == Value::Bool(true);
                    out.extend(
                        rule["must_not_include"]
                            .as_str()
                            .map(|f| forms(regex, f))
                            .unwrap_or_default(),
                    );
                }
            }
            _ => {}
        }
    }
    out
}

/// Whether some grader of the task flags `output` with an autofail marker.
async fn flags_autofail(graders: &Registry, task: &Task, output: &str) -> bool {
    let grade_task = GradeTask {
        id: task.id.clone(),
        capability: task.capability.clone(),
        prompt: task.prompt.clone(),
        attachments_json: task.attachments.get().to_owned(),
    };
    for envelope in envelopes(task) {
        if !matches!(envelope.kind, GraderKind::KeyFacts | GraderKind::RuleScan) {
            continue;
        }
        let input = GradeInput {
            task: Some(&grade_task),
            output,
            run: None,
        };
        if let Ok(result) = graders
            .grade(envelope.kind.as_str(), &envelope.spec, input)
            .await
            && autofails(&serde_json::from_str(&result.detail).expect("detail"))
        {
            return true;
        }
    }
    false
}

/// One trap per capability: the first high-stakes task (by id) with a literal that its
/// own graders flag as autofail on the bad output and not on the good output.
async fn derive_traps(graders: &Registry, stored: &[Task]) -> BTreeMap<String, (String, String)> {
    let mut traps = BTreeMap::new();
    let mut by_capability: BTreeMap<&str, Vec<&Task>> = BTreeMap::new();
    for task in stored {
        by_capability
            .entry(&task.capability)
            .or_default()
            .push(task);
    }
    for (capability, mut candidates) in by_capability {
        candidates.sort_by(|a, b| a.id.cmp(&b.id));
        'tasks: for task in candidates {
            if !high_stakes(&task.domain_tags) {
                continue;
            }
            for literal in trap_candidates(task) {
                let bad = format!("{literal} {BAD_MARKER}");
                if flags_autofail(graders, task, &bad).await
                    && !flags_autofail(graders, task, GOOD_MARKER).await
                {
                    traps.insert(capability.to_owned(), (task.id.clone(), literal));
                    break 'tasks;
                }
            }
        }
    }
    traps
}

async fn count(pool: &PgPool, sql: &'static str) -> i64 {
    sqlx::query_scalar::<_, i64>(sql)
        .fetch_one(pool)
        .await
        .expect("count")
}

async fn runs_of(pool: &PgPool, task_id: &str, column: &str) -> i64 {
    sqlx::query_scalar("select count(*) from runs where task_id = $1 and model_column_id = $2")
        .bind(task_id)
        .bind(column)
        .fetch_one(pool)
        .await
        .expect("runs")
}

/// parity: jobs.TestRealCorpusSimulatedNight
#[tokio::test]
async fn real_corpus_simulated_night() {
    let Some(dir) = corpus_dir() else {
        return;
    };
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let capture = init_test_tracing();
    let pool = db.pool().await;
    step!("load the corpus");
    let stored = load_corpus(&pool, &dir, "real-corpus-night", false).await;
    guard_corpus_text(&capture, &stored);
    let smoke = stored.iter().filter(|t| t.smoke_member).count();
    seed_column(&pool, "col_warm", "anthropic", "claude-simulated", "active").await;
    seed_column(&pool, "col_cold", "openai", "gpt-simulated", "cold_start").await;
    let provider = Arc::new(ScriptedProvider {
        outputs: HashMap::from([
            (
                "claude-simulated".to_owned(),
                r#"{"answer": "simulated"}"#.to_owned(),
            ),
            (
                "gpt-simulated".to_owned(),
                r#"{"answer": "simulated"}"#.to_owned(),
            ),
        ]),
        prefixed: (String::new(), HashMap::new()),
        calls: AtomicUsize::new(0),
    });
    let judge = Arc::new(ScriptedJudge::default());
    let config = BatchConfig {
        repeats: 3,
        caps: SpendCaps::default(),
        lease: lease(),
        runner: RunnerConfig::default(),
        clients: Clients::new()
            .with(
                "anthropic",
                Arc::clone(&provider) as Arc<dyn ProviderClient>,
            )
            .with("openai", Arc::clone(&provider) as Arc<dyn ProviderClient>),
        graders: Arc::new(registry(&pool, &judge)),
        notifier: None,
    };

    step!("run the nightly smoke for 2030-07-10");
    let job = JobArgs::NightlySmoke(NightlySmokeArgs {
        night: "2030-07-10".to_owned(),
    });
    let out = match run_batch(&pool, &job, &config, "worker-corpus", &Cancellation::new())
        .await
        .expect("batch")
    {
        BatchOutcome::Ran(out) => out,
        other => panic!("not run: {other:?}"),
    };
    let planned = i64::try_from((smoke + stored.len()) * 3).expect("planned");
    step!(
        "batch: planned {}, completed {}, completion {}",
        out.planned_runs,
        out.completed_runs,
        out.completion_pct
    );
    assert_eq!(out.planned_runs, planned);
    assert_eq!(out.completed_runs, planned);
    assert!((out.completion_pct - 1.0).abs() < f64::EPSILON);
    assert_eq!(
        count(
            &pool,
            "select count(*) from graphile_worker._private_jobs j
             join graphile_worker._private_tasks t on t.id = j.task_id
             where t.identifier = 'aggregate_rebuild'"
        )
        .await,
        1,
        "one chain insert"
    );

    step!("rebuild the night");
    let rebuilt = match aggregate_rebuild(
        &pool,
        &BatchFollowupArgs {
            night: "2030-07-10".to_owned(),
            batch_id: out.batch_id.clone(),
        },
        &rebuild_config(),
        "worker-corpus",
    )
    .await
    .expect("rebuild")
    {
        RebuildOutcome::Rebuilt(rebuilt) => rebuilt,
        other => panic!("not rebuilt: {other:?}"),
    };
    step!(
        "rebuild: {} runs, {} cells, {} rows",
        rebuilt.runs,
        rebuilt.cells,
        rebuilt.aggregate_rows
    );
    assert!(rebuilt.cells > 0 && rebuilt.aggregate_rows > 0);
    assert!(
        capture.secret_violations().is_empty(),
        "task text reached a log line"
    );
    pool.close().await;
    db.close().await;
}

/// The M4 exit gate.
///
/// parity: jobs.TestFullMatrixWeeklySweep
#[tokio::test]
async fn full_matrix_weekly_sweep() {
    let Some(dir) = corpus_dir() else {
        return;
    };
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let capture = init_test_tracing();
    let pool = db.pool().await;
    step!("load the corpus strictly");
    let stored = load_corpus(&pool, &dir, "full-matrix-exit", true).await;
    guard_corpus_text(&capture, &stored);
    seed_column(
        &pool,
        "col_good",
        "anthropic",
        "claude-fullmatrix-good",
        "active",
    )
    .await;
    seed_column(
        &pool,
        "col_bad",
        "google",
        "gemini-fullmatrix-bad",
        "active",
    )
    .await;
    seed_column(
        &pool,
        "col_judge_google",
        "google",
        "gemini-3.1-pro",
        "retired",
    )
    .await;
    seed_column(&pool, "col_judge_openai", "openai", "gpt-5.5", "retired").await;
    let judge = Arc::new(ScriptedJudge::default());
    let graders = registry(&pool, &judge);

    step!("derive one autofail trap per capability");
    let traps = derive_traps(&graders, &stored).await;
    let capabilities: BTreeSet<&str> = stored.iter().map(|t| t.capability.as_str()).collect();
    step!(
        "{} traps for {} capabilities",
        traps.len(),
        capabilities.len()
    );
    assert_eq!(traps.len(), capabilities.len(), "one trap per capability");
    let by_id: HashMap<&str, &Task> = stored.iter().map(|t| (t.id.as_str(), t)).collect();
    let prefixed: HashMap<String, String> = traps
        .values()
        .map(|(task, literal)| (assembled(by_id[task.as_str()]), literal.clone()))
        .collect();
    for literal in prefixed.values().filter(|l| l.chars().count() >= 12) {
        capture.register_secret(literal);
    }
    let provider = Arc::new(ScriptedProvider {
        outputs: HashMap::from([
            ("claude-fullmatrix-good".to_owned(), GOOD_MARKER.to_owned()),
            ("gemini-fullmatrix-bad".to_owned(), BAD_MARKER.to_owned()),
        ]),
        prefixed: ("gemini-fullmatrix-bad".to_owned(), prefixed),
        calls: AtomicUsize::new(0),
    });
    let batch = BatchConfig {
        repeats: 3,
        caps: SpendCaps {
            weekly_usd: 25_000.0,
            ..SpendCaps::default()
        },
        lease: lease(),
        runner: RunnerConfig::default(),
        clients: Clients::new()
            .with(
                "anthropic",
                Arc::clone(&provider) as Arc<dyn ProviderClient>,
            )
            .with("google", Arc::clone(&provider) as Arc<dyn ProviderClient>),
        graders: Arc::new(graders),
        notifier: None,
    };

    step!("run the weekly sweep for 2032-07-04 through graphile_worker");
    let night = NaiveDate::from_ymd_opt(2032, 7, 4).expect("night");
    let week = night.format("%G-W%V").to_string();
    let worker = WorkerOptions::default()
        .concurrency(1)
        .schema(GRAPHILE_WORKER_SCHEMA)
        .pg_pool(pool.clone())
        .add_extension(batch)
        .add_extension(rebuild_config())
        .add_extension(RegressionScanConfig {
            alpha: 0.3,
            lease: lease(),
            notifier: None,
        })
        .add_extension(CalibrationConfig { lease: lease() })
        .define_job::<WeeklySweepJob>()
        .define_job::<AggregateRebuildJob>()
        .define_job::<RegressionScanJob>()
        .define_job::<CalibrationScoreJob>()
        .init()
        .await
        .expect("worker");
    worker
        .create_utils()
        .add_job(
            WeeklySweepJob(WeeklySweepArgs {
                week: week.clone(),
                night: "2032-07-04".to_owned(),
            }),
            JobKind::WeeklySweep.job_spec(&week),
        )
        .await
        .expect("enqueue");
    for _ in 0..8 {
        worker.run_once().await.expect("run");
        if count(&pool, "select count(*) from graphile_worker._private_jobs").await == 0 {
            break;
        }
    }
    assert_eq!(
        count(&pool, "select count(*) from graphile_worker._private_jobs").await,
        0,
        "the chain drained"
    );

    let tasks_n = i64::try_from(stored.len()).expect("tasks");
    let planned = tasks_n * 2 * 3;
    step!("check the sweep: {planned} planned runs");
    assert_eq!(
        count(
            &pool,
            "select count(*) from batches where kind = 'weekly_sweep' and status = 'completed'"
        )
        .await,
        1,
        "not refused"
    );
    assert_eq!(
        count(
            &pool,
            "select count(*) from alerts where kind = 'spend_refusal'"
        )
        .await,
        0
    );
    assert_eq!(count(&pool, "select count(*) from runs").await, planned);
    assert_eq!(
        count(&pool, "select count(*) from runs where status = 'ok'").await,
        planned,
        "completed = planned"
    );
    assert_eq!(
        count(
            &pool,
            "select count(*) from runs r where not exists (select 1 from scores s where s.run_id = r.id)"
        )
        .await,
        0,
        "graded = planned: every run has score rows"
    );
    let graders_per_run: i64 = stored
        .iter()
        .map(|t| i64::try_from(envelopes(t).len()).expect("n"))
        .sum();
    let judge_envelopes: i64 = stored
        .iter()
        .map(|t| {
            i64::try_from(
                envelopes(t)
                    .iter()
                    .filter(|e| e.kind == GraderKind::JudgeRubric)
                    .count(),
            )
            .expect("n")
        })
        .sum();
    assert_eq!(
        count(
            &pool,
            "select count(*) from (select distinct run_id, grader_idx from scores) g"
        )
        .await,
        graders_per_run * 2 * 3,
        "no grader failures"
    );
    assert_eq!(
        count(
            &pool,
            "select count(*) from (select distinct run_id, grader_idx from scores
                                   where detail ? 'judged_family') g"
        )
        .await,
        judge_envelopes * 2 * 3,
        "judge attempts, 0 failures"
    );
    assert_eq!(
        i64::try_from(judge.calls.load(Ordering::SeqCst)).expect("calls"),
        judge_envelopes * 2 * 3 * 2,
        "two ordered judge calls per judge grading"
    );
    assert_eq!(
        count(
            &pool,
            "select count(*) from alerts where kind = 'job_failure'"
        )
        .await,
        0
    );
    let families: Vec<(String, String, String)> = sqlx::query_as(
        "select distinct r.model_column_id, s.detail->>'judged_family', s.detail->'judge'->>'family'
         from scores s join runs r on r.id = s.run_id
         where s.detail ? 'judged_family'
         order by 1",
    )
    .fetch_all(&pool)
    .await
    .expect("judge rows");
    assert_eq!(
        families,
        [
            ("col_bad", "google", "openai"),
            ("col_good", "anthropic", "google"),
        ]
        .map(|(c, j, f)| (c.to_owned(), j.to_owned(), f.to_owned()))
    );

    step!("check the aggregates");
    // D6 adds `availability`: 7 dimensions per cell where Go had 6.
    let _ = parity_support::divergence!("D6");
    let corpus_cells: BTreeSet<(&str, i16)> = stored
        .iter()
        .map(|t| (t.capability.as_str(), t.difficulty_tier))
        .collect();
    assert_eq!(corpus_cells.len(), 10 * 5, "10 capabilities x 5 tiers");
    let cells = i64::try_from(corpus_cells.len() * 2).expect("cells");
    assert_eq!(
        count(
            &pool,
            "select count(distinct (model_column_id, capability, difficulty_tier)) from aggregates"
        )
        .await,
        cells
    );
    assert_eq!(
        count(&pool, "select count(*) from aggregates").await,
        7 * cells
    );
    assert_eq!(
        count(
            &pool,
            "select count(*) from aggregates
             where n <> 1 or measured or matrix_version <> 'full-matrix-exit'
                or as_of <> date '2032-07-04'"
        )
        .await,
        0,
        "n 1, not measured, the corpus version, the sweep night"
    );
    assert_eq!(
        count(
            &pool,
            "select count(*) from aggregates
             where (dimension in ('quality', 'pass_rate', 'ambiguity', 'availability')
                    and (ewma < 0 or ewma > 1))
                or (dimension in ('latency_p50_ms', 'latency_p95_ms', 'cost_per_run_usd')
                    and ewma < 0)"
        )
        .await,
        0,
        "ranges"
    );
    let quality: Vec<(String, String, i16, f32)> = sqlx::query_as(
        "select model_column_id, capability, difficulty_tier, ewma
         from aggregates where dimension = 'quality'",
    )
    .fetch_all(&pool)
    .await
    .expect("quality");
    let mean = |column: &str| {
        let values: Vec<f64> = quality
            .iter()
            .filter(|q| q.0 == column)
            .map(|q| f64::from(q.3))
            .collect();
        values.iter().sum::<f64>() / values.len() as f64
    };
    let (good, bad) = (mean("col_good"), mean("col_bad"));
    step!("mean quality: good {good:.4}, bad {bad:.4}");
    assert!(good > bad, "mean good quality > mean bad");

    step!("check the traps");
    let judged: BTreeSet<&str> = stored
        .iter()
        .filter(|t| {
            envelopes(t)
                .iter()
                .any(|e| e.kind == GraderKind::JudgeRubric)
        })
        .map(|t| t.capability.as_str())
        .collect();
    let details: Vec<(String, String, Value)> =
        sqlx::query_as::<_, (String, String, sqlx::types::Json<Value>)>(
            "select r.task_id, r.model_column_id, s.detail
         from scores s join runs r on r.id = s.run_id
         where s.detail is not null",
        )
        .fetch_all(&pool)
        .await
        .expect("details")
        .into_iter()
        .map(|(t, c, d)| (t, c, d.0))
        .collect();
    for (capability, (task_id, _)) in &traps {
        assert_eq!(
            (
                runs_of(&pool, task_id, "col_bad").await,
                runs_of(&pool, task_id, "col_good").await
            ),
            (3, 3)
        );
        let autofailed = |column: &str| {
            details
                .iter()
                .filter(|(t, c, d)| t == task_id && c == column && autofails(d))
                .count()
        };
        assert!(
            autofailed("col_bad") >= 3,
            "{capability} trap {task_id}: the bad runs autofail"
        );
        assert_eq!(
            autofailed("col_good"),
            0,
            "{capability} trap {task_id}: the good runs do not"
        );
        if !judged.contains(capability.as_str()) {
            let tier = by_id[task_id.as_str()].difficulty_tier;
            let cell = |column: &str| {
                quality
                    .iter()
                    .find(|q| q.0 == column && q.1 == *capability && q.2 == tier)
                    .map(|q| q.3)
                    .expect("cell")
            };
            assert!(
                cell("col_bad") < cell("col_good"),
                "{capability} tier {tier}: the trapped cell's bad quality is below the good"
            );
        }
    }
    step!(
        "{} judge-free capabilities checked",
        capabilities.len() - judged.len()
    );
    assert!(
        capture.secret_violations().is_empty(),
        "task text reached a log line"
    );
    pool.close().await;
    db.close().await;
}
