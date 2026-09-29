//! The job metrics (spec §14.3) through an in-memory exporter installed as the global
//! meter provider. One test per binary, because the provider is process-wide.

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use graphile_worker::WorkerUtils;
use maverick_domain::graders::{GoldenAnswer, Registry};
use maverick_domain::spend::SpendCaps;
use maverick_jobs::batch::{BatchConfig, BatchOutcome, run_batch};
use maverick_jobs::{
    BatchLeaseConfig, BatchLeaseManager, BatchLeaseOutcome, JobArgs, JobKind, NightlySmokeArgs,
    run_with_timeout,
};
use maverick_providers::fake::{FakeProvider, FixtureSpec};
use maverick_runner::{Cancellation, Clients, RunnerConfig};
use maverick_store::catalog::{self, NewModelColumn};
use maverick_store::corpus::{self, TaskUpsert};
use maverick_store::health::GRAPHILE_WORKER_SCHEMA;
use maverick_store::{JsonRawValue, MIGRATOR};
use maverick_testdb::Schema;
use opentelemetry::{KeyValue, global};
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData, ResourceMetrics};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
use sqlx::postgres::PgPoolOptions;

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| {
    Schema::sqlx(&MIGRATOR).then(Schema::custom("graphile_worker 0.13", |url| async move {
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await?;
        WorkerUtils::new(pool.clone(), GRAPHILE_WORKER_SCHEMA)
            .migrate()
            .await?;
        pool.close().await;
        Ok(())
    }))
});

/// `(metric, sorted labels, value)`: a sum's value, or a histogram's count.
type Point = (String, Vec<(String, String)>, f64);

fn points(exported: &[ResourceMetrics]) -> Vec<Point> {
    fn labels<'a>(attributes: impl Iterator<Item = &'a KeyValue>) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = attributes
            .map(|kv| (kv.key.to_string(), kv.value.to_string()))
            .collect();
        out.sort();
        out
    }
    let mut out = Vec::new();
    for resource in exported {
        for scope in resource.scope_metrics() {
            for metric in scope.metrics() {
                let name = metric.name().to_owned();
                match metric.data() {
                    AggregatedMetrics::U64(MetricData::Sum(sum)) => {
                        for p in sum.data_points() {
                            #[allow(clippy::cast_precision_loss)]
                            out.push((name.clone(), labels(p.attributes()), p.value() as f64));
                        }
                    }
                    AggregatedMetrics::F64(MetricData::Histogram(histogram)) => {
                        for p in histogram.data_points() {
                            #[allow(clippy::cast_precision_loss)]
                            out.push((name.clone(), labels(p.attributes()), p.count() as f64));
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    out
}

fn value(points: &[Point], name: &str, labels: &[(&str, &str)]) -> Option<f64> {
    let mut wanted: Vec<(String, String)> = labels
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    wanted.sort();
    points
        .iter()
        .find(|(n, l, _)| n == name && *l == wanted)
        .map(|(_, _, v)| *v)
}

fn json(text: &str) -> Box<JsonRawValue> {
    JsonRawValue::from_string(text.to_owned()).expect("json")
}

#[tokio::test]
async fn job_metrics_are_recorded_with_fixed_labels() {
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(exporter.clone()).build())
        .build();
    global::set_meter_provider(provider.clone());

    // Handler outcomes.
    let kind = JobKind::AggregateRebuild;
    assert!(
        run_with_timeout(kind, Duration::from_secs(5), async { Ok::<(), String>(()) })
            .await
            .is_ok()
    );
    assert!(
        run_with_timeout(kind, Duration::from_secs(5), async { Err::<(), _>("boom") })
            .await
            .is_err()
    );
    let slow = async {
        tokio::time::sleep(Duration::from_secs(30)).await;
        Ok::<(), String>(())
    };
    assert!(
        run_with_timeout(kind, Duration::from_millis(20), slow)
            .await
            .is_err()
    );

    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let lease = BatchLeaseManager::new(BatchLeaseConfig {
        lease_ttl: Duration::from_secs(600),
        heartbeat_interval: Duration::from_secs(200),
        owner_stale_after: Duration::from_secs(300),
        graphile_schema: GRAPHILE_WORKER_SCHEMA.to_string(),
    });

    // A takeover: the first handler dropped, so its worker resumes the window.
    let first = lease
        .claim(&pool, JobKind::WeeklySweep, "2031-W40", "worker-m")
        .await
        .expect("claim");
    assert!(matches!(first, BatchLeaseOutcome::Claimed(_)));
    drop(first);
    let again = lease
        .claim(&pool, JobKind::WeeklySweep, "2031-W40", "worker-m")
        .await
        .expect("claim");
    assert!(matches!(again, BatchLeaseOutcome::TookOver(_)));
    drop(again);

    // A one-task batch for the spend histogram.
    corpus::upsert_corpus_version(&pool, "corpus-metrics", "", 1, true)
        .await
        .expect("version");
    let tags: Vec<String> = Vec::new();
    corpus::upsert_task(
        &pool,
        &TaskUpsert {
            id: "task_metrics",
            capability: "structured_extraction",
            domain_tags: &tags,
            difficulty_tier: 1,
            difficulty_knobs: &json("{}"),
            prompt: "What is six times seven?",
            attachments: &json("[]"),
            temperature: 0.0,
            max_tokens: 16,
            grader_specs: &json(
                r#"[{"type":"golden_answer","version":1,"weight_dimensions":{"answer_correctness":1},"spec":{"answers":[{"value":"42"}]}}]"#,
            ),
            dimension_weights: &json(r#"{"answer_correctness":1.0}"#),
            pass_threshold: 0.7,
            smoke_member: true,
            corpus_version: "corpus-metrics",
            active: true,
        },
    )
    .await
    .expect("task");
    catalog::create_model_column(
        &pool,
        &NewModelColumn {
            id: "col_metrics",
            provider: "anthropic",
            model_id: "claude-metrics",
            endpoint: "direct",
            settings: &json("{}"),
            status: "active",
            pricing: &json(r#"{"input_per_mtok": 3.0, "output_per_mtok": 15.0}"#),
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
    let fake = FakeProvider::from_fixtures(&[serde_json::from_value::<FixtureSpec>(
        serde_json::json!({"model": "claude-metrics", "prompt": "What is six times seven?", "output": "42", "tokens_in": 100, "tokens_out": 10}),
    )
    .expect("fixture")])
    .expect("fake");
    let mut registry = Registry::new();
    registry.must_register(Arc::new(GoldenAnswer));
    let config = BatchConfig {
        repeats: 1,
        caps: SpendCaps::default(),
        lease: lease.clone(),
        runner: RunnerConfig::default(),
        clients: Clients::new().with("anthropic", Arc::new(fake)),
        graders: Arc::new(registry),
        notifier: None,
    };
    let job = JobArgs::NightlySmoke(NightlySmokeArgs {
        night: "2031-10-01".to_owned(),
    });
    let out = run_batch(&pool, &job, &config, "worker-m", &Cancellation::new())
        .await
        .expect("batch");
    assert!(matches!(out, BatchOutcome::Ran(_)));

    provider.force_flush().expect("flush");
    let exported = points(&exporter.get_finished_metrics().expect("metrics"));
    let rebuild = "aggregate_rebuild";
    for outcome in ["ok", "error", "timeout"] {
        assert_eq!(
            value(
                &exported,
                "maverick_job_duration_ms",
                &[("kind", rebuild), ("outcome", outcome)]
            ),
            Some(1.0),
            "duration {outcome}"
        );
    }
    for outcome in ["error", "timeout"] {
        assert_eq!(
            value(
                &exported,
                "maverick_job_failures_total",
                &[("kind", rebuild), ("outcome", outcome)]
            ),
            Some(1.0),
            "failures {outcome}"
        );
    }
    assert_eq!(
        value(
            &exported,
            "maverick_job_failures_total",
            &[("kind", rebuild), ("outcome", "ok")]
        ),
        None
    );
    assert_eq!(
        value(
            &exported,
            "maverick_job_recoveries_total",
            &[("kind", "weekly_sweep")]
        ),
        Some(1.0)
    );
    assert_eq!(
        value(
            &exported,
            "maverick_batch_spend_usd",
            &[("kind", "nightly_smoke")]
        ),
        Some(1.0)
    );
    // D19: the labels are fixed names only.
    for (_, labels, _) in &exported {
        for (key, _) in labels {
            assert!(matches!(key.as_str(), "kind" | "outcome"), "label {key}");
        }
    }
    pool.close().await;
    db.close().await;
}
