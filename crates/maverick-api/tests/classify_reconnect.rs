//! D41: the resolver's classifier columns resolve again after startup
//! (`Resolver::reconnect`), without a restart.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use maverick_api::classify::{Connection, Resolver, ResolverSettings};
use maverick_providers::{CompletionRequest, CompletionResponse, ProviderClient, ProviderFuture};
use maverick_store::catalog::{self, NewModelColumn};
use maverick_store::{JsonRawValue, MIGRATOR};
use maverick_testdb::Schema;
use sqlx::PgPool;

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

struct Silent;

impl ProviderClient for Silent {
    fn complete<'a>(&'a self, _request: CompletionRequest) -> ProviderFuture<'a> {
        Box::pin(async move {
            Ok(CompletionResponse {
                text: String::new(),
                tokens_in: 0,
                tokens_out: 0,
            })
        })
    }

    fn name(&self) -> &str {
        "anthropic"
    }
}

async fn column(pool: &PgPool, id: &str, model: &str, endpoint: &str) {
    let empty = JsonRawValue::from_string("{}".to_owned()).expect("json");
    catalog::create_model_column(
        pool,
        &NewModelColumn {
            id,
            provider: "anthropic",
            model_id: model,
            endpoint,
            settings: &empty,
            status: "active",
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

#[tokio::test]
async fn reconnect_resolves_columns_added_after_boot() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let _ = parity_support::divergence!("D41");
    let clients: HashMap<String, Arc<dyn ProviderClient>> = HashMap::from([(
        "anthropic".to_owned(),
        Arc::new(Silent) as Arc<dyn ProviderClient>,
    )]);
    let settings = ResolverSettings {
        column: "claude-haiku-4-5/direct".to_owned(),
        zero_retention_column: Some("claude-haiku-4-5/vertex".to_owned()),
        ..ResolverSettings::default()
    };

    let resolver = Resolver::new(settings.clone()).connect(&[], &clients);
    let unresolved = Connection {
        default: false,
        zero_retention: Some(false),
    };
    assert_eq!(resolver.connection(), unresolved);
    assert!(!resolver.connection().complete());

    column(&pool, "col_direct", "claude-haiku-4-5", "direct").await;
    let columns = catalog::list_model_columns(&pool).await.expect("columns");
    let partial = resolver.reconnect(&columns, &clients);
    assert_eq!(
        partial,
        Connection {
            default: true,
            zero_retention: Some(false),
        }
    );
    assert!(
        !partial.complete(),
        "the zero-retention column is still missing"
    );

    column(&pool, "col_vertex", "claude-haiku-4-5", "vertex").await;
    let columns = catalog::list_model_columns(&pool).await.expect("columns");
    assert!(resolver.reconnect(&columns, &clients).complete());
    assert_eq!(
        resolver.connection(),
        Connection {
            default: true,
            zero_retention: Some(true),
        }
    );

    // Without a zero-retention column configured, only the default column counts.
    let plain = Resolver::new(ResolverSettings {
        zero_retention_column: None,
        ..settings
    })
    .connect(&columns, &clients);
    assert_eq!(
        plain.connection(),
        Connection {
            default: true,
            zero_retention: None,
        }
    );
    pool.close().await;
    db.close().await;
}
