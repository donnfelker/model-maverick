//! Debug capture: windows, encrypted captures, and the access log (spec §9.8, §10.4).
//!
//! Go source: `parity/schema/queries/capture.sql`. Rust adds `capture_key_version` (D18).
//! The page queries never select `ciphertext` or `nonce`, so no prompt-bearing column can
//! leak through them.

use chrono::{DateTime, Utc};
use sqlx::PgExecutor;

use crate::Json;

/// A `capture_windows` row.
#[derive(Debug, Clone, PartialEq)]
pub struct CaptureWindow {
    pub id: String,
    pub tenant_id: String,
    pub api_key_id: Option<String>,
    pub confidence_below: Option<f32>,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

/// A `debug_captures` row.
#[derive(Debug, Clone, PartialEq)]
pub struct DebugCapture {
    pub id: String,
    pub tenant_id: String,
    pub recommendation_id: String,
    pub ciphertext: Vec<u8>,
    pub nonce: Vec<u8>,
    pub captured_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub window_id: Option<String>,
    /// The master-key ring version that encrypted the capture (D18).
    pub capture_key_version: i32,
}

/// The values of `CreateDebugCapture`.
#[derive(Debug, Clone, Copy)]
pub struct NewDebugCapture<'a> {
    pub id: &'a str,
    pub tenant_id: &'a str,
    pub window_id: Option<&'a str>,
    pub recommendation_id: &'a str,
    pub ciphertext: &'a [u8],
    pub nonce: &'a [u8],
    pub capture_key_version: i32,
    pub expires_at: DateTime<Utc>,
}

/// A row of the operator capture list: no ciphertext, no nonce.
#[derive(Debug, Clone)]
pub struct CaptureSummary {
    pub id: String,
    pub tenant_id: String,
    pub window_id: Option<String>,
    pub recommendation_id: String,
    pub captured_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub prompt_hash: Option<String>,
    pub classification: Json,
}

/// A row of the tenant capture list: [`CaptureSummary`] without the tenant.
#[derive(Debug, Clone)]
pub struct TenantCaptureSummary {
    pub id: String,
    pub window_id: Option<String>,
    pub recommendation_id: String,
    pub captured_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub prompt_hash: Option<String>,
    pub classification: Json,
}

/// The filters of `ListDebugCapturesPage` (operator, no tenant scope).
#[derive(Debug, Clone, Copy)]
pub struct CapturePageQuery<'a> {
    /// Empty means every window.
    pub window_id: &'a str,
    /// Empty means every tenant.
    pub tenant_id: &'a str,
    /// Empty is the first page.
    pub cursor_id: &'a str,
    pub page_limit: i32,
}

/// The filters of `ListDebugCapturesPageForTenant`. The tenant scope is unconditional.
#[derive(Debug, Clone, Copy)]
pub struct TenantCapturePageQuery<'a> {
    pub tenant_id: &'a str,
    /// Empty means every window.
    pub window_id: &'a str,
    /// Empty is the first page.
    pub cursor_id: &'a str,
    pub page_limit: i32,
}

/// The identity of a hard-deleted capture, for its `delete` access-log row.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DeletedCapture {
    pub id: String,
    pub tenant_id: String,
}

/// A `capture_access_log` row. It has no FK and survives the capture's deletion.
#[derive(Debug, Clone, PartialEq)]
pub struct CaptureAccessLog {
    pub id: String,
    pub capture_id: String,
    pub tenant_id: String,
    pub actor: String,
    pub action: String,
    pub created_at: DateTime<Utc>,
}

/// `CreateCaptureWindow`.
pub async fn create_capture_window<'e>(
    db: impl PgExecutor<'e>,
    id: &str,
    tenant_id: &str,
    api_key_id: Option<&str>,
    confidence_below: Option<f32>,
    starts_at: DateTime<Utc>,
    ends_at: DateTime<Utc>,
) -> sqlx::Result<CaptureWindow> {
    sqlx::query_as!(
        CaptureWindow,
        "insert into capture_windows (id, tenant_id, api_key_id, confidence_below, starts_at, ends_at)
         values ($1, $2, $3, $4, $5, $6)
         returning id, tenant_id, api_key_id, confidence_below, starts_at, ends_at, created_at",
        id,
        tenant_id,
        api_key_id,
        confidence_below,
        starts_at,
        ends_at
    )
    .fetch_one(db)
    .await
}

/// `DeleteCaptureWindowForTenant`: 0 for an unknown window and for another tenant's.
pub async fn delete_capture_window_for_tenant<'e>(
    db: impl PgExecutor<'e>,
    id: &str,
    tenant_id: &str,
) -> sqlx::Result<u64> {
    sqlx::query!(
        "delete from capture_windows where id = $1 and tenant_id = $2",
        id,
        tenant_id
    )
    .execute(db)
    .await
    .map(|done| done.rows_affected())
}

/// `ListActiveCaptureWindows`: windows with `starts_at <= at < ends_at`. The order is
/// unspecified, as in Go.
pub async fn list_active_capture_windows<'e>(
    db: impl PgExecutor<'e>,
    tenant_id: &str,
    at: DateTime<Utc>,
) -> sqlx::Result<Vec<CaptureWindow>> {
    sqlx::query_as!(
        CaptureWindow,
        "select id, tenant_id, api_key_id, confidence_below, starts_at, ends_at, created_at
         from capture_windows
         where tenant_id = $1
           and starts_at <= $2
           and ends_at > $2",
        tenant_id,
        at
    )
    .fetch_all(db)
    .await
}

/// `CreateDebugCapture`, with the key version (D18).
pub async fn create_debug_capture<'e>(
    db: impl PgExecutor<'e>,
    capture: &NewDebugCapture<'_>,
) -> sqlx::Result<DebugCapture> {
    sqlx::query_as!(
        DebugCapture,
        "insert into debug_captures
           (id, tenant_id, window_id, recommendation_id, ciphertext, nonce, expires_at,
            capture_key_version)
         values ($1, $2, $3, $4, $5, $6, $7, $8)
         returning id, tenant_id, recommendation_id, ciphertext, nonce, captured_at,
                   expires_at, window_id, capture_key_version",
        capture.id,
        capture.tenant_id,
        capture.window_id,
        capture.recommendation_id,
        capture.ciphertext,
        capture.nonce,
        capture.expires_at,
        capture.capture_key_version,
    )
    .fetch_one(db)
    .await
}

/// `GetDebugCaptureForTenant`.
pub async fn get_debug_capture_for_tenant<'e>(
    db: impl PgExecutor<'e>,
    id: &str,
    tenant_id: &str,
) -> sqlx::Result<DebugCapture> {
    sqlx::query_as!(
        DebugCapture,
        "select id, tenant_id, recommendation_id, ciphertext, nonce, captured_at, expires_at,
                window_id, capture_key_version
         from debug_captures
         where id = $1 and tenant_id = $2",
        id,
        tenant_id
    )
    .fetch_one(db)
    .await
}

/// `GetDebugCaptureByID`: the operator read, with no tenant scope.
pub async fn get_debug_capture_by_id<'e>(
    db: impl PgExecutor<'e>,
    id: &str,
) -> sqlx::Result<DebugCapture> {
    sqlx::query_as!(
        DebugCapture,
        "select id, tenant_id, recommendation_id, ciphertext, nonce, captured_at, expires_at,
                window_id, capture_key_version
         from debug_captures
         where id = $1",
        id
    )
    .fetch_one(db)
    .await
}

/// `ListDebugCapturesByTenant`: ordered by id.
pub async fn list_debug_captures_by_tenant<'e>(
    db: impl PgExecutor<'e>,
    tenant_id: &str,
) -> sqlx::Result<Vec<DebugCapture>> {
    sqlx::query_as!(
        DebugCapture,
        "select id, tenant_id, recommendation_id, ciphertext, nonce, captured_at, expires_at,
                window_id, capture_key_version
         from debug_captures
         where tenant_id = $1
         order by id",
        tenant_id
    )
    .fetch_all(db)
    .await
}

/// `ListDebugCapturesPage`: every tenant's captures, newest first, with the
/// recommendation's prompt hash and classification.
pub async fn list_debug_captures_page<'e>(
    db: impl PgExecutor<'e>,
    query: &CapturePageQuery<'_>,
) -> sqlx::Result<Vec<CaptureSummary>> {
    sqlx::query_as!(
        CaptureSummary,
        r#"select d.id, d.tenant_id, d.window_id, d.recommendation_id, d.captured_at,
                  d.expires_at, r.prompt_hash, r.classification as "classification: _"
           from debug_captures d
           join recommendations r on r.id = d.recommendation_id
           where ($1::text = '' or d.window_id = $1)
             and ($2::text = '' or d.tenant_id = $2)
             and ($3::text = '' or d.id < $3)
           order by d.id desc
           limit $4"#,
        query.window_id,
        query.tenant_id,
        query.cursor_id,
        i64::from(query.page_limit),
    )
    .fetch_all(db)
    .await
}

/// `ListDebugCapturesPageForTenant`: the tenant's own captures, newest first.
pub async fn list_debug_captures_page_for_tenant<'e>(
    db: impl PgExecutor<'e>,
    query: &TenantCapturePageQuery<'_>,
) -> sqlx::Result<Vec<TenantCaptureSummary>> {
    sqlx::query_as!(
        TenantCaptureSummary,
        r#"select d.id, d.window_id, d.recommendation_id, d.captured_at, d.expires_at,
                  r.prompt_hash, r.classification as "classification: _"
           from debug_captures d
           join recommendations r on r.id = d.recommendation_id
           where d.tenant_id = $1
             and ($2::text = '' or d.window_id = $2)
             and ($3::text = '' or d.id < $3)
           order by d.id desc
           limit $4"#,
        query.tenant_id,
        query.window_id,
        query.cursor_id,
        i64::from(query.page_limit),
    )
    .fetch_all(db)
    .await
}

/// `DeleteExpiredDebugCaptures`: hard-deletes captures with `expires_at <= cutoff` and
/// returns their identities, in unspecified order, for the `delete` log rows.
pub async fn delete_expired_debug_captures<'e>(
    db: impl PgExecutor<'e>,
    cutoff: DateTime<Utc>,
) -> sqlx::Result<Vec<DeletedCapture>> {
    sqlx::query_as!(
        DeletedCapture,
        "delete from debug_captures
         where expires_at <= $1
         returning id, tenant_id",
        cutoff
    )
    .fetch_all(db)
    .await
}

/// `DeleteExpiredCaptureWindows`: deletes windows with `ends_at <= cutoff`.
pub async fn delete_expired_capture_windows<'e>(
    db: impl PgExecutor<'e>,
    cutoff: DateTime<Utc>,
) -> sqlx::Result<u64> {
    sqlx::query!("delete from capture_windows where ends_at <= $1", cutoff)
        .execute(db)
        .await
        .map(|done| done.rows_affected())
}

/// `DeleteCapturesForRecommendationsBefore`: hard-deletes the captures of
/// recommendations created before the cutoff and returns their identities.
pub async fn delete_captures_for_recommendations_before<'e>(
    db: impl PgExecutor<'e>,
    cutoff: DateTime<Utc>,
) -> sqlx::Result<Vec<DeletedCapture>> {
    sqlx::query_as!(
        DeletedCapture,
        "delete from debug_captures d
         using recommendations r
         where d.recommendation_id = r.id
           and r.created_at < $1
         returning d.id, d.tenant_id",
        cutoff
    )
    .fetch_all(db)
    .await
}

/// `InsertCaptureAccessLog`. `action` is `replay`, `read`, or `delete`.
pub async fn insert_capture_access_log<'e>(
    db: impl PgExecutor<'e>,
    id: &str,
    capture_id: &str,
    tenant_id: &str,
    actor: &str,
    action: &str,
) -> sqlx::Result<CaptureAccessLog> {
    sqlx::query_as!(
        CaptureAccessLog,
        "insert into capture_access_log (id, capture_id, tenant_id, actor, action)
         values ($1, $2, $3, $4, $5)
         returning id, capture_id, tenant_id, actor, action, created_at",
        id,
        capture_id,
        tenant_id,
        actor,
        action
    )
    .fetch_one(db)
    .await
}

/// `ListCaptureAccessLogByCapture`: ordered by id.
pub async fn list_capture_access_log_by_capture<'e>(
    db: impl PgExecutor<'e>,
    capture_id: &str,
    tenant_id: &str,
) -> sqlx::Result<Vec<CaptureAccessLog>> {
    sqlx::query_as!(
        CaptureAccessLog,
        "select id, capture_id, tenant_id, actor, action, created_at
         from capture_access_log
         where capture_id = $1 and tenant_id = $2
         order by id",
        capture_id,
        tenant_id
    )
    .fetch_all(db)
    .await
}

/// `CaptureWasDeleted`: true when a `delete` log row exists. It tells a hard-deleted
/// capture (410) from one that never existed (404).
pub async fn capture_was_deleted<'e>(
    db: impl PgExecutor<'e>,
    capture_id: &str,
    tenant_id: &str,
) -> sqlx::Result<bool> {
    sqlx::query_scalar!(
        r#"select exists (
             select 1 from capture_access_log
             where capture_id = $1 and tenant_id = $2 and action = 'delete'
           ) as "deleted!""#,
        capture_id,
        tenant_id
    )
    .fetch_one(db)
    .await
}

/// `FindCaptureDeleteLog`: the operator path's `delete` row, which also names the tenant
/// the capture belonged to. No row is [`sqlx::Error::RowNotFound`].
pub async fn find_capture_delete_log<'e>(
    db: impl PgExecutor<'e>,
    capture_id: &str,
) -> sqlx::Result<CaptureAccessLog> {
    sqlx::query_as!(
        CaptureAccessLog,
        "select id, capture_id, tenant_id, actor, action, created_at
         from capture_access_log
         where capture_id = $1 and action = 'delete'
         limit 1",
        capture_id
    )
    .fetch_one(db)
    .await
}
