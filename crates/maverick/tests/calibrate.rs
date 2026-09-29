//! `maverick corpus calibrate` end to end on real Postgres (testcontainers), with the Go
//! crafted grid (reference Part 1, `main.TestRunCorpusCalibrateNamesTheInversionAndThe
//! OutlierAndNothingElse`; bead 3.14). The report must equal
//! `parity/fixtures/store/calibrate.json#report_crafted_grid` byte for byte: the older row
//! and the unmeasured column are read from the database and must be left out. Skips when
//! Docker is not available.

use std::io::{self, Write};
use std::sync::{Arc, LazyLock, Mutex};

use chrono::NaiveDate;
use maverick_cli::{Output, run};
use maverick_store::catalog::{NewModelColumn, create_model_column};
use maverick_store::measurement::{AggregateUpsert, upsert_aggregate};
use maverick_store::{JsonRawValue, MIGRATOR};
use maverick_testdb::{Schema, database};
use serde_json::Value;
use sqlx::PgPool;

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

const FIXTURE: &str = include_str!("../../../parity/fixtures/store/calibrate.json");

#[derive(Clone)]
struct Shared(Arc<Mutex<Vec<u8>>>);

impl Write for Shared {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// One CLI invocation. `run` builds its own current-thread runtime, so it runs on a
/// blocking thread, outside the test's runtime.
async fn cli(args: &[&str], env: Vec<(String, String)>) -> (i32, String, String) {
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    tokio::task::spawn_blocking(move || {
        let out = Arc::new(Mutex::new(Vec::new()));
        let err = Arc::new(Mutex::new(Vec::new()));
        let lookup = |name: &str| {
            env.iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };
        let code = run(
            &args,
            &lookup,
            Output::new(Shared(out.clone())),
            Output::new(Shared(err.clone())),
        );
        let read = |buffer: &Arc<Mutex<Vec<u8>>>| {
            String::from_utf8(buffer.lock().expect("lock").clone()).expect("utf8")
        };
        (code, read(&out), read(&err))
    })
    .await
    .expect("join")
}

async fn column(pool: &PgPool, id: &str, model: &str) {
    let empty = JsonRawValue::from_string("{}".to_owned()).unwrap();
    create_model_column(
        pool,
        &NewModelColumn {
            id,
            provider: "anthropic",
            model_id: model,
            endpoint: "direct",
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
    .unwrap();
}

/// Pass-rate rows for tiers `1..`, with n 5, variance 0, and matrix `2026-07-01`.
/// Pass-rate rows of one column and capability, from `first_tier` up.
struct Series<'a> {
    column: &'a str,
    capability: &'a str,
    first_tier: i16,
    rates: &'a [f32],
    measured: bool,
    as_of: NaiveDate,
}

async fn pass_rates(pool: &PgPool, series: &Series<'_>) {
    for (tier, rate) in (series.first_tier..).zip(series.rates) {
        upsert_aggregate(
            pool,
            &AggregateUpsert {
                model_column_id: series.column,
                capability: series.capability,
                difficulty_tier: tier,
                dimension: "pass_rate",
                ewma: *rate,
                variance: 0.0,
                n: 5,
                measured: series.measured,
                matrix_version: "2026-07-01",
                as_of: series.as_of,
            },
        )
        .await
        .unwrap();
    }
}

// parity: main.TestRunCorpusCalibrateNamesTheInversionAndTheOutlierAndNothingElse
#[tokio::test]
async fn calibrate_names_the_inversion_and_the_outlier_and_nothing_else() {
    let Some(db) = database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    for (id, model) in [
        ("col_mono", "cal-mono"),
        ("col_inverted", "cal-inverted"),
        ("col_outlier", "cal-outlier"),
        ("col_unmeasured", "cal-unmeasured"),
    ] {
        column(&pool, id, model).await;
    }
    let latest = NaiveDate::from_ymd_opt(2026, 7, 5).unwrap();
    let older = NaiveDate::from_ymd_opt(2026, 7, 1).unwrap();
    let series = [
        Series {
            column: "col_mono",
            capability: "structured_extraction",
            first_tier: 1,
            rates: &[0.95, 0.90, 0.80, 0.70, 0.60],
            measured: true,
            as_of: latest,
        },
        Series {
            column: "col_inverted",
            capability: "structured_extraction",
            first_tier: 1,
            rates: &[0.90, 0.70, 0.78, 0.65, 0.55],
            measured: true,
            as_of: latest,
        },
        // An older tier-3 row that the latest row supersedes.
        Series {
            column: "col_inverted",
            capability: "structured_extraction",
            first_tier: 3,
            rates: &[0.62],
            measured: true,
            as_of: older,
        },
        Series {
            column: "col_outlier",
            capability: "factual_qa",
            first_tier: 1,
            rates: &[0.95, 0.80, 0.45, 0.42, 0.40],
            measured: true,
            as_of: latest,
        },
        // Not measured: never reported, though tier 2 beats tier 1.
        Series {
            column: "col_unmeasured",
            capability: "structured_extraction",
            first_tier: 1,
            rates: &[0.20, 0.90],
            measured: false,
            as_of: latest,
        },
    ];
    for one in &series {
        pass_rates(&pool, one).await;
    }

    let env = vec![
        ("MAVERICK_DATABASE_URL".to_owned(), db.url().to_owned()),
        ("MAVERICK_CAPTURE_MASTER_KEY".to_owned(), "ab".repeat(32)),
        (
            "MAVERICK_INTERNAL_TOKENS".to_owned(),
            "rails:s3cret".to_owned(),
        ),
    ];
    let (code, out, err) = cli(&["corpus", "calibrate"], env).await;
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(err, "");

    // The Go assertions, then the exact report.
    let lines = |kind: &str| out.lines().filter(|line| line.contains(kind)).count();
    assert_eq!(lines("INVERSION"), 1, "{out}");
    assert_eq!(lines("OUTLIER"), 1, "{out}");
    assert!(
        out.contains("cal-mono") && !out.contains("cal-unmeasured"),
        "{out}"
    );
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    assert_eq!(
        out,
        fixture["report_crafted_grid"]["stdout"].as_str().unwrap()
    );
    db.close().await;
}
