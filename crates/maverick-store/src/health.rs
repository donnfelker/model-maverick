//! Job queue health reads for `GET /health/detail` (spec §9.9, D35).
//!
//! Go read River's tables with two hand-written queries (reference Part 7, "Raw SQL
//! outside sqlc"). Rust reads graphile_worker's worker registry instead. That schema
//! belongs to another library and is created by its own migrations, so this query is
//! checked at run time, not by the sqlx macros. The last batch times come from
//! [`crate::batches::list_last_completed_batches`].

use sqlx::{AssertSqlSafe, PgExecutor};

/// The default graphile_worker schema.
pub const GRAPHILE_WORKER_SCHEMA: &str = "graphile_worker";

/// A registered graphile_worker worker and the age of its last heartbeat.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct WorkerHeartbeat {
    pub id: String,
    /// Seconds since the last heartbeat, by the database clock.
    pub age_seconds: f64,
}

/// Every registered worker, ordered by id. An empty list means no worker is running.
/// Workers deregister on clean shutdown; a crashed worker stays until graphile_worker
/// sweeps it, with a growing heartbeat age.
pub async fn list_worker_heartbeats<'e>(
    db: impl PgExecutor<'e>,
    schema: &str,
) -> sqlx::Result<Vec<WorkerHeartbeat>> {
    let sql = format!(
        "select id, extract(epoch from (now() - last_heartbeat_at))::float8 as age_seconds
         from {}._private_workers
         order by id",
        quote_identifier(schema)
    );
    sqlx::query_as::<_, WorkerHeartbeat>(AssertSqlSafe(sql))
        .fetch_all(db)
        .await
}

/// Quotes a Postgres identifier: wraps it in double quotes and doubles any inside.
fn quote_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::quote_identifier;

    #[test]
    fn identifiers_are_quoted() {
        assert_eq!(quote_identifier("graphile_worker"), "\"graphile_worker\"");
        assert_eq!(quote_identifier("a\"b"), "\"a\"\"b\"");
    }
}
