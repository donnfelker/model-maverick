//! Store tests for `model_columns` (real Postgres).

use std::sync::LazyLock;

use chrono::{NaiveDate, TimeZone, Utc};
use maverick_store::catalog::{self, ModelColumn, ModelColumnUpdate, NewModelColumn};
use maverick_store::{Json, JsonRawValue, MIGRATOR, is_not_found, sqlstate};
use maverick_testdb::Schema;

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

fn json(text: &str) -> Json {
    JsonRawValue::from_string(text.to_owned()).expect("valid JSON")
}

fn new_column<'a>(
    id: &'a str,
    model_id: &'a str,
    settings: &'a JsonRawValue,
    empty: &'a JsonRawValue,
) -> NewModelColumn<'a> {
    NewModelColumn {
        id,
        provider: "anthropic",
        model_id,
        endpoint: "direct",
        settings,
        status: "cold_start",
        pricing: empty,
        quality_source_column_id: None,
        cold_start_until: None,
        deprecation_date: None,
        execution_aliases: None,
        retention_mode: None,
        retention_verified_at: None,
        retention_valid_until: None,
        retention_source: None,
        execution_alias_retention: None,
        param_constraints: None,
    }
}

fn update_from(column: &ModelColumn) -> ModelColumnUpdate<'_> {
    ModelColumnUpdate {
        settings: &column.settings,
        status: &column.status,
        pricing: &column.pricing,
        quality_source_column_id: column.quality_source_column_id.as_deref(),
        cold_start_until: column.cold_start_until,
        deprecation_date: column.deprecation_date,
        execution_aliases: None,
        retention_mode: &column.retention_mode,
        retention_verified_at: column.retention_verified_at,
        retention_valid_until: column.retention_valid_until,
        retention_source: column.retention_source.as_deref(),
        execution_alias_retention: None,
        param_constraints: None,
    }
}

fn code(result: sqlx::Result<impl Sized>) -> String {
    match result {
        Ok(_) => panic!("expected a database error"),
        Err(err) => sqlstate(&err).unwrap_or_else(|| panic!("not a database error: {err}")),
    }
}

fn ids(columns: Vec<ModelColumn>) -> Vec<String> {
    columns.into_iter().map(|c| c.id).collect()
}

#[tokio::test]
async fn a_new_column_gets_the_defaults_and_normalized_jsonb() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let empty = json("{}");
    let settings = json(r#"{"thinking":true,  "budget":  1024}"#);

    let column = catalog::create_model_column(
        &pool,
        &new_column("col_1", "claude-sonnet-4-6", &settings, &empty),
    )
    .await
    .expect("create");
    // Postgres returns jsonb in its normalized text: keys by length then bytes, canonical
    // spacing. The Go engine read the same text.
    assert_eq!(
        column.settings.get(),
        r#"{"budget": 1024, "thinking": true}"#
    );
    assert_eq!(column.execution_aliases.get(), "{}");
    assert_eq!(column.execution_alias_retention.get(), "{}");
    assert_eq!(column.param_constraints.get(), "{}");
    assert_eq!(column.retention_mode, "unknown");
    assert_eq!(column.retention_valid_until, None);
    assert_eq!(column.status, "cold_start");

    let read = catalog::get_model_column(&pool, "col_1")
        .await
        .expect("get");
    assert_eq!(read.settings.get(), column.settings.get());
    assert!(is_not_found(
        &catalog::get_model_column(&pool, "col_missing")
            .await
            .unwrap_err()
    ));
    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn the_identity_index_treats_equal_settings_as_one_column() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let empty = json("{}");
    let a = json(r#"{"a":1,"b":2}"#);
    let b = json(r#"{"b": 2, "a": 1}"#);
    catalog::create_model_column(&pool, &new_column("col_1", "m", &a, &empty))
        .await
        .expect("create");
    assert_eq!(
        code(catalog::create_model_column(&pool, &new_column("col_2", "m", &b, &empty)).await),
        "23505",
        "model_columns_identity hashes the normalized settings text"
    );
    let other = json(r#"{"a":1}"#);
    catalog::create_model_column(&pool, &new_column("col_3", "m", &other, &empty))
        .await
        .expect("different settings are a different column");

    let mut bad = new_column("col_4", "m2", &empty, &empty);
    bad.provider = "mistral";
    assert_eq!(
        code(catalog::create_model_column(&pool, &bad).await),
        "23514"
    );
    let mut bad = new_column("col_5", "m3", &empty, &empty);
    bad.retention_mode = Some("zeroish");
    assert_eq!(
        code(catalog::create_model_column(&pool, &bad).await),
        "23514"
    );
    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn columns_are_listed_by_id_status_and_ids() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let empty = json("{}");
    for (id, model, status) in [
        ("col_c", "m-c", "active"),
        ("col_a", "m-a", "retired"),
        ("col_b", "m-b", "active"),
    ] {
        let mut column = new_column(id, model, &empty, &empty);
        column.status = status;
        catalog::create_model_column(&pool, &column)
            .await
            .expect("create");
    }

    assert_eq!(
        ids(catalog::list_model_columns(&pool).await.expect("list")),
        ["col_a", "col_b", "col_c"]
    );
    let statuses = |s: &[&str]| s.iter().map(|x| (*x).to_owned()).collect::<Vec<_>>();
    assert_eq!(
        ids(
            catalog::list_model_columns_by_statuses(&pool, &statuses(&["active"]))
                .await
                .expect("by status")
        ),
        ["col_b", "col_c"]
    );
    assert_eq!(
        ids(
            catalog::list_model_columns_by_statuses(&pool, &statuses(&["active", "retired"]))
                .await
                .expect("by status")
        ),
        ["col_a", "col_b", "col_c"]
    );
    assert!(
        catalog::list_model_columns_by_statuses(&pool, &[])
            .await
            .expect("no statuses")
            .is_empty()
    );
    // Input order does not matter and unknown ids are absent.
    assert_eq!(
        ids(catalog::list_model_columns_by_ids(
            &pool,
            &statuses(&["col_c", "col_missing", "col_a"])
        )
        .await
        .expect("by ids")),
        ["col_a", "col_c"]
    );
    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn update_writes_every_field_and_keeps_maps_on_none() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let empty = json("{}");
    let aliases = json(r#"{"openrouter":"anthropic/claude-sonnet-4-6"}"#);
    let primary = catalog::create_model_column(&pool, &new_column("col_p", "m", &empty, &empty))
        .await
        .expect("primary");

    let mut column = new_column("col_1", "m", &empty, &empty);
    column.endpoint = "vertex";
    column.execution_aliases = Some(&aliases);
    column.quality_source_column_id = Some("col_p");
    column.cold_start_until = NaiveDate::from_ymd_opt(2026, 10, 1);
    let created = catalog::create_model_column(&pool, &column)
        .await
        .expect("create");
    assert_eq!(created.quality_source_column_id.as_deref(), Some("col_p"));
    assert_eq!(
        created.execution_aliases.get(),
        r#"{"openrouter": "anthropic/claude-sonnet-4-6"}"#
    );

    // None on a nullable field clears it; None on a map keeps it.
    let verified = Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap();
    let valid = Utc.with_ymd_and_hms(2026, 11, 30, 0, 0, 0).unwrap();
    let pricing = json(r#"{"input_per_mtok": 3.00, "output_per_mtok": 15.00}"#);
    let mut update = update_from(&created);
    update.status = "active";
    update.pricing = &pricing;
    update.quality_source_column_id = None;
    update.cold_start_until = None;
    update.retention_mode = "zero";
    update.retention_verified_at = Some(verified);
    update.retention_valid_until = Some(valid);
    update.retention_source = Some("https://example.test/zdr");
    let updated = catalog::update_model_column(&pool, "col_1", &update)
        .await
        .expect("update");
    assert_eq!(updated.status, "active");
    assert_eq!(
        updated.pricing.get(),
        r#"{"input_per_mtok": 3.00, "output_per_mtok": 15.00}"#
    );
    assert_eq!(updated.quality_source_column_id, None);
    assert_eq!(updated.cold_start_until, None);
    assert_eq!(
        updated.execution_aliases.get(),
        created.execution_aliases.get()
    );
    assert_eq!(updated.retention_mode, "zero");
    assert_eq!(updated.retention_verified_at, Some(verified));
    assert_eq!(updated.retention_valid_until, Some(valid));
    assert_eq!(
        updated.retention_source.as_deref(),
        Some("https://example.test/zdr")
    );
    assert_eq!(updated.created_at, created.created_at);
    assert!(updated.updated_at >= created.updated_at);

    // A map value replaces the stored map whole.
    let alias_retention = json(r#"{"openrouter":{"retention_mode":"zero"}}"#);
    let constraints =
        json(r#"{"reasoning_effort":{"allowed":["low","high"],"order":["low","medium","high"]}}"#);
    let mut update = update_from(&updated);
    update.execution_aliases = Some(&empty);
    update.execution_alias_retention = Some(&alias_retention);
    update.param_constraints = Some(&constraints);
    let replaced = catalog::update_model_column(&pool, "col_1", &update)
        .await
        .expect("replace maps");
    assert_eq!(replaced.execution_aliases.get(), "{}");
    assert_eq!(
        replaced.execution_alias_retention.get(),
        r#"{"openrouter": {"retention_mode": "zero"}}"#
    );
    assert_eq!(
        replaced.param_constraints.get(),
        r#"{"reasoning_effort": {"order": ["low", "medium", "high"], "allowed": ["low", "high"]}}"#
    );

    assert!(is_not_found(
        &catalog::update_model_column(&pool, "col_missing", &update_from(&primary))
            .await
            .unwrap_err()
    ));
    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn delete_removes_a_column_unless_another_references_it() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let empty = json("{}");
    catalog::create_model_column(&pool, &new_column("col_p", "m", &empty, &empty))
        .await
        .expect("primary");
    let mut secondary = new_column("col_s", "m", &empty, &empty);
    secondary.endpoint = "bedrock";
    secondary.quality_source_column_id = Some("col_p");
    catalog::create_model_column(&pool, &secondary)
        .await
        .expect("secondary");

    assert_eq!(
        code(catalog::delete_model_column(&pool, "col_p").await),
        "23503"
    );
    catalog::delete_model_column(&pool, "col_s")
        .await
        .expect("delete");
    catalog::delete_model_column(&pool, "col_p")
        .await
        .expect("delete");
    assert!(
        catalog::list_model_columns(&pool)
            .await
            .expect("list")
            .is_empty()
    );
    pool.close().await;
    db.close().await;
}
