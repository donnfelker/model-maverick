//! The G8 spend guard on real Postgres (reference Part 3 §2.6 and §6 "Spend guard").

use std::str::FromStr;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use chrono::{NaiveDate, Utc};
use maverick_alerts::AlertNotifier;
use maverick_domain::spend::{Projection, SpendCaps};
use maverick_jobs::JobKind;
use maverick_jobs::spend_guard::{SpendOutcome, check_spend};
use maverick_store::alerts::Alert;
use maverick_store::catalog::{self, NewModelColumn};
use maverick_store::corpus::{self, TaskUpsert};
use maverick_store::measurement::{self, NewRun};
use maverick_store::{Decimal, JsonRawValue, MIGRATOR};
use maverick_testdb::Schema;
use serde_json::Value;
use sqlx::PgPool;

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

#[derive(Default)]
struct Counter(AtomicUsize);

impl AlertNotifier for Counter {
    fn notify(&self, _alert: &Alert) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn day(s: &str) -> NaiveDate {
    s.parse().expect("date")
}

fn caps(nightly: f64, weekly: f64) -> SpendCaps {
    SpendCaps {
        nightly_usd: nightly,
        weekly_usd: weekly,
    }
}

fn projection(usd: f64, runs: i64) -> Projection {
    Projection {
        projected_usd: usd,
        runs,
    }
}

/// `(kind, severity, subject, message)` of the alerts whose subject names this night.
async fn alerts_for(pool: &PgPool, night: &str) -> Vec<(String, String, Value, String)> {
    sqlx::query_as::<_, (String, String, sqlx::types::Json<Value>, String)>(
        "select kind, severity, subject, message from alerts
         where subject->>'night' = $1 order by created_at, id",
    )
    .bind(night)
    .fetch_all(pool)
    .await
    .expect("alerts")
    .into_iter()
    .map(|(k, s, subject, m)| (k, s, subject.0, m))
    .collect()
}

fn spend_messages() -> Value {
    parity_support::fixture("measurement-core", "spend.json").expect("fixture")["messages"].clone()
}

async fn check(
    pool: &PgPool,
    counter: &Counter,
    job: JobKind,
    night: &str,
    projection: Projection,
    caps: SpendCaps,
) -> SpendOutcome {
    check_spend(pool, Some(counter), job, day(night), &projection, caps)
        .await
        .expect("spend check")
}

/// parity: jobs.TestSpendGuardRefusesOverNightlyCap
#[tokio::test]
async fn refuses_over_nightly_cap() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let counter = Counter::default();
    let out = check(
        &pool,
        &counter,
        JobKind::NightlySmoke,
        "2030-08-10",
        projection(5.0, 3),
        caps(4.99, 500.0),
    )
    .await;
    assert!(!out.allowed && !out.warned);
    let alerts = alerts_for(&pool, "2030-08-10").await;
    assert_eq!(alerts.len(), 1);
    let (kind, severity, subject, message) = &alerts[0];
    assert_eq!(
        (kind.as_str(), severity.as_str()),
        ("spend_refusal", "critical")
    );
    assert_eq!(subject["job"], "nightly_smoke");
    assert_eq!(subject["projected_usd"], 5.0);
    assert_eq!(subject["nightly_cap_usd"], 4.99);
    assert_eq!(subject["planned_runs"], 3);
    let want = &spend_messages()["cases"][0];
    assert_eq!(message, want["refusal_message"].as_str().expect("message"));
    assert_eq!(counter.0.load(Ordering::SeqCst), 1);
    db.close().await;
}

/// parity: jobs.TestSpendGuardRefusesOverWeeklyCap
#[tokio::test]
async fn refuses_over_weekly_cap() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let counter = Counter::default();
    let out = check(
        &pool,
        &counter,
        JobKind::NightlySmoke,
        "2030-09-10",
        projection(5.0, 3),
        caps(100.0, 3.0),
    )
    .await;
    assert!(!out.allowed);
    let alerts = alerts_for(&pool, "2030-09-10").await;
    assert_eq!(alerts.len(), 1);
    assert_eq!(alerts[0].0, "spend_refusal");
    db.close().await;
}

/// parity: jobs.TestSpendGuardWarnsAtEightyPercent
#[tokio::test]
async fn warns_at_eighty_percent() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let counter = Counter::default();
    let out = check(
        &pool,
        &counter,
        JobKind::NightlySmoke,
        "2030-10-10",
        projection(5.0, 3),
        caps(6.0, 500.0),
    )
    .await;
    assert!(out.allowed && out.warned);
    let alerts = alerts_for(&pool, "2030-10-10").await;
    assert_eq!(alerts.len(), 1);
    let (kind, severity, _, message) = &alerts[0];
    assert_eq!(
        (kind.as_str(), severity.as_str()),
        ("spend_warning", "warning")
    );
    assert_eq!(
        message,
        "nightly_smoke for 2030-10-10 projects $5.0000, bringing night to $5.0000 (cap $6.00) and week to $5.0000 of $500.00 cap (\u{2265}80%)"
    );
    assert_eq!(counter.0.load(Ordering::SeqCst), 1);
    db.close().await;
}

/// parity: jobs.TestSpendGuardAllowsUnderCap
#[tokio::test]
async fn allows_under_cap() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let counter = Counter::default();
    let out = check(
        &pool,
        &counter,
        JobKind::NightlySmoke,
        "2030-11-10",
        projection(1.0, 3),
        SpendCaps::default(),
    )
    .await;
    assert!(out.allowed && !out.warned);
    assert!(alerts_for(&pool, "2030-11-10").await.is_empty());
    assert_eq!(counter.0.load(Ordering::SeqCst), 0);
    db.close().await;
}

fn json(text: &str) -> Box<JsonRawValue> {
    JsonRawValue::from_string(text.to_owned()).expect("json")
}

async fn seed_costed_runs(pool: &PgPool, costs: &[&str]) {
    corpus::upsert_corpus_version(pool, "v-spend", "", 1, true)
        .await
        .expect("version");
    let tags: Vec<String> = Vec::new();
    corpus::upsert_task(
        pool,
        &TaskUpsert {
            id: "task_spend",
            capability: "factual_qa",
            domain_tags: &tags,
            difficulty_tier: 1,
            difficulty_knobs: &json("{}"),
            prompt: "p",
            attachments: &json("[]"),
            temperature: 0.0,
            max_tokens: 16,
            grader_specs: &json("[]"),
            dimension_weights: &json("{}"),
            pass_threshold: 0.7,
            smoke_member: false,
            corpus_version: "v-spend",
            active: true,
        },
    )
    .await
    .expect("task");
    catalog::create_model_column(
        pool,
        &NewModelColumn {
            id: "col_spend",
            provider: "anthropic",
            model_id: "m",
            endpoint: "direct",
            settings: &json("{}"),
            status: "active",
            pricing: &json("{}"),
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
    for (i, cost) in costs.iter().enumerate() {
        let id = format!("run_spend_{i}");
        measurement::insert_run(
            pool,
            &NewRun {
                id: &id,
                batch_id: "bat_spend",
                task_id: "task_spend",
                model_column_id: "col_spend",
                repeat_index: i16::try_from(i).expect("repeat"),
                output: Some("x"),
                tokens_in: Some(1),
                tokens_out: Some(1),
                latency_ms: Some(1),
                cost: Some(Decimal::from_str(cost).expect("decimal")),
                temperature: 0.0,
                status: "ok",
                error: None,
                matrix_version: "v-spend",
            },
        )
        .await
        .expect("run");
    }
}

/// parity: jobs.TestSpendGuardSumsWindowActuals
#[tokio::test]
async fn sums_window_actuals() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let counter = Counter::default();
    let today = Utc::now().date_naive().to_string();
    let base = check(
        &pool,
        &counter,
        JobKind::NightlySmoke,
        &today,
        projection(0.0, 0),
        SpendCaps::default(),
    )
    .await;
    seed_costed_runs(&pool, &["2.5", "2.5"]).await;
    let refused = check(
        &pool,
        &counter,
        JobKind::NightlySmoke,
        &today,
        projection(1.5, 1),
        caps(base.night_spent_usd + 6.0, 500.0),
    )
    .await;
    assert!(!refused.allowed);
    assert!((refused.night_spent_usd - (base.night_spent_usd + 5.0)).abs() < 1e-9);
    assert!((refused.week_spent_usd - (base.week_spent_usd + 5.0)).abs() < 1e-9);
    let allowed = check(
        &pool,
        &counter,
        JobKind::NightlySmoke,
        &today,
        projection(1.5, 1),
        caps(base.night_spent_usd + 20.0, refused.week_spent_usd + 100.0),
    )
    .await;
    assert!(allowed.allowed && !allowed.warned);
    db.close().await;
}

/// `weekly_sweep` checks only the weekly cap; a zero cap with a zero projection warns
/// (QUIRK J7).
#[tokio::test]
async fn weekly_sweep_and_zero_caps() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let counter = Counter::default();
    let sweep = check(
        &pool,
        &counter,
        JobKind::WeeklySweep,
        "2031-01-12",
        projection(5.0, 30),
        caps(0.000001, 500.0),
    )
    .await;
    assert!(sweep.allowed && !sweep.warned);
    let refused = check(
        &pool,
        &counter,
        JobKind::WeeklySweep,
        "2031-01-13",
        projection(0.0123456, 3),
        caps(100.0, 0.000001),
    )
    .await;
    assert!(!refused.allowed);
    let alerts = alerts_for(&pool, "2031-01-13").await;
    assert_eq!(
        alerts[0].3,
        "weekly_sweep refused for 2031-01-13: projected $0.0123 would bring week to $0.0123 (cap $0.00)"
    );
    let zero = check(
        &pool,
        &counter,
        JobKind::NightlySmoke,
        "2031-02-01",
        projection(0.0, 0),
        caps(0.0, 0.0),
    )
    .await;
    assert!(zero.allowed && zero.warned);
    db.close().await;
}

/// A failed alert write fails the job so it retries.
#[tokio::test]
async fn failed_alert_write_is_an_error() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    sqlx::query("alter table alerts rename to alerts_gone")
        .execute(&pool)
        .await
        .expect("rename");
    let counter = Counter::default();
    let error = check_spend(
        &pool,
        Some(&counter),
        JobKind::NightlySmoke,
        day("2030-12-01"),
        &projection(5.0, 3),
        caps(1.0, 500.0),
    )
    .await
    .expect_err("alert write fails");
    assert!(
        error
            .to_string()
            .starts_with("jobs: writing spend_refusal alert: "),
        "{error}"
    );
    assert_eq!(counter.0.load(Ordering::SeqCst), 0);
    db.close().await;
}
