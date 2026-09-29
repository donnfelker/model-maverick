//! The catalog lookups the jobs build on (reference Part 3 §6 "catalog"): the provider
//! client for a column, and the column lookups by id and lifecycle state.

use std::sync::{Arc, LazyLock};

use maverick_providers::fake::FakeProvider;
use maverick_runner::{BatchColumn, Clients};
use maverick_store::catalog::{self, NewModelColumn};
use maverick_store::{JsonRawValue, MIGRATOR};
use maverick_testdb::Schema;

/// parity: catalog.TestMain
static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

fn batch_column(id: &str, provider: &str) -> BatchColumn {
    BatchColumn {
        id: id.to_owned(),
        provider: provider.to_owned(),
        model_id: format!("model-{id}"),
        status: "active".to_owned(),
        settings_json: "{}".to_owned(),
        pricing_json: "{}".to_owned(),
    }
}

/// Go's `Clients{openai: nil}` case cannot be built in Rust: a registered client is an
/// `Arc`, never null. A provider with no registered client is the same error.
///
/// parity: catalog.TestClientsFor
#[test]
fn clients_for_a_column() {
    let fake = Arc::new(FakeProvider::new());
    let clients = Clients::new().with("anthropic", fake);
    assert!(
        clients
            .for_column(&batch_column("col_a", "anthropic"))
            .is_ok()
    );
    assert_eq!(
        clients
            .for_column(&batch_column("col_o", "openai"))
            .err()
            .as_deref(),
        Some(r#"no provider client configured for "openai" (column col_o)"#)
    );
    assert!(
        Clients::new()
            .for_column(&batch_column("col_o", "openai"))
            .is_err()
    );
}

/// parity: catalog.TestColumnLookupByIDAndLifecycleState
#[tokio::test]
async fn column_lookup_by_id_and_lifecycle_state() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let empty = JsonRawValue::from_string("{}".to_owned()).expect("json");
    for status in ["cold_start", "active", "deprecated", "retired"] {
        let id = format!("col_{status}");
        catalog::create_model_column(
            &pool,
            &NewModelColumn {
                id: &id,
                provider: "anthropic",
                model_id: &format!("claude-{status}"),
                endpoint: "direct",
                settings: &empty,
                status,
                pricing: &empty,
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
            },
        )
        .await
        .expect("column");
    }

    let active = catalog::get_model_column(&pool, "col_active")
        .await
        .expect("by id");
    assert_eq!(
        (active.id.as_str(), active.status.as_str()),
        ("col_active", "active")
    );

    let runnable: Vec<String> = catalog::list_model_columns(&pool)
        .await
        .expect("columns")
        .into_iter()
        .filter(|c| maverick_domain::catalog::runnable(&c.status))
        .map(|c| c.id)
        .collect();
    assert_eq!(runnable, ["col_active", "col_cold_start", "col_deprecated"]);

    let deprecated: Vec<(String, String)> =
        catalog::list_model_columns_by_statuses(&pool, &["deprecated".to_owned()])
            .await
            .expect("by status")
            .into_iter()
            .map(|c| (c.id, c.status))
            .collect();
    assert_eq!(
        deprecated,
        [("col_deprecated".to_owned(), "deprecated".to_owned())]
    );
    pool.close().await;
    db.close().await;
}
