//! Tenant and API key provisioning, shared by the internal API (spec §13), a replica's
//! first start, and `maverick key` (§14.4, D49), so every path writes the same rows and
//! the same audit lines. Each function audits only after its write commits.

use std::fmt;

use maverick_domain::gojson::{self, JsonValue};
use maverick_domain::{ids, keys};
use maverick_store::{catalog, tenancy};
use sqlx::{Connection, PgConnection, PgExecutor, PgPool};

use crate::audit;

/// The advisory lock that serializes replica bootstraps: `0x6d76626f6f74` ("mvboot").
const BOOTSTRAP_LOCK_KEY: i64 = 0x6d76_626f_6f74;

/// The audit actor of a replica's bootstrap.
pub const BOOTSTRAP_ACTOR: &str = "bootstrap";

/// The name of the API key a replica's bootstrap issues.
pub const BOOTSTRAP_KEY_NAME: &str = "bootstrap";

/// A key just issued. The raw key exists only here: `Debug` never shows it.
#[derive(Clone)]
pub struct IssuedKey {
    pub id: String,
    pub tenant_id: String,
    pub name: String,
    pub last4: String,
    pub raw_key: String,
}

impl fmt::Debug for IssuedKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IssuedKey")
            .field("id", &self.id)
            .field("tenant_id", &self.tenant_id)
            .field("name", &self.name)
            .field("last4", &self.last4)
            .field("raw_key", &"<redacted>")
            .finish()
    }
}

/// What a replica's first start created.
#[derive(Clone)]
pub struct Bootstrapped {
    pub tenant: tenancy::Tenant,
    pub key: IssuedKey,
}

impl fmt::Debug for Bootstrapped {
    /// Never shows the tenant's HMAC key or the raw API key.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Bootstrapped")
            .field("tenant_id", &self.tenant.id)
            .field("tenant_name", &self.tenant.name)
            .field("key", &self.key)
            .finish()
    }
}

/// The tenant row and its default pool (every active column; none when no column is
/// active). Returns the tenant and the pool's column count.
async fn insert_tenant(
    conn: &mut PgConnection,
    name: &str,
) -> sqlx::Result<(tenancy::Tenant, usize)> {
    let active: Vec<String> =
        catalog::list_model_columns_by_statuses(&mut *conn, &["active".to_owned()])
            .await?
            .into_iter()
            .map(|column| column.id)
            .collect();
    let hmac_key: [u8; 32] = rand::random();
    let id = ids::new(ids::prefix::TENANT);
    let tenant = tenancy::create_tenant(&mut *conn, &id, name, &hmac_key).await?;
    if !active.is_empty() {
        let pool_id = ids::new(ids::prefix::POOL);
        tenancy::create_pool(&mut *conn, &pool_id, &id, "default", &active, true).await?;
    }
    Ok((tenant, active.len()))
}

async fn insert_key<'e>(
    db: impl PgExecutor<'e>,
    tenant_id: &str,
    name: &str,
) -> sqlx::Result<IssuedKey> {
    let minted = keys::mint_key();
    let id = ids::new(ids::prefix::API_KEY);
    tenancy::create_api_key(db, &id, tenant_id, &minted.key_hash, &minted.last4, name).await?;
    Ok(IssuedKey {
        id,
        tenant_id: tenant_id.to_owned(),
        name: name.to_owned(),
        last4: minted.last4,
        raw_key: minted.raw_key,
    })
}

/// `POST /internal/v1/tenants` (§7.1): a tenant with the default pool, in one
/// transaction, then the `tenant.create` audit line.
pub async fn create_tenant(db: &PgPool, actor: &str, name: &str) -> sqlx::Result<tenancy::Tenant> {
    let mut tx = db.begin().await?;
    let (tenant, columns) = insert_tenant(&mut tx, name).await?;
    tx.commit().await?;
    audit::tenant_create(actor, &tenant.id, name, columns);
    Ok(tenant)
}

/// `POST /internal/v1/tenants/{id}/api-keys` (§7.4): a new key for `tenant_id`, then the
/// `api_key.issue` audit line.
pub async fn issue_key(
    db: &PgPool,
    actor: &str,
    tenant_id: &str,
    name: &str,
) -> sqlx::Result<IssuedKey> {
    let key = insert_key(db, tenant_id, name).await?;
    audit::api_key_issue(actor, tenant_id, &key.id, name, &key.last4);
    Ok(key)
}

/// `DELETE /internal/v1/api-keys/{id}` (§7.5): idempotent. A revoked key stays as it is,
/// and the `api_key.revoke` audit line is written either way.
pub async fn revoke_key(
    db: &PgPool,
    actor: &str,
    key: tenancy::ApiKey,
) -> sqlx::Result<tenancy::ApiKey> {
    let key = if key.status == "revoked" {
        key
    } else {
        tenancy::revoke_api_key(db, &key.id).await?
    };
    audit::api_key_revoke(actor, &key.tenant_id, &key.id);
    Ok(key)
}

/// A replica's first start (§14.4, D49). Under a transaction-scoped advisory lock, when
/// no tenant exists, creates one tenant named `name` with the default pool and one API
/// key, as the internal API does. `None` when a tenant exists, so later starts create
/// nothing and concurrent first starts create one. It takes a connection, so a caller can
/// bound the wait for one without cutting off a commit (the key would then be lost).
pub async fn bootstrap(conn: &mut PgConnection, name: &str) -> sqlx::Result<Option<Bootstrapped>> {
    let mut tx = conn.begin().await?;
    sqlx::query("select pg_advisory_xact_lock($1)")
        .bind(BOOTSTRAP_LOCK_KEY)
        .execute(&mut *tx)
        .await?;
    if !tenancy::list_tenants(&mut *tx).await?.is_empty() {
        return Ok(None);
    }
    let (tenant, columns) = insert_tenant(&mut tx, name).await?;
    let key = insert_key(&mut *tx, &tenant.id, BOOTSTRAP_KEY_NAME).await?;
    tx.commit().await?;
    audit::tenant_create(BOOTSTRAP_ACTOR, &tenant.id, name, columns);
    audit::api_key_issue(BOOTSTRAP_ACTOR, &tenant.id, &key.id, &key.name, &key.last4);
    Ok(Some(Bootstrapped { tenant, key }))
}

/// A key as the internal API renders it: never the hash or the raw key.
#[must_use]
pub fn api_key_json(key: &tenancy::ApiKey) -> String {
    gojson::marshal(&crate::internal::api_key_json(key))
}

/// A key just issued, as `POST /internal/v1/tenants/{id}/api-keys` answers it:
/// `{"id", "key"}`, the only time the raw key is shown.
#[must_use]
pub fn issued_key_json(key: &IssuedKey) -> String {
    gojson::marshal(&issued_json(key))
}

pub(crate) fn issued_json(key: &IssuedKey) -> JsonValue {
    JsonValue::Object(
        [
            ("id".to_owned(), JsonValue::String(key.id.clone())),
            ("key".to_owned(), JsonValue::String(key.raw_key.clone())),
        ]
        .into_iter()
        .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_shows_the_raw_key() {
        let key = IssuedKey {
            id: "key_1".to_owned(),
            tenant_id: "ten_1".to_owned(),
            name: "app".to_owned(),
            last4: "wxyz".to_owned(),
            raw_key: "mk_live_secretwxyz".to_owned(),
        };
        let shown = format!("{key:?}");
        assert!(!shown.contains("mk_live_secret"), "{shown}");
        assert!(shown.contains("wxyz"), "{shown}");
        assert_eq!(
            issued_key_json(&key),
            r#"{"id":"key_1","key":"mk_live_secretwxyz"}"#
        );
    }
}
