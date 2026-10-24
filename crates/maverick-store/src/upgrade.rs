//! The upgrade history, the `serve` heartbeats, and the upgrade lock (spec §3, §14.5,
//! D50). `maverick upgrade` records one `upgrades` row per run; every `serve` process
//! refreshes its `serve_heartbeats` row, so an upgrade can see which releases still run.

use chrono::{DateTime, TimeDelta, Utc};
use sqlx::{AssertSqlSafe, PgConnection, PgExecutor};

/// The Postgres advisory lock key that serializes `maverick upgrade` runs.
pub const UPGRADE_LOCK_KEY: i64 = 5_570_194_218_848_332;

/// A `serve` heartbeat older than this many seconds counts as a stopped process.
pub const HEARTBEAT_STALE_AFTER_SECS: i64 = 120;

/// How often a `serve` process refreshes its heartbeat, in seconds.
pub const HEARTBEAT_EVERY_SECS: u64 = 30;

/// One `upgrades` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upgrade {
    pub id: String,
    pub from_release: Option<String>,
    pub to_release: String,
    pub operator: String,
    pub migrations: Vec<String>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub verify_failed: Option<String>,
}

/// One `serve_heartbeats` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heartbeat {
    pub process_id: String,
    pub release: String,
    pub host_label: String,
    pub last_seen_at: DateTime<Utc>,
}

/// A new `upg_<ulid>` id.
#[must_use]
pub fn new_upgrade_id() -> String {
    maverick_domain::ids::new("upg")
}

/// A new `srv_<ulid>` heartbeat id, one per `serve` process start.
#[must_use]
pub fn new_process_id() -> String {
    maverick_domain::ids::new("srv")
}

/// Records the start of an upgrade run.
pub async fn start_upgrade<'e>(
    db: impl PgExecutor<'e>,
    id: &str,
    from_release: Option<&str>,
    to_release: &str,
    operator: &str,
    started_at: DateTime<Utc>,
) -> sqlx::Result<()> {
    sqlx::query!(
        "insert into upgrades (id, from_release, to_release, operator, started_at)
         values ($1, $2, $3, $4, $5)",
        id,
        from_release,
        to_release,
        operator,
        started_at
    )
    .execute(db)
    .await?;
    Ok(())
}

/// Records the end of an upgrade run: what it applied, and a failed verify check or table.
pub async fn finish_upgrade<'e>(
    db: impl PgExecutor<'e>,
    id: &str,
    migrations: &[String],
    finished_at: DateTime<Utc>,
    verify_failed: Option<&str>,
) -> sqlx::Result<()> {
    sqlx::query!(
        "update upgrades set migrations = $2, finished_at = $3, verify_failed = $4
         where id = $1",
        id,
        migrations,
        finished_at,
        verify_failed
    )
    .execute(db)
    .await?;
    Ok(())
}

/// The newest upgrade run, if any.
pub async fn last_upgrade<'e>(db: impl PgExecutor<'e>) -> sqlx::Result<Option<Upgrade>> {
    sqlx::query_as!(
        Upgrade,
        "select id, from_release, to_release, operator, migrations, started_at,
                finished_at, verify_failed
         from upgrades order by started_at desc, id desc limit 1"
    )
    .fetch_optional(db)
    .await
}

/// Writes or refreshes this process's heartbeat.
pub async fn beat<'e>(
    db: impl PgExecutor<'e>,
    process_id: &str,
    release: &str,
    host_label: &str,
    now: DateTime<Utc>,
) -> sqlx::Result<()> {
    sqlx::query!(
        "insert into serve_heartbeats (process_id, release, host_label, last_seen_at)
         values ($1, $2, $3, $4)
         on conflict (process_id) do update
           set release = excluded.release, host_label = excluded.host_label,
               last_seen_at = excluded.last_seen_at",
        process_id,
        release,
        host_label,
        now
    )
    .execute(db)
    .await?;
    Ok(())
}

/// Removes this process's heartbeat on a clean shutdown.
pub async fn stop_beating<'e>(db: impl PgExecutor<'e>, process_id: &str) -> sqlx::Result<()> {
    sqlx::query!(
        "delete from serve_heartbeats where process_id = $1",
        process_id
    )
    .execute(db)
    .await?;
    Ok(())
}

/// The heartbeats seen within [`HEARTBEAT_STALE_AFTER_SECS`] of `now`: the `serve`
/// processes that still run.
pub async fn live_heartbeats<'e>(
    db: impl PgExecutor<'e>,
    now: DateTime<Utc>,
) -> sqlx::Result<Vec<Heartbeat>> {
    let since = now - TimeDelta::seconds(HEARTBEAT_STALE_AFTER_SECS);
    sqlx::query_as!(
        Heartbeat,
        "select process_id, release, host_label, last_seen_at
         from serve_heartbeats where last_seen_at > $1 order by process_id",
        since
    )
    .fetch_all(db)
    .await
}

/// Takes the upgrade lock on `conn` without waiting. `false` means another run holds it.
/// It is a session lock: it lasts until [`release_upgrade_lock`] or the connection closes.
pub async fn try_upgrade_lock(conn: &mut PgConnection) -> sqlx::Result<bool> {
    sqlx::query_scalar("select pg_try_advisory_lock($1)")
        .bind(UPGRADE_LOCK_KEY)
        .fetch_one(conn)
        .await
}

/// Releases the upgrade lock taken on `conn`.
pub async fn release_upgrade_lock(conn: &mut PgConnection) -> sqlx::Result<()> {
    sqlx::query("select pg_advisory_unlock($1)")
        .bind(UPGRADE_LOCK_KEY)
        .execute(conn)
        .await?;
    Ok(())
}

/// Reads one row, every column, from `table`. The names come from the migrations'
/// declared tables, never from input; they are quoted anyway.
pub async fn read_one_row(conn: &mut PgConnection, table: &str) -> sqlx::Result<()> {
    let quoted = format!("\"{}\"", table.replace('"', "\"\""));
    sqlx::query(AssertSqlSafe(format!("select * from {quoted} limit 1")))
        .fetch_optional(conn)
        .await?;
    Ok(())
}
