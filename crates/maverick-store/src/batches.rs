//! The `batches` lease: engine-owned uniqueness per job kind and window (spec §11.2, D10).
//!
//! New in the Rust port. The Go engine relied on River unique jobs. Lease times use the
//! database clock, so every worker agrees on expiry.

use chrono::{DateTime, Utc};
use sqlx::{Connection, PgConnection, PgExecutor};

/// A `batches` row.
#[derive(Debug, Clone, PartialEq)]
pub struct Batch {
    pub id: String,
    pub kind: String,
    pub batch_window: String,
    pub status: String,
    pub lease_owner: Option<String>,
    pub lease_expires_at: Option<DateTime<Utc>>,
    pub heartbeat_at: Option<DateTime<Utc>>,
    pub attempt: i32,
    pub completed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// What a handler asks for when it starts.
#[derive(Debug, Clone, Copy)]
pub struct BatchClaim<'a> {
    /// The deterministic batch id for (kind, window). Used only when the row is new.
    pub id: &'a str,
    pub kind: &'a str,
    pub batch_window: &'a str,
    /// The queue worker id of the caller.
    pub worker_id: &'a str,
    /// How long the lease lives without a heartbeat, in seconds.
    pub lease_ttl_secs: f64,
}

/// The result of [`claim_batch`].
#[derive(Debug, Clone, PartialEq)]
pub enum ClaimOutcome {
    /// The row was new. The caller holds the lease, attempt 1.
    Claimed(Batch),
    /// The row was running with an expired lease, no owner, the caller as owner, or an
    /// owner that is gone. The caller holds the lease and resumes the batch under the
    /// same id; `attempt` was incremented.
    TookOver(Batch),
    /// The row is `completed` or `skipped`. The handler completes as a no-op.
    Finished(Batch),
    /// Another live worker holds an unexpired lease. The handler completes as a no-op.
    HeldByOther(Batch),
}

impl ClaimOutcome {
    /// The row after the claim.
    pub fn batch(&self) -> &Batch {
        match self {
            Self::Claimed(batch)
            | Self::TookOver(batch)
            | Self::Finished(batch)
            | Self::HeldByOther(batch) => batch,
        }
    }

    /// True when the caller now holds the lease and must run the batch.
    pub fn holds_lease(&self) -> bool {
        matches!(self, Self::Claimed(_) | Self::TookOver(_))
    }
}

/// Claims the lease on (kind, window) in one transaction.
///
/// The row is inserted when absent. Otherwise it is locked with `FOR UPDATE`, so
/// concurrent claimers are serialized and exactly one of them holds the lease.
/// `owner_is_gone` is asked only about a running row whose lease is live and held by
/// another worker. It returns true when the queue recovered that worker's job, which
/// allows a takeover before `lease_expires_at` (spec §11.2).
pub async fn claim_batch(
    conn: &mut PgConnection,
    claim: &BatchClaim<'_>,
    owner_is_gone: impl FnOnce(&Batch) -> bool,
) -> sqlx::Result<ClaimOutcome> {
    let mut tx = conn.begin().await?;
    let inserted = sqlx::query_as!(
        Batch,
        "insert into batches
           (id, kind, batch_window, status, lease_owner, lease_expires_at, heartbeat_at, attempt)
         values ($1, $2, $3, 'running', $4, now() + make_interval(secs => $5), now(), 1)
         on conflict (kind, batch_window) do nothing
         returning id, kind, batch_window, status, lease_owner, lease_expires_at,
                   heartbeat_at, attempt, completed_at, created_at, updated_at",
        claim.id,
        claim.kind,
        claim.batch_window,
        claim.worker_id,
        claim.lease_ttl_secs
    )
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(batch) = inserted {
        tx.commit().await?;
        return Ok(ClaimOutcome::Claimed(batch));
    }

    let row = sqlx::query_as!(
        Batch,
        "select id, kind, batch_window, status, lease_owner, lease_expires_at, heartbeat_at,
                attempt, completed_at, created_at, updated_at
         from batches
         where kind = $1 and batch_window = $2
         for update",
        claim.kind,
        claim.batch_window
    )
    .fetch_one(&mut *tx)
    .await?;
    let now = sqlx::query_scalar!(r#"select now() as "now!""#)
        .fetch_one(&mut *tx)
        .await?;
    let expired = row.lease_expires_at.is_none_or(|at| at <= now);

    let outcome = if row.status != "running" {
        ClaimOutcome::Finished(row)
    } else if expired
        || row
            .lease_owner
            .as_deref()
            .is_none_or(|owner| owner == claim.worker_id)
        || owner_is_gone(&row)
    {
        ClaimOutcome::TookOver(take_over(&mut tx, &row.id, claim).await?)
    } else {
        ClaimOutcome::HeldByOther(row)
    };
    tx.commit().await?;
    Ok(outcome)
}

async fn take_over(
    conn: &mut PgConnection,
    id: &str,
    claim: &BatchClaim<'_>,
) -> sqlx::Result<Batch> {
    sqlx::query_as!(
        Batch,
        "update batches
         set lease_owner = $2,
             lease_expires_at = now() + make_interval(secs => $3),
             heartbeat_at = now(),
             attempt = attempt + 1,
             updated_at = now()
         where id = $1
         returning id, kind, batch_window, status, lease_owner, lease_expires_at,
                   heartbeat_at, attempt, completed_at, created_at, updated_at",
        id,
        claim.worker_id,
        claim.lease_ttl_secs
    )
    .fetch_one(conn)
    .await
}

/// Renews the lease. Returns `None` when the caller no longer holds it (another worker
/// took over, or the batch finished).
pub async fn heartbeat_batch<'e>(
    db: impl PgExecutor<'e>,
    id: &str,
    worker_id: &str,
    lease_ttl_secs: f64,
) -> sqlx::Result<Option<Batch>> {
    sqlx::query_as!(
        Batch,
        "update batches
         set lease_expires_at = now() + make_interval(secs => $3),
             heartbeat_at = now(),
             updated_at = now()
         where id = $1 and lease_owner = $2 and status = 'running'
         returning id, kind, batch_window, status, lease_owner, lease_expires_at,
                   heartbeat_at, attempt, completed_at, created_at, updated_at",
        id,
        worker_id,
        lease_ttl_secs
    )
    .fetch_optional(db)
    .await
}

/// The terminal state of a batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchEnd {
    Completed,
    /// A spend refusal records the window as skipped (spec §11).
    Skipped,
}

impl BatchEnd {
    fn status(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Skipped => "skipped",
        }
    }
}

/// Ends a running batch that the caller holds: sets the status and `completed_at`, and
/// clears the lease. Returns `None` when the caller does not hold the lease.
pub async fn finish_batch<'e>(
    db: impl PgExecutor<'e>,
    id: &str,
    worker_id: &str,
    end: BatchEnd,
) -> sqlx::Result<Option<Batch>> {
    sqlx::query_as!(
        Batch,
        "update batches
         set status = $3,
             completed_at = now(),
             lease_owner = null,
             lease_expires_at = null,
             updated_at = now()
         where id = $1 and lease_owner = $2 and status = 'running'
         returning id, kind, batch_window, status, lease_owner, lease_expires_at,
                   heartbeat_at, attempt, completed_at, created_at, updated_at",
        id,
        worker_id,
        end.status()
    )
    .fetch_optional(db)
    .await
}

/// The batch of (kind, window).
pub async fn get_batch<'e>(
    db: impl PgExecutor<'e>,
    kind: &str,
    batch_window: &str,
) -> sqlx::Result<Batch> {
    sqlx::query_as!(
        Batch,
        "select id, kind, batch_window, status, lease_owner, lease_expires_at, heartbeat_at,
                attempt, completed_at, created_at, updated_at
         from batches
         where kind = $1 and batch_window = $2",
        kind,
        batch_window
    )
    .fetch_one(db)
    .await
}

/// The batch with this id.
pub async fn get_batch_by_id<'e>(db: impl PgExecutor<'e>, id: &str) -> sqlx::Result<Batch> {
    sqlx::query_as!(
        Batch,
        "select id, kind, batch_window, status, lease_owner, lease_expires_at, heartbeat_at,
                attempt, completed_at, created_at, updated_at
         from batches
         where id = $1",
        id
    )
    .fetch_one(db)
    .await
}

/// The newest `completed_at` of a completed batch, per kind, ordered by kind. The health
/// component reads it (D35).
pub async fn list_last_completed_batches<'e>(
    db: impl PgExecutor<'e>,
) -> sqlx::Result<Vec<(String, DateTime<Utc>)>> {
    let rows = sqlx::query!(
        r#"select kind, max(completed_at) as "completed_at!"
           from batches
           where status = 'completed' and completed_at is not null
           group by kind
           order by kind"#
    )
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.kind, row.completed_at))
        .collect())
}
