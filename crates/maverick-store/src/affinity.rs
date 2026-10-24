//! Conversation affinity rows: the last pick per conversation (spec §3, §7.6, D44).
//!
//! New in the Rust port. The Go engine ranks each request alone. A row is keyed by the
//! tenant and the affinity hash (`maverick_domain::capture::affinity_hash`), never the
//! caller's key. Times come from the caller's clock, so a row's liveness matches the
//! TTL that the caller computed.

use chrono::{DateTime, Utc};
use sqlx::PgExecutor;

/// A `conversation_affinity` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AffinityRow {
    pub tenant_id: String,
    pub affinity_hash: String,
    pub column_id: String,
    pub recommendation_id: String,
    pub last_used_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

/// The row for the tenant and hash when it is live at `now` (`expires_at > now`). An
/// expired row reads as missing.
pub async fn get_affinity<'e>(
    db: impl PgExecutor<'e>,
    tenant_id: &str,
    affinity_hash: &str,
    now: DateTime<Utc>,
) -> sqlx::Result<Option<AffinityRow>> {
    sqlx::query_as!(
        AffinityRow,
        "select tenant_id, affinity_hash, column_id, recommendation_id, last_used_at,
                expires_at
         from conversation_affinity
         where tenant_id = $1 and affinity_hash = $2 and expires_at > $3",
        tenant_id,
        affinity_hash,
        now
    )
    .fetch_optional(db)
    .await
}

/// Writes the row after a successful recommend: the pick, this recommendation,
/// `last_used_at`, and `expires_at`. The last write wins, also over an expired row. Any
/// executor works, so it runs inside the recommend transaction.
pub async fn upsert_affinity<'e>(db: impl PgExecutor<'e>, row: &AffinityRow) -> sqlx::Result<()> {
    sqlx::query!(
        "insert into conversation_affinity
           (tenant_id, affinity_hash, column_id, recommendation_id, last_used_at, expires_at)
         values ($1, $2, $3, $4, $5, $6)
         on conflict (tenant_id, affinity_hash) do update set
           column_id = excluded.column_id,
           recommendation_id = excluded.recommendation_id,
           last_used_at = excluded.last_used_at,
           expires_at = excluded.expires_at",
        row.tenant_id,
        row.affinity_hash,
        row.column_id,
        row.recommendation_id,
        row.last_used_at,
        row.expires_at
    )
    .execute(db)
    .await
    .map(|_| ())
}

/// Moves the row to the chain entry that served the turn (spec §9.10). Compare and set:
/// the row changes only while its `recommendation_id` is still `recommendation_id`, so a
/// later turn is never overwritten. Returns whether a row changed.
pub async fn follow_served_column<'e>(
    db: impl PgExecutor<'e>,
    tenant_id: &str,
    affinity_hash: &str,
    recommendation_id: &str,
    column_id: &str,
) -> sqlx::Result<bool> {
    sqlx::query!(
        "update conversation_affinity
         set column_id = $4
         where tenant_id = $1 and affinity_hash = $2 and recommendation_id = $3",
        tenant_id,
        affinity_hash,
        recommendation_id,
        column_id
    )
    .execute(db)
    .await
    .map(|result| result.rows_affected() == 1)
}

/// Deletes every row whose `expires_at` is before `now`, for the hourly `affinity_prune`
/// job. Returns the number of rows deleted.
pub async fn prune_affinity<'e>(db: impl PgExecutor<'e>, now: DateTime<Utc>) -> sqlx::Result<u64> {
    sqlx::query!(
        "delete from conversation_affinity where expires_at < $1",
        now
    )
    .execute(db)
    .await
    .map(|result| result.rows_affected())
}
