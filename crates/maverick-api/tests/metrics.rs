//! The engine request metrics (spec §14.3) through an in-memory exporter installed as
//! the global meter provider: recommend, the classifier, and the job queue at a health
//! read. One test per binary, because the provider is process-wide.

use std::sync::{Arc, LazyLock};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use chrono::Utc;
use maverick_api::classify::{Classifier, ResolveInput, Resolver, ResolverSettings};
use maverick_api::{
    ApiState, AuthState, HealthState, Identity, RateLimiter, RecommendConfig, StaticAuthBackend,
    internal_router_with, recommend,
};
use maverick_domain::keys;
use maverick_providers::{
    CompletionRequest, CompletionResponse, ProviderClient, ProviderError, ProviderErrorKind,
    ProviderFuture,
};
use maverick_store::catalog::{self, NewModelColumn};
use maverick_store::health::GRAPHILE_WORKER_SCHEMA;
use maverick_store::measurement::{self, AggregateUpsert};
use maverick_store::{JsonRawValue, corpus, tenancy};
use maverick_testdb::Schema;
use opentelemetry::{KeyValue, global};
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData, ResourceMetrics};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| {
    Schema::sqlx(&maverick_store::MIGRATOR).then(Schema::custom(
        "graphile_worker 0.13",
        |url| async move {
            let pool = PgPoolOptions::new()
                .max_connections(2)
                .connect(&url)
                .await?;
            graphile_worker::WorkerUtils::new(pool.clone(), GRAPHILE_WORKER_SCHEMA)
                .migrate()
                .await?;
            pool.close().await;
            Ok(())
        },
    ))
});

const TENANT: &str = "ten_metrics";
const KEY: &str = "key_metrics";
const CORPUS: &str = "2026-07-01";
const CLASSIFIER_COLUMN: &str = "claude-haiku-4-5/direct";
const VALID: &str = r#"{"capability":"structured_extraction","difficulty":3,"domain_tags":["legal"],"confidence":0.91}"#;

/// A classifier provider that always answers `VALID`, or always fails.
struct Answering(bool);

impl ProviderClient for Answering {
    fn complete<'a>(&'a self, _request: CompletionRequest) -> ProviderFuture<'a> {
        let answers = self.0;
        Box::pin(async move {
            if answers {
                Ok(CompletionResponse {
                    text: VALID.to_owned(),
                    tokens_in: 100,
                    tokens_out: 20,
                })
            } else {
                Err(
                    ProviderError::new("anthropic", ProviderErrorKind::ProviderError)
                        .with_status(503),
                )
            }
        })
    }

    fn name(&self) -> &str {
        "anthropic"
    }
}

/// `(metric, sorted labels, value)`: a counter's sum, a histogram's count, a gauge's value.
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
                #[allow(clippy::cast_precision_loss)]
                match metric.data() {
                    AggregatedMetrics::U64(MetricData::Sum(sum)) => out.extend(
                        sum.data_points()
                            .map(|p| (name.clone(), labels(p.attributes()), p.value() as f64)),
                    ),
                    AggregatedMetrics::F64(MetricData::Histogram(histogram)) => out.extend(
                        histogram
                            .data_points()
                            .map(|p| (name.clone(), labels(p.attributes()), p.count() as f64)),
                    ),
                    AggregatedMetrics::I64(MetricData::Gauge(gauge)) => out.extend(
                        gauge
                            .data_points()
                            .map(|p| (name.clone(), labels(p.attributes()), p.value() as f64)),
                    ),
                    AggregatedMetrics::F64(MetricData::Gauge(gauge)) => out.extend(
                        gauge
                            .data_points()
                            .map(|p| (name.clone(), labels(p.attributes()), p.value())),
                    ),
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

fn raw(text: &str) -> Box<JsonRawValue> {
    JsonRawValue::from_string(text.to_owned()).expect("json")
}

async fn aggregate(pool: &PgPool, tier: i16, dimension: &str, ewma: f32) {
    measurement::upsert_aggregate(
        pool,
        &AggregateUpsert {
            model_column_id: "col_m",
            capability: "structured_extraction",
            difficulty_tier: tier,
            dimension,
            ewma,
            variance: 0.0,
            n: 20,
            measured: true,
            matrix_version: CORPUS,
            as_of: Utc::now().date_naive(),
        },
    )
    .await
    .expect("aggregate");
}

/// One measured column in the tenant's default pool, able to serve tier 2.
async fn seed_recommendable(pool: &PgPool) {
    corpus::upsert_corpus_version(pool, CORPUS, "", 0, false)
        .await
        .expect("version");
    corpus::activate_corpus_version(pool, CORPUS)
        .await
        .expect("activate");
    catalog::create_model_column(
        pool,
        &NewModelColumn {
            id: "col_m",
            provider: "anthropic",
            model_id: "claude-metrics",
            endpoint: "direct",
            settings: &raw("{}"),
            status: "active",
            pricing: &raw("{}"),
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
    tenancy::create_pool(
        pool,
        "pool_default",
        TENANT,
        "default",
        &["col_m".to_owned()],
        true,
    )
    .await
    .expect("pool");
    for (tier, rate) in (1..).zip([0.99, 0.98, 0.95, 0.9, 0.85]) {
        aggregate(pool, tier, "pass_rate", rate).await;
    }
    aggregate(pool, 2, "quality", 0.95).await;
    aggregate(pool, 2, "cost_per_run_usd", 0.001).await;
    aggregate(pool, 2, "latency_p95_ms", 900.0).await;
}

#[tokio::test]
async fn request_metrics_are_recorded_with_fixed_labels() {
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(exporter.clone()).build())
        .build();
    global::set_meter_provider(provider.clone());

    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    tenancy::create_tenant(&pool, TENANT, "Acme", &[1; 32])
        .await
        .expect("tenant");
    tenancy::create_api_key(&pool, KEY, TENANT, b"hash", "abcd", "")
        .await
        .expect("key");

    // The classifier: a miss that classifies, then a hit, then a miss that falls back.
    let settings = ResolverSettings {
        column: CLASSIFIER_COLUMN.to_owned(),
        ..ResolverSettings::default()
    };
    let input = |prompt: &'static str| ResolveInput {
        tenant_id: TENANT,
        prompt_hmac_key: &[0; 32],
        quota_override: None,
        zero_retention: false,
        prompt,
        projection_version: None,
        columns: &[],
        now: Utc::now(),
    };
    let answering = Resolver::new(settings.clone()).with_classifier(Classifier::new(
        Arc::new(Answering(true)),
        CLASSIFIER_COLUMN,
    ));
    answering
        .resolve(&pool, &input("first prompt"))
        .await
        .expect("resolve");
    answering
        .resolve(&pool, &input("first prompt"))
        .await
        .expect("resolve");
    let failing = Resolver::new(settings).with_classifier(Classifier::new(
        Arc::new(Answering(false)),
        CLASSIFIER_COLUMN,
    ));
    failing
        .resolve(&pool, &input("second prompt"))
        .await
        .expect("resolve");

    // Recommend: one declared success and one refused body.
    seed_recommendable(&pool).await;
    let identity = Identity {
        tenant_id: TENANT.to_owned(),
        api_key_id: KEY.to_owned(),
    };
    let config = RecommendConfig::default();
    recommend(
        &pool,
        &config,
        &identity,
        br#"{"workload":{"capability":"structured_extraction","difficulty":2}}"#,
    )
    .await
    .expect("recommended");
    assert!(recommend(&pool, &config, &identity, b"{}").await.is_err());

    // A health read samples the job queue.
    let auth = AuthState::new(
        Arc::new(StaticAuthBackend::authenticated(identity.clone())),
        keys::parse_internal_tokens("ops:alpha").expect("tokens"),
        RateLimiter::new(300),
    );
    let health = HealthState::with_providers(pool.clone(), Vec::new());
    let router = internal_router_with(auth, ApiState::with_health(pool.clone(), health));
    let response = router
        .oneshot(
            Request::builder()
                .uri("/health/detail")
                .header(header::AUTHORIZATION, "Bearer ops:alpha")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);

    provider.force_flush().expect("flush");
    let exported = points(&exporter.get_finished_metrics().expect("metrics"));
    let get = |name: &str, labels: &[(&str, &str)]| value(&exported, name, labels);
    assert_eq!(
        get("maverick_classifier_cache_total", &[("result", "miss")]),
        Some(2.0)
    );
    assert_eq!(
        get("maverick_classifier_cache_total", &[("result", "hit")]),
        Some(1.0)
    );
    assert_eq!(
        get(
            "maverick_classifier_duration_ms",
            &[("outcome", "classified")]
        ),
        Some(1.0)
    );
    assert_eq!(
        get(
            "maverick_classifier_duration_ms",
            &[("outcome", "fallback_default")]
        ),
        Some(1.0)
    );
    assert_eq!(
        get(
            "maverick_classifier_fallback_total",
            &[("reason", "failure")]
        ),
        Some(1.0)
    );
    assert_eq!(
        get("maverick_recommend_duration_ms", &[("outcome", "ok")]),
        Some(1.0)
    );
    assert_eq!(
        get("maverick_recommend_duration_ms", &[("outcome", "error")]),
        Some(1.0)
    );
    assert_eq!(
        get(
            "maverick_recommendations_total",
            &[("classification_method", "declared"), ("measured", "true")]
        ),
        Some(1.0)
    );
    assert_eq!(
        get("maverick_queue_depth", &[("queue", "default")]),
        Some(0.0)
    );
    assert_eq!(
        get(
            "maverick_queue_oldest_job_age_seconds",
            &[("queue", "default")]
        ),
        Some(0.0)
    );
    // D19: fixed label names only; no tenant, prompt, or body ever labels a metric.
    for (_, labels, _) in &exported {
        for (key, _) in labels {
            assert!(
                matches!(
                    key.as_str(),
                    "outcome"
                        | "result"
                        | "reason"
                        | "measured"
                        | "classification_method"
                        | "queue"
                ),
                "label {key}"
            );
        }
    }
    pool.close().await;
    db.close().await;
}
