//! `latency_probes` on real Postgres with the fake provider (reference Part 3 §2.12 and
//! §6 "latency_probes").

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use chrono::NaiveDate;
use maverick_jobs::fold::{CellKey, FoldParams, fold_observations};
use maverick_jobs::probes::{
    LatencyProbesJob, PINGS_PER_COLUMN, ProbeConfig, ProbeOutput, latency_probes, probe_prompt,
};
use maverick_jobs::{HourArgs, JobClientConfig, JobKind};
use maverick_providers::fake::{FakeProvider, FixtureSpec};
use maverick_runner::{Cancellation, Clients};
use maverick_store::catalog::{self, NewModelColumn};
use maverick_store::measurement::{self, CellDimension};
use maverick_store::{JsonRawValue, MIGRATOR};
use maverick_testdb::Schema;
use sqlx::PgPool;

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

fn json(text: &str) -> Box<JsonRawValue> {
    JsonRawValue::from_string(text.to_owned()).expect("json")
}

fn day(s: &str) -> NaiveDate {
    s.parse().expect("date")
}

async fn seed_column(
    pool: &PgPool,
    id: &str,
    provider: &str,
    endpoint: &str,
    status: &str,
    settings: &str,
    source: Option<&str>,
) {
    catalog::create_model_column(
        pool,
        &NewModelColumn {
            id,
            provider,
            model_id: &format!("model-{id}"),
            endpoint,
            settings: &json(settings),
            status,
            pricing: &json("{}"),
            quality_source_column_id: source,
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

/// The source's quality 0.9 and pass_rate 0.8 rows at tiers 2 and 4 on 2031-06-01.
async fn seed_source_cells(pool: &PgPool, source: &str) {
    let mut conn = pool.acquire().await.expect("conn");
    for tier in [2, 4] {
        fold_observations(
            &mut conn,
            &CellKey {
                model_column_id: source.to_owned(),
                capability: "structured_extraction".to_owned(),
                difficulty_tier: tier,
            },
            &BTreeMap::from([("quality", 0.9), ("pass_rate", 0.8)]),
            day("2031-06-01"),
            "corpus-probe",
            &FoldParams {
                alpha: 0.3,
                min_nights: 3,
            },
        )
        .await
        .expect("fold");
    }
}

fn fake_for(models: &[&str]) -> Arc<FakeProvider> {
    let specs: Vec<FixtureSpec> = models
        .iter()
        .map(|model| {
            serde_json::from_value(serde_json::json!({
                "model": model,
                "prompt": probe_prompt(),
                "output": "pong",
                "tokens_in": 1000,
                "tokens_out": 1,
            }))
            .expect("fixture")
        })
        .collect();
    Arc::new(FakeProvider::from_fixtures(&specs).expect("fake"))
}

fn config(clients: Clients) -> ProbeConfig {
    ProbeConfig {
        clients,
        runner_timeout: Duration::from_secs(30),
        alpha: 0.3,
        min_nights: 3,
    }
}

async fn run(pool: &PgPool, config: &ProbeConfig, hour: &str) -> ProbeOutput {
    latency_probes(
        pool,
        &HourArgs {
            hour: hour.to_owned(),
        },
        config,
        &Cancellation::new(),
    )
    .await
    .expect("probes")
}

async fn rows(
    pool: &PgPool,
    column: &str,
    tier: i16,
    dimension: &str,
) -> Vec<measurement::Aggregate> {
    measurement::list_aggregate_series_before(
        pool,
        &CellDimension {
            model_column_id: column,
            capability: "structured_extraction",
            difficulty_tier: tier,
            dimension,
        },
        day("2100-01-01"),
    )
    .await
    .expect("series")
}

/// parity: jobs.TestLatencyProbesWriteLatencyDimensionsOnly
#[tokio::test]
async fn write_latency_dimensions_only() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed_column(
        &pool,
        "col_source",
        "anthropic",
        "direct",
        "active",
        "{}",
        None,
    )
    .await;
    seed_column(
        &pool,
        "col_probe",
        "anthropic",
        "vertex",
        "active",
        "{}",
        Some("col_source"),
    )
    .await;
    seed_source_cells(&pool, "col_source").await;
    let config = config(Clients::new().with("anthropic", fake_for(&["model-col_probe"])));

    let out = run(&pool, &config, "2031-06-10T15").await;
    assert_eq!(
        out,
        ProbeOutput {
            columns: 1,
            probes_ok: 5,
            probes_failed: 0,
            cells_updated: 2,
        }
    );
    for tier in [2, 4] {
        for dimension in ["latency_p50_ms", "latency_p95_ms"] {
            let series = rows(&pool, "col_probe", tier, dimension).await;
            assert_eq!(series.len(), 1, "{tier} {dimension}");
            let row = &series[0];
            assert_eq!((row.n, row.measured), (1, false));
            assert!(row.ewma >= 0.0);
            assert_eq!(row.as_of, day("2031-06-10"));
            assert_eq!(row.matrix_version, "corpus-probe");
        }
        for dimension in [
            "quality",
            "pass_rate",
            "ambiguity",
            "cost_per_run_usd",
            "availability",
        ] {
            assert!(
                rows(&pool, "col_probe", tier, dimension).await.is_empty(),
                "{dimension}"
            );
        }
        assert!(
            rows(&pool, "col_source", tier, "latency_p95_ms")
                .await
                .is_empty()
        );
    }
    // A second pass on the same UTC day replaces the day's row: n stays 1.
    run(&pool, &config, "2031-06-10T16").await;
    let series = rows(&pool, "col_probe", 2, "latency_p95_ms").await;
    assert_eq!(series.len(), 1);
    assert_eq!(series[0].n, 1);
    // The next day folds on top.
    run(&pool, &config, "2031-06-11T00").await;
    assert_eq!(
        rows(&pool, "col_probe", 2, "latency_p95_ms")
            .await
            .last()
            .expect("row")
            .n,
        2
    );
    db.close().await;
}

/// Columns without a source, retired columns, and sources without cells are skipped; a
/// missing client or unusable settings is five failures and no ping.
#[tokio::test]
async fn skips_and_failures() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed_column(
        &pool,
        "col_source",
        "anthropic",
        "direct",
        "active",
        "{}",
        None,
    )
    .await;
    seed_column(
        &pool,
        "col_empty_source",
        "anthropic",
        "direct",
        "active",
        "{}",
        None,
    )
    .await;
    seed_source_cells(&pool, "col_source").await;
    // Counted, probed: no client for google.
    seed_column(
        &pool,
        "col_a_noclient",
        "google",
        "vertex",
        "active",
        "{}",
        Some("col_source"),
    )
    .await;
    // Counted, probed: settings are not an object.
    seed_column(
        &pool,
        "col_b_badsettings",
        "anthropic",
        "vertex",
        "active",
        "[1]",
        Some("col_source"),
    )
    .await;
    // Counted, not probed: the source has no cells.
    seed_column(
        &pool,
        "col_c_nocells",
        "anthropic",
        "vertex",
        "active",
        "{}",
        Some("col_empty_source"),
    )
    .await;
    // Not counted: retired.
    seed_column(
        &pool,
        "col_d_retired",
        "anthropic",
        "vertex",
        "retired",
        "{}",
        Some("col_source"),
    )
    .await;
    let config = config(Clients::new().with("anthropic", fake_for(&[])));
    let out = run(&pool, &config, "2031-06-10T15").await;
    assert_eq!(
        out,
        ProbeOutput {
            columns: 3,
            probes_ok: 0,
            probes_failed: 2 * PINGS_PER_COLUMN,
            cells_updated: 0,
        }
    );
    for column in [
        "col_a_noclient",
        "col_b_badsettings",
        "col_c_nocells",
        "col_d_retired",
    ] {
        assert!(
            rows(&pool, column, 2, "latency_p95_ms").await.is_empty(),
            "{column}"
        );
    }
    db.close().await;
}

/// A cancelled job stops without counting the remaining pings.
#[tokio::test]
async fn cancel_stops_early() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed_column(
        &pool,
        "col_source",
        "anthropic",
        "direct",
        "active",
        "{}",
        None,
    )
    .await;
    seed_column(
        &pool,
        "col_probe",
        "anthropic",
        "vertex",
        "active",
        "{}",
        Some("col_source"),
    )
    .await;
    seed_source_cells(&pool, "col_source").await;
    let config = config(Clients::new().with("anthropic", fake_for(&["model-col_probe"])));
    let cancel = Cancellation::new();
    cancel.cancel();
    let out = latency_probes(
        &pool,
        &HourArgs {
            hour: "2031-06-10T15".to_owned(),
        },
        &config,
        &cancel,
    )
    .await
    .expect("probes");
    assert_eq!((out.columns, out.probes_ok, out.probes_failed), (1, 0, 0));
    assert!(
        latency_probes(
            &pool,
            &HourArgs {
                hour: "2031-06-10".to_owned(),
            },
            &config,
            &cancel,
        )
        .await
        .is_err()
    );
    assert_eq!(
        <LatencyProbesJob as graphile_worker::TaskHandler>::IDENTIFIER,
        JobKind::LatencyProbes.as_str()
    );
    db.close().await;
}

/// parity: jobs.TestLatencyProbesTimeoutUsesAnalyticsJobTimeout
#[test]
fn timeout_uses_the_analytics_job_timeout() {
    assert_eq!(
        JobKind::LatencyProbes.timeout(&JobClientConfig::default()),
        Duration::from_secs(15 * 60)
    );
    assert_eq!(probe_prompt().len(), 4020);
    assert!(
        probe_prompt()
            .starts_with("Reply with the single word pong to this latency probe line. Reply")
    );
}
