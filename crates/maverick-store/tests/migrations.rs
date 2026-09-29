//! The additive migrations on top of the baseline (D37, spec v1 §3).
//!
//! The catalog after all migrations, minus the catalog after the baseline alone, must be
//! exactly the objects listed here. The baseline itself is proven equal to the Go schema
//! in `schema_parity.rs`. Nothing is dropped or renamed except the widened outcomes CHECK.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use maverick_store::{BASELINE_VERSION, MIGRATOR};
use maverick_testdb::{Schema, TestDatabase};
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::{Connection, PgConnection};

static ALL: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

static BASELINE: LazyLock<Schema> = LazyLock::new(|| {
    let baseline = MIGRATOR
        .iter()
        .find(|migration| migration.version == BASELINE_VERSION)
        .expect("the baseline migration is embedded");
    let key = format!("maverick-store baseline {:?}", baseline.checksum);
    Schema::custom(key, |url| async move {
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await?;
        MIGRATOR.run_to(BASELINE_VERSION, &pool).await?;
        pool.close().await;
        Ok(())
    })
});

/// Catalog entries that the additive migrations create.
const ADDED: &[&str] = &[
    // 0002_batches.sql
    "table batches",
    "column batches.id text not null",
    "column batches.kind text not null",
    "column batches.batch_window text not null",
    "column batches.status text not null default 'running'::text",
    "column batches.lease_owner text null",
    "column batches.lease_expires_at timestamp with time zone null",
    "column batches.heartbeat_at timestamp with time zone null",
    "column batches.attempt integer not null default 1",
    "column batches.completed_at timestamp with time zone null",
    "column batches.created_at timestamp with time zone not null default now()",
    "column batches.updated_at timestamp with time zone not null default now()",
    "constraint batches.batches_pkey PRIMARY KEY (id)",
    "constraint batches.batches_kind_batch_window_key UNIQUE (kind, batch_window)",
    "constraint batches.batches_status_check CHECK ((status = ANY (ARRAY['running'::text, \
     'completed'::text, 'skipped'::text])))",
    "index batches_pkey CREATE UNIQUE INDEX batches_pkey ON public.batches USING btree (id)",
    "index batches_kind_batch_window_key CREATE UNIQUE INDEX batches_kind_batch_window_key \
     ON public.batches USING btree (kind, batch_window)",
    // 0003_model_columns_retention.sql
    "column model_columns.retention_mode text not null default 'unknown'::text",
    "column model_columns.retention_verified_at timestamp with time zone null",
    "column model_columns.retention_valid_until timestamp with time zone null",
    "column model_columns.retention_source text null",
    "column model_columns.execution_alias_retention jsonb not null default '{}'::jsonb",
    "constraint model_columns.model_columns_retention_mode_check CHECK ((retention_mode = \
     ANY (ARRAY['unknown'::text, 'standard'::text, 'zero'::text])))",
    // 0004_model_columns_param_constraints.sql
    "column model_columns.param_constraints jsonb not null default '{}'::jsonb",
    // 0005_debug_captures_capture_key_version.sql
    "column debug_captures.capture_key_version integer not null",
    // 0006_outcomes_completed_unchecked.sql (the widened set; the old one is in REMOVED)
    "constraint outcomes.outcomes_outcome_check CHECK ((outcome = ANY (ARRAY['success'::text, \
     'parse_failure'::text, 'quality_insufficient'::text, 'latency_exceeded'::text, \
     'provider_error'::text, 'completed_unchecked'::text, 'other'::text])))",
    // 0007_tenants_classification_quota_override.sql
    "column tenants.classification_quota_override integer null",
    // 0008_classifier_usage.sql
    "table classifier_usage",
    "column classifier_usage.usage_date date not null",
    "column classifier_usage.tenant_id text not null",
    "column classifier_usage.calls integer not null default 0",
    "column classifier_usage.spend_usd numeric(12,6) not null default 0",
    "column classifier_usage.updated_at timestamp with time zone not null default now()",
    "constraint classifier_usage.classifier_usage_pkey PRIMARY KEY (usage_date, tenant_id)",
    "constraint classifier_usage.classifier_usage_tenant_id_fkey FOREIGN KEY (tenant_id) \
     REFERENCES tenants(id)",
    "index classifier_usage_pkey CREATE UNIQUE INDEX classifier_usage_pkey ON \
     public.classifier_usage USING btree (usage_date, tenant_id)",
    // 0009_tenant_constraints_telemetry_opt_out.sql
    "column tenant_constraints.telemetry_opt_out boolean not null default false",
];

/// Catalog entries that the additive migrations replace.
const REMOVED: &[&str] = &[
    "constraint outcomes.outcomes_outcome_check CHECK ((outcome = ANY (ARRAY['success'::text, \
     'parse_failure'::text, 'quality_insufficient'::text, 'latency_exceeded'::text, \
     'provider_error'::text, 'other'::text])))",
];

/// Every table, column, constraint, and index in schema `public`, one line each.
async fn catalog(conn: &mut PgConnection) -> BTreeSet<String> {
    let tables: Vec<(String,)> = sqlx::query_as(
        "SELECT c.relname::text FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 'public' AND c.relkind = 'r'",
    )
    .fetch_all(&mut *conn)
    .await
    .expect("tables");
    let columns: Vec<(String, String, String, bool, Option<String>)> = sqlx::query_as(
        "SELECT c.relname::text, a.attname::text, format_type(a.atttypid, a.atttypmod), \
                a.attnotnull, pg_get_expr(d.adbin, d.adrelid) \
         FROM pg_attribute a \
         JOIN pg_class c ON c.oid = a.attrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
         WHERE n.nspname = 'public' AND c.relkind = 'r' AND a.attnum > 0 \
           AND NOT a.attisdropped",
    )
    .fetch_all(&mut *conn)
    .await
    .expect("columns");
    let constraints: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT c.relname::text, k.conname::text, pg_get_constraintdef(k.oid) \
         FROM pg_constraint k \
         JOIN pg_class c ON c.oid = k.conrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 'public'",
    )
    .fetch_all(&mut *conn)
    .await
    .expect("constraints");
    let indexes: Vec<(String, String)> = sqlx::query_as(
        "SELECT indexname::text, indexdef FROM pg_indexes WHERE schemaname = 'public'",
    )
    .fetch_all(&mut *conn)
    .await
    .expect("indexes");

    let mut lines = BTreeSet::new();
    lines.extend(tables.into_iter().map(|(table,)| format!("table {table}")));
    lines.extend(
        columns
            .into_iter()
            .map(|(table, column, kind, not_null, default)| {
                let null = if not_null { "not null" } else { "null" };
                match default {
                    Some(default) => {
                        format!("column {table}.{column} {kind} {null} default {default}")
                    }
                    None => format!("column {table}.{column} {kind} {null}"),
                }
            }),
    );
    lines.extend(
        constraints
            .into_iter()
            .map(|(table, name, def)| format!("constraint {table}.{name} {def}")),
    );
    lines.extend(
        indexes
            .into_iter()
            .map(|(name, def)| format!("index {name} {def}")),
    );
    lines.retain(|line| !line.contains("_sqlx_migrations"));
    lines
}

async fn catalog_of(db: &TestDatabase) -> BTreeSet<String> {
    let mut conn = db.connect().await;
    let lines = catalog(&mut conn).await;
    conn.close().await.expect("close");
    lines
}

/// Prints each expected and actual entry that disagree.
fn assert_same(label: &str, actual: &BTreeSet<String>, expected: &BTreeSet<String>) {
    let missing: Vec<&String> = expected.difference(actual).collect();
    let unexpected: Vec<&String> = actual.difference(expected).collect();
    assert!(
        missing.is_empty() && unexpected.is_empty(),
        "{label}\n\nexpected but missing ({}):\n{}\n\npresent but not expected ({}):\n{}",
        missing.len(),
        missing
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        unexpected.len(),
        unexpected
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
    );
}

// parity: store.TestMigrationsUpDownUpClean
// D37: migrations are up only. This test and the next replace the Go up/down/up test.
#[tokio::test]
async fn the_additive_migrations_add_exactly_the_spec_objects() {
    let Some(baseline) = maverick_testdb::database(&BASELINE).await else {
        return;
    };
    let full = maverick_testdb::database(&ALL)
        .await
        .expect("server already running");
    let before = catalog_of(&baseline).await;
    let after = catalog_of(&full).await;
    baseline.close().await;
    full.close().await;

    let added: BTreeSet<String> = after.difference(&before).cloned().collect();
    let removed: BTreeSet<String> = before.difference(&after).cloned().collect();
    let expected_added = ADDED.iter().map(|s| (*s).to_owned()).collect();
    let expected_removed = REMOVED.iter().map(|s| (*s).to_owned()).collect();
    assert_same("objects added after the baseline", &added, &expected_added);
    assert_same(
        "objects removed after the baseline",
        &removed,
        &expected_removed,
    );

    let tables = after
        .iter()
        .filter(|line| line.starts_with("table "))
        .count();
    assert_eq!(tables, 22, "20 Go tables plus batches and classifier_usage");
}

async fn history(pool: &PgPool) -> Vec<(i64, Vec<u8>)> {
    sqlx::query_as("SELECT version, checksum FROM _sqlx_migrations ORDER BY version")
        .fetch_all(pool)
        .await
        .expect("migration history")
}

// parity: store.TestMigrationsUpDownUpClean (D37, second half)
#[tokio::test]
async fn a_second_run_applies_nothing_and_changes_nothing() {
    let Some(db) = maverick_testdb::database(&ALL).await else {
        return;
    };
    let pool = db.pool().await;
    let applied = history(&pool).await;
    assert_eq!(applied.len(), MIGRATOR.iter().count());
    let before = catalog_of(&db).await;

    MIGRATOR.run(&pool).await.expect("second run");

    assert_eq!(history(&pool).await, applied);
    assert_same(
        "catalog after a second run",
        &catalog_of(&db).await,
        &before,
    );
    pool.close().await;
    db.close().await;
}

/// Seeds a tenant, an API key, and a recommendation, and returns the recommendation id.
async fn seed_recommendation(conn: &mut PgConnection) -> String {
    sqlx::raw_sql(
        "INSERT INTO tenants (id, name, prompt_hmac_key) VALUES ('ten_1', 'Acme', '\\x00');
         INSERT INTO api_keys (id, tenant_id, key_hash, last4) VALUES ('key_1', 'ten_1', '\\x01', 'abcd');
         INSERT INTO recommendations (id, tenant_id, api_key_id, request, classification, \
             candidates, response, matrix_version)
           VALUES ('rec_1', 'ten_1', 'key_1', '{}', '{}', '[]', '{}', '2026-07-01');",
    )
    .execute(&mut *conn)
    .await
    .expect("seed");
    "rec_1".to_owned()
}

fn sqlstate(err: &sqlx::Error) -> Option<String> {
    err.as_database_error()
        .and_then(|db| db.code())
        .map(|code| code.into_owned())
}

#[tokio::test]
async fn outcomes_accept_completed_unchecked_and_still_reject_unknown_values() {
    let Some(db) = maverick_testdb::database(&ALL).await else {
        return;
    };
    let mut conn = db.connect().await;
    let rec = seed_recommendation(&mut conn).await;
    let insert = "INSERT INTO outcomes (id, recommendation_id, model_used, outcome) \
                  VALUES ($1, $2, 'col_1', $3)";

    sqlx::query(insert)
        .bind("out_1")
        .bind(&rec)
        .bind("completed_unchecked")
        .execute(&mut conn)
        .await
        .expect("completed_unchecked is allowed (D16)");
    let endpoint: String =
        sqlx::query_scalar("SELECT endpoint_used FROM outcomes WHERE id = 'out_1'")
            .fetch_one(&mut conn)
            .await
            .expect("read back");
    assert_eq!(endpoint, "direct");

    let err = sqlx::query(insert)
        .bind("out_2")
        .bind(&rec)
        .bind("completed")
        .execute(&mut conn)
        .await
        .expect_err("an unknown outcome is rejected");
    assert_eq!(sqlstate(&err).as_deref(), Some("23514"), "{err}");

    conn.close().await.expect("close");
    db.close().await;
}

#[tokio::test]
async fn batches_reject_a_second_row_for_the_same_kind_and_window() {
    let Some(db) = maverick_testdb::database(&ALL).await else {
        return;
    };
    let mut conn = db.connect().await;
    let insert = "INSERT INTO batches (id, kind, batch_window) VALUES ($1, $2, $3)";
    sqlx::query(insert)
        .bind("bat_1")
        .bind("nightly_smoke")
        .bind("2026-09-28")
        .execute(&mut conn)
        .await
        .expect("first claim");
    let (status, attempt): (String, i32) =
        sqlx::query_as("SELECT status, attempt FROM batches WHERE id = 'bat_1'")
            .fetch_one(&mut conn)
            .await
            .expect("read back");
    assert_eq!((status.as_str(), attempt), ("running", 1));

    let err = sqlx::query(insert)
        .bind("bat_2")
        .bind("nightly_smoke")
        .bind("2026-09-28")
        .execute(&mut conn)
        .await
        .expect_err("same kind and window");
    assert_eq!(sqlstate(&err).as_deref(), Some("23505"), "{err}");

    sqlx::query(insert)
        .bind("bat_3")
        .bind("nightly_smoke")
        .bind("2026-09-29")
        .execute(&mut conn)
        .await
        .expect("the next window is a new batch");

    conn.close().await.expect("close");
    db.close().await;
}

#[tokio::test]
async fn the_classifier_quota_upsert_stops_at_the_quota() {
    let Some(db) = maverick_testdb::database(&ALL).await else {
        return;
    };
    let mut conn = db.connect().await;
    seed_recommendation(&mut conn).await;
    // The counting pattern that 0008_classifier_usage.sql documents.
    let count = "INSERT INTO classifier_usage (usage_date, tenant_id, calls) \
                 VALUES ('2026-09-28', 'ten_1', 1) \
                 ON CONFLICT (usage_date, tenant_id) \
                 DO UPDATE SET calls = classifier_usage.calls + 1, updated_at = now() \
                 WHERE classifier_usage.calls < $1 \
                 RETURNING calls";
    let quota = 2_i32;
    let mut granted = Vec::new();
    for _ in 0..3 {
        let calls: Option<i32> = sqlx::query_scalar(count)
            .bind(quota)
            .fetch_optional(&mut conn)
            .await
            .expect("count");
        granted.push(calls);
    }
    assert_eq!(granted, vec![Some(1), Some(2), None]);

    conn.close().await.expect("close");
    db.close().await;
}
