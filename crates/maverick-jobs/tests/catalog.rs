//! price_sync, catalog_sync, and the retirement guard on real Postgres (reference Part 3
//! §2.13, §5, and the §6 catalog cases; spec §4.5 retention_expiring).

use std::collections::HashMap;
use std::sync::LazyLock;

use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use maverick_domain::pricing::{Pricing, parse_pricing};
use maverick_jobs::catalog::{
    self, CatalogDeps, CatalogError, CatalogSyncOutput, ModelsDevPriceMapping,
    ModelsDevPriceSource, PriceKey, PriceSource, PriceSyncOutput, StaticPrices,
};
use maverick_jobs::{DayArgs, JobsError};
use maverick_store::alerts::{Alert, list_alerts_by_kind};
use maverick_store::catalog::{NewModelColumn, create_model_column, get_model_column};
use maverick_store::measurement::{AggregateUpsert, upsert_aggregate};
use maverick_store::{Decimal, JsonRawValue, MIGRATOR, tenancy};
use maverick_testdb::Schema;
use serde_json::{Value, json};
use sqlx::PgPool;

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

fn raw(text: &str) -> Box<JsonRawValue> {
    JsonRawValue::from_string(text.to_owned()).expect("json")
}

fn date(text: &str) -> NaiveDate {
    text.parse().expect("date")
}

/// 06:10 UTC on `day`.
fn at(day: NaiveDate) -> DateTime<Utc> {
    Utc.from_utc_datetime(&day.and_hms_opt(6, 10, 0).expect("time"))
}

/// A column to create. Owned, so the store's borrowed row can point into it.
struct Column {
    id: &'static str,
    provider: &'static str,
    model_id: &'static str,
    endpoint: &'static str,
    status: &'static str,
    pricing: &'static str,
    cold_start_until: Option<NaiveDate>,
    deprecation_date: Option<NaiveDate>,
    retention_mode: &'static str,
    retention_valid_until: Option<DateTime<Utc>>,
    alias_retention: String,
}

impl Column {
    fn new(id: &'static str, status: &'static str) -> Self {
        Column {
            id,
            provider: "anthropic",
            model_id: id,
            endpoint: "direct",
            status,
            pricing: "{}",
            cold_start_until: None,
            deprecation_date: None,
            retention_mode: "unknown",
            retention_valid_until: None,
            alias_retention: "{}".to_owned(),
        }
    }

    async fn create(self, pool: &PgPool) {
        let settings = raw("{}");
        let pricing = raw(self.pricing);
        let alias_retention = raw(&self.alias_retention);
        create_model_column(
            pool,
            &NewModelColumn {
                id: self.id,
                provider: self.provider,
                model_id: self.model_id,
                endpoint: self.endpoint,
                settings: &settings,
                status: self.status,
                pricing: &pricing,
                quality_source_column_id: None,
                cold_start_until: self.cold_start_until,
                deprecation_date: self.deprecation_date,
                execution_aliases: None,
                retention_mode: Some(self.retention_mode),
                retention_verified_at: self.retention_valid_until.map(|u| u - Duration::days(60)),
                retention_valid_until: self.retention_valid_until,
                retention_source: None,
                execution_alias_retention: Some(&alias_retention),
                param_constraints: None,
            },
        )
        .await
        .expect("column");
    }
}

async fn aggregate(
    pool: &PgPool,
    column: &str,
    dimension: &str,
    ewma: f32,
    measured: bool,
    as_of: NaiveDate,
) {
    upsert_aggregate(
        pool,
        &AggregateUpsert {
            model_column_id: column,
            capability: "coding",
            difficulty_tier: 3,
            dimension,
            ewma,
            variance: 0.0,
            n: 3,
            measured,
            matrix_version: "2031-01-01",
            as_of,
        },
    )
    .await
    .expect("aggregate");
}

async fn tenant(pool: &PgPool, id: &str) {
    tenancy::create_tenant(pool, id, id, &[1; 32])
        .await
        .expect("tenant");
}

async fn stored_pool(pool: &PgPool, tenant_id: &str, name: &str, columns: &[&str]) {
    let columns: Vec<String> = columns.iter().map(|c| (*c).to_owned()).collect();
    tenancy::create_pool(
        pool,
        &format!("pool_{tenant_id}_{name}"),
        tenant_id,
        name,
        &columns,
        false,
    )
    .await
    .expect("pool");
}

async fn constraints(pool: &PgPool, tenant_id: &str, max_cost: Option<&str>, zero_retention: bool) {
    tenancy::upsert_tenant_constraints(
        pool,
        tenant_id,
        max_cost.map(|c| c.parse::<Decimal>().expect("decimal")),
        None,
        zero_retention,
        false,
    )
    .await
    .expect("constraints");
}

async fn alerts(pool: &PgPool, kind: &str) -> Vec<Alert> {
    list_alerts_by_kind(pool, kind).await.expect("alerts")
}

fn subject(alert: &Alert) -> Value {
    serde_json::from_str(alert.subject.get()).expect("subject")
}

fn deps<'a>(pool: &'a PgPool, source: Option<&'a dyn PriceSource>) -> CatalogDeps<'a> {
    CatalogDeps {
        pool,
        notifier: None,
        price_source: source,
    }
}

fn list_price(input: f64, output: f64, as_of: &str) -> Pricing {
    Pricing {
        input_per_mtok: input,
        output_per_mtok: output,
        currency: "USD".to_owned(),
        as_of: as_of.to_owned(),
        source: "list".to_owned(),
    }
}

/// parity: jobs.TestPriceSyncUpdatesSnapshotAndIsIdempotent
#[tokio::test]
async fn price_sync_updates_snapshot_and_is_idempotent() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = &db.pool().await;
    Column {
        model_id: "claude-pricesync",
        pricing: r#"{"input_per_mtok":3,"output_per_mtok":15,"currency":"USD","as_of":"2031-01-01","source":"old"}"#,
        ..Column::new("col_sourced", "active")
    }
    .create(pool)
    .await;
    Column {
        provider: "openai",
        pricing: r#"{"input_per_mtok":1,"output_per_mtok":2,"currency":"USD"}"#,
        ..Column::new("col_unsourced", "active")
    }
    .create(pool)
    .await;
    let entry = list_price(4.0, 20.0, "2031-07-01");
    let source = StaticPrices(HashMap::from([(
        PriceKey::new("anthropic", "claude-pricesync", "direct"),
        entry.clone(),
    )]));
    let unsourced_before = get_model_column(pool, "col_unsourced").await.unwrap();
    let before = get_model_column(pool, "col_sourced").await.unwrap();

    let output = catalog::price_sync(&deps(pool, Some(&source)), date("2031-07-10"))
        .await
        .expect("price_sync");
    assert_eq!(
        output,
        PriceSyncOutput {
            columns: 2,
            updated: 1,
            missing_from_source: 1,
        }
    );
    let after = get_model_column(pool, "col_sourced").await.unwrap();
    assert_eq!(parse_pricing(after.pricing.get()).unwrap(), entry);
    assert!(after.updated_at > before.updated_at);
    let unsourced = get_model_column(pool, "col_unsourced").await.unwrap();
    assert_eq!(unsourced.pricing.get(), unsourced_before.pricing.get());
    assert_eq!(unsourced.updated_at, unsourced_before.updated_at);

    let again = catalog::price_sync(&deps(pool, Some(&source)), date("2031-07-11"))
        .await
        .expect("price_sync again");
    assert_eq!(again.updated, 0);
    let unchanged = get_model_column(pool, "col_sourced").await.unwrap();
    assert_eq!(unchanged.updated_at, after.updated_at);
}

/// parity: jobs.TestPriceSyncWithoutSourceIsANoOp
#[tokio::test]
async fn price_sync_without_source_is_a_no_op() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = &db.pool().await;
    Column {
        pricing: r#"{"input_per_mtok":3,"output_per_mtok":15,"currency":"USD"}"#,
        ..Column::new("col_a", "active")
    }
    .create(pool)
    .await;
    let before = get_model_column(pool, "col_a").await.unwrap();
    let output = catalog::price_sync(&deps(pool, None), date("2031-07-10"))
        .await
        .expect("price_sync");
    assert_eq!(output, PriceSyncOutput::default());
    let after = get_model_column(pool, "col_a").await.unwrap();
    assert_eq!(after.pricing.get(), before.pricing.get());
}

/// A missing `as_of` takes the job day; unreadable stored pricing is replaced; retired
/// columns are not runnable, so they are not counted.
#[tokio::test]
async fn price_sync_fills_as_of_and_replaces_unreadable_pricing() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = &db.pool().await;
    Column {
        pricing: r#"{"input_per_mtok":"not a number"}"#,
        ..Column::new("col_broken", "cold_start")
    }
    .create(pool)
    .await;
    Column::new("col_retired", "retired").create(pool).await;
    let source = StaticPrices(HashMap::from([
        (
            PriceKey::new("anthropic", "col_broken", "direct"),
            list_price(0.5, 1.5, ""),
        ),
        (
            PriceKey::new("anthropic", "col_retired", "direct"),
            list_price(9.0, 9.0, "2031-01-01"),
        ),
    ]));
    let output = catalog::price_sync(&deps(pool, Some(&source)), date("2031-07-10"))
        .await
        .expect("price_sync");
    assert_eq!(
        output,
        PriceSyncOutput {
            columns: 1,
            updated: 1,
            missing_from_source: 0,
        }
    );
    let stored = get_model_column(pool, "col_broken").await.unwrap();
    let stored: Value = serde_json::from_str(stored.pricing.get()).unwrap();
    assert_eq!(
        stored,
        json!({"input_per_mtok": 0.5, "output_per_mtok": 1.5, "currency": "USD", "as_of": "2031-07-10", "source": "list"})
    );
    let retired = get_model_column(pool, "col_retired").await.unwrap();
    assert_eq!(retired.pricing.get(), "{}");
}

#[test]
fn models_dev_payload_uses_explicit_mapping_and_validation() {
    let source = ModelsDevPriceSource::new(
        "https://models.dev/api.json",
        [
            ModelsDevPriceMapping::new(
                "anthropic",
                "internal-sonnet",
                "direct",
                "anthropic",
                "claude-sonnet-4-5",
            ),
            ModelsDevPriceMapping::new(
                "anthropic",
                "internal-sonnet",
                "openrouter",
                "openrouter",
                "anthropic/claude-sonnet-4.5",
            ),
            ModelsDevPriceMapping::new("openai", "chat-default", "direct", "openai", "gpt-4.1"),
            ModelsDevPriceMapping::new(
                "anthropic",
                "invalid-negative",
                "direct",
                "anthropic",
                "negative-price",
            ),
            ModelsDevPriceMapping::new(
                "anthropic",
                "missing-source-model",
                "direct",
                "anthropic",
                "not-present",
            ),
        ],
    );

    let prices = source
        .parse_payload(include_str!("fixtures/models_dev_api.json"))
        .expect("fixture payload");

    assert_eq!(prices.len(), 3);
    assert_eq!(
        prices
            .get(&PriceKey::new("anthropic", "internal-sonnet", "direct"))
            .expect("direct"),
        &Pricing {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
            currency: "USD".to_owned(),
            as_of: String::new(),
            source: "models.dev".to_owned(),
        }
    );
    assert_eq!(
        prices
            .get(&PriceKey::new("anthropic", "internal-sonnet", "openrouter"))
            .expect("openrouter")
            .output_per_mtok,
        17.0
    );
    assert_eq!(
        prices
            .get(&PriceKey::new("openai", "chat-default", "direct"))
            .expect("openai")
            .as_of,
        "2031-07-01"
    );
    assert!(!prices.contains_key(&PriceKey::new("anthropic", "invalid-negative", "direct")));
    assert!(!prices.contains_key(&PriceKey::new(
        "anthropic",
        "missing-source-model",
        "direct"
    )));
}

#[tokio::test]
async fn price_sync_uses_models_dev_mapping_and_endpoint() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = &db.pool().await;
    Column {
        id: "col_direct",
        model_id: "internal-sonnet",
        pricing: r#"{"input_per_mtok":2,"output_per_mtok":10,"currency":"USD","as_of":"2031-07-09","source":"old"}"#,
        ..Column::new("col_direct", "active")
    }
    .create(pool)
    .await;
    Column {
        id: "col_openrouter",
        model_id: "internal-sonnet",
        endpoint: "openrouter",
        pricing: r#"{"input_per_mtok":2,"output_per_mtok":10,"currency":"USD","as_of":"2031-07-09","source":"old"}"#,
        ..Column::new("col_openrouter", "active")
    }
    .create(pool)
    .await;
    Column {
        id: "col_unmapped",
        model_id: "internal-unmapped",
        pricing: r#"{"input_per_mtok":1,"output_per_mtok":1,"currency":"USD","as_of":"2031-07-09","source":"old"}"#,
        ..Column::new("col_unmapped", "active")
    }
    .create(pool)
    .await;
    let source = ModelsDevPriceSource::new(
        "https://models.dev/api.json",
        [
            ModelsDevPriceMapping::new(
                "anthropic",
                "internal-sonnet",
                "direct",
                "anthropic",
                "claude-sonnet-4-5",
            ),
            ModelsDevPriceMapping::new(
                "anthropic",
                "internal-sonnet",
                "openrouter",
                "openrouter",
                "anthropic/claude-sonnet-4.5",
            ),
        ],
    );
    let source = StaticPrices(
        source
            .parse_payload(include_str!("fixtures/models_dev_api.json"))
            .expect("fixture payload"),
    );

    let output = catalog::price_sync(&deps(pool, Some(&source)), date("2031-07-10"))
        .await
        .expect("price_sync");

    assert_eq!(
        output,
        PriceSyncOutput {
            columns: 3,
            updated: 2,
            missing_from_source: 1,
        }
    );
    let direct = get_model_column(pool, "col_direct").await.unwrap();
    assert_eq!(
        parse_pricing(direct.pricing.get()).unwrap(),
        Pricing {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
            currency: "USD".to_owned(),
            as_of: "2031-07-10".to_owned(),
            source: "models.dev".to_owned(),
        }
    );
    let openrouter = get_model_column(pool, "col_openrouter").await.unwrap();
    assert_eq!(
        parse_pricing(openrouter.pricing.get()).unwrap(),
        Pricing {
            input_per_mtok: 3.4,
            output_per_mtok: 17.0,
            currency: "USD".to_owned(),
            as_of: "2031-07-10".to_owned(),
            source: "models.dev".to_owned(),
        }
    );
    let unmapped = get_model_column(pool, "col_unmapped").await.unwrap();
    assert_eq!(
        parse_pricing(unmapped.pricing.get()).unwrap(),
        Pricing {
            input_per_mtok: 1.0,
            output_per_mtok: 1.0,
            currency: "USD".to_owned(),
            as_of: "2031-07-09".to_owned(),
            source: "old".to_owned(),
        }
    );
}

#[tokio::test]
async fn price_sync_rejects_absurd_daily_change() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = &db.pool().await;
    Column {
        id: "col_absurd",
        model_id: "model_absurd",
        pricing: r#"{"input_per_mtok":1,"output_per_mtok":2,"currency":"USD","as_of":"2031-07-09","source":"models.dev"}"#,
        ..Column::new("col_absurd", "active")
    }
    .create(pool)
    .await;
    let before = get_model_column(pool, "col_absurd").await.unwrap();
    let source = StaticPrices(HashMap::from([(
        PriceKey::new("anthropic", "model_absurd", "direct"),
        Pricing {
            input_per_mtok: 10.01,
            output_per_mtok: 2.0,
            currency: "USD".to_owned(),
            as_of: "2031-07-10".to_owned(),
            source: "models.dev".to_owned(),
        },
    )]));

    let output = catalog::price_sync(&deps(pool, Some(&source)), date("2031-07-10"))
        .await
        .expect("price_sync");

    assert_eq!(
        output,
        PriceSyncOutput {
            columns: 1,
            updated: 0,
            missing_from_source: 0,
        }
    );
    let after = get_model_column(pool, "col_absurd").await.unwrap();
    assert_eq!(after.pricing.get(), before.pricing.get());
    assert_eq!(after.updated_at, before.updated_at);
}

#[tokio::test]
#[ignore = "manual live models.dev smoke; not for CI"]
async fn live_models_dev_price_source_updates_test_db() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = &db.pool().await;
    Column {
        id: "col_live_models_dev",
        provider: "openai",
        model_id: "internal-live-gpt-4-1",
        pricing: "{}",
        ..Column::new("col_live_models_dev", "active")
    }
    .create(pool)
    .await;
    let source = ModelsDevPriceSource::new(
        "https://models.dev/api.json",
        [ModelsDevPriceMapping::new(
            "openai",
            "internal-live-gpt-4-1",
            "direct",
            "openai",
            "gpt-4.1",
        )],
    );

    let output = catalog::price_sync(&deps(pool, Some(&source)), date("2031-07-10"))
        .await
        .expect("price_sync");

    assert_eq!(
        output,
        PriceSyncOutput {
            columns: 1,
            updated: 1,
            missing_from_source: 0,
        }
    );
    let stored = get_model_column(pool, "col_live_models_dev").await.unwrap();
    let pricing = parse_pricing(stored.pricing.get()).unwrap();
    assert!(pricing.input_per_mtok >= 0.0, "{pricing:?}");
    assert!(pricing.output_per_mtok >= 0.0, "{pricing:?}");
    assert_eq!(pricing.currency, "USD");
    assert!(!pricing.as_of.is_empty(), "{pricing:?}");
    assert_eq!(pricing.source, "models.dev");
}

/// parity: jobs.TestRetirementGuardBlocksLastPoolMember
#[tokio::test]
async fn retirement_guard_blocks_last_pool_member() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = &db.pool().await;
    for id in ["col_s", "col_a", "col_b"] {
        Column::new(id, "deprecated").create(pool).await;
    }
    tenant(pool, "ten_solo").await;
    tenant(pool, "ten_pair").await;
    stored_pool(pool, "ten_solo", "pool-guardsolo", &["col_s"]).await;
    stored_pool(pool, "ten_pair", "pool-guardpair", &["col_a", "col_b"]).await;
    let now = at(date("2031-08-10"));

    let blocked = catalog::guard_retirement(&deps(pool, None), "col_s", now)
        .await
        .expect("guard")
        .expect("blocked");
    assert_eq!(blocked.pools.len(), 1);
    assert_eq!(blocked.pools[0].pool, "pool-guardsolo");
    assert_eq!(blocked.pools[0].tenant_id, "ten_solo");
    assert_eq!(
        blocked.message(),
        r#"retiring column col_s would leave pool(s) "pool-guardsolo" of tenant ten_solo with zero usable columns; amend the pools first (spec 4.1)"#
    );
    let written = alerts(pool, "pool_would_be_empty").await;
    assert_eq!(written.len(), 1);
    assert_eq!(written[0].severity, "critical");
    assert_eq!(written[0].message, blocked.message());
    assert_eq!(subject(&written[0])["column_id"], "col_s");
    assert_eq!(subject(&written[0])["pools"][0]["pool"], "pool-guardsolo");

    let allowed = catalog::guard_retirement(&deps(pool, None), "col_a", now)
        .await
        .expect("guard");
    assert!(allowed.is_none());
    assert_eq!(alerts(pool, "pool_would_be_empty").await.len(), 1);
}

/// parity: jobs.TestCatalogSyncStatusTransitions
#[tokio::test]
async fn catalog_sync_status_transitions() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = &db.pool().await;
    let day = date("2031-08-10");
    let days = Duration::days;
    Column {
        cold_start_until: Some(day - days(3)),
        ..Column::new("col_cold_ready", "cold_start")
    }
    .create(pool)
    .await;
    aggregate(pool, "col_cold_ready", "quality", 0.9, true, day - days(1)).await;
    Column {
        cold_start_until: Some(day - days(3)),
        ..Column::new("col_cold_unmeasured", "cold_start")
    }
    .create(pool)
    .await;
    Column {
        cold_start_until: Some(day + days(3)),
        ..Column::new("col_cold_early", "cold_start")
    }
    .create(pool)
    .await;
    aggregate(pool, "col_cold_early", "quality", 0.9, true, day - days(1)).await;
    Column {
        deprecation_date: Some(day - days(3)),
        ..Column::new("col_sunset", "deprecated")
    }
    .create(pool)
    .await;
    Column::new("col_partner", "active").create(pool).await;
    Column {
        deprecation_date: Some(day - days(3)),
        ..Column::new("col_sunset_blocked", "deprecated")
    }
    .create(pool)
    .await;
    tenant(pool, "ten_1").await;
    stored_pool(pool, "ten_1", "pool-shared", &["col_sunset", "col_partner"]).await;
    stored_pool(pool, "ten_1", "pool-sole", &["col_sunset_blocked"]).await;

    let output = catalog::catalog_sync(&deps(pool, None), day, at(day))
        .await
        .expect("catalog_sync");
    assert_eq!(
        (output.activated, output.retired, output.retirement_blocked),
        (1, 1, 1)
    );
    let status =
        |id: &'static str| async move { get_model_column(pool, id).await.expect("column").status };
    assert_eq!(status("col_cold_ready").await, "active");
    assert_eq!(status("col_cold_unmeasured").await, "cold_start");
    assert_eq!(status("col_cold_early").await, "cold_start");
    assert_eq!(status("col_sunset").await, "retired");
    assert_eq!(status("col_sunset_blocked").await, "deprecated");
    let guard_alerts: Vec<Alert> = alerts(pool, "pool_would_be_empty")
        .await
        .into_iter()
        .filter(|a| subject(a).get("column_id").is_some())
        .collect();
    assert_eq!(guard_alerts.len(), 1);
    assert_eq!(subject(&guard_alerts[0])["column_id"], "col_sunset_blocked");
    // Both pools are now down to one usable column: two drift warnings.
    assert_eq!(output.drift_alerts, 2);
}

/// parity: jobs.TestCatalogSyncDriftAlerts
#[tokio::test]
async fn catalog_sync_drift_alerts() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = &db.pool().await;
    let day = date("2031-09-10");
    for (tenant_id, pool_name, costs) in [
        ("ten_empty", "pool-driftempty", [1.0, 1.0]),
        ("ten_warn", "pool-driftwarn", [1.0, 0.1]),
        ("ten_ok", "pool-drifthealthy", [0.1, 0.2]),
    ] {
        let ids = [
            format!("col_{tenant_id}_1").leak() as &'static str,
            format!("col_{tenant_id}_2").leak() as &'static str,
        ];
        for (id, cost) in ids.iter().zip(costs) {
            Column::new(id, "active").create(pool).await;
            aggregate(
                pool,
                id,
                "cost_per_run_usd",
                cost,
                true,
                day - Duration::days(1),
            )
            .await;
        }
        tenant(pool, tenant_id).await;
        constraints(pool, tenant_id, Some("0.5"), false).await;
        stored_pool(pool, tenant_id, pool_name, &ids).await;
    }

    let output = catalog::catalog_sync(&deps(pool, None), day, at(day))
        .await
        .expect("catalog_sync");
    assert!(output.drift_alerts >= 2);
    let written = alerts(pool, "pool_would_be_empty").await;
    let for_tenant = |tenant: &str| -> Vec<&Alert> {
        written
            .iter()
            .filter(|a| subject(a)["tenant_id"] == tenant)
            .collect()
    };
    let emptied = for_tenant("ten_empty");
    assert_eq!(emptied.len(), 1);
    assert_eq!(emptied[0].severity, "critical");
    assert_eq!(
        subject(emptied[0]),
        json!({
            "capability": "coding",
            "constraint": "max_cost_per_run",
            "day": "2031-09-10",
            "difficulty_tier": 3,
            "pool": "pool-driftempty",
            "survivors": 0,
            "tenant_id": "ten_empty",
        })
    );
    assert_eq!(
        emptied[0].message,
        r#"pool "pool-driftempty" of tenant ten_empty has no usable columns under current aggregates (0 surviving; spec 7.2 drift check)"#
    );
    let warned = for_tenant("ten_warn");
    assert_eq!(warned.len(), 1);
    assert_eq!(warned[0].severity, "warning");
    assert_eq!(subject(warned[0])["survivors"], 1);
    assert_eq!(
        warned[0].message,
        r#"pool "pool-driftwarn" of tenant ten_warn is within one column of empty under current aggregates (1 surviving; spec 7.2 drift check)"#
    );
    assert!(for_tenant("ten_ok").is_empty());
}

/// D1 in the drift check: expired zero-retention evidence reads as `unknown`, which can
/// empty a zero-retention tenant's pool (spec §4.5).
#[tokio::test]
async fn drift_check_includes_zero_retention() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = &db.pool().await;
    let day = date("2031-09-10");
    let now = at(day);
    Column {
        retention_mode: "zero",
        retention_valid_until: Some(now - Duration::days(1)),
        ..Column::new("col_expired", "active")
    }
    .create(pool)
    .await;
    Column {
        retention_mode: "zero",
        retention_valid_until: Some(now + Duration::days(60)),
        ..Column::new("col_valid", "active")
    }
    .create(pool)
    .await;
    tenant(pool, "ten_zr").await;
    constraints(pool, "ten_zr", None, true).await;
    stored_pool(pool, "ten_zr", "pool-expired", &["col_expired"]).await;
    stored_pool(pool, "ten_zr", "pool-mixed", &["col_expired", "col_valid"]).await;
    // The same pools without the constraint are healthy... except for the single column.
    tenant(pool, "ten_std").await;
    stored_pool(pool, "ten_std", "pool-pair", &["col_expired", "col_valid"]).await;

    let output = catalog::catalog_sync(&deps(pool, None), day, now)
        .await
        .expect("catalog_sync");
    assert_eq!(output.drift_alerts, 2);
    let written = alerts(pool, "pool_would_be_empty").await;
    let by_pool = |name: &str| -> &Alert {
        written
            .iter()
            .find(|a| subject(a)["pool"] == name)
            .expect("alert")
    };
    let expired = by_pool("pool-expired");
    assert_eq!(expired.severity, "critical");
    assert_eq!(subject(expired)["constraint"], "zero_retention");
    assert_eq!(subject(expired)["capability"], "");
    assert_eq!(subject(expired)["difficulty_tier"], 0);
    let mixed = by_pool("pool-mixed");
    assert_eq!(mixed.severity, "warning");
    assert_eq!(subject(mixed)["survivors"], 1);
}

/// Spec §4.5: `retention_expiring` 14 days before a runnable column's or alias's
/// `retention_valid_until`; not for later, already expired, or retired evidence.
#[tokio::test]
async fn catalog_sync_raises_retention_expiring() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = &db.pool().await;
    let day = date("2031-09-10");
    let now = at(day);
    let in_days = |n| now + Duration::days(n);
    Column {
        retention_mode: "zero",
        retention_valid_until: Some(in_days(10)),
        ..Column::new("col_soon", "active")
    }
    .create(pool)
    .await;
    Column {
        retention_mode: "zero",
        retention_valid_until: Some(in_days(14)),
        ..Column::new("col_edge", "deprecated")
    }
    .create(pool)
    .await;
    Column {
        retention_mode: "zero",
        retention_valid_until: Some(in_days(20)),
        ..Column::new("col_later", "active")
    }
    .create(pool)
    .await;
    Column {
        retention_mode: "zero",
        retention_valid_until: Some(in_days(-1)),
        ..Column::new("col_expired", "active")
    }
    .create(pool)
    .await;
    Column {
        retention_mode: "zero",
        retention_valid_until: Some(in_days(5)),
        ..Column::new("col_retired", "retired")
    }
    .create(pool)
    .await;
    Column {
        alias_retention: json!({
            "openrouter": {"retention_mode": "zero", "retention_valid_until": "2031-09-13T00:00:00Z"},
            "later": {"retention_mode": "zero", "retention_valid_until": "2031-12-01T00:00:00Z"},
            "broken": {"retention_mode": "zero"},
        })
        .to_string(),
        ..Column::new("col_alias", "active")
    }
    .create(pool)
    .await;

    let output = catalog::catalog_sync(&deps(pool, None), day, now)
        .await
        .expect("catalog_sync");
    assert_eq!(output.retention_alerts, 3);
    let written = alerts(pool, "retention_expiring").await;
    assert!(written.iter().all(|a| a.severity == "warning"));
    let mut subjects: Vec<Value> = written.iter().map(subject).collect();
    subjects.sort_by_key(|s| s["column_id"].as_str().map(str::to_owned));
    assert_eq!(
        subjects,
        [
            json!({"column_id": "col_alias", "alias": "openrouter", "retention_mode": "zero", "retention_valid_until": "2031-09-13T00:00:00Z", "day": "2031-09-10"}),
            json!({"column_id": "col_edge", "retention_mode": "zero", "retention_valid_until": "2031-09-24T06:10:00Z", "day": "2031-09-10"}),
            json!({"column_id": "col_soon", "retention_mode": "zero", "retention_valid_until": "2031-09-20T06:10:00Z", "day": "2031-09-10"}),
        ]
    );
    let alias = written
        .iter()
        .find(|a| subject(a)["column_id"] == "col_alias")
        .expect("alias alert");
    assert_eq!(
        alias.message,
        r#"retention evidence for alias "openrouter" of column col_alias expires at 2031-09-13T00:00:00Z; revalidate it through the internal column API (spec 4.5)"#
    );
}

#[tokio::test]
async fn run_wrappers_validate_the_day() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = &db.pool().await;
    let bad = DayArgs {
        day: "2031-13-01".to_owned(),
    };
    let error = catalog::run_price_sync(&deps(pool, None), &bad)
        .await
        .expect_err("bad day");
    assert!(
        matches!(
            error,
            CatalogError::InvalidArgument(JobsError::InvalidDay { .. })
        ),
        "{error}"
    );
    let good = DayArgs {
        day: "2031-09-10".to_owned(),
    };
    let output = catalog::run_catalog_sync(&deps(pool, None), &good, at(date("2031-09-10")))
        .await
        .expect("catalog_sync");
    assert_eq!(output, CatalogSyncOutput::default());
}
