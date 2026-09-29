//! Job-level tests for the `batches` lease wrapper.

use std::sync::LazyLock;
use std::time::Duration;

use graphile_worker::WorkerUtils;
use maverick_jobs::{
    BatchEnd, BatchLeaseConfig, BatchLeaseError, BatchLeaseManager, BatchLeaseOutcome, JobKind,
    deterministic_batch_id,
};
use maverick_store::MIGRATOR;
use maverick_store::batches;
use maverick_store::health::GRAPHILE_WORKER_SCHEMA;
use maverick_testdb::Schema;
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

fn manager() -> BatchLeaseManager {
    BatchLeaseManager::new(BatchLeaseConfig {
        lease_ttl: Duration::from_secs(600),
        heartbeat_interval: Duration::from_secs(200),
        owner_stale_after: Duration::from_secs(60),
        graphile_schema: GRAPHILE_WORKER_SCHEMA.to_string(),
    })
}

async fn register_worker(pool: &sqlx::PgPool, worker_id: &str) {
    sqlx::query("SELECT graphile_worker.worker_heartbeat($1)")
        .bind(worker_id)
        .execute(pool)
        .await
        .expect("register worker");
}

async fn age_worker(pool: &sqlx::PgPool, worker_id: &str, seconds: i64) {
    sqlx::query(
        "UPDATE graphile_worker._private_workers
         SET last_heartbeat_at = now() - $2::int * interval '1 second'
         WHERE id = $1",
    )
    .bind(worker_id)
    .bind(seconds as i32)
    .execute(pool)
    .await
    .expect("age worker");
}

#[test]
fn deterministic_batch_ids_are_ulid_shaped_and_stable() {
    let id = deterministic_batch_id(JobKind::NightlySmoke, "2026-09-28");
    assert_eq!(
        id,
        deterministic_batch_id(JobKind::NightlySmoke, "2026-09-28")
    );
    assert_ne!(
        id,
        deterministic_batch_id(JobKind::NightlySmoke, "2026-09-29")
    );
    assert_ne!(
        id,
        deterministic_batch_id(JobKind::WeeklySweep, "2026-09-28")
    );
    assert!(id.starts_with("bat_"), "{id}");
    assert_eq!(id.len(), 30);
    assert!(ulid::Ulid::from_string(&id[4..]).is_ok(), "{id}");
}

#[tokio::test]
async fn claim_live_foreign_and_completed_outcomes_are_no_ops() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let manager = manager();

    let BatchLeaseOutcome::Claimed(lease) = manager
        .claim(&pool, JobKind::NightlySmoke, "2026-09-28", "worker-1")
        .await
        .expect("claim")
    else {
        panic!("new window is claimed");
    };
    assert_eq!(
        lease.id(),
        deterministic_batch_id(JobKind::NightlySmoke, "2026-09-28")
    );
    assert_eq!(lease.batch().attempt, 1);
    assert_eq!(lease.batch().lease_owner.as_deref(), Some("worker-1"));

    register_worker(&pool, "worker-1").await;
    let held = manager
        .claim(&pool, JobKind::NightlySmoke, "2026-09-28", "worker-2")
        .await
        .expect("claim");
    let BatchLeaseOutcome::HeldByOther(held) = held else {
        panic!("live foreign lease is a no-op");
    };
    assert_eq!(held.lease_owner.as_deref(), Some("worker-1"));
    assert_eq!(held.attempt, 1);

    let done = lease
        .finish(&pool, BatchEnd::Completed)
        .await
        .expect("finish");
    assert_eq!(done.status, "completed");
    let finished = manager
        .claim(&pool, JobKind::NightlySmoke, "2026-09-28", "worker-2")
        .await
        .expect("claim");
    let BatchLeaseOutcome::Finished(finished) = finished else {
        panic!("completed window is a no-op");
    };
    assert_eq!(finished.id, done.id);
    assert_eq!(finished.status, "completed");

    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn missing_or_stale_worker_heartbeat_allows_immediate_takeover() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let manager = manager();

    let BatchLeaseOutcome::Claimed(first) = manager
        .claim(&pool, JobKind::WeeklySweep, "2026-W39", "missing-owner")
        .await
        .expect("claim")
    else {
        panic!("new window is claimed");
    };
    let BatchLeaseOutcome::TookOver(taken) = manager
        .claim(&pool, JobKind::WeeklySweep, "2026-W39", "worker-2")
        .await
        .expect("claim")
    else {
        panic!("missing owner is taken over");
    };
    assert_eq!(taken.id(), first.id());
    assert_eq!(taken.batch().attempt, 2);
    assert_eq!(taken.batch().lease_owner.as_deref(), Some("worker-2"));

    register_worker(&pool, "worker-2").await;
    age_worker(&pool, "worker-2", 90).await;
    let BatchLeaseOutcome::TookOver(taken_again) = manager
        .claim(&pool, JobKind::WeeklySweep, "2026-W39", "worker-3")
        .await
        .expect("claim")
    else {
        panic!("stale owner is taken over");
    };
    assert_eq!(taken_again.id(), first.id());
    assert_eq!(taken_again.batch().attempt, 3);
    assert_eq!(taken_again.batch().lease_owner.as_deref(), Some("worker-3"));

    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn expired_lease_is_taken_over_under_the_same_id() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let manager = manager();
    register_worker(&pool, "worker-1").await;

    let BatchLeaseOutcome::Claimed(first) = manager
        .claim(&pool, JobKind::NightlySmoke, "2026-09-29", "worker-1")
        .await
        .expect("claim")
    else {
        panic!("new window is claimed");
    };
    sqlx::query("UPDATE batches SET lease_expires_at = now() - interval '1 second'")
        .execute(&pool)
        .await
        .expect("expire");

    let BatchLeaseOutcome::TookOver(taken) = manager
        .claim(&pool, JobKind::NightlySmoke, "2026-09-29", "worker-2")
        .await
        .expect("claim")
    else {
        panic!("expired lease is taken over");
    };
    assert_eq!(taken.id(), first.id());
    assert_eq!(taken.batch().attempt, 2);
    assert_eq!(taken.batch().lease_owner.as_deref(), Some("worker-2"));

    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn registry_read_errors_keep_live_foreign_leases_as_no_ops() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let manager = BatchLeaseManager::new(BatchLeaseConfig {
        graphile_schema: "no_such_schema".to_string(),
        ..manager().config().clone()
    });

    let BatchLeaseOutcome::Claimed(first) = manager
        .claim(&pool, JobKind::NightlySmoke, "2026-10-03", "worker-1")
        .await
        .expect("claim")
    else {
        panic!("new window is claimed even when the registry cannot be read");
    };
    let BatchLeaseOutcome::HeldByOther(held) = manager
        .claim(&pool, JobKind::NightlySmoke, "2026-10-03", "worker-2")
        .await
        .expect("claim")
    else {
        panic!("unknown owner liveness is a no-op");
    };
    assert_eq!(held.id, first.id());
    assert_eq!(held.lease_owner.as_deref(), Some("worker-1"));

    sqlx::query("UPDATE batches SET lease_expires_at = now() - interval '1 second'")
        .execute(&pool)
        .await
        .expect("expire");
    let BatchLeaseOutcome::TookOver(taken) = manager
        .claim(&pool, JobKind::NightlySmoke, "2026-10-03", "worker-2")
        .await
        .expect("claim")
    else {
        panic!("expired lease is taken over without registry liveness");
    };
    assert_eq!(taken.id(), first.id());
    assert_eq!(taken.batch().attempt, 2);

    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn heartbeat_renews_and_reports_lease_loss() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let manager = manager();

    let BatchLeaseOutcome::Claimed(mut lease) = manager
        .claim(&pool, JobKind::NightlySmoke, "2026-09-30", "worker-1")
        .await
        .expect("claim")
    else {
        panic!("new window is claimed");
    };
    let first_heartbeat = lease.batch().heartbeat_at;
    let renewed = lease.heartbeat(&pool).await.expect("heartbeat").clone();
    assert!(renewed.heartbeat_at >= first_heartbeat);
    assert_eq!(renewed.lease_owner.as_deref(), Some("worker-1"));

    sqlx::query("UPDATE batches SET lease_owner = 'worker-2' WHERE id = $1")
        .bind(lease.id())
        .execute(&pool)
        .await
        .expect("steal lease");
    let err = lease.heartbeat(&pool).await.expect_err("lease lost");
    assert!(matches!(err, BatchLeaseError::LeaseLost { .. }), "{err:?}");

    let BatchLeaseOutcome::Claimed(lease) = manager
        .claim(&pool, JobKind::NightlySmoke, "2026-10-01", "worker-3")
        .await
        .expect("claim")
    else {
        panic!("new window is claimed");
    };
    sqlx::query("UPDATE batches SET lease_owner = 'worker-4' WHERE id = $1")
        .bind(lease.id())
        .execute(&pool)
        .await
        .expect("steal lease");
    let err = lease
        .finish(&pool, BatchEnd::Skipped)
        .await
        .expect_err("finish lost");
    assert!(matches!(err, BatchLeaseError::LeaseLost { .. }), "{err:?}");

    pool.close().await;
    db.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_claimers_leave_exactly_one_runner() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    const CLAIMERS: usize = 12;
    for n in 0..CLAIMERS {
        register_worker(&pool, &format!("worker-{n}")).await;
    }

    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(CLAIMERS));
    let manager = std::sync::Arc::new(manager());
    let mut tasks = Vec::new();
    for n in 0..CLAIMERS {
        let pool = pool.clone();
        let barrier = barrier.clone();
        let manager = manager.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            manager
                .claim(
                    &pool,
                    JobKind::NightlySmoke,
                    "2026-10-02",
                    &format!("worker-{n}"),
                )
                .await
                .expect("claim")
        }));
    }

    let mut runners = 0;
    let mut held = 0;
    let mut owner = None;
    for task in tasks {
        match task.await.expect("task") {
            BatchLeaseOutcome::Claimed(lease) | BatchLeaseOutcome::TookOver(lease) => {
                runners += 1;
                owner = Some(lease.worker_id().to_string());
            }
            BatchLeaseOutcome::HeldByOther(_) => held += 1,
            other => panic!("unexpected outcome {other:?}"),
        }
    }
    assert_eq!((runners, held), (1, CLAIMERS - 1));

    let stored = batches::get_batch(&pool, "nightly_smoke", "2026-10-02")
        .await
        .expect("get");
    assert_eq!(stored.lease_owner, owner);
    assert_eq!(stored.attempt, 1);

    pool.close().await;
    db.close().await;
}
