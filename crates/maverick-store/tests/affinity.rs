//! Store tests for the `conversation_affinity` rows (spec §7.6, §9.10, D44) on real
//! Postgres.

use std::sync::LazyLock;

use chrono::{DateTime, Duration, TimeZone, Utc};
use maverick_store::affinity::{self, AffinityRow};
use maverick_store::{MIGRATOR, tenancy};
use maverick_testdb::Schema;
use sqlx::PgPool;

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

const HASH: &str = "hmac_sha256:b92e993712d7b4efa47d74dade3450c1f966f89ff4d3146e3719ddd3c43210f1";

fn at(minute: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 24, 12, minute, 0).unwrap()
}

fn row(
    tenant_id: &str,
    column_id: &str,
    recommendation_id: &str,
    used: DateTime<Utc>,
) -> AffinityRow {
    AffinityRow {
        tenant_id: tenant_id.to_owned(),
        affinity_hash: HASH.to_owned(),
        column_id: column_id.to_owned(),
        recommendation_id: recommendation_id.to_owned(),
        last_used_at: used,
        expires_at: used + Duration::hours(24),
    }
}

async fn seed(pool: &PgPool) {
    for tenant in ["ten_a", "ten_b"] {
        tenancy::create_tenant(pool, tenant, tenant, &[1; 32])
            .await
            .expect("tenant");
    }
    for (id, model) in [
        ("col_p", "model-p"),
        ("col_t", "model-t"),
        ("col_s", "model-s"),
    ] {
        sqlx::query(
            "insert into model_columns (id, provider, model_id, endpoint)
             values ($1, 'anthropic', $2, 'direct')",
        )
        .bind(id)
        .bind(model)
        .execute(pool)
        .await
        .expect("column");
    }
}

#[tokio::test]
async fn upsert_then_get_and_the_last_write_wins() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed(&pool).await;

    assert_eq!(
        affinity::get_affinity(&pool, "ten_a", HASH, at(0))
            .await
            .unwrap(),
        None
    );
    let first = row("ten_a", "col_p", "rec_1", at(0));
    affinity::upsert_affinity(&pool, &first)
        .await
        .expect("insert");
    assert_eq!(
        affinity::get_affinity(&pool, "ten_a", HASH, at(1))
            .await
            .unwrap(),
        Some(first)
    );

    let second = row("ten_a", "col_t", "rec_2", at(5));
    affinity::upsert_affinity(&pool, &second)
        .await
        .expect("update");
    assert_eq!(
        affinity::get_affinity(&pool, "ten_a", HASH, at(6))
            .await
            .unwrap(),
        Some(second)
    );

    // Tenant isolation: the same hash under another tenant has no row.
    assert_eq!(
        affinity::get_affinity(&pool, "ten_b", HASH, at(6))
            .await
            .unwrap(),
        None
    );
    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn an_expired_row_reads_as_missing_and_a_new_write_revives_it() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed(&pool).await;

    let mut old = row("ten_a", "col_p", "rec_1", at(0));
    old.expires_at = at(10);
    affinity::upsert_affinity(&pool, &old)
        .await
        .expect("insert");
    assert!(
        affinity::get_affinity(&pool, "ten_a", HASH, at(9))
            .await
            .unwrap()
            .is_some()
    );
    // expires_at must be in the future: equal is expired.
    assert_eq!(
        affinity::get_affinity(&pool, "ten_a", HASH, at(10))
            .await
            .unwrap(),
        None
    );

    let fresh = row("ten_a", "col_t", "rec_2", at(20));
    affinity::upsert_affinity(&pool, &fresh)
        .await
        .expect("revive");
    assert_eq!(
        affinity::get_affinity(&pool, "ten_a", HASH, at(21))
            .await
            .unwrap(),
        Some(fresh)
    );
    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn following_the_served_column_is_a_compare_and_set() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed(&pool).await;
    affinity::upsert_affinity(&pool, &row("ten_a", "col_p", "rec_1", at(0)))
        .await
        .expect("insert");

    // The row still belongs to rec_1, so the served column replaces the pick.
    assert!(
        affinity::follow_served_column(&pool, "ten_a", HASH, "rec_1", "col_s")
            .await
            .unwrap()
    );
    let followed = affinity::get_affinity(&pool, "ten_a", HASH, at(1))
        .await
        .unwrap()
        .expect("row");
    assert_eq!(followed.column_id, "col_s");
    assert_eq!(followed.recommendation_id, "rec_1");
    assert_eq!(followed.last_used_at, at(0));

    // A later turn wrote rec_2, so late telemetry for rec_1 changes nothing.
    let later = row("ten_a", "col_t", "rec_2", at(3));
    affinity::upsert_affinity(&pool, &later)
        .await
        .expect("later turn");
    assert!(
        !affinity::follow_served_column(&pool, "ten_a", HASH, "rec_1", "col_p")
            .await
            .unwrap()
    );
    // Neither does a follow for another tenant or a missing row.
    assert!(
        !affinity::follow_served_column(&pool, "ten_b", HASH, "rec_2", "col_p")
            .await
            .unwrap()
    );
    assert_eq!(
        affinity::get_affinity(&pool, "ten_a", HASH, at(4))
            .await
            .unwrap(),
        Some(later)
    );
    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn prune_deletes_only_expired_rows_and_a_rerun_is_a_no_op() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed(&pool).await;

    let mut expired = row("ten_a", "col_p", "rec_1", at(0));
    expired.expires_at = at(10);
    let mut live = row("ten_b", "col_t", "rec_2", at(0));
    live.expires_at = at(30);
    affinity::upsert_affinity(&pool, &expired)
        .await
        .expect("expired");
    affinity::upsert_affinity(&pool, &live).await.expect("live");

    assert_eq!(affinity::prune_affinity(&pool, at(20)).await.unwrap(), 1);
    assert_eq!(affinity::prune_affinity(&pool, at(20)).await.unwrap(), 0);
    let left: Vec<(String,)> =
        sqlx::query_as("select tenant_id from conversation_affinity order by tenant_id")
            .fetch_all(&pool)
            .await
            .expect("rows");
    assert_eq!(left, [("ten_b".to_owned(),)]);
    pool.close().await;
    db.close().await;
}
