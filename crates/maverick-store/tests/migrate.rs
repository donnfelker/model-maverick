//! `maverick migrate` on real Postgres (reference Part 1 §2.1, D21, D22, D37).
//!
//! A fresh database gets every engine migration, then graphile_worker's schema, and the
//! result equals a database built by the sqlx migrator plus graphile_worker's migrate.
//! A second run applies nothing. A changed history is refused, as sqlx refuses it.
//!
//! Every test here installs a capture subscriber, even when it asserts no log line.
//! With a single scoped subscriber in the process, `tracing` caches a callsite as
//! disabled when another thread (with no subscriber) reaches it first, and the test
//! that does capture then misses those lines.

use std::sync::LazyLock;

use maverick_store::MIGRATOR;
use maverick_store::health::{self, GRAPHILE_WORKER_SCHEMA};
use maverick_store::migrate::{Applied, MigrateError, migrate};
use maverick_store::schema::{SafetyError, binary_schema_version};
use maverick_testdb::Schema;
use serde_json::Value;
use sqlx::migrate::MigrateError as SqlxMigrateError;
use sqlx::postgres::PgPoolOptions;
use test_support::{TestTracing, init_test_tracing};

static EMPTY: LazyLock<Schema> = LazyLock::new(Schema::empty);

/// The expected result: the sqlx migrator, then graphile_worker's own migrate.
static MIGRATED: LazyLock<Schema> = LazyLock::new(|| {
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

/// The files in `migrations/`, sorted.
fn migration_files() -> Vec<String> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let mut files: Vec<String> = std::fs::read_dir(dir)
        .expect("migrations dir")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .into_string()
                .expect("utf8")
        })
        .collect();
    files.sort();
    files
}

/// A schema dump without comments and psql meta-commands (pg_dump 16.10 and later
/// print `\restrict` with a random key).
fn normalized(dump: &str) -> String {
    dump.lines()
        .filter(|line| !line.starts_with("--") && !line.starts_with('\\'))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The captured log lines with this `msg`.
fn logged(tracing: &TestTracing, msg: &str) -> Vec<Value> {
    tracing
        .captured_json()
        .into_iter()
        .filter(|line| line["msg"] == msg)
        .collect()
}

#[tokio::test]
async fn a_fresh_database_gets_everything_once() {
    let tracing = init_test_tracing();
    let Some(db) = maverick_testdb::database(&EMPTY).await else {
        return;
    };
    let Some(expected) = maverick_testdb::database(&MIGRATED).await else {
        return;
    };

    let first = migrate(db.url()).await.expect("first run");
    eprintln!("first run applied {first:?}");
    let files = migration_files();
    assert_eq!(first.schema, files);
    assert_eq!(first.queue.first(), Some(&1), "{first:?}");
    assert!(first.queue.windows(2).all(|pair| pair[0] < pair[1]));

    let applied = logged(&tracing, "applied schema migration");
    let sources: Vec<&str> = applied
        .iter()
        .map(|line| line["source"].as_str().expect("source"))
        .collect();
    assert_eq!(sources, files);
    assert!(applied.iter().all(|line| line["duration"].is_i64()));
    let versions: Vec<i64> = logged(&tracing, "applied graphile_worker migration")
        .iter()
        .map(|line| line["version"].as_i64().expect("version"))
        .collect();
    assert_eq!(
        versions,
        first
            .queue
            .iter()
            .map(|&v| i64::from(v))
            .collect::<Vec<_>>()
    );
    assert!(logged(&tracing, "schema migrations already up to date").is_empty());

    // The engine and queue schema equal what the migrator and graphile_worker build.
    let dump = normalized(&db.dump_schema().await);
    assert_eq!(dump, normalized(&expected.dump_schema().await));
    expected.close().await;

    // The engine can read graphile_worker's registry.
    let pool = db.pool().await;
    let workers = health::list_worker_heartbeats(&pool, GRAPHILE_WORKER_SCHEMA)
        .await
        .expect("graphile_worker registry");
    assert!(workers.is_empty());
    pool.close().await;

    // A second run is a no-op.
    let second = migrate(db.url()).await.expect("second run");
    eprintln!("second run applied {second:?}");
    assert_eq!(second, Applied::default());
    assert_eq!(
        logged(&tracing, "schema migrations already up to date").len(),
        1
    );
    assert_eq!(
        logged(&tracing, "graphile_worker migrations already up to date").len(),
        1
    );
    assert_eq!(
        logged(&tracing, "applied schema migration").len(),
        files.len()
    );
    assert_eq!(normalized(&db.dump_schema().await), dump);
    db.close().await;
}

#[tokio::test]
async fn a_changed_history_is_refused() {
    let _tracing = init_test_tracing();
    let Some(db) = maverick_testdb::database(&EMPTY).await else {
        return;
    };
    migrate(db.url()).await.expect("first run");
    let pool = db.pool().await;
    let last = MIGRATOR.iter().last().expect("migrations").version;

    let exec = |sql: String| {
        let pool = pool.clone();
        async move {
            sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
                .execute(&pool)
                .await
                .expect("tamper");
        }
    };
    let refused = || async {
        let error = migrate(db.url()).await.expect_err("refused");
        eprintln!("refused: {error}");
        error
    };

    // A failed (dirty) migration blocks the run.
    exec(format!(
        "UPDATE _sqlx_migrations SET success = false WHERE version = {last}"
    ))
    .await;
    let error = refused().await;
    assert!(matches!(error, MigrateError::Schema(SqlxMigrateError::Dirty(v)) if v == last));
    exec(format!(
        "UPDATE _sqlx_migrations SET success = true WHERE version = {last}"
    ))
    .await;

    // An applied version that `migrations/` does not have. DIVERGENCE: D50. The shared
    // schema check refuses this newer database before sqlx's own check, and names both
    // schema versions (spec §14.5).
    exec(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
         VALUES (9999, 'unknown', true, '\\x00', 0)"
            .to_owned(),
    )
    .await;
    let error = refused().await;
    assert!(matches!(
        &error,
        MigrateError::Safety(SafetyError::Newer { database: 9999, .. })
    ));
    assert_eq!(
        error.to_string(),
        format!(
            "schema migrations: the database schema is 9999, newer than this release's {} \
             (unknown migrations: 9999); run a release that knows them, or restore a backup",
            binary_schema_version()
        )
    );
    exec("DELETE FROM _sqlx_migrations WHERE version = 9999".to_owned()).await;

    // An applied file whose text changed.
    exec("UPDATE _sqlx_migrations SET checksum = '\\x00' WHERE version = 1".to_owned()).await;
    let error = refused().await;
    assert!(matches!(
        error,
        MigrateError::Schema(SqlxMigrateError::VersionMismatch(1))
    ));

    // Each refusal released the advisory lock: the repaired history migrates again.
    let checksum = MIGRATOR.iter().next().expect("baseline").checksum.to_vec();
    sqlx::query("UPDATE _sqlx_migrations SET checksum = $1 WHERE version = 1")
        .bind(checksum)
        .execute(&pool)
        .await
        .expect("repair");
    assert_eq!(
        migrate(db.url()).await.expect("repaired"),
        Applied::default()
    );
    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn an_unreachable_database_fails_before_any_migration() {
    let _tracing = init_test_tracing();
    let error =
        migrate("postgres://maverick@127.0.0.1:1/maverick?connect_timeout=1&sslmode=disable")
            .await
            .expect_err("unreachable");
    eprintln!("unreachable: {error}");
    assert!(matches!(error, MigrateError::Connect(_)));
    assert!(
        error.to_string().starts_with("schema migrations: "),
        "{error}"
    );
}
