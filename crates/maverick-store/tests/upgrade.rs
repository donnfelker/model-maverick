//! DIVERGENCE: D50. The upgrade history, the `serve` heartbeats, the upgrade lock, and the
//! verify read (spec §3, §14.5) against real Postgres.

use chrono::{TimeDelta, TimeZone, Utc};
use maverick_store::MIGRATOR;
use maverick_store::upgrade::{
    self, HEARTBEAT_STALE_AFTER_SECS, Heartbeat, Upgrade, new_process_id, new_upgrade_id,
};
use maverick_testdb::Schema;
use sqlx::{Connection, PgConnection};

#[tokio::test]
async fn an_upgrade_is_recorded_and_finished() {
    let Some(db) = maverick_testdb::database(&Schema::sqlx(&MIGRATOR)).await else {
        return;
    };
    let pool = db.pool().await;
    assert_eq!(upgrade::last_upgrade(&pool).await.expect("none"), None);
    let started = Utc.with_ymd_and_hms(2027, 1, 10, 9, 0, 0).unwrap();
    let id = new_upgrade_id();
    assert!(id.starts_with("upg_") && id.len() == 30, "{id}");
    upgrade::start_upgrade(&pool, &id, None, "1.0.0", "cli:ops", started)
        .await
        .expect("start");
    let unfinished = upgrade::last_upgrade(&pool).await.expect("read");
    assert_eq!(
        unfinished,
        Some(Upgrade {
            id: id.clone(),
            from_release: None,
            to_release: "1.0.0".to_owned(),
            operator: "cli:ops".to_owned(),
            migrations: Vec::new(),
            started_at: started,
            finished_at: None,
            verify_failed: None,
        })
    );
    let finished = started + TimeDelta::seconds(4);
    let applied = vec!["0012_upgrades.sql".to_owned()];
    upgrade::finish_upgrade(&pool, &id, &applied, finished, Some("table upgrades: gone"))
        .await
        .expect("finish");
    let row = upgrade::last_upgrade(&pool)
        .await
        .expect("read")
        .expect("row");
    assert_eq!(row.migrations, applied);
    assert_eq!(row.finished_at, Some(finished));
    assert_eq!(row.verify_failed.as_deref(), Some("table upgrades: gone"));

    // The newest run wins.
    let later = new_upgrade_id();
    upgrade::start_upgrade(&pool, &later, Some("1.0.0"), "1.1.0", "cli:ops", finished)
        .await
        .expect("start");
    let row = upgrade::last_upgrade(&pool)
        .await
        .expect("read")
        .expect("row");
    assert_eq!(
        (row.id, row.from_release.as_deref()),
        (later, Some("1.0.0"))
    );
    db.close().await;
}

#[tokio::test]
async fn heartbeats_refresh_go_stale_and_stop() {
    let Some(db) = maverick_testdb::database(&Schema::sqlx(&MIGRATOR)).await else {
        return;
    };
    let pool = db.pool().await;
    let now = Utc.with_ymd_and_hms(2027, 1, 10, 9, 0, 0).unwrap();
    let a = new_process_id();
    let b = new_process_id();
    assert!(a.starts_with("srv_") && a != b);
    upgrade::beat(&pool, &a, "1.0.0", "host-a", now)
        .await
        .expect("beat");
    upgrade::beat(
        &pool,
        &b,
        "0.9.0",
        "host-b",
        now - TimeDelta::seconds(HEARTBEAT_STALE_AFTER_SECS),
    )
    .await
    .expect("beat");
    // b's last beat is exactly the stale age: stopped.
    let live = upgrade::live_heartbeats(&pool, now).await.expect("live");
    assert_eq!(
        live,
        [Heartbeat {
            process_id: a.clone(),
            release: "1.0.0".to_owned(),
            host_label: "host-a".to_owned(),
            last_seen_at: now,
        }]
    );
    // A refresh moves b forward and can change its release.
    upgrade::beat(&pool, &b, "1.0.0", "host-b", now)
        .await
        .expect("beat");
    assert_eq!(
        upgrade::live_heartbeats(&pool, now)
            .await
            .expect("live")
            .len(),
        2
    );
    upgrade::stop_beating(&pool, &a).await.expect("stop");
    let live = upgrade::live_heartbeats(&pool, now).await.expect("live");
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].process_id, b);
    db.close().await;
}

#[tokio::test]
async fn one_upgrade_holds_the_lock_at_a_time() {
    let Some(db) = maverick_testdb::database(&Schema::sqlx(&MIGRATOR)).await else {
        return;
    };
    let mut first = PgConnection::connect(db.url()).await.expect("connect");
    let mut second = PgConnection::connect(db.url()).await.expect("connect");
    assert!(upgrade::try_upgrade_lock(&mut first).await.expect("lock"));
    assert!(!upgrade::try_upgrade_lock(&mut second).await.expect("lock"));
    upgrade::release_upgrade_lock(&mut first)
        .await
        .expect("unlock");
    assert!(upgrade::try_upgrade_lock(&mut second).await.expect("lock"));
    // Closing the holder's connection releases the lock too. The server frees a session
    // lock when the backend exits, a moment after `close` returns, so wait for that
    // (bead xq7v.20; it failed on CI when the retry came first).
    second.close().await.expect("close");
    let mut taken = false;
    for _ in 0..100 {
        if upgrade::try_upgrade_lock(&mut first).await.expect("lock") {
            taken = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        taken,
        "closing the holder's connection never released the lock"
    );
    first.close().await.expect("close");
    db.close().await;
}

#[tokio::test]
async fn the_verify_read_names_a_missing_table() {
    let Some(db) = maverick_testdb::database(&Schema::sqlx(&MIGRATOR)).await else {
        return;
    };
    let mut conn = PgConnection::connect(db.url()).await.expect("connect");
    upgrade::read_one_row(&mut conn, "upgrades")
        .await
        .expect("an empty table reads");
    upgrade::read_one_row(&mut conn, "tenants")
        .await
        .expect("a baseline table reads");
    sqlx::query("drop table serve_heartbeats")
        .execute(&mut conn)
        .await
        .expect("drop");
    let error = upgrade::read_one_row(&mut conn, "serve_heartbeats")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("serve_heartbeats"), "{error}");
    // A name is quoted, never spliced into the statement.
    assert!(
        upgrade::read_one_row(&mut conn, "tenants; drop table tenants")
            .await
            .is_err()
    );
    upgrade::read_one_row(&mut conn, "tenants")
        .await
        .expect("tenants is still there");
    conn.close().await.expect("close");
    db.close().await;
}
