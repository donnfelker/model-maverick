//! DIVERGENCE: D49. Authenticated import transactions on real Postgres.
use chrono::{TimeZone, Utc};
use ed25519_dalek::SigningKey;
use maverick_bundle::{
    Aggregate, Calibration, CatalogColumn, Manifest, PublicMatrix, SchemaRange, SigningKeys,
    VerifiedBundle,
};
use maverick_store::{
    MIGRATOR, catalog, matrix,
    matrix_import::{self, ImportError, ImportOutcome},
};
use maverick_testdb::Schema;
use serde_json::json;
use std::{collections::BTreeMap, sync::LazyLock};
static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

#[tokio::test]
async fn an_import_waits_for_bootstrap_before_completing_the_default_pool() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let mut bootstrap = pool.begin().await.unwrap();
    sqlx::query("select pg_advisory_xact_lock($1)")
        .bind(matrix_import::REPLICA_BOOTSTRAP_LOCK_KEY)
        .execute(&mut *bootstrap)
        .await
        .unwrap();
    maverick_store::tenancy::create_tenant(&mut *bootstrap, "replica", "replica", &[1; 32])
        .await
        .unwrap();
    let other = pool.clone();
    let bundle = verified(23, data());
    let mut pending =
        tokio::spawn(async move { matrix_import::import(&other, &bundle, false).await });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut pending)
            .await
            .is_err()
    );
    bootstrap.commit().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), pending)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let pools = maverick_store::tenancy::list_pools_by_tenant(&pool, "replica")
        .await
        .unwrap();
    assert_eq!(pools.len(), 1);
    assert_eq!(pools[0].model_column_ids, vec!["public"]);
    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn an_import_waits_for_an_operator_pool_write_and_preserves_it() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    matrix_import::import(&pool, &verified(23, data()), false)
        .await
        .unwrap();
    maverick_store::tenancy::create_tenant(&pool, "replica", "replica", &[1; 32])
        .await
        .unwrap();
    let mut operator = pool.begin().await.unwrap();
    matrix_import::lock_pool_configuration(&mut operator, "replica")
        .await
        .unwrap();
    maverick_store::tenancy::create_pool(
        &mut *operator,
        "team",
        "replica",
        "team",
        &["public".into()],
        true,
    )
    .await
    .unwrap();
    let other = pool.clone();
    let mut next = data();
    next.catalog.push(column("other"));
    let bundle = verified(24, next);
    let mut pending =
        tokio::spawn(async move { matrix_import::import(&other, &bundle, false).await });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut pending)
            .await
            .is_err()
    );
    operator.commit().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), pending)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let pools = maverick_store::tenancy::list_pools_by_tenant(&pool, "replica")
        .await
        .unwrap();
    assert_eq!(pools.len(), 1);
    assert_eq!(pools[0].id, "team");
    assert_eq!(pools[0].model_column_ids, vec!["public"]);
    pool.close().await;
    db.close().await;
}

/// xq7v.26: deferred defaults are created once and never widen an existing pool.
#[tokio::test]
async fn first_import_completes_missing_defaults_and_later_imports_preserve_membership() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    maverick_store::tenancy::create_tenant(&pool, "replica", "replica", &[1; 32])
        .await
        .unwrap();
    let mut initial = data();
    initial.catalog.push(column("second"));
    matrix_import::import(&pool, &verified(23, initial), false)
        .await
        .unwrap();
    let pools = maverick_store::tenancy::list_pools_by_tenant(&pool, "replica")
        .await
        .unwrap();
    assert_eq!(pools.len(), 1);
    assert_eq!(pools[0].name, "default");
    assert!(pools[0].is_default);
    assert_eq!(pools[0].model_column_ids, vec!["public", "second"]);
    let id = pools[0].id.clone();
    let mut next = data();
    next.catalog.extend([column("second"), column("third")]);
    matrix_import::import(&pool, &verified(24, next), false)
        .await
        .unwrap();
    let pools = maverick_store::tenancy::list_pools_by_tenant(&pool, "replica")
        .await
        .unwrap();
    assert_eq!(pools.len(), 1);
    assert_eq!(pools[0].id, id);
    assert_eq!(pools[0].model_column_ids, vec!["public", "second"]);
    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn an_existing_custom_pool_prevents_any_automatic_default() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    matrix_import::import(&pool, &verified(23, data()), false)
        .await
        .unwrap();
    maverick_store::tenancy::create_tenant(&pool, "replica", "replica", &[1; 32])
        .await
        .unwrap();
    maverick_store::tenancy::create_pool(
        &pool,
        "team",
        "replica",
        "team",
        &["public".into()],
        true,
    )
    .await
    .unwrap();
    let mut next = data();
    next.catalog.push(column("other"));
    matrix_import::import(&pool, &verified(24, next), false)
        .await
        .unwrap();
    let pools = maverick_store::tenancy::list_pools_by_tenant(&pool, "replica")
        .await
        .unwrap();
    assert_eq!(pools.len(), 1);
    assert_eq!(pools[0].id, "team");
    assert_eq!(pools[0].model_column_ids, vec!["public"]);
    pool.close().await;
    db.close().await;
}
fn column(id: &str) -> CatalogColumn {
    CatalogColumn {
        id: id.into(),
        provider: "openai".into(),
        model_id: format!("model-{id}"),
        endpoint: "direct".into(),
        settings: json!({}),
        status: "active".into(),
        pricing: json!({"input_per_mtok":3,"source":"list"}),
        quality_source_column_id: None,
        cold_start_until: None,
        deprecation_date: None,
        execution_aliases: json!({}),
        retention_mode: "unknown".into(),
        retention_verified_at: None,
        retention_valid_until: None,
        retention_source: None,
        execution_alias_retention: json!({}),
        param_constraints: json!({}),
    }
}
fn data() -> PublicMatrix {
    PublicMatrix {
        catalog: vec![column("public")],
        aggregates: vec![Aggregate {
            model_column_id: "public".into(),
            capability: "coding".into(),
            difficulty_tier: 1,
            dimension: "quality".into(),
            ewma: 0.75,
            variance: 0.125,
            n: 4,
            measured: true,
            matrix_version: "v1".into(),
            as_of: "2026-10-23".parse().unwrap(),
        }],
        calibration: vec![Calibration {
            batch_id: "batch".into(),
            capability: "coding".into(),
            brier_score: 0.25,
            overconfidence: 0.125,
            n: 4,
            scored_at: Utc.with_ymd_and_hms(2026, 10, 23, 3, 0, 0).unwrap(),
        }],
        checks: BTreeMap::new(),
    }
}
fn verified(day: u32, mut matrix: PublicMatrix) -> VerifiedBundle {
    for row in &mut matrix.aggregates {
        row.matrix_version = "v1".into();
    }
    let keys = SigningKeys::new(BTreeMap::from([(
        "test".into(),
        SigningKey::from_bytes(&[31; 32]),
    )]))
    .unwrap();
    let bytes = maverick_bundle::build(
        Manifest {
            format: maverick_bundle::FORMAT.into(),
            matrix_version: "v1".into(),
            published_at: Utc.with_ymd_and_hms(2026, 10, day, 3, 0, 0).unwrap(),
            origin_release: "test".into(),
            schema: SchemaRange { min: 11, max: 12 },
            files: BTreeMap::new(),
        },
        &matrix,
        &keys,
    )
    .unwrap();
    maverick_bundle::verify(&bytes, &keys.public_keys(), 12).unwrap()
}
#[tokio::test]
async fn import_installs_catalog_measurements_and_metadata_without_a_corpus() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let bundle = verified(23, data());
    assert_eq!(
        matrix_import::import(&pool, &bundle, false).await.unwrap(),
        ImportOutcome::Imported
    );
    let c = catalog::get_model_column(&pool, "public").await.unwrap();
    assert_eq!(c.model_id, "model-public");
    let active = matrix::active_bundle(&pool).await.unwrap().unwrap();
    assert_eq!(active.matrix_version, "v1");
    assert_eq!(active.published_at, bundle.manifest.published_at);
    let measured: f32 = sqlx::query_scalar("select ewma from aggregates")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(measured, 0.75);
    let brier: f32 = sqlx::query_scalar("select brier_score from calibration")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(brier, 0.25);
    let corpus_count: i64 = sqlx::query_scalar("select count(*) from corpus_versions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(corpus_count, 0);
    assert_eq!(
        matrix_import::import(&pool, &bundle, false).await.unwrap(),
        ImportOutcome::AlreadyActive
    );
    assert_eq!(matrix::active_bundle(&pool).await.unwrap().unwrap(), active);
    let count: i64 = sqlx::query_scalar("select count(*) from matrix_imports")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    pool.close().await;
    db.close().await;
}
#[tokio::test]
async fn older_is_refused_and_allow_older_reactivates_a_previously_imported_bundle() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let old = verified(23, data());
    let new = verified(24, data());
    matrix_import::import(&pool, &old, false).await.unwrap();
    matrix_import::import(&pool, &new, false).await.unwrap();
    assert!(matches!(
        matrix_import::import(&pool, &old, false).await,
        Err(ImportError::Older)
    ));
    assert_eq!(
        matrix::active_bundle(&pool)
            .await
            .unwrap()
            .unwrap()
            .published_at,
        new.manifest.published_at
    );
    matrix_import::import(&pool, &old, true).await.unwrap();
    assert_eq!(
        matrix::active_bundle(&pool)
            .await
            .unwrap()
            .unwrap()
            .published_at,
        old.manifest.published_at
    );
    let active: i64 = sqlx::query_scalar("select count(*) from matrix_imports where active")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(active, 1);
    pool.close().await;
    db.close().await;
}
#[tokio::test]
async fn local_prices_and_existing_latency_and_aggregate_rows_survive_a_new_bundle() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    matrix_import::import(&pool, &verified(23, data()), false)
        .await
        .unwrap();
    sqlx::query("update model_columns set pricing='{\"Source\":\"local\",\"input_per_mtok\":0.5}'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("insert into aggregates values ('public','coding',1,'latency_p50_ms',12,0,4,true,'v1','2026-10-23')").execute(&pool).await.unwrap();
    let mut next = data();
    next.catalog[0].status = "deprecated".into();
    next.aggregates[0].ewma = 0.1;
    let mut latency = next.aggregates[0].clone();
    latency.dimension = "latency_p50_ms".into();
    latency.ewma = 999.;
    next.aggregates.push(latency);
    let mut fresh = next.aggregates[0].clone();
    fresh.as_of = "2026-10-24".parse().unwrap();
    fresh.ewma = 0.875;
    next.aggregates.push(fresh);
    matrix_import::import(&pool, &verified(24, next), false)
        .await
        .unwrap();
    let c = catalog::get_model_column(&pool, "public").await.unwrap();
    assert_eq!(c.status, "deprecated");
    let price: serde_json::Value = serde_json::from_str(c.pricing.get()).unwrap();
    assert_eq!(price["input_per_mtok"], 0.5);
    let rows: Vec<f32> =
        sqlx::query_scalar("select ewma from aggregates where dimension='quality' order by as_of")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(rows, vec![0.75, 0.875]);
    let latency: f32 =
        sqlx::query_scalar("select ewma from aggregates where dimension='latency_p50_ms'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(latency, 12.);
    pool.close().await;
    db.close().await;
}
#[tokio::test]
async fn catalog_references_are_resolved_after_all_columns_exist_and_list_prices_refresh() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let mut initial = data();
    let mut alias = column("a-alias");
    alias.quality_source_column_id = Some("z-source".into());
    initial.catalog.extend([alias, column("z-source")]);
    matrix_import::import(&pool, &verified(23, initial), false)
        .await
        .unwrap();
    assert_eq!(
        catalog::get_model_column(&pool, "a-alias")
            .await
            .unwrap()
            .quality_source_column_id
            .as_deref(),
        Some("z-source")
    );
    let mut next = data();
    next.catalog[0].pricing = json!({"input_per_mtok":2,"source":"list"});
    matrix_import::import(&pool, &verified(24, next), false)
        .await
        .unwrap();
    let p: serde_json::Value = serde_json::from_str(
        catalog::get_model_column(&pool, "public")
            .await
            .unwrap()
            .pricing
            .get(),
    )
    .unwrap();
    assert_eq!(p["input_per_mtok"], 2);
    pool.close().await;
    db.close().await;
}
#[tokio::test]
async fn a_late_failure_rolls_back_catalog_data_and_active_metadata() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    matrix_import::import(&pool, &verified(23, data()), false)
        .await
        .unwrap();
    let before = matrix::active_bundle(&pool).await.unwrap();
    let mut broken = data();
    broken.catalog[0].model_id = "changed".into();
    broken.aggregates[0].model_column_id = "missing-column".into();
    assert!(matches!(
        matrix_import::import(&pool, &verified(24, broken), false).await,
        Err(ImportError::Database(_))
    ));
    assert_eq!(
        catalog::get_model_column(&pool, "public")
            .await
            .unwrap()
            .model_id,
        "model-public"
    );
    assert_eq!(matrix::active_bundle(&pool).await.unwrap(), before);
    let count: i64 = sqlx::query_scalar("select count(*) from matrix_imports")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    pool.close().await;
    db.close().await;
}
#[tokio::test]
async fn unauthenticated_and_mixed_version_bundles_never_write() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let mut bundle = verified(23, data());
    bundle.signing_kids.clear();
    assert!(matches!(
        matrix_import::import(&pool, &bundle, false).await,
        Err(ImportError::Unauthenticated)
    ));
    let mut bundle = verified(23, data());
    bundle.matrix.aggregates[0].matrix_version = "other".into();
    assert!(matches!(
        matrix_import::import(&pool, &bundle, false).await,
        Err(ImportError::AggregateVersion)
    ));
    let count: i64 = sqlx::query_scalar("select count(*) from model_columns")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    pool.close().await;
    db.close().await;
}
