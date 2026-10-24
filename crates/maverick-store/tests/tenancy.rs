//! Store tests for tenants, API keys, pools, weights, and constraints (real Postgres).

use std::str::FromStr;
use std::sync::LazyLock;

use maverick_store::tenancy::{self, Tenant};
use maverick_store::{Decimal, MIGRATOR, is_not_found, numeric_to_f64, sqlstate};
use maverick_testdb::Schema;
use sqlx::PgPool;

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

async fn tenant(pool: &PgPool, id: &str) -> Tenant {
    tenancy::create_tenant(pool, id, "Acme", &[7; 32])
        .await
        .expect("create tenant")
}

fn code(result: sqlx::Result<impl Sized>) -> String {
    match result {
        Ok(_) => panic!("expected a database error"),
        Err(err) => sqlstate(&err).unwrap_or_else(|| panic!("not a database error: {err}")),
    }
}

#[tokio::test]
async fn tenants_round_trip_order_and_update() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;

    let b = tenant(&pool, "ten_b").await;
    assert_eq!(b.status, "active");
    assert_eq!(b.prompt_hmac_key, vec![7; 32]);
    assert_eq!(b.classification_quota_override, None);
    tenant(&pool, "ten_a").await;

    let ids: Vec<String> = tenancy::list_tenants(&pool)
        .await
        .expect("list")
        .into_iter()
        .map(|t| t.id)
        .collect();
    assert_eq!(ids, ["ten_a", "ten_b"]);

    let updated = tenancy::update_tenant(&pool, "ten_b", "Acme 2", "suspended")
        .await
        .expect("update");
    assert_eq!(
        (updated.name.as_str(), updated.status.as_str()),
        ("Acme 2", "suspended")
    );
    assert_eq!(updated.created_at, b.created_at);
    assert!(updated.updated_at >= b.updated_at);
    assert_eq!(
        tenancy::get_tenant(&pool, "ten_b").await.expect("get").name,
        "Acme 2"
    );

    assert_eq!(
        code(tenancy::update_tenant(&pool, "ten_b", "Acme", "deleted").await),
        "23514"
    );
    let missing = tenancy::get_tenant(&pool, "ten_missing").await.unwrap_err();
    assert!(is_not_found(&missing), "{missing}");
    assert!(is_not_found(
        &tenancy::update_tenant(&pool, "ten_missing", "x", "active")
            .await
            .unwrap_err()
    ));

    tenancy::delete_tenant(&pool, "ten_a")
        .await
        .expect("delete");
    assert!(is_not_found(
        &tenancy::get_tenant(&pool, "ten_a").await.unwrap_err()
    ));
    // Deleting an unknown tenant is not an error (`:exec`).
    tenancy::delete_tenant(&pool, "ten_a")
        .await
        .expect("delete again");

    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn deleting_a_referenced_tenant_fails_with_a_foreign_key_error() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    tenant(&pool, "ten_1").await;
    tenancy::create_api_key(&pool, "key_1", "ten_1", b"hash", "abcd", "")
        .await
        .expect("key");
    assert_eq!(code(tenancy::delete_tenant(&pool, "ten_1").await), "23503");
    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn the_classification_quota_override_is_set_and_cleared() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    tenant(&pool, "ten_1").await;
    let set = tenancy::set_tenant_classification_quota(&pool, "ten_1", Some(50))
        .await
        .expect("set");
    assert_eq!(set.classification_quota_override, Some(50));
    let cleared = tenancy::set_tenant_classification_quota(&pool, "ten_1", None)
        .await
        .expect("clear");
    assert_eq!(cleared.classification_quota_override, None);
    assert!(is_not_found(
        &tenancy::set_tenant_classification_quota(&pool, "ten_missing", Some(1))
            .await
            .unwrap_err()
    ));
    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn api_keys_are_created_listed_revoked_and_found_by_hash() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    tenant(&pool, "ten_1").await;
    tenant(&pool, "ten_2").await;

    let key = tenancy::create_api_key(&pool, "key_b", "ten_1", b"hash-b", "bbbb", "ci")
        .await
        .expect("create");
    assert_eq!(key.status, "active");
    assert_eq!(key.revoked_at, None);
    assert_eq!(key.name, "ci");
    tenancy::create_api_key(&pool, "key_a", "ten_1", b"hash-a", "aaaa", "")
        .await
        .expect("create");
    tenancy::create_api_key(&pool, "key_c", "ten_2", b"hash-c", "cccc", "")
        .await
        .expect("create");

    assert_eq!(
        code(tenancy::create_api_key(&pool, "key_d", "ten_2", b"hash-c", "dddd", "").await),
        "23505",
        "key_hash is unique"
    );

    let ids: Vec<String> = tenancy::list_api_keys_by_tenant(&pool, "ten_1")
        .await
        .expect("list")
        .into_iter()
        .map(|k| k.id)
        .collect();
    assert_eq!(ids, ["key_a", "key_b"]);
    assert!(
        tenancy::list_api_keys_by_tenant(&pool, "ten_none")
            .await
            .expect("empty list")
            .is_empty()
    );

    let revoked = tenancy::revoke_api_key(&pool, "key_b")
        .await
        .expect("revoke");
    assert_eq!(revoked.status, "revoked");
    let first = revoked.revoked_at.expect("revoked_at set");
    let again = tenancy::revoke_api_key(&pool, "key_b")
        .await
        .expect("revoke again");
    assert!(again.revoked_at.expect("revoked_at set") >= first);

    // Lookup by hash returns any status; the caller checks it.
    let by_hash = tenancy::get_api_key_by_hash(&pool, b"hash-b")
        .await
        .expect("by hash");
    assert_eq!(
        (by_hash.id.as_str(), by_hash.status.as_str()),
        ("key_b", "revoked")
    );
    assert!(is_not_found(
        &tenancy::get_api_key_by_hash(&pool, b"nope")
            .await
            .unwrap_err()
    ));
    assert_eq!(
        tenancy::get_api_key(&pool, "key_a")
            .await
            .expect("get")
            .last4,
        "aaaa"
    );

    tenancy::delete_api_key(&pool, "key_a")
        .await
        .expect("delete");
    assert!(is_not_found(
        &tenancy::get_api_key(&pool, "key_a").await.unwrap_err()
    ));
    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn pools_are_listed_by_tenant_by_name_and_by_column() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    tenant(&pool, "ten_1").await;
    tenant(&pool, "ten_2").await;
    let cols = |ids: &[&str]| ids.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();

    let p = tenancy::create_pool(
        &pool,
        "pool_1",
        "ten_2",
        "vertex-only",
        &cols(&["col_a"]),
        false,
    )
    .await
    .expect("create");
    assert_eq!(p.model_column_ids, ["col_a"]);
    tenancy::create_pool(
        &pool,
        "pool_2",
        "ten_1",
        "default",
        &cols(&["col_a", "col_b"]),
        true,
    )
    .await
    .expect("create");
    tenancy::create_pool(&pool, "pool_3", "ten_1", "cheap", &cols(&["col_b"]), false)
        .await
        .expect("create");
    tenancy::create_pool(&pool, "pool_4", "ten_2", "empty", &[], false)
        .await
        .expect("an empty pool is allowed by the schema");
    assert_eq!(
        code(tenancy::create_pool(&pool, "pool_5", "ten_1", "cheap", &[], false).await),
        "23505",
        "(tenant_id, name) is unique"
    );

    let names = |pools: Vec<tenancy::Pool>| {
        pools
            .into_iter()
            .map(|p| format!("{}/{}", p.tenant_id, p.name))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        names(
            tenancy::list_pools_by_tenant(&pool, "ten_1")
                .await
                .expect("by tenant")
        ),
        ["ten_1/cheap", "ten_1/default"]
    );
    assert_eq!(
        names(tenancy::list_pools(&pool).await.expect("all")),
        [
            "ten_1/cheap",
            "ten_1/default",
            "ten_2/empty",
            "ten_2/vertex-only"
        ]
    );
    assert_eq!(
        names(
            tenancy::list_pools_by_column(&pool, "col_a")
                .await
                .expect("by column")
        ),
        ["ten_1/default", "ten_2/vertex-only"]
    );
    assert!(
        tenancy::list_pools_by_column(&pool, "col_none")
            .await
            .expect("by column")
            .is_empty()
    );

    tenancy::delete_pools_by_tenant(&pool, "ten_1")
        .await
        .expect("delete");
    assert_eq!(
        names(tenancy::list_pools(&pool).await.expect("all")),
        ["ten_2/empty", "ten_2/vertex-only"]
    );
    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn weights_are_real_and_listed_by_capability() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    tenant(&pool, "ten_1").await;

    let w = tenancy::create_tenant_weight(&pool, "ten_1", "summarization", 0.7, 0.2, 0.1, 0.8)
        .await
        .expect("create");
    // real is float32: 0.7 reads back as the f32 nearest to 0.7, which widens to
    // 0.699999988079071 (reference Part 7 note on real).
    assert_eq!(w.w_quality, 0.7_f32);
    assert_eq!(f64::from(w.w_quality), 0.699_999_988_079_071);
    assert_eq!(w.quality_bar, 0.8_f32);
    tenancy::create_tenant_weight(&pool, "ten_1", "classification", 0.5, 0.25, 0.25, 0.9)
        .await
        .expect("create");
    assert_eq!(
        code(
            tenancy::create_tenant_weight(&pool, "ten_1", "classification", 1.0, 0.0, 0.0, 0.8)
                .await
        ),
        "23505",
        "a plain insert: a duplicate capability is an error"
    );

    let caps: Vec<String> = tenancy::list_tenant_weights_by_tenant(&pool, "ten_1")
        .await
        .expect("list")
        .into_iter()
        .map(|w| w.capability)
        .collect();
    assert_eq!(caps, ["classification", "summarization"]);

    tenancy::delete_tenant_weights_by_tenant(&pool, "ten_1")
        .await
        .expect("delete");
    assert!(
        tenancy::list_tenant_weights_by_tenant(&pool, "ten_1")
            .await
            .expect("list")
            .is_empty()
    );
    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn constraints_upsert_replaces_every_value() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    tenant(&pool, "ten_1").await;

    assert!(is_not_found(
        &tenancy::get_tenant_constraints(&pool, "ten_1")
            .await
            .unwrap_err()
    ));

    let cost = Decimal::from_str("0.012345").expect("decimal");
    let first =
        tenancy::upsert_tenant_constraints(&pool, "ten_1", Some(cost), Some(2000), true, true)
            .await
            .expect("insert");
    assert_eq!(first.max_cost_per_run, Some(cost));
    assert_eq!(numeric_to_f64(first.max_cost_per_run.unwrap()), 0.012345);
    assert_eq!(first.max_latency_ms, Some(2000));
    assert!(first.zero_retention && first.telemetry_opt_out);

    // A conflict replaces all values, including NULLs.
    let second = tenancy::upsert_tenant_constraints(&pool, "ten_1", None, None, false, false)
        .await
        .expect("update");
    assert_eq!(second.max_cost_per_run, None);
    assert_eq!(second.max_latency_ms, None);
    assert!(!second.zero_retention && !second.telemetry_opt_out);
    assert!(second.updated_at >= first.updated_at);

    let read = tenancy::get_tenant_constraints(&pool, "ten_1")
        .await
        .expect("get");
    assert_eq!(read.max_cost_per_run, None);

    // numeric(12,6) rounds on write.
    let rounded = tenancy::upsert_tenant_constraints(
        &pool,
        "ten_1",
        Some(Decimal::from_str("0.1234567").expect("decimal")),
        None,
        false,
        false,
    )
    .await
    .expect("rounded");
    assert_eq!(
        rounded.max_cost_per_run,
        Some(Decimal::from_str("0.123457").expect("decimal"))
    );
    pool.close().await;
    db.close().await;
}

/// DIVERGENCE: D44. `affinity_cost_aware` defaults to false, every read returns it, and
/// the upsert replaces it with false like every other value (no payload carries it yet).
#[tokio::test]
async fn constraints_carry_affinity_cost_aware() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    tenant(&pool, "ten_1").await;

    let first = tenancy::upsert_tenant_constraints(&pool, "ten_1", None, None, false, false)
        .await
        .expect("insert");
    assert!(!first.affinity_cost_aware);

    sqlx::query("update tenant_constraints set affinity_cost_aware = true where tenant_id = $1")
        .bind("ten_1")
        .execute(&pool)
        .await
        .expect("set");
    let read = tenancy::get_tenant_constraints(&pool, "ten_1")
        .await
        .expect("get");
    assert!(read.affinity_cost_aware);
    let context = tenancy::get_tenant_context(&pool, "ten_1")
        .await
        .expect("context");
    assert!(
        context
            .constraints
            .expect("constraints")
            .affinity_cost_aware
    );

    let replaced = tenancy::upsert_tenant_constraints(&pool, "ten_1", None, None, true, false)
        .await
        .expect("replace");
    assert!(!replaced.affinity_cost_aware);
    pool.close().await;
    db.close().await;
}
