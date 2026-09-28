//! The sqlx baseline equals the Go schema at goose head (D21, reference Part 1 §2).
//!
//! The expected schema is `parity/schema/schema.sql`, a `pg_dump --schema-only --no-owner`
//! of a database that the Go engine migrated with goose 0001 to 0004. River tables and
//! `goose_db_version` are not in it (D22).

use std::sync::LazyLock;

use maverick_store::{BASELINE_VERSION, MIGRATOR};
use maverick_testdb::Schema;
use sqlx::postgres::PgPoolOptions;

const GO_SCHEMA: &str = include_str!("../../../parity/schema/schema.sql");

/// Only the baseline, also after additive migrations exist.
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

/// Splits a pg_dump into statements. Drops comments, session settings, psql
/// meta-commands (pg_dump 16.10 and later print `\restrict`), and the bookkeeping
/// objects of sqlx and graphile_worker, which the Go dump cannot contain.
fn statements(dump: &str) -> Vec<String> {
    let mut statements = Vec::new();
    let mut current = String::new();
    for line in dump.lines().map(str::trim_end) {
        if line.is_empty() || line.starts_with("--") || line.starts_with('\\') {
            continue;
        }
        if current.is_empty()
            && (line.starts_with("SET ") || line.starts_with("SELECT pg_catalog.set_config("))
        {
            continue;
        }
        current.push_str(line);
        current.push('\n');
        if line.ends_with(';') {
            statements.push(current.trim_end().to_owned());
            current.clear();
        }
    }
    assert!(current.is_empty(), "unterminated statement: {current}");
    statements.retain(|statement| {
        !statement.contains("_sqlx_migrations") && !statement.contains("graphile_worker")
    });
    statements
}

#[test]
fn the_go_dump_splits_into_the_expected_objects() {
    let go = statements(GO_SCHEMA);
    let tables = go
        .iter()
        .filter(|statement| statement.starts_with("CREATE TABLE public."))
        .count();
    assert_eq!(tables, 20, "reference Part 1 §2.2: 20 app tables");
    assert!(
        go.iter()
            .all(|statement| !statement.contains("river") && !statement.contains("goose"))
    );
    assert!(go.iter().any(|statement| statement.contains(
        "CREATE UNIQUE INDEX model_columns_identity ON public.model_columns USING btree \
         (provider, model_id, endpoint, md5((settings)::text));"
    )));
}

#[tokio::test]
async fn the_baseline_migration_equals_the_go_schema() {
    let Some(db) = maverick_testdb::database(&BASELINE).await else {
        return;
    };
    let rust = statements(&db.dump_schema().await);
    let go = statements(GO_SCHEMA);
    db.close().await;

    if rust == go {
        return;
    }
    let missing: Vec<&String> = go.iter().filter(|s| !rust.contains(s)).collect();
    let unexpected: Vec<&String> = rust.iter().filter(|s| !go.contains(s)).collect();
    if missing.is_empty() && unexpected.is_empty() {
        panic!("the baseline has the Go statements in a different order");
    }
    let join = |list: &[&String]| {
        list.iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    panic!(
        "the baseline differs from parity/schema/schema.sql\n\n\
         missing ({}):\n{}\n\nunexpected ({}):\n{}",
        missing.len(),
        join(&missing),
        unexpected.len(),
        join(&unexpected)
    );
}
