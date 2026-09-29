//! Store test for the job queue health read against graphile_worker's own schema.

use std::sync::LazyLock;

use maverick_store::MIGRATOR;
use maverick_store::health::{self, GRAPHILE_WORKER_SCHEMA};
use maverick_testdb::Schema;
use sqlx::postgres::PgPoolOptions;

/// The engine migrations, then graphile_worker's migrations, as a deployment has them.
static SCHEMA: LazyLock<Schema> = LazyLock::new(|| {
    Schema::sqlx(&MIGRATOR).then(Schema::custom("graphile_worker 0.13", |url| async move {
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await?;
        graphile_worker::WorkerUtils::new(pool.clone(), GRAPHILE_WORKER_SCHEMA)
            .migrate()
            .await?;
        pool.close().await;
        Ok(())
    }))
});

#[tokio::test]
async fn worker_heartbeats_come_from_the_graphile_registry() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;

    assert!(
        health::list_worker_heartbeats(&pool, GRAPHILE_WORKER_SCHEMA)
            .await
            .expect("empty registry")
            .is_empty(),
        "no worker has registered"
    );

    // graphile_worker's own registration function, as a running worker calls it.
    for worker in ["graphile_worker_b", "graphile_worker_a"] {
        sqlx::query("SELECT graphile_worker.worker_heartbeat($1)")
            .bind(worker)
            .execute(&pool)
            .await
            .expect("heartbeat");
    }
    sqlx::query(
        "UPDATE graphile_worker._private_workers
         SET last_heartbeat_at = now() - interval '90 seconds'
         WHERE id = 'graphile_worker_a'",
    )
    .execute(&pool)
    .await
    .expect("age a heartbeat");

    let workers = health::list_worker_heartbeats(&pool, GRAPHILE_WORKER_SCHEMA)
        .await
        .expect("workers");
    let ids: Vec<&str> = workers.iter().map(|w| w.id.as_str()).collect();
    assert_eq!(ids, ["graphile_worker_a", "graphile_worker_b"]);
    assert!(
        (89.0..91.0).contains(&workers[0].age_seconds),
        "{workers:?}"
    );
    assert!((0.0..1.0).contains(&workers[1].age_seconds), "{workers:?}");

    let err = health::list_worker_heartbeats(&pool, "no_such_schema")
        .await
        .unwrap_err();
    assert!(
        err.as_database_error().is_some(),
        "a missing schema is an error: {err}"
    );
    pool.close().await;
    db.close().await;
}
