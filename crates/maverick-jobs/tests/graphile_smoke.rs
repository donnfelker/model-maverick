//! graphile_worker smoke test on real Postgres.

use std::sync::LazyLock;

use graphile_worker::{
    IntoTaskHandlerResult, JobSpec, TaskHandler, WorkerContext, WorkerOptions, WorkerUtils,
};
use maverick_jobs::{DEFAULT_QUEUE_NAME, JobKind};
use maverick_testdb::Schema;
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPoolOptions;

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| {
    Schema::custom("graphile_worker 0.13", |url| async move {
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await?;
        WorkerUtils::new(pool.clone(), "graphile_worker")
            .migrate()
            .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS job_smoke_runs (
                label text primary key,
                ran_at timestamptz not null default now()
            )",
        )
        .execute(&pool)
        .await?;
        pool.close().await;
        Ok(())
    })
});

#[derive(Clone, Debug, Deserialize, Serialize)]
struct SmokeJob {
    label: String,
}

impl TaskHandler for SmokeJob {
    const IDENTIFIER: &'static str = "maverick_jobs_smoke";

    async fn run(self, ctx: WorkerContext) -> impl IntoTaskHandlerResult {
        sqlx::query(
            "INSERT INTO job_smoke_runs (label)
             VALUES ($1)
             ON CONFLICT (label) DO UPDATE SET ran_at = now()",
        )
        .bind(self.label)
        .execute(ctx.pg_pool())
        .await
        .map(|_| ())
        .map_err(|err| err.to_string())
    }
}

#[tokio::test]
async fn graphile_worker_runs_a_trivial_job_end_to_end() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let worker = WorkerOptions::default()
        .concurrency(1)
        .schema("graphile_worker")
        .pg_pool(pool.clone())
        .define_job::<SmokeJob>()
        .init()
        .await
        .expect("worker");
    let utils = worker.create_utils();

    utils
        .add_job(
            SmokeJob {
                label: "ran".to_string(),
            },
            JobSpec::builder()
                .queue_name(DEFAULT_QUEUE_NAME)
                .job_key(JobKind::NightlySmoke.job_key("smoke"))
                .max_attempts(JobKind::NightlySmoke.max_attempts())
                .build(),
        )
        .await
        .expect("enqueue");

    worker.run_once().await.expect("run once");

    let ran: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM job_smoke_runs)")
        .fetch_one(&pool)
        .await
        .expect("smoke row");
    assert!(ran);

    pool.close().await;
    db.close().await;
}
