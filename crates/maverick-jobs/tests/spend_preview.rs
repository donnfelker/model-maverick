//! `preview_spend` on real Postgres (bead 9.15): the batch's composition, projection, and
//! G8 guard verdict, read only. The expected amounts are computed by hand below.
//!
//! Tasks (repeats 3; est_in = ceil(chars / 4) + 600):
//! - `task_1`: smoke, "abcd" (4 chars): est_in 601, max_tokens 100.
//! - `task_2`: smoke, "abcde" (5 chars): est_in 602, max_tokens 50.
//! - `task_3`: not smoke, "abcdefgh" plus one attachment "n"/"xyz": the prompt text is
//!   8 + 18 ("\n\n--- attachment: ") + 1 + 5 (" ---\n") + 3 = 35 chars, est_in 609,
//!   max_tokens 200.
//! - an inactive task and a task of an inactive version: never composed.
//!
//! Columns (USD per Mtok in / out): `col_a` active 2/10, `col_b` deprecated 1/1,
//! `col_c` cold_start 4/20, `col_d` retired (not runnable).
//!
//! nightly_smoke: smoke = {task_1, task_2} x {col_a, col_b}; cold_start = all 3 x {col_c}.
//! - col_a: (601*2 + 100*10 + 602*2 + 50*10) / 1e6 * 3 = 3906 * 3 / 1e6 = 0.011718
//! - col_b: (601 + 100 + 602 + 50) / 1e6 * 3 = 1353 * 3 / 1e6 = 0.004059
//! - col_c: (601*4 + 100*20 + 602*4 + 50*20 + 609*4 + 200*20) / 1e6 * 3
//!   = 14248 * 3 / 1e6 = 0.042744
//! - total 0.058521 over 6 + 6 + 9 = 21 runs.
//!
//! weekly_sweep: all 3 tasks x {col_a, col_b, col_c}.
//! - col_a: (3906 + 609*2 + 200*10) * 3 / 1e6 = 7124 * 3 / 1e6 = 0.021372
//! - col_b: (1353 + 609 + 200) * 3 / 1e6 = 2162 * 3 / 1e6 = 0.006486
//! - col_c: 0.042744; total 0.070602 over 27 runs.

use std::str::FromStr;
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;

use chrono::{NaiveDate, Utc};
use graphile_worker::WorkerUtils;
use maverick_domain::graders::Registry;
use maverick_domain::spend::{SpendCaps, SpendDecision};
use maverick_jobs::batch::{BatchConfig, BatchOutcome, SpendPreview, preview_spend, run_batch};
use maverick_jobs::{
    BatchLeaseConfig, BatchLeaseManager, JobArgs, JobKind, NightlySmokeArgs, deterministic_batch_id,
};
use maverick_runner::{Cancellation, Clients, RunnerConfig};
use maverick_store::catalog::{self, NewModelColumn};
use maverick_store::corpus::{self, TaskUpsert};
use maverick_store::health::GRAPHILE_WORKER_SCHEMA;
use maverick_store::measurement::{self, NewRun};
use maverick_store::{Decimal, JsonRawValue, MIGRATOR};
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

const VERSION: &str = "v-preview";

fn json(text: &str) -> Box<JsonRawValue> {
    JsonRawValue::from_string(text.to_owned()).expect("json")
}

fn caps(nightly: f64, weekly: f64) -> SpendCaps {
    SpendCaps {
        nightly_usd: nightly,
        weekly_usd: weekly,
    }
}

fn batch_config(caps: SpendCaps) -> BatchConfig {
    BatchConfig {
        repeats: 0,
        caps,
        lease: BatchLeaseManager::new(BatchLeaseConfig {
            lease_ttl: Duration::from_secs(600),
            heartbeat_interval: Duration::from_secs(200),
            owner_stale_after: Duration::from_secs(60),
            graphile_schema: GRAPHILE_WORKER_SCHEMA.to_string(),
        }),
        runner: RunnerConfig::default(),
        clients: Clients::new(),
        graders: Arc::new(Registry::new()),
        notifier: None,
    }
}

fn today() -> NaiveDate {
    Utc::now().date_naive()
}

fn close(actual: f64, want: f64) {
    assert!((actual - want).abs() < 1e-12, "{actual} != {want}");
}

#[allow(clippy::too_many_arguments)]
async fn task(
    pool: &PgPool,
    id: &str,
    version: &str,
    prompt: &str,
    attachments: &str,
    max_tokens: i32,
    smoke: bool,
    active: bool,
) {
    let tags: Vec<String> = Vec::new();
    corpus::upsert_task(
        pool,
        &TaskUpsert {
            id,
            capability: "factual_qa",
            domain_tags: &tags,
            difficulty_tier: 1,
            difficulty_knobs: &json("{}"),
            prompt,
            attachments: &json(attachments),
            temperature: 0.0,
            max_tokens,
            grader_specs: &json("[]"),
            dimension_weights: &json("{}"),
            pass_threshold: 0.7,
            smoke_member: smoke,
            corpus_version: version,
            active,
        },
    )
    .await
    .expect("task");
}

async fn column(pool: &PgPool, id: &str, status: &str, pricing: &str) {
    catalog::create_model_column(
        pool,
        &NewModelColumn {
            id,
            provider: "anthropic",
            model_id: id,
            endpoint: "direct",
            settings: &json("{}"),
            status,
            pricing: &json(pricing),
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

async fn seed(pool: &PgPool) {
    corpus::upsert_corpus_version(pool, "v-old", "", 1, false)
        .await
        .expect("old version");
    corpus::upsert_corpus_version(pool, VERSION, "", 3, true)
        .await
        .expect("version");
    task(pool, "task_1", VERSION, "abcd", "[]", 100, true, true).await;
    task(pool, "task_2", VERSION, "abcde", "[]", 50, true, true).await;
    let attachment = r#"[{"name":"n","content":"xyz"}]"#;
    task(
        pool, "task_3", VERSION, "abcdefgh", attachment, 200, false, true,
    )
    .await;
    task(
        pool, "task_off", VERSION, "inactive", "[]", 9999, true, false,
    )
    .await;
    task(
        pool,
        "task_old",
        "v-old",
        "old version",
        "[]",
        9999,
        true,
        true,
    )
    .await;
    column(
        pool,
        "col_a",
        "active",
        r#"{"input_per_mtok":2,"output_per_mtok":10}"#,
    )
    .await;
    column(
        pool,
        "col_b",
        "deprecated",
        r#"{"input_per_mtok":1,"output_per_mtok":1}"#,
    )
    .await;
    column(
        pool,
        "col_c",
        "cold_start",
        r#"{"input_per_mtok":4,"output_per_mtok":20}"#,
    )
    .await;
    column(
        pool,
        "col_d",
        "retired",
        r#"{"input_per_mtok":99,"output_per_mtok":99}"#,
    )
    .await;
}

/// A costed run of `task_1` on `col_a` in a batch of its own, `days_ago` days before now.
async fn run(pool: &PgPool, id: &str, cost: &str, days_ago: i32) {
    measurement::insert_run(
        pool,
        &NewRun {
            id,
            batch_id: id,
            task_id: "task_1",
            model_column_id: "col_a",
            repeat_index: 0,
            output: Some("x"),
            tokens_in: Some(1),
            tokens_out: Some(1),
            latency_ms: Some(1),
            cost: Some(Decimal::from_str(cost).expect("decimal")),
            temperature: 0.0,
            status: "ok",
            error: None,
            matrix_version: VERSION,
        },
    )
    .await
    .expect("run");
    sqlx::query("update runs set ran_at = now() - make_interval(days => $2) where id = $1")
        .bind(id)
        .bind(days_ago)
        .execute(pool)
        .await
        .expect("ran_at");
}

/// `(alerts, batches, runs)` row counts: the preview must leave all three unchanged.
async fn counts(pool: &PgPool) -> (i64, i64, i64) {
    let count = |sql: &'static str| async move {
        sqlx::query_scalar::<_, i64>(sql)
            .fetch_one(pool)
            .await
            .expect("count")
    };
    (
        count("select count(*) from alerts").await,
        count("select count(*) from batches").await,
        count("select count(*) from runs").await,
    )
}

async fn preview(pool: &PgPool, kind: JobKind, caps: SpendCaps) -> SpendPreview {
    preview_spend(pool, kind, today(), 0, caps)
        .await
        .expect("preview")
}

#[tokio::test]
async fn nightly_preview_composes_and_projects_like_the_batch() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed(&pool).await;
    let before = counts(&pool).await;

    let p = preview(&pool, JobKind::NightlySmoke, SpendCaps::default()).await;
    assert_eq!(p.kind, JobKind::NightlySmoke);
    assert_eq!(p.matrix_version, VERSION);
    assert_eq!(p.repeats, 3, "0 repeats means 3");
    type Group<'a> = (&'a str, usize, Vec<(&'a str, &'a str, i64)>);
    let shape: Vec<Group<'_>> = p
        .groups
        .iter()
        .map(|g| {
            (
                g.name,
                g.tasks,
                g.columns
                    .iter()
                    .map(|c| (c.id.as_str(), c.status.as_str(), c.runs))
                    .collect(),
            )
        })
        .collect();
    assert_eq!(
        shape,
        vec![
            (
                "smoke",
                2,
                vec![("col_a", "active", 6), ("col_b", "deprecated", 6)]
            ),
            ("cold_start", 3, vec![("col_c", "cold_start", 9)]),
        ]
    );
    close(p.groups[0].columns[0].projected_usd, 0.011718);
    close(p.groups[0].columns[1].projected_usd, 0.004059);
    close(p.groups[1].columns[0].projected_usd, 0.042744);
    close(p.projection.projected_usd, 0.058521);
    assert_eq!(p.projection.runs, 21);
    assert_eq!((p.done_runs, p.remaining), (0, p.projection));
    assert_eq!((p.night_spent_usd, p.week_spent_usd), (0.0, 0.0));
    assert!(p.apply_nightly_cap);
    assert_eq!(p.decision, SpendDecision::Allow);

    assert_eq!(counts(&pool).await, before, "the preview wrote nothing");
    db.close().await;
}

#[tokio::test]
async fn weekly_preview_runs_every_task_on_every_column_under_the_weekly_cap_only() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed(&pool).await;

    // A nightly cap far below the projection does not apply to weekly_sweep.
    let p = preview(&pool, JobKind::WeeklySweep, caps(0.000001, 500.0)).await;
    assert_eq!(p.groups.len(), 1);
    assert_eq!(p.groups[0].name, "full");
    assert_eq!(p.groups[0].tasks, 3);
    let ids: Vec<&str> = p.groups[0].columns.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, ["col_a", "col_b", "col_c"]);
    close(p.groups[0].columns[0].projected_usd, 0.021372);
    close(p.groups[0].columns[1].projected_usd, 0.006486);
    close(p.groups[0].columns[2].projected_usd, 0.042744);
    close(p.projection.projected_usd, 0.070602);
    assert_eq!(p.projection.runs, 27);
    assert!(!p.apply_nightly_cap);
    assert_eq!(p.decision, SpendDecision::Allow);

    let refused = preview(&pool, JobKind::WeeklySweep, caps(100.0, 0.07)).await;
    assert_eq!(refused.decision, SpendDecision::Refuse);
    db.close().await;
}

#[tokio::test]
async fn window_actuals_and_the_verdict_follow_the_guard() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed(&pool).await;
    run(&pool, "run_now_1", "0.02", 0).await;
    run(&pool, "run_now_2", "0.01", 0).await;
    run(&pool, "run_week", "0.5", 3).await;
    run(&pool, "run_old", "9", 8).await;
    let before = counts(&pool).await;

    // night 0.03 + 0.058521 = 0.088521; week 0.53 + 0.058521 = 0.588521.
    let decide = |nightly: f64, weekly: f64| {
        let pool = pool.clone();
        async move {
            preview(&pool, JobKind::NightlySmoke, caps(nightly, weekly))
                .await
                .decision
        }
    };
    let p = preview(&pool, JobKind::NightlySmoke, caps(1.0, 100.0)).await;
    close(p.night_spent_usd, 0.03);
    close(p.week_spent_usd, 0.53);
    assert_eq!(p.decision, SpendDecision::Allow);
    assert_eq!(
        decide(0.08, 100.0).await,
        SpendDecision::Refuse,
        "night over"
    );
    assert_eq!(
        decide(0.1, 100.0).await,
        SpendDecision::Warn,
        "night at 88%"
    );
    assert_eq!(decide(1.0, 0.5).await, SpendDecision::Refuse, "week over");
    assert_eq!(decide(1.0, 0.7).await, SpendDecision::Warn, "week at 84%");
    assert_eq!(counts(&pool).await, before, "the preview wrote nothing");
    db.close().await;
}

#[tokio::test]
async fn the_batch_refuses_with_exactly_the_previewed_projection() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed(&pool).await;
    let caps = caps(0.05, 500.0);
    let p = preview(&pool, JobKind::NightlySmoke, caps).await;
    assert_eq!(p.decision, SpendDecision::Refuse);

    let config = batch_config(caps);
    let job = JobArgs::NightlySmoke(NightlySmokeArgs {
        night: today().format("%Y-%m-%d").to_string(),
    });
    let outcome = run_batch(&pool, &job, &config, "worker-preview", &Cancellation::new())
        .await
        .expect("batch");
    let BatchOutcome::Ran(output) = outcome else {
        panic!("the batch did not run: {outcome:?}");
    };
    assert!(output.refused, "{output:?}");
    assert_eq!(output.projected_usd, p.projection.projected_usd);
    assert_eq!(output.planned_runs, p.projection.runs);
    let (kind, projected): (String, f64) = sqlx::query_as(
        "select kind, (subject->>'projected_usd')::float8 from alerts where kind = 'spend_refusal'",
    )
    .fetch_one(&pool)
    .await
    .expect("alert");
    assert_eq!(kind, "spend_refusal");
    assert_eq!(projected, p.remaining.projected_usd);
    db.close().await;
}

/// One run of the window's own batch, as a crashed delivery left it.
async fn done_run(pool: &PgPool, batch_id: &str, task: &str, column: &str, repeat: i16) {
    let id = format!("run_{batch_id}_{task}_{column}_{repeat}");
    measurement::insert_run(
        pool,
        &NewRun {
            id: &id,
            batch_id,
            task_id: task,
            model_column_id: column,
            repeat_index: repeat,
            output: Some("x"),
            tokens_in: Some(1),
            tokens_out: Some(1),
            latency_ms: Some(1),
            cost: Some(Decimal::from_str("0.001").expect("decimal")),
            temperature: 0.0,
            status: "ok",
            error: None,
            matrix_version: VERSION,
        },
    )
    .await
    .expect("run");
}

/// A night that is already partly run projects only its remaining runs, like the guard of
/// the resumed delivery (bead 8mfe): task_1 on col_a twice (2 * 0.002202) and task_3 on
/// col_c once (0.006436) are done, so 0.058521 - 0.004404 - 0.006436 = 0.047681 is left
/// over 21 - 3 = 18 runs. With every run made, nothing is left.
#[tokio::test]
async fn a_partly_run_night_projects_only_the_remaining_runs() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed(&pool).await;
    let night = today().format("%Y-%m-%d").to_string();
    let batch_id = deterministic_batch_id(JobKind::NightlySmoke, &night);
    done_run(&pool, &batch_id, "task_1", "col_a", 0).await;
    done_run(&pool, &batch_id, "task_1", "col_a", 1).await;
    done_run(&pool, &batch_id, "task_3", "col_c", 0).await;
    // Runs of another window's batch do not count as done.
    done_run(&pool, "bat_other", "task_2", "col_b", 0).await;

    let p = preview(&pool, JobKind::NightlySmoke, caps(1.0, 100.0)).await;
    close(p.projection.projected_usd, 0.058521);
    assert_eq!(p.projection.runs, 21);
    assert_eq!(p.done_runs, 3);
    close(p.remaining.projected_usd, 0.047681);
    assert_eq!(p.remaining.runs, 18);
    close(p.night_spent_usd, 0.004);
    // The guard checks the remaining runs: 0.004 + 0.047681 = 0.051681 is 86% of a 0.06
    // cap, a warning. The full projection would make 0.062521, a refusal.
    let tight = preview(&pool, JobKind::NightlySmoke, caps(0.06, 100.0)).await;
    assert_eq!(tight.decision, SpendDecision::Warn);

    for (tasks, column) in [
        (&["task_1", "task_2"][..], "col_a"),
        (&["task_1", "task_2"][..], "col_b"),
        (&["task_1", "task_2", "task_3"][..], "col_c"),
    ] {
        for task in tasks {
            for repeat in 0..3 {
                let made = (*task == "task_1" && column == "col_a" && repeat < 2)
                    || (*task == "task_3" && column == "col_c" && repeat == 0);
                if !made {
                    done_run(&pool, &batch_id, task, column, repeat).await;
                }
            }
        }
    }
    let all = preview(&pool, JobKind::NightlySmoke, caps(1.0, 100.0)).await;
    assert_eq!(all.done_runs, 21);
    assert_eq!(all.remaining.runs, 0);
    assert_eq!(all.projection.runs, 21);
    let before = counts(&pool).await;
    let exhausted = preview(&pool, JobKind::NightlySmoke, caps(0.0, 0.0)).await;
    assert!(exhausted.night_spent_usd > 0.0);
    assert_eq!(exhausted.remaining, Default::default());
    assert_eq!(exhausted.decision, SpendDecision::Allow);
    assert_eq!(counts(&pool).await, before, "the preview wrote nothing");

    let job = JobArgs::NightlySmoke(NightlySmokeArgs { night });
    let outcome = run_batch(
        &pool,
        &job,
        &batch_config(exhausted.caps),
        "worker-completed-preview",
        &Cancellation::new(),
    )
    .await
    .expect("batch");
    let BatchOutcome::Ran(output) = outcome else {
        panic!("the batch did not run: {outcome:?}");
    };
    assert!(!output.refused);
    assert!(!output.spend_warned);
    assert_eq!(output.planned_runs, exhausted.projection.runs);
    assert_eq!(counts(&pool).await.0, before.0, "no spend alert");
    db.close().await;
}

#[tokio::test]
async fn an_empty_batch_skips_the_guard_even_when_the_window_has_spend() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed(&pool).await;
    run(&pool, "run_spent", "1", 0).await;
    sqlx::query("update corpus_versions set active = false")
        .execute(&pool)
        .await
        .expect("deactivate corpus");
    let before = counts(&pool).await;

    for kind in [JobKind::NightlySmoke, JobKind::WeeklySweep] {
        let p = preview(&pool, kind, caps(0.0, 0.0)).await;
        assert!(p.matrix_version.is_empty());
        assert_eq!(p.projection, Default::default());
        assert_eq!(p.remaining, Default::default());
        close(p.night_spent_usd, 1.0);
        assert_eq!(p.decision, SpendDecision::Allow);
    }
    assert_eq!(counts(&pool).await, before, "the preview wrote nothing");
    db.close().await;
}

#[tokio::test]
async fn weekly_resume_uses_the_iso_week_and_configured_repeats() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed(&pool).await;
    // Friday January 1 belongs to the previous ISO year. The batch lease uses the ISO
    // week, so its completed runs must be found by a preview for any night in that week.
    let night = NaiveDate::from_ymd_opt(2027, 1, 1).expect("date");
    let batch_id = deterministic_batch_id(JobKind::WeeklySweep, "2026-W53");
    done_run(&pool, &batch_id, "task_1", "col_a", 0).await;
    done_run(&pool, &batch_id, "task_3", "col_c", 0).await;
    let before = counts(&pool).await;

    let p = preview_spend(&pool, JobKind::WeeklySweep, night, 1, caps(0.0, 1.0))
        .await
        .expect("preview");
    assert_eq!(p.repeats, 1);
    close(p.projection.projected_usd, 0.023534);
    assert_eq!(p.projection.runs, 9);
    assert_eq!(p.done_runs, 2);
    // Remove task_1/col_a (0.002202) and task_3/col_c (0.006436) from 0.023534.
    close(p.remaining.projected_usd, 0.014896);
    assert_eq!(p.remaining.runs, 7);
    assert_eq!(counts(&pool).await, before, "the preview wrote nothing");
    db.close().await;
}
