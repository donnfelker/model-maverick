//! The one-round-trip reads of the recommend path and public auth (e3x) return exactly
//! what the queries they replace return.

use std::str::FromStr;
use std::sync::LazyLock;

use maverick_store::{Decimal, MIGRATOR, corpus, tenancy};
use maverick_testdb::Schema;
use sqlx::PgPool;

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

/// The structs have no `PartialEq`; their `Debug` output covers every field.
fn same<T: std::fmt::Debug>(a: &T, b: &T) -> bool {
    format!("{a:?}") == format!("{b:?}")
}

async fn separately(pool: &PgPool, tenant: &str) -> tenancy::TenantContext {
    let constraints = match tenancy::get_tenant_constraints(pool, tenant).await {
        Ok(row) => Some(row),
        Err(error) if maverick_store::is_not_found(&error) => None,
        Err(error) => panic!("{error}"),
    };
    let active_corpus_version = match corpus::get_active_corpus_version(pool).await {
        Ok(version) => Some(version.version),
        Err(error) if maverick_store::is_not_found(&error) => None,
        Err(error) => panic!("{error}"),
    };
    tenancy::TenantContext {
        pools: tenancy::list_pools_by_tenant(pool, tenant)
            .await
            .expect("pools"),
        weights: tenancy::list_tenant_weights_by_tenant(pool, tenant)
            .await
            .expect("weights"),
        constraints,
        active_corpus_version,
    }
}

#[tokio::test]
async fn tenant_context_matches_the_separate_queries() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    tenancy::create_tenant(&pool, "ten_full", "Full", &[1; 32])
        .await
        .expect("tenant");
    tenancy::create_tenant(&pool, "ten_empty", "Empty", &[2; 32])
        .await
        .expect("tenant");

    // Empty: no pools, weights, constraints, or active corpus.
    let empty = tenancy::get_tenant_context(&pool, "ten_empty")
        .await
        .expect("context");
    assert!(empty.pools.is_empty() && empty.weights.is_empty());
    assert!(empty.constraints.is_none() && empty.active_corpus_version.is_none());
    assert!(same(&empty, &separately(&pool, "ten_empty").await));

    // Full: pools inserted out of name order, awkward floats, a numeric cap.
    for (id, name, cols, default) in [
        (
            "pool_z",
            "zeta",
            vec!["col_b".to_owned(), "col_a".to_owned()],
            false,
        ),
        ("pool_a", "alpha", vec!["col_a".to_owned()], true),
    ] {
        tenancy::create_pool(&pool, id, "ten_full", name, &cols, default)
            .await
            .expect("pool");
    }
    for (capability, q, c, l, bar) in [
        (
            "structured_extraction",
            0.3_f32,
            0.123_456_79_f32,
            0.576_543_2_f32,
            0.85_f32,
        ),
        ("factual_qa", 1.0 / 3.0, 0.1, 0.2, 0.8),
    ] {
        tenancy::create_tenant_weight(&pool, "ten_full", capability, q, c, l, bar)
            .await
            .expect("weight");
    }
    tenancy::upsert_tenant_constraints(
        &pool,
        "ten_full",
        Some(Decimal::from_str("0.012345").expect("decimal")),
        Some(2000),
        true,
        true,
        false,
    )
    .await
    .expect("constraints");
    corpus::upsert_corpus_version(&pool, "2026-07-01", "", 0, false)
        .await
        .expect("version");
    corpus::activate_corpus_version(&pool, "2026-07-01")
        .await
        .expect("activate");

    let full = tenancy::get_tenant_context(&pool, "ten_full")
        .await
        .expect("context");
    assert_eq!(
        full.pools
            .iter()
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>(),
        ["alpha", "zeta"]
    );
    assert_eq!(
        full.weights
            .iter()
            .map(|w| w.capability.as_str())
            .collect::<Vec<_>>(),
        ["factual_qa", "structured_extraction"]
    );
    assert_eq!(full.weights[0].w_quality, 1.0_f32 / 3.0);
    assert_eq!(full.active_corpus_version.as_deref(), Some("2026-07-01"));
    assert!(
        same(&full, &separately(&pool, "ten_full").await),
        "{full:?}"
    );
    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn key_auth_matches_the_key_and_tenant_rows() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    tenancy::create_tenant(&pool, "ten_auth", "Auth", &[3; 32])
        .await
        .expect("tenant");
    tenancy::create_api_key(&pool, "key_live", "ten_auth", b"hash-live", "live", "")
        .await
        .expect("key");
    tenancy::create_api_key(
        &pool,
        "key_revoked",
        "ten_auth",
        b"hash-revoked",
        "rvkd",
        "",
    )
    .await
    .expect("key");
    tenancy::revoke_api_key(&pool, "key_revoked")
        .await
        .expect("revoke");

    assert_eq!(
        tenancy::get_key_auth_by_hash(&pool, b"hash-none")
            .await
            .expect("query"),
        None
    );
    let live = tenancy::get_key_auth_by_hash(&pool, b"hash-live")
        .await
        .expect("query")
        .expect("row");
    assert_eq!(
        live,
        tenancy::KeyAuth {
            key_id: "key_live".to_owned(),
            key_status: "active".to_owned(),
            tenant_id: "ten_auth".to_owned(),
            tenant_status: Some("active".to_owned()),
        }
    );
    let revoked = tenancy::get_key_auth_by_hash(&pool, b"hash-revoked")
        .await
        .expect("query")
        .expect("row");
    assert_eq!(revoked.key_status, "revoked");

    tenancy::update_tenant(&pool, "ten_auth", "Auth", "suspended")
        .await
        .expect("suspend");
    let suspended = tenancy::get_key_auth_by_hash(&pool, b"hash-live")
        .await
        .expect("query")
        .expect("row");
    assert_eq!(suspended.tenant_status.as_deref(), Some("suspended"));
    pool.close().await;
    db.close().await;
}
