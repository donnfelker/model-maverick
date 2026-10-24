//! `store.Migrate` (reference Part 1 §2.1): the engine migrations, then the job queue's.
//!
//! Go ran goose, then River's migrations. Rust runs the sqlx migrations in `migrations/`
//! (D21, D37), then graphile_worker's own schema migrations (D22), on the same database.
//! The engine migrations run one at a time on one connection, so each applied file is
//! logged with its duration, as goose did. A second run applies nothing.
//!
//! Before it applies anything, a run makes the shared schema checks (spec §14.5, D50): it
//! refuses a database newer than this binary and one older than the 12-month upgrade
//! window ([`crate::schema::check`]).

use std::collections::BTreeMap;
use std::error::Error as StdError;
use std::time::Duration;

use sqlx::migrate::{Migrate, MigrateError as SqlxMigrateError, Migration};
use sqlx::postgres::{PgConnection, PgPool, PgPoolOptions};
use sqlx::{AssertSqlSafe, Connection};

use crate::MIGRATOR;
use crate::health::GRAPHILE_WORKER_SCHEMA;
use crate::schema::{self, SafetyError};

/// Why a migration run failed. The text names the step, like Go's
/// `goose migrations: <err>` and `river migrations: <err>`.
#[derive(Debug, thiserror::Error)]
pub enum MigrateError {
    /// The connection for the engine migrations failed.
    #[error("schema migrations: {0}")]
    Connect(sqlx::Error),
    /// An engine migration failed, or the applied history does not match `migrations/`.
    #[error("schema migrations: {0}")]
    Schema(SqlxMigrateError),
    /// graphile_worker's migrations failed.
    #[error("graphile_worker migrations: {0}")]
    Queue(Box<dyn StdError + Send + Sync>),
    /// The schema checks refused the database (D50); nothing was applied.
    #[error("schema migrations: {0}")]
    Safety(SafetyError),
    /// The schema status could not be read.
    #[error("schema migrations: reading the schema status: {0}")]
    Status(sqlx::Error),
}

/// What one run applied.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Applied {
    /// The engine migration files, in the order they were applied.
    pub schema: Vec<String>,
    /// The graphile_worker migration versions, in order.
    pub queue: Vec<i32>,
}

/// Brings the database at `database_url` to the latest engine and queue schema.
///
/// Logs INFO `applied schema migration` (`source`, `duration` in nanoseconds) per
/// applied file or `schema migrations already up to date`, then INFO
/// `applied graphile_worker migration` (`version`) per applied queue migration or
/// `graphile_worker migrations already up to date`.
pub async fn migrate(database_url: &str) -> Result<Applied, MigrateError> {
    let mut conn = PgConnection::connect(database_url)
        .await
        .map_err(MigrateError::Connect)?;
    let checked = match schema::status(&mut conn).await {
        Ok(status) => schema::check(&status).map_err(MigrateError::Safety),
        Err(error) => Err(MigrateError::Status(error)),
    };
    if let Err(error) = checked {
        let _ = conn.close().await;
        return Err(error);
    }
    let schema = apply_schema(&mut conn).await;
    let _ = conn.close().await;
    let schema = schema.map_err(MigrateError::Schema)?;
    let queue = apply_queue(database_url)
        .await
        .map_err(MigrateError::Queue)?;
    Ok(Applied { schema, queue })
}

/// sqlx `Migrator::run_direct`, with one log line per applied file: the same advisory
/// lock, dirty check, missing-version check, and checksum check.
async fn apply_schema(conn: &mut PgConnection) -> Result<Vec<String>, SqlxMigrateError> {
    let table = MIGRATOR.table_name.as_ref();
    conn.lock().await?;
    conn.ensure_migrations_table(table).await?;
    if let Some(version) = conn.dirty_version(table).await? {
        return Err(SqlxMigrateError::Dirty(version));
    }
    let done: BTreeMap<i64, _> = conn
        .list_applied_migrations(table)
        .await?
        .into_iter()
        .map(|applied| (applied.version, applied.checksum))
        .collect();
    if let Some(&missing) = done
        .keys()
        .find(|version| !MIGRATOR.iter().any(|m| m.version == **version))
    {
        return Err(SqlxMigrateError::VersionMissing(missing));
    }

    let mut applied = Vec::new();
    for migration in MIGRATOR
        .iter()
        .filter(|m| !m.migration_type.is_down_migration())
    {
        match done.get(&migration.version) {
            Some(checksum) if *checksum != migration.checksum => {
                return Err(SqlxMigrateError::VersionMismatch(migration.version));
            }
            Some(_) => {}
            None => {
                let elapsed = conn.apply(table, migration).await?;
                let source = source_name(migration);
                tracing::info!(
                    source = source.as_str(),
                    duration = nanos(elapsed),
                    "applied schema migration"
                );
                applied.push(source);
            }
        }
    }
    if applied.is_empty() {
        tracing::info!("schema migrations already up to date");
    }
    conn.unlock().await?;
    Ok(applied)
}

/// The migration's file name: sqlx keeps the version and the description with `_` read
/// as a space, and every file in `migrations/` is `NNNN_<words>.sql`.
fn source_name(migration: &Migration) -> String {
    format!(
        "{:04}_{}.sql",
        migration.version,
        migration.description.replace(' ', "_")
    )
}

/// A duration as Go's slog writes it: integer nanoseconds.
fn nanos(elapsed: Duration) -> i64 {
    i64::try_from(elapsed.as_nanos()).unwrap_or(i64::MAX)
}

/// graphile_worker's own migrations, logged by the versions they added.
async fn apply_queue(database_url: &str) -> Result<Vec<i32>, Box<dyn StdError + Send + Sync>> {
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(database_url)
        .await?;
    let applied = apply_queue_on(&pool).await;
    pool.close().await;
    applied
}

async fn apply_queue_on(pool: &PgPool) -> Result<Vec<i32>, Box<dyn StdError + Send + Sync>> {
    let before = queue_versions(pool).await?;
    graphile_worker::WorkerUtils::new(pool.clone(), GRAPHILE_WORKER_SCHEMA)
        .migrate()
        .await?;
    let applied: Vec<i32> = queue_versions(pool)
        .await?
        .into_iter()
        .filter(|version| !before.contains(version))
        .collect();
    for &version in &applied {
        tracing::info!(version, "applied graphile_worker migration");
    }
    if applied.is_empty() {
        tracing::info!("graphile_worker migrations already up to date");
    }
    Ok(applied)
}

/// The versions in graphile_worker's `migrations` table; none before its first run.
/// The table belongs to graphile_worker, so these queries are checked at run time.
async fn queue_versions(pool: &PgPool) -> sqlx::Result<Vec<i32>> {
    let table = format!("{GRAPHILE_WORKER_SCHEMA}.migrations");
    let exists: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
        .bind(&table)
        .fetch_one(pool)
        .await?;
    if !exists {
        return Ok(Vec::new());
    }
    sqlx::query_scalar(AssertSqlSafe(format!("SELECT id FROM {table} ORDER BY id")))
        .fetch_all(pool)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_names_are_the_migration_files() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
        let mut files: Vec<String> = std::fs::read_dir(&dir)
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
        let names: Vec<String> = MIGRATOR.iter().map(source_name).collect();
        assert_eq!(names, files);
        assert_eq!(names[0], "0001_baseline.sql");
    }

    #[test]
    fn durations_are_integer_nanoseconds() {
        assert_eq!(nanos(Duration::from_millis(3)), 3_000_000);
        assert_eq!(nanos(Duration::MAX), i64::MAX);
    }
}
