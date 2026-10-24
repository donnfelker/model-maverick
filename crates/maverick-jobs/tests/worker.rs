//! The worker wiring on real Postgres and graphile_worker: each cron firing enqueues its
//! own window, and the catalog and retention tasks run through the worker.

use std::sync::{Arc, LazyLock};

use graphile_worker::{JobSpec, WorkerOptions, WorkerUtils};
use maverick_domain::graders::Registry;
use maverick_jobs::worker::{self, WorkerDeps};
use maverick_jobs::{DEFAULT_QUEUE_NAME, JobClient};
use maverick_runner::Clients;
use maverick_store::MIGRATOR;
use maverick_store::health::GRAPHILE_WORKER_SCHEMA;
use maverick_testdb::Schema;
use serde_json::{Value, json};
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

fn config() -> maverick_config::Config {
    let vars = [
        ("MAVERICK_ROLE", "origin".to_owned()),
        ("MAVERICK_DATABASE_URL", "postgres://u:p@h/db".to_owned()),
        ("MAVERICK_INTERNAL_TOKENS", "ops:s".to_owned()),
        ("MAVERICK_CAPTURE_MASTER_KEY", "ab".repeat(32)),
    ];
    maverick_config::Config::load_with(|name| {
        vars.iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.clone())
    })
    .expect("config")
}

fn deps() -> WorkerDeps {
    WorkerDeps {
        clients: Clients::new(),
        graders: Arc::new(Registry::new()),
        notifier: None,
        price_source: None,
        model_listers: Vec::new(),
    }
}

/// `(task identifier, job key, payload)` of every queued job, in insertion order.
async fn queued(pool: &PgPool) -> Vec<(String, Option<String>, Value)> {
    sqlx::query_as::<_, (String, Option<String>, sqlx::types::Json<Value>)>(
        "select tasks.identifier, jobs.key, jobs.payload
         from graphile_worker._private_jobs as jobs
         join graphile_worker._private_tasks as tasks on tasks.id = jobs.task_id
         order by jobs.id",
    )
    .fetch_all(pool)
    .await
    .expect("jobs")
    .into_iter()
    .map(|(t, k, p)| (t, k, p.0))
    .collect()
}

async fn add(utils: &WorkerUtils, identifier: &str, payload: Value) {
    utils
        .add_raw_job(
            identifier,
            payload,
            JobSpec::builder().queue_name(DEFAULT_QUEUE_NAME).build(),
        )
        .await
        .expect("add job");
}

/// Two nightly firings enqueue two different windows: the window comes from `_cron.ts`,
/// not from a payload fixed at startup.
#[tokio::test]
async fn each_cron_firing_enqueues_its_own_window() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    // Only the triggers: the jobs they enqueue stay queued for inspection.
    let worker = WorkerOptions::default()
        .schema(GRAPHILE_WORKER_SCHEMA)
        .pg_pool(pool.clone())
        .define_job::<worker::CronNightlySmoke>()
        .define_job::<worker::CronCaptureExpiry>()
        .init()
        .await
        .expect("worker");
    let utils = worker.create_utils();

    for ts in ["2031-01-02T03:00:00Z", "2031-01-03T03:00:00Z"] {
        add(
            &utils,
            "cron.nightly_smoke",
            json!({"_cron": {"ts": ts, "backfilled": false}}),
        )
        .await;
        worker.run_once().await.expect("trigger");
    }
    add(
        &utils,
        "cron.capture_expiry",
        json!({"_cron": {"ts": "2031-01-03T07:00:00Z", "backfilled": true}}),
    )
    .await;
    worker.run_once().await.expect("trigger");

    let jobs = queued(&pool).await;
    let enqueued: Vec<(String, Option<String>, Value)> = jobs
        .into_iter()
        .filter(|(identifier, _, _)| !identifier.starts_with("cron."))
        .collect();
    assert_eq!(
        enqueued,
        [
            (
                "nightly_smoke".to_owned(),
                Some("nightly_smoke:2031-01-02".to_owned()),
                json!({"night": "2031-01-02"})
            ),
            (
                "nightly_smoke".to_owned(),
                Some("nightly_smoke:2031-01-03".to_owned()),
                json!({"night": "2031-01-03"})
            ),
            (
                "capture_expiry".to_owned(),
                Some("capture_expiry:2031-01-03T07".to_owned()),
                json!({"hour": "2031-01-03T07"})
            ),
        ]
    );
}

#[tokio::test]
async fn a_bad_firing_time_fails_the_trigger() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let error = worker::enqueue_scheduled(
        &pool,
        maverick_jobs::JobKind::PriceSync,
        &worker::CronFiring {
            cron: worker::CronMeta {
                ts: "yesterday".to_owned(),
                backfilled: false,
            },
        },
    )
    .await
    .expect_err("bad ts");
    assert!(
        error.starts_with("jobs: cron.price_sync: firing time \"yesterday\""),
        "{error}"
    );
    assert!(queued(&pool).await.is_empty());
}

/// The catalog, discovery, and retention tasks find their configuration on the worker
/// and run.
#[tokio::test]
async fn catalog_and_retention_tasks_run_through_the_worker() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let config = config();
    let client = JobClient::from_config(&config).expect("client");
    let worker = worker::worker_options(&client, &config, deps())
        .pg_pool(pool.clone())
        .init()
        .await
        .expect("worker");
    let utils = worker.create_utils();
    add(&utils, "price_sync", json!({"day": "2031-01-02"})).await;
    add(&utils, "catalog_sync", json!({"day": "2031-01-02"})).await;
    // D51: with no model listers, every provider is skipped and the day completes.
    add(&utils, "model_discovery", json!({"day": "2031-01-02"})).await;
    add(&utils, "capture_expiry", json!({"hour": "2031-01-02T05"})).await;
    add(&utils, "history_prune", json!({"day": "2031-01-02"})).await;
    for _ in 0..5 {
        worker.run_once().await.expect("run");
    }
    // Completed jobs are deleted; a failed one would stay queued with its error.
    assert!(queued(&pool).await.is_empty());
}

/// `start` runs the worker until `stop`, which drains within the deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_started_worker_stops_on_request() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let config = config();
    let client = JobClient::from_config(&config).expect("client");
    let running =
        worker::start(worker::worker_options(&client, &config, deps()).pg_pool(pool.clone()))
            .await
            .expect("start");
    running
        .stop(std::time::Duration::from_secs(10))
        .await
        .expect("stopped");
}

/// DIVERGENCE: D49. Check the handlers graphile actually registered, not just crons.
#[tokio::test]
async fn deployment_role_limits_registered_jobs_and_crons() {
    use maverick_config::Role;
    for (role, prices, export) in [
        (Role::Origin, true, false),
        (Role::Origin, true, true),
        (Role::Replica, false, true),
        (Role::Replica, true, false),
    ] {
        let Some(db) = maverick_testdb::database(&SCHEMA).await else {
            return;
        };
        let pool = db.pool().await;
        let mut config = config();
        config.role = role;
        if export {
            config.matrix_export_dir = "/unused/export".into();
            config.matrix_signing_keys = "test:/unused/key".into();
        }
        if !prices {
            config.cron_price_sync.clear();
        }
        let client = JobClient::from_config(&config).unwrap();
        let worker = worker::worker_options(&client, &config, deps())
            .pg_pool(pool.clone())
            .init()
            .await
            .unwrap();
        let actual: Vec<String> = sqlx::query_scalar(
            "select identifier from graphile_worker._private_tasks order by identifier",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let mut expected = vec![
            "latency_probes",
            "catalog_sync",
            "capture_expiry",
            // D44: both roles prune expired conversation affinity rows.
            "affinity_prune",
            "history_prune",
        ];
        if role == Role::Origin {
            expected.extend([
                "prediction_lock",
                "nightly_smoke",
                "weekly_sweep",
                "aggregate_rebuild",
                "regression_scan",
                "calibration_score",
                "model_discovery",
            ]);
        }
        if export && role == Role::Origin {
            expected.push("matrix_export");
        }
        if prices {
            expected.push("price_sync");
        }
        let mut handlers: Vec<String> = expected.iter().map(|s| (*s).to_owned()).collect();
        let mut crons: Vec<String> = expected
            .iter()
            .filter(|s| {
                ![
                    "aggregate_rebuild",
                    "regression_scan",
                    "calibration_score",
                    "matrix_export",
                ]
                .contains(s)
            })
            .map(|s| format!("cron.{s}"))
            .collect();
        handlers.extend(crons.clone());
        handlers.sort();
        assert_eq!(actual, handlers, "{role:?}, prices={prices}");
        let mut actual_crons: Vec<_> = worker::cron_triggers(&client)
            .into_iter()
            .map(|c| c.task_identifier)
            .collect();
        actual_crons.sort();
        crons.sort();
        assert_eq!(actual_crons, crons);
        drop(worker);
        pool.close().await;
        db.close().await;
    }
}
