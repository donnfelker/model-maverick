//! Tenants, API keys, pools, weights, and standing constraints.
//!
//! Go sources: `parity/schema/queries/tenants.sql`, `api_keys.sql`, `pools.sql`,
//! `tenant_weights.sql`, and `tenant_constraints.sql`.

use chrono::{DateTime, Utc};
use sqlx::PgExecutor;

use crate::Decimal;

/// A `tenants` row.
#[derive(Debug, Clone)]
pub struct Tenant {
    pub id: String,
    pub name: String,
    pub status: String,
    pub prompt_hmac_key: Vec<u8>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Daily classifier quota override (spec §8.3, D4). `None` means the config default.
    pub classification_quota_override: Option<i32>,
}

/// An `api_keys` row.
#[derive(Debug, Clone)]
pub struct ApiKey {
    pub id: String,
    pub tenant_id: String,
    pub key_hash: Vec<u8>,
    pub last4: String,
    pub name: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
}

/// A `pools` row.
#[derive(Debug, Clone)]
pub struct Pool {
    pub id: String,
    pub tenant_id: String,
    pub name: String,
    pub model_column_ids: Vec<String>,
    pub is_default: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A `tenant_weights` row. The weights are `real`, so they are `f32`.
#[derive(Debug, Clone)]
pub struct TenantWeight {
    pub tenant_id: String,
    pub capability: String,
    pub w_quality: f32,
    pub w_cost: f32,
    pub w_latency: f32,
    pub quality_bar: f32,
}

/// A `tenant_constraints` row.
#[derive(Debug, Clone)]
pub struct TenantConstraint {
    pub tenant_id: String,
    pub max_cost_per_run: Option<Decimal>,
    pub max_latency_ms: Option<i32>,
    pub zero_retention: bool,
    pub updated_at: DateTime<Utc>,
    /// Tenant telemetry opt-out (D39).
    pub telemetry_opt_out: bool,
}

// ---- tenants.sql ----

/// `CreateTenant`: status defaults to `active`.
pub async fn create_tenant<'e>(
    db: impl PgExecutor<'e>,
    id: &str,
    name: &str,
    prompt_hmac_key: &[u8],
) -> sqlx::Result<Tenant> {
    sqlx::query_as!(
        Tenant,
        "insert into tenants (id, name, prompt_hmac_key)
         values ($1, $2, $3)
         returning id, name, status, prompt_hmac_key, created_at, updated_at,
                   classification_quota_override",
        id,
        name,
        prompt_hmac_key
    )
    .fetch_one(db)
    .await
}

/// `GetTenant`.
pub async fn get_tenant<'e>(db: impl PgExecutor<'e>, id: &str) -> sqlx::Result<Tenant> {
    sqlx::query_as!(
        Tenant,
        "select id, name, status, prompt_hmac_key, created_at, updated_at,
                classification_quota_override
         from tenants
         where id = $1",
        id
    )
    .fetch_one(db)
    .await
}

/// `ListTenants`: ordered by id.
pub async fn list_tenants<'e>(db: impl PgExecutor<'e>) -> sqlx::Result<Vec<Tenant>> {
    sqlx::query_as!(
        Tenant,
        "select id, name, status, prompt_hmac_key, created_at, updated_at,
                classification_quota_override
         from tenants
         order by id"
    )
    .fetch_all(db)
    .await
}

/// `UpdateTenant`: sets name and status, and `updated_at = now()`.
pub async fn update_tenant<'e>(
    db: impl PgExecutor<'e>,
    id: &str,
    name: &str,
    status: &str,
) -> sqlx::Result<Tenant> {
    sqlx::query_as!(
        Tenant,
        "update tenants
         set name = $2,
             status = $3,
             updated_at = now()
         where id = $1
         returning id, name, status, prompt_hmac_key, created_at, updated_at,
                   classification_quota_override",
        id,
        name,
        status
    )
    .fetch_one(db)
    .await
}

/// `DeleteTenant`. Fails with SQLSTATE 23503 while any row references the tenant.
pub async fn delete_tenant<'e>(db: impl PgExecutor<'e>, id: &str) -> sqlx::Result<()> {
    sqlx::query!("delete from tenants where id = $1", id)
        .execute(db)
        .await
        .map(|_| ())
}

/// Sets or clears (`None`) the daily classifier quota override (spec §13
/// `PUT /internal/v1/tenants/{id}/limits`, D4). Rust only.
pub async fn set_tenant_classification_quota<'e>(
    db: impl PgExecutor<'e>,
    id: &str,
    quota: Option<i32>,
) -> sqlx::Result<Tenant> {
    sqlx::query_as!(
        Tenant,
        "update tenants
         set classification_quota_override = $2,
             updated_at = now()
         where id = $1
         returning id, name, status, prompt_hmac_key, created_at, updated_at,
                   classification_quota_override",
        id,
        quota
    )
    .fetch_one(db)
    .await
}

// ---- api_keys.sql ----

/// `CreateAPIKey`: status defaults to `active`.
pub async fn create_api_key<'e>(
    db: impl PgExecutor<'e>,
    id: &str,
    tenant_id: &str,
    key_hash: &[u8],
    last4: &str,
    name: &str,
) -> sqlx::Result<ApiKey> {
    sqlx::query_as!(
        ApiKey,
        "insert into api_keys (id, tenant_id, key_hash, last4, name)
         values ($1, $2, $3, $4, $5)
         returning id, tenant_id, key_hash, last4, name, status, created_at, revoked_at",
        id,
        tenant_id,
        key_hash,
        last4,
        name
    )
    .fetch_one(db)
    .await
}

/// `GetAPIKey`.
pub async fn get_api_key<'e>(db: impl PgExecutor<'e>, id: &str) -> sqlx::Result<ApiKey> {
    sqlx::query_as!(
        ApiKey,
        "select id, tenant_id, key_hash, last4, name, status, created_at, revoked_at
         from api_keys
         where id = $1",
        id
    )
    .fetch_one(db)
    .await
}

/// `GetAPIKeyByHash`: any status. The caller checks it.
pub async fn get_api_key_by_hash<'e>(
    db: impl PgExecutor<'e>,
    key_hash: &[u8],
) -> sqlx::Result<ApiKey> {
    sqlx::query_as!(
        ApiKey,
        "select id, tenant_id, key_hash, last4, name, status, created_at, revoked_at
         from api_keys
         where key_hash = $1",
        key_hash
    )
    .fetch_one(db)
    .await
}

/// `ListAPIKeysByTenant`: ordered by id.
pub async fn list_api_keys_by_tenant<'e>(
    db: impl PgExecutor<'e>,
    tenant_id: &str,
) -> sqlx::Result<Vec<ApiKey>> {
    sqlx::query_as!(
        ApiKey,
        "select id, tenant_id, key_hash, last4, name, status, created_at, revoked_at
         from api_keys
         where tenant_id = $1
         order by id",
        tenant_id
    )
    .fetch_all(db)
    .await
}

/// `RevokeAPIKey`: re-revoking overwrites `revoked_at`.
pub async fn revoke_api_key<'e>(db: impl PgExecutor<'e>, id: &str) -> sqlx::Result<ApiKey> {
    sqlx::query_as!(
        ApiKey,
        "update api_keys
         set status = 'revoked',
             revoked_at = now()
         where id = $1
         returning id, tenant_id, key_hash, last4, name, status, created_at, revoked_at",
        id
    )
    .fetch_one(db)
    .await
}

/// `DeleteAPIKey`.
pub async fn delete_api_key<'e>(db: impl PgExecutor<'e>, id: &str) -> sqlx::Result<()> {
    sqlx::query!("delete from api_keys where id = $1", id)
        .execute(db)
        .await
        .map(|_| ())
}

// ---- pools.sql ----

/// `CreatePool`.
pub async fn create_pool<'e>(
    db: impl PgExecutor<'e>,
    id: &str,
    tenant_id: &str,
    name: &str,
    model_column_ids: &[String],
    is_default: bool,
) -> sqlx::Result<Pool> {
    sqlx::query_as!(
        Pool,
        "insert into pools (id, tenant_id, name, model_column_ids, is_default)
         values ($1, $2, $3, $4, $5)
         returning id, tenant_id, name, model_column_ids, is_default, created_at, updated_at",
        id,
        tenant_id,
        name,
        model_column_ids,
        is_default
    )
    .fetch_one(db)
    .await
}

/// `ListPoolsByTenant`: ordered by name.
pub async fn list_pools_by_tenant<'e>(
    db: impl PgExecutor<'e>,
    tenant_id: &str,
) -> sqlx::Result<Vec<Pool>> {
    sqlx::query_as!(
        Pool,
        "select id, tenant_id, name, model_column_ids, is_default, created_at, updated_at
         from pools
         where tenant_id = $1
         order by name",
        tenant_id
    )
    .fetch_all(db)
    .await
}

/// `ListPools`: ordered by tenant, then name.
pub async fn list_pools<'e>(db: impl PgExecutor<'e>) -> sqlx::Result<Vec<Pool>> {
    sqlx::query_as!(
        Pool,
        "select id, tenant_id, name, model_column_ids, is_default, created_at, updated_at
         from pools
         order by tenant_id, name"
    )
    .fetch_all(db)
    .await
}

/// `ListPoolsByColumn`: pools that contain the column, ordered by tenant, then name.
pub async fn list_pools_by_column<'e>(
    db: impl PgExecutor<'e>,
    column_id: &str,
) -> sqlx::Result<Vec<Pool>> {
    sqlx::query_as!(
        Pool,
        "select id, tenant_id, name, model_column_ids, is_default, created_at, updated_at
         from pools
         where $1::text = any(model_column_ids)
         order by tenant_id, name",
        column_id
    )
    .fetch_all(db)
    .await
}

/// `DeletePoolsByTenant`.
pub async fn delete_pools_by_tenant<'e>(
    db: impl PgExecutor<'e>,
    tenant_id: &str,
) -> sqlx::Result<()> {
    sqlx::query!("delete from pools where tenant_id = $1", tenant_id)
        .execute(db)
        .await
        .map(|_| ())
}

// ---- tenant_weights.sql ----

/// `CreateTenantWeight`: a plain insert. A duplicate (tenant, capability) fails.
pub async fn create_tenant_weight<'e>(
    db: impl PgExecutor<'e>,
    tenant_id: &str,
    capability: &str,
    w_quality: f32,
    w_cost: f32,
    w_latency: f32,
    quality_bar: f32,
) -> sqlx::Result<TenantWeight> {
    sqlx::query_as!(
        TenantWeight,
        "insert into tenant_weights (tenant_id, capability, w_quality, w_cost, w_latency, quality_bar)
         values ($1, $2, $3, $4, $5, $6)
         returning tenant_id, capability, w_quality, w_cost, w_latency, quality_bar",
        tenant_id,
        capability,
        w_quality,
        w_cost,
        w_latency,
        quality_bar
    )
    .fetch_one(db)
    .await
}

/// `ListTenantWeightsByTenant`: ordered by capability.
pub async fn list_tenant_weights_by_tenant<'e>(
    db: impl PgExecutor<'e>,
    tenant_id: &str,
) -> sqlx::Result<Vec<TenantWeight>> {
    sqlx::query_as!(
        TenantWeight,
        "select tenant_id, capability, w_quality, w_cost, w_latency, quality_bar
         from tenant_weights
         where tenant_id = $1
         order by capability",
        tenant_id
    )
    .fetch_all(db)
    .await
}

/// `DeleteTenantWeightsByTenant`.
pub async fn delete_tenant_weights_by_tenant<'e>(
    db: impl PgExecutor<'e>,
    tenant_id: &str,
) -> sqlx::Result<()> {
    sqlx::query!("delete from tenant_weights where tenant_id = $1", tenant_id)
        .execute(db)
        .await
        .map(|_| ())
}

// ---- tenant_constraints.sql ----

/// `GetTenantConstraints`. No row means defaults; see [`crate::is_not_found`].
pub async fn get_tenant_constraints<'e>(
    db: impl PgExecutor<'e>,
    tenant_id: &str,
) -> sqlx::Result<TenantConstraint> {
    sqlx::query_as!(
        TenantConstraint,
        "select tenant_id, max_cost_per_run, max_latency_ms, zero_retention, updated_at,
                telemetry_opt_out
         from tenant_constraints
         where tenant_id = $1",
        tenant_id
    )
    .fetch_one(db)
    .await
}

/// `UpsertTenantConstraints`: a conflict replaces every value and sets
/// `updated_at = now()`. Rust adds `telemetry_opt_out` (D39).
pub async fn upsert_tenant_constraints<'e>(
    db: impl PgExecutor<'e>,
    tenant_id: &str,
    max_cost_per_run: Option<Decimal>,
    max_latency_ms: Option<i32>,
    zero_retention: bool,
    telemetry_opt_out: bool,
) -> sqlx::Result<TenantConstraint> {
    sqlx::query_as!(
        TenantConstraint,
        "insert into tenant_constraints
           (tenant_id, max_cost_per_run, max_latency_ms, zero_retention, telemetry_opt_out)
         values ($1, $2, $3, $4, $5)
         on conflict (tenant_id) do update set
           max_cost_per_run = excluded.max_cost_per_run,
           max_latency_ms = excluded.max_latency_ms,
           zero_retention = excluded.zero_retention,
           telemetry_opt_out = excluded.telemetry_opt_out,
           updated_at = now()
         returning tenant_id, max_cost_per_run, max_latency_ms, zero_retention, updated_at,
                   telemetry_opt_out",
        tenant_id,
        max_cost_per_run,
        max_latency_ms,
        zero_retention,
        telemetry_opt_out
    )
    .fetch_one(db)
    .await
}
