//! Active public bundle metadata (spec §14.4, D49).

use chrono::{DateTime, Utc};
use sqlx::PgExecutor;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveBundle {
    pub matrix_version: String,
    pub published_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

/// No active bundle is a valid cold start on a replica.
pub async fn active_bundle<'e>(db: impl PgExecutor<'e>) -> sqlx::Result<Option<ActiveBundle>> {
    sqlx::query_as!(
        ActiveBundle,
        "select matrix_version, published_at, created_at from matrix_imports where active"
    )
    .fetch_optional(db)
    .await
}
