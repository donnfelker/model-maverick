//! DIVERGENCE: D50. The schema status and the shared schema checks (spec §14.5) against
//! real Postgres: a fresh, a migrated, and a newer database. `migrate` refuses a newer
//! database before it applies anything, and names both schema versions.

use maverick_store::MIGRATOR;
use maverick_store::migrate::{MigrateError, migrate};
use maverick_store::schema::{self, SafetyError, Status, binary_schema_version};
use maverick_testdb::{Schema, TestDatabase};
use sqlx::{Connection, PgConnection};

fn engine_versions() -> Vec<i64> {
    MIGRATOR.iter().map(|m| m.version).collect()
}

async fn status_of(db: &TestDatabase) -> Status {
    let mut conn = db.connect().await;
    let status = schema::status(&mut conn).await.expect("status");
    conn.close().await.expect("close");
    status
}

/// A database with the engine and job-queue migrations applied by `migrate` itself.
async fn migrated() -> Option<TestDatabase> {
    let db = maverick_testdb::database(&Schema::empty()).await?;
    migrate(db.url()).await.expect("migrate");
    Some(db)
}

#[tokio::test]
async fn a_fresh_database_has_everything_pending() {
    let Some(db) = maverick_testdb::database(&Schema::empty()).await else {
        return;
    };
    let status = status_of(&db).await;
    assert!(status.applied.is_empty());
    assert_eq!(status.pending, engine_versions());
    assert!(status.unknown.is_empty() && status.dirty.is_empty());
    assert!(status.queue_applied.is_empty());
    assert!(!status.queue_pending.is_empty());
    assert!(status.queue_unknown.is_empty());
    assert_eq!(status.schema_version(), 0);
    assert_eq!(schema::check(&status), Ok(()));
    db.close().await;
}

#[tokio::test]
async fn a_migrated_database_has_nothing_pending() {
    let Some(db) = migrated().await else {
        return;
    };
    let status = status_of(&db).await;
    assert_eq!(status.applied, engine_versions());
    assert!(status.pending.is_empty() && status.unknown.is_empty());
    assert!(!status.queue_applied.is_empty());
    assert!(status.queue_pending.is_empty() && status.queue_unknown.is_empty());
    assert_eq!(status.schema_version(), binary_schema_version());
    assert_eq!(schema::check(&status), Ok(()));
    // A second run applies nothing.
    let applied = migrate(db.url()).await.expect("second run");
    assert!(applied.schema.is_empty() && applied.queue.is_empty());
    db.close().await;
}

#[tokio::test]
async fn migrate_refuses_a_newer_database_before_applying_anything() {
    let Some(db) = migrated().await else {
        return;
    };
    let mut conn = PgConnection::connect(db.url()).await.expect("connect");
    // A migration from a newer release, as sqlx records it.
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
         VALUES (9001, 'from a newer release', true, '\\x00', 0)",
    )
    .execute(&mut conn)
    .await
    .expect("insert");
    let status = schema::status(&mut conn).await.expect("status");
    assert_eq!(status.unknown, [9001]);
    assert_eq!(status.schema_version(), 9001);
    let expected = SafetyError::Newer {
        database: 9001,
        binary: binary_schema_version(),
        unknown: "9001".to_owned(),
    };
    assert_eq!(schema::check_newer(&status), Err(expected.clone()));

    // A known migration's row removed, so `migrate` would re-apply it if it did not stop.
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version = $1")
        .bind(binary_schema_version())
        .execute(&mut conn)
        .await
        .expect("delete");
    let error = migrate(db.url()).await.unwrap_err();
    assert!(
        matches!(&error, MigrateError::Safety(e) if *e == SafetyError::Newer {
            database: 9001,
            binary: binary_schema_version(),
            unknown: "9001".to_owned(),
        }),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        format!(
            "schema migrations: the database schema is 9001, newer than this release's {} (unknown migrations: 9001); run a release that knows them, or restore a backup",
            binary_schema_version()
        )
    );
    let after = schema::status(&mut conn).await.expect("status");
    assert_eq!(
        after.pending,
        [binary_schema_version()],
        "nothing was applied"
    );
    conn.close().await.expect("close");
    db.close().await;
}

#[tokio::test]
async fn a_newer_job_queue_is_refused_too() {
    let Some(db) = migrated().await else {
        return;
    };
    let mut conn = PgConnection::connect(db.url()).await.expect("connect");
    sqlx::query("INSERT INTO graphile_worker.migrations (id) VALUES (9001)")
        .execute(&mut conn)
        .await
        .expect("insert");
    let status = schema::status(&mut conn).await.expect("status");
    assert_eq!(status.queue_unknown, [9001]);
    assert_eq!(
        schema::check(&status),
        Err(SafetyError::QueueNewer {
            unknown: "9001".to_owned()
        })
    );
    assert!(matches!(
        migrate(db.url()).await,
        Err(MigrateError::Safety(SafetyError::QueueNewer { .. }))
    ));
    conn.close().await.expect("close");
    db.close().await;
}
