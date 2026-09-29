//! `regression_scan` on real Postgres (reference Part 3 §2.10 and §6 "regression_scan").
//! Series are seeded by folding with a float64 stream and storing float32 EWMA and
//! variance, `n`, and `measured = n >= 3`, matrix version `corpus-regr`.

use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use chrono::{Days, NaiveDate, TimeZone, Utc};
use graphile_worker::{JobSpec, WorkerOptions, WorkerUtils};
use maverick_alerts::AlertNotifier;
use maverick_domain::scoring::Stream;
use maverick_jobs::fold::save_scores;
use maverick_jobs::rebuild::{AggregateRebuildJob, RebuildConfig};
use maverick_jobs::regression_scan::{
    RegressionScanConfig, RegressionScanJob, RegressionScanOutcome, RegressionScanOutput,
    regression_scan,
};
use maverick_jobs::{
    BatchFollowupArgs, BatchLeaseConfig, BatchLeaseManager, DEFAULT_QUEUE_NAME, JobClientConfig,
    JobKind,
};
use maverick_store::alerts::Alert;
use maverick_store::catalog::{self, NewModelColumn};
use maverick_store::corpus::{self, TaskUpsert};
use maverick_store::health::GRAPHILE_WORKER_SCHEMA;
use maverick_store::measurement::{self, AggregateUpsert, NewRun};
use maverick_store::tenancy;
use maverick_store::{Decimal, JsonRawValue, MIGRATOR};
use maverick_testdb::Schema;
use serde_json::Value;
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
        graphile_schema: GRAPHILE_WORKER_SCHEMA.to_owned(),
    })
}

fn config(counter: &Arc<Counter>) -> RegressionScanConfig {
    RegressionScanConfig {
        alpha: 0.3,
        lease: lease(),
        notifier: Some(Arc::clone(counter) as Arc<dyn AlertNotifier>),
    }
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
}

/// Folds `observations` from `start` with a float64 stream and stores each night's
/// float32 EWMA and variance.
async fn seed_series(pool: &PgPool, column: &str, tier: i16, start: &str, observations: &[f64]) {
    let mut stream = Stream::new(0.3);
    for (i, x) in observations.iter().enumerate() {
        stream.fold(*x);
        let as_of = day(start) + Days::new(i as u64);
        #[allow(clippy::cast_possible_truncation)]
        measurement::upsert_aggregate(
            pool,
            &AggregateUpsert {
                model_column_id: column,
                capability: "structured_extraction",
                difficulty_tier: tier,
                dimension: "quality",
                ewma: stream.ewma() as f32,
                variance: stream.variance() as f32,
                n: i32::try_from(stream.n()).expect("n"),
                measured: stream.n() >= 3,
                matrix_version: "corpus-regr",
                as_of,
            },
        )
        .await
        .expect("aggregate");
    }
}

fn decline(nights: usize) -> Vec<f64> {
    (0..nights)
        .map(|i| {
            if i < 3 {
                0.90
            } else {
                0.90 - 0.01 * (i - 2) as f64
            }
        })
        .collect()
}

async fn scan(
    pool: &PgPool,
    config: &RegressionScanConfig,
    night: &str,
    batch: &str,
) -> RegressionScanOutput {
    match regression_scan(
        pool,
        &BatchFollowupArgs {
            night: night.to_owned(),
            batch_id: batch.to_owned(),
        },
        config,
        "worker-test",
    )
    .await
    .expect("scan")
    {
        RegressionScanOutcome::Scanned(output) => output,
        other => panic!("not scanned: {other:?}"),
    }
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

/// parity: jobs.TestRegressionScanDropDedupAndDeepening
#[tokio::test]
async fn drop_dedup_and_deepening() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed_column(&pool, "col_decline").await;
    seed_series(&pool, "col_decline", 3, "2031-06-01", &decline(11)).await;
    let counter = Arc::new(Counter::default());
    let config = config(&counter);

    assert_eq!(
        scan(&pool, &config, "2031-06-07", "bat_n6")
            .await
            .regression_alerts,
        0
    );
    let night7 = scan(&pool, &config, "2031-06-08", "bat_n7").await;
    assert_eq!((night7.cells_scanned, night7.regression_alerts), (1, 1));
    let first = alerts(&pool, "regression").await;
    assert_eq!(first.len(), 1);
    let (severity, subject, message) = &first[0];
    assert_eq!(severity, "warning");
    assert_eq!(subject["model_column_id"], "col_decline");
    assert_eq!(subject["capability"], "structured_extraction");
    assert_eq!(subject["difficulty_tier"], 3);
    assert_eq!(subject["dimension"], "quality");
    assert_eq!(subject["as_of"], "2031-06-08");
    assert_eq!(subject["regression_kind"], "drop");
    assert_eq!(subject["batch_id"], "bat_n7");
    assert!((subject["baseline"].as_f64().expect("baseline") - 90.0).abs() < 1e-3);
    assert!((subject["current"].as_f64().expect("current") - 86.94117).abs() < 1e-3);
    assert!((subject["delta"].as_f64().expect("delta") - 3.05883).abs() < 1e-3);
    assert_eq!(
        message,
        "quality regression (drop) on col_decline structured_extraction tier 3: 90.0 \u{2192} 86.9 (\u{2212}3.1 pts) as of 2031-06-08"
    );
    assert_eq!(counter.0.load(Ordering::SeqCst), 1);

    // D10: the same batch again is a no-op; another batch the same night is deduplicated.
    let _ = parity_support::divergence!("D10");
    let same = regression_scan(
        &pool,
        &BatchFollowupArgs {
            night: "2031-06-08".to_owned(),
            batch_id: "bat_n7".to_owned(),
        },
        &config,
        "worker-2",
    )
    .await
    .expect("same batch");
    assert_eq!(same, RegressionScanOutcome::AlreadyFinished);
    assert_eq!(
        scan(&pool, &config, "2031-06-08", "bat_n7b")
            .await
            .regression_alerts,
        0
    );
    assert_eq!(
        scan(&pool, &config, "2031-06-09", "bat_n8")
            .await
            .regression_alerts,
        0
    );
    assert_eq!(
        scan(&pool, &config, "2031-06-10", "bat_n9")
            .await
            .regression_alerts,
        0
    );
    assert_eq!(
        scan(&pool, &config, "2031-06-11", "bat_n10")
            .await
            .regression_alerts,
        1
    );
    let all = alerts(&pool, "regression").await;
    assert_eq!(all.len(), 2);
    assert_eq!(all[1].1["as_of"], "2031-06-11");
    assert!((all[1].1["current"].as_f64().expect("current") - 84.19882).abs() < 1e-3);
    assert_eq!(counter.0.load(Ordering::SeqCst), 2);
    db.close().await;
}

/// parity: jobs.TestRegressionScanShock
#[tokio::test]
async fn shock() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed_column(&pool, "col_shock").await;
    seed_series(
        &pool,
        "col_shock",
        2,
        "2031-06-20",
        &[0.9, 0.9, 0.9, 0.9, 0.8],
    )
    .await;
    let counter = Arc::new(Counter::default());
    let out = scan(&pool, &config(&counter), "2031-06-24", "bat_shock").await;
    assert_eq!(out.regression_alerts, 1);
    let (_, subject, _) = &alerts(&pool, "regression").await[0];
    assert_eq!(subject["regression_kind"], "shock");
    assert_eq!(subject["as_of"], "2031-06-24");
    assert!((subject["current"].as_f64().expect("current") - 80.0).abs() < 1e-3);
    assert!((subject["delta"].as_f64().expect("delta") - 10.0).abs() < 1e-3);
    db.close().await;
}

async fn seed_tenant(pool: &PgPool, tenant: &str, column: &str, bar: Option<f32>) {
    tenancy::create_tenant(pool, tenant, &format!("Tenant {tenant}"), &[7u8; 32])
        .await
        .expect("tenant");
    tenancy::create_pool(
        pool,
        &format!("pool_{tenant}"),
        tenant,
        "default",
        &[column.to_owned()],
        true,
    )
    .await
    .expect("pool");
    if let Some(bar) = bar {
        tenancy::create_tenant_weight(pool, tenant, "structured_extraction", 0.5, 0.3, 0.2, bar)
            .await
            .expect("weights");
    }
}

/// parity: jobs.TestRegressionScanSeverity
#[tokio::test]
async fn severity_follows_the_tenant_bar() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    for column in ["col_bar88", "col_default"] {
        seed_column(&pool, column).await;
        seed_series(&pool, column, 3, "2031-06-01", &decline(8)).await;
    }
    seed_tenant(&pool, "ten_bar88", "col_bar88", Some(0.88)).await;
    seed_tenant(&pool, "ten_default", "col_default", None).await;
    let counter = Arc::new(Counter::default());
    let out = scan(&pool, &config(&counter), "2031-06-08", "bat_sev").await;
    assert_eq!(out.regression_alerts, 2);
    let severities: Vec<(String, String)> = alerts(&pool, "regression")
        .await
        .into_iter()
        .map(|(s, subject, _)| {
            (
                subject["model_column_id"]
                    .as_str()
                    .expect("column")
                    .to_owned(),
                s,
            )
        })
        .collect();
    assert!(
        severities.contains(&("col_bar88".to_owned(), "critical".to_owned())),
        "{severities:?}"
    );
    assert!(
        severities.contains(&("col_default".to_owned(), "warning".to_owned())),
        "{severities:?}"
    );
    db.close().await;
}

async fn seed_runs(pool: &PgPool, batch: &str, column: &str, total: usize, failures: usize) {
    for i in 0..total {
        let id = format!("run_{batch}_{column}_{i}");
        let status = if i < failures { "parse_failure" } else { "ok" };
        measurement::insert_run(
            pool,
            &NewRun {
                id: &id,
                batch_id: batch,
                task_id: "task_pf",
                model_column_id: column,
                repeat_index: i16::try_from(i).expect("repeat"),
                output: Some("x"),
                tokens_in: Some(1),
                tokens_out: Some(1),
                latency_ms: Some(1),
                cost: Some(Decimal::from_str("0.000001").expect("decimal")),
                temperature: 0.0,
                status,
                error: None,
                matrix_version: "corpus-regr",
            },
        )
        .await
        .expect("run");
    }
}

/// parity: jobs.TestRegressionScanParseFailureWatch
#[tokio::test]
async fn parse_failure_watch() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    corpus::upsert_corpus_version(&pool, "corpus-regr", "", 1, true)
        .await
        .expect("version");
    let tags: Vec<String> = Vec::new();
    corpus::upsert_task(
        &pool,
        &TaskUpsert {
            id: "task_pf",
            capability: "structured_extraction",
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
            corpus_version: "corpus-regr",
            active: true,
        },
    )
    .await
    .expect("task");
    for column in ["col_fresh", "col_noisy", "col_quiet"] {
        seed_column(&pool, column).await;
    }
    seed_runs(&pool, "bat_regr_pf", "col_fresh", 10, 2).await;
    seed_runs(&pool, "bat_regr_pf", "col_noisy", 10, 2).await;
    seed_runs(&pool, "bat_regr_pf", "col_quiet", 20, 1).await;
    // The noisy column's trailing runs, two days before the night (wall clock, J5).
    seed_runs(&pool, "bat_trailing", "col_noisy", 10, 3).await;
    sqlx::query("update runs set ran_at = $1 where batch_id = 'bat_trailing'")
        .bind(
            Utc.with_ymd_and_hms(2031, 7, 23, 12, 0, 0)
                .single()
                .expect("time"),
        )
        .execute(&pool)
        .await
        .expect("ran_at");
    let counter = Arc::new(Counter::default());
    let config = config(&counter);
    let out = scan(&pool, &config, "2031-07-25", "bat_regr_pf").await;
    assert_eq!(out.parse_failure_alerts, 1);
    let found = alerts(&pool, "parse_failure_rate").await;
    assert_eq!(found.len(), 1);
    let (severity, subject, message) = &found[0];
    assert_eq!(severity, "warning");
    assert_eq!(subject["model_column_id"], "col_fresh");
    assert_eq!(subject["batch_id"], "bat_regr_pf");
    assert_eq!(subject["night"], "2031-07-25");
    assert_eq!(subject["parse_failure_rate"], 0.2);
    assert_eq!(subject["trailing_rate"], 0.0);
    assert_eq!(subject["parse_failures"], 2);
    assert_eq!(subject["total_runs"], 10);
    assert_eq!(
        message,
        "col_fresh parse-failure rate 20.0% over 10 runs on 2031-07-25 (> 5% and > 2\u{d7} trailing 7-day rate 0.0%; spec 11)"
    );
    assert_eq!(counter.0.load(Ordering::SeqCst), 1);
    // A second scan of the night (another delivery, D10) adds nothing.
    let again = scan(&pool, &config, "2031-07-25", "bat_regr_pf_again").await;
    assert_eq!(again.parse_failure_alerts, 0);
    assert_eq!(alerts(&pool, "parse_failure_rate").await.len(), 1);
    db.close().await;
}

/// The rebuild chains the scan with its night and batch id; nothing regressed.
///
/// parity: jobs.TestRebuildChainsRegressionScan
#[tokio::test]
async fn rebuild_chains_regression_scan() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    corpus::upsert_corpus_version(&pool, "corpus-regr", "", 1, true)
        .await
        .expect("version");
    let tags: Vec<String> = Vec::new();
    corpus::upsert_task(
        &pool,
        &TaskUpsert {
            id: "task_pf",
            capability: "structured_extraction",
            domain_tags: &tags,
            difficulty_tier: 1,
            difficulty_knobs: &json("{}"),
            prompt: "p",
            attachments: &json("[]"),
            temperature: 0.0,
            max_tokens: 16,
            grader_specs: &json("[]"),
            dimension_weights: &json(r#"{"answer_correctness":1.0}"#),
            pass_threshold: 0.7,
            smoke_member: false,
            corpus_version: "corpus-regr",
            active: true,
        },
    )
    .await
    .expect("task");
    seed_column(&pool, "col_chain").await;
    seed_runs(&pool, "bat_chain", "col_chain", 3, 0).await;
    let mut conn = pool.acquire().await.expect("conn");
    for i in 0..3 {
        save_scores(
            &mut conn,
            &format!("run_bat_chain_col_chain_{i}"),
            0,
            &BTreeMap::from([("answer_correctness".to_owned(), 1.0)]),
            Some(&json("{}")),
        )
        .await
        .expect("scores");
    }
    drop(conn);
    let counter = Arc::new(Counter::default());
    let worker = WorkerOptions::default()
        .concurrency(1)
        .schema(GRAPHILE_WORKER_SCHEMA)
        .pg_pool(pool.clone())
        .add_extension(RebuildConfig {
            alpha: 0.3,
            lambda_var: 0.5,
            lambda_amb: 0.25,
            min_nights: 3,
            runner_timeout: Duration::from_secs(120),
            lease: lease(),
        })
        .add_extension(config(&counter))
        .define_job::<AggregateRebuildJob>()
        .define_job::<RegressionScanJob>()
        .init()
        .await
        .expect("worker");
    worker
        .create_utils()
        .add_job(
            AggregateRebuildJob(BatchFollowupArgs {
                night: "2031-08-10".to_owned(),
                batch_id: "bat_chain".to_owned(),
            }),
            JobSpec::builder().queue_name(DEFAULT_QUEUE_NAME).build(),
        )
        .await
        .expect("enqueue");
    worker.run_once().await.expect("rebuild");
    worker.run_once().await.expect("scan");
    let scanned: i64 = sqlx::query_scalar(
        "select count(*) from batches where kind = 'regression_scan' and batch_window = 'bat_chain' and status = 'completed'",
    )
    .fetch_one(&pool)
    .await
    .expect("scan lease");
    assert_eq!(scanned, 1);
    assert!(alerts(&pool, "regression").await.is_empty());
    assert!(alerts(&pool, "parse_failure_rate").await.is_empty());
    assert_eq!(
        <RegressionScanJob as graphile_worker::TaskHandler>::IDENTIFIER,
        JobKind::RegressionScan.as_str()
    );
    db.close().await;
}

/// parity: jobs.TestRegressionScanTimeoutUsesAnalyticsJobTimeout
#[test]
fn timeout_uses_the_analytics_job_timeout() {
    assert_eq!(
        JobKind::RegressionScan.timeout(&JobClientConfig::default()),
        Duration::from_secs(15 * 60)
    );
}
