//! Store tests for the `batches` lease (spec §11.2, D10) on real Postgres.

use std::sync::LazyLock;

use maverick_store::batches::{self, BatchClaim, BatchEnd, ClaimOutcome};
use maverick_store::{MIGRATOR, is_not_found};
use maverick_testdb::Schema;
use sqlx::{Connection, PgConnection, PgPool};

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

const TTL: f64 = 600.0;

fn claim<'a>(worker_id: &'a str) -> BatchClaim<'a> {
    BatchClaim {
        id: "bat_nightly_2026-09-28",
        kind: "nightly_smoke",
        batch_window: "2026-09-28",
        worker_id,
        lease_ttl_secs: TTL,
    }
}

async fn claim_as(conn: &mut PgConnection, worker_id: &str, owner_is_gone: bool) -> ClaimOutcome {
    batches::claim_batch(conn, &claim(worker_id), |_| owner_is_gone)
        .await
        .expect("claim")
}

async fn expire_lease(pool: &PgPool) {
    sqlx::query("UPDATE batches SET lease_expires_at = now() - interval '1 second'")
        .execute(pool)
        .await
        .expect("expire");
}

#[tokio::test]
async fn a_new_window_is_claimed_and_a_live_foreign_lease_is_a_no_op() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let mut conn = db.connect().await;

    let ClaimOutcome::Claimed(batch) = claim_as(&mut conn, "worker-1", false).await else {
        panic!("a new window is claimed");
    };
    assert_eq!(batch.id, "bat_nightly_2026-09-28");
    assert_eq!(batch.status, "running");
    assert_eq!(batch.attempt, 1);
    assert_eq!(batch.lease_owner.as_deref(), Some("worker-1"));
    let expires = batch.lease_expires_at.expect("lease");
    let heartbeat = batch.heartbeat_at.expect("heartbeat");
    let ttl = (expires - heartbeat).num_seconds();
    assert!((599..=600).contains(&ttl), "lease ttl {ttl}s");

    // A live foreign lease whose owner is alive: no-op, nothing changes.
    let outcome = claim_as(&mut conn, "worker-2", false).await;
    assert!(!outcome.holds_lease());
    let ClaimOutcome::HeldByOther(held) = outcome else {
        panic!("held by worker-1");
    };
    assert_eq!(held, batch);

    // A different window of the same kind is a new batch.
    let other = BatchClaim {
        id: "bat_nightly_2026-09-29",
        batch_window: "2026-09-29",
        ..claim("worker-2")
    };
    assert!(matches!(
        batches::claim_batch(&mut conn, &other, |_| false)
            .await
            .expect("claim"),
        ClaimOutcome::Claimed(_)
    ));
    assert_eq!(
        batches::get_batch(&pool, "nightly_smoke", "2026-09-29")
            .await
            .expect("get")
            .lease_owner
            .as_deref(),
        Some("worker-2")
    );
    assert!(is_not_found(
        &batches::get_batch(&pool, "weekly_sweep", "2026-09-28")
            .await
            .unwrap_err()
    ));
    conn.close().await.expect("close");
    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn takeover_resumes_the_same_batch_and_counts_the_attempt() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let mut conn = db.connect().await;
    claim_as(&mut conn, "worker-1", false).await;

    // The owner is gone (the queue recovered its job): take over before expiry.
    let mut asked = None;
    let outcome = batches::claim_batch(&mut conn, &claim("worker-2"), |batch| {
        asked = batch.lease_owner.clone();
        true
    })
    .await
    .expect("claim");
    assert_eq!(
        asked.as_deref(),
        Some("worker-1"),
        "the predicate sees the owner"
    );
    let ClaimOutcome::TookOver(batch) = outcome else {
        panic!("taken over");
    };
    assert_eq!(
        (batch.id.as_str(), batch.attempt),
        ("bat_nightly_2026-09-28", 2)
    );
    assert_eq!(batch.lease_owner.as_deref(), Some("worker-2"));

    // An expired lease is taken over without asking about the owner.
    expire_lease(&pool).await;
    let ClaimOutcome::TookOver(batch) =
        batches::claim_batch(&mut conn, &claim("worker-3"), |_| panic!("not asked"))
            .await
            .expect("claim")
    else {
        panic!("expired lease taken over");
    };
    assert_eq!(batch.attempt, 3);
    assert!(batch.lease_expires_at.expect("lease") > batch.updated_at);

    // The owner itself claiming again (a retry on the same worker) resumes.
    let ClaimOutcome::TookOver(batch) = claim_as(&mut conn, "worker-3", false).await else {
        panic!("own lease resumed");
    };
    assert_eq!(batch.attempt, 4);
    conn.close().await.expect("close");
    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn heartbeat_and_finish_apply_only_to_the_owner() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let mut conn = db.connect().await;
    let ClaimOutcome::Claimed(batch) = claim_as(&mut conn, "worker-1", false).await else {
        panic!("claimed");
    };

    assert!(
        batches::heartbeat_batch(&pool, &batch.id, "worker-2", TTL)
            .await
            .expect("heartbeat")
            .is_none(),
        "not the owner"
    );
    let renewed = batches::heartbeat_batch(&pool, &batch.id, "worker-1", TTL)
        .await
        .expect("heartbeat")
        .expect("owner renews");
    assert!(renewed.heartbeat_at >= batch.heartbeat_at);
    assert!(renewed.lease_expires_at >= batch.lease_expires_at);

    assert!(
        batches::finish_batch(&pool, &batch.id, "worker-2", BatchEnd::Completed)
            .await
            .expect("finish")
            .is_none()
    );
    let done = batches::finish_batch(&pool, &batch.id, "worker-1", BatchEnd::Completed)
        .await
        .expect("finish")
        .expect("owner completes");
    assert_eq!(done.status, "completed");
    assert!(done.completed_at.is_some());
    assert_eq!((done.lease_owner, done.lease_expires_at), (None, None));

    // After completion: every claim is a no-op, heartbeats and finishes do nothing.
    let outcome = claim_as(&mut conn, "worker-2", true).await;
    assert!(matches!(outcome, ClaimOutcome::Finished(_)), "{outcome:?}");
    assert_eq!(outcome.batch().attempt, 1);
    assert!(
        batches::heartbeat_batch(&pool, &batch.id, "worker-1", TTL)
            .await
            .expect("heartbeat")
            .is_none()
    );
    assert!(
        batches::finish_batch(&pool, &batch.id, "worker-1", BatchEnd::Skipped)
            .await
            .expect("finish")
            .is_none()
    );
    conn.close().await.expect("close");
    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn a_skipped_window_is_finished_and_last_completions_are_per_kind() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let mut conn = db.connect().await;
    let windows = [
        ("bat_1", "nightly_smoke", "2026-09-27", BatchEnd::Completed),
        ("bat_2", "nightly_smoke", "2026-09-28", BatchEnd::Completed),
        ("bat_3", "aggregate_rebuild", "bat_2", BatchEnd::Completed),
        ("bat_4", "weekly_sweep", "2026-W39", BatchEnd::Skipped),
    ];
    for (id, kind, window, end) in windows {
        let request = BatchClaim {
            id,
            kind,
            batch_window: window,
            worker_id: "w",
            lease_ttl_secs: TTL,
        };
        batches::claim_batch(&mut conn, &request, |_| false)
            .await
            .expect("claim");
        batches::finish_batch(&pool, id, "w", end)
            .await
            .expect("finish")
            .expect("owner");
    }
    let skipped = BatchClaim {
        id: "bat_other",
        kind: "weekly_sweep",
        batch_window: "2026-W39",
        worker_id: "w2",
        lease_ttl_secs: TTL,
    };
    let outcome = batches::claim_batch(&mut conn, &skipped, |_| true)
        .await
        .expect("claim");
    let ClaimOutcome::Finished(batch) = outcome else {
        panic!("a skipped window is finished");
    };
    assert_eq!(
        (batch.id.as_str(), batch.status.as_str()),
        ("bat_4", "skipped")
    );

    let last = batches::list_last_completed_batches(&pool)
        .await
        .expect("last");
    let kinds: Vec<&str> = last.iter().map(|(kind, _)| kind.as_str()).collect();
    assert_eq!(
        kinds,
        ["aggregate_rebuild", "nightly_smoke"],
        "skipped kinds are absent"
    );
    let bat_2 = batches::get_batch_by_id(&pool, "bat_2").await.expect("get");
    assert_eq!(
        Some(last[1].1),
        bat_2.completed_at,
        "the newest completion per kind"
    );
    conn.close().await.expect("close");
    pool.close().await;
    db.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_claimers_leave_exactly_one_lease_holder() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    const CLAIMERS: usize = 12;
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(CLAIMERS));
    let mut tasks = Vec::new();
    for n in 0..CLAIMERS {
        let url = db.url().to_owned();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            let mut conn = PgConnection::connect(&url).await.expect("connect");
            let worker = format!("worker-{n}");
            barrier.wait().await;
            let outcome = batches::claim_batch(&mut conn, &claim(&worker), |_| false)
                .await
                .expect("claim");
            conn.close().await.expect("close");
            outcome
        }));
    }
    let mut claimed = 0;
    let mut held = 0;
    let mut owner = None;
    for task in tasks {
        match task.await.expect("task") {
            ClaimOutcome::Claimed(batch) => {
                claimed += 1;
                owner = batch.lease_owner;
            }
            ClaimOutcome::HeldByOther(_) => held += 1,
            other => panic!("unexpected outcome {other:?}"),
        }
    }
    assert_eq!((claimed, held), (1, CLAIMERS - 1));

    let pool = db.pool().await;
    let stored = batches::get_batch(&pool, "nightly_smoke", "2026-09-28")
        .await
        .expect("get");
    assert_eq!(stored.lease_owner, owner);
    assert_eq!(stored.attempt, 1);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM batches")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(rows, 1);
    pool.close().await;
    db.close().await;
}
