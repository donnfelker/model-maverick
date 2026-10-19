//! The zero-retention classifier column is covered like a pool member (spec §8.3, D2):
//! a column write that takes its zero evidence away writes one critical
//! `classifier_degraded` alert that names the column, and restoring the evidence writes
//! one `info` alert. Repeats write nothing, and other columns are not its concern. The
//! D41 re-resolution loop runs the same check, which also catches evidence that expires
//! with no write.

use std::sync::{Arc, LazyLock, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use chrono::{DateTime, Duration, TimeZone, Utc};
use http_body_util::BodyExt;
use maverick_alerts::AlertNotifier;
use maverick_api::classifier_evidence::{self, LOST, RESTORED};
use maverick_api::classify::{Resolver, ResolverSettings};
use maverick_api::{
    ApiState, AuthState, HealthState, InternalSettings, PgAuthBackend, RateLimiter,
    RecommendConfig, internal_router_with,
};
use maverick_domain::keys;
use maverick_store::alerts::Alert;
use maverick_store::catalog::{self, NewModelColumn};
use maverick_store::{JsonRawValue, MIGRATOR};
use maverick_testdb::Schema;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

const OPS: &str = "ops:alpha";
const CLASSIFIER: &str = "claude-haiku-4-5/direct";
const ZERO: &str = "zr-classifier/direct";

/// Records every alert it is told about.
#[derive(Default)]
struct Notified(Mutex<Vec<Alert>>);

impl AlertNotifier for Notified {
    fn notify(&self, alert: &Alert) {
        self.0.lock().unwrap().push(alert.clone());
    }
}

fn settings(zero_retention_column: Option<&str>) -> ResolverSettings {
    ResolverSettings {
        column: CLASSIFIER.to_owned(),
        zero_retention_column: zero_retention_column.map(str::to_owned),
        ..ResolverSettings::default()
    }
}

/// The internal seam with `classifier.zero_retention_column` set to `zero`.
fn seam(pool: &PgPool, zero: Option<&str>, notified: &Arc<Notified>) -> Router {
    let auth = AuthState::new(
        Arc::new(PgAuthBackend::new(pool.clone())),
        keys::parse_internal_tokens(OPS).unwrap(),
        RateLimiter::new(1000),
    );
    let recommend = RecommendConfig {
        alerts: Some(Arc::clone(notified) as Arc<dyn AlertNotifier>),
        classifier: Arc::new(Resolver::new(settings(zero))),
        ..RecommendConfig::default()
    };
    let api = ApiState::with_health(pool.clone(), HealthState::detached())
        .with_recommend_config(recommend)
        .with_internal(InternalSettings::default());
    internal_router_with(auth, api)
}

async fn ops(router: &Router, method: &str, path: &str, body: &Value) -> (u16, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {OPS}"))
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Valid zero-retention evidence: verified 10 days ago, valid for 60 more days.
fn evidence() -> Value {
    let now = Utc::now();
    json!({
        "retention_mode": "zero",
        "retention_verified_at": (now - Duration::days(10)).to_rfc3339(),
        "retention_valid_until": (now + Duration::days(60)).to_rfc3339(),
        "retention_source": "https://example.com/zdr",
    })
}

/// Creates a direct anthropic column for `model` with valid zero evidence.
async fn create(router: &Router, model: &str) -> String {
    let mut body = evidence();
    body["provider"] = json!("anthropic");
    body["model_id"] = json!(model);
    body["endpoint"] = json!("direct");
    let (status, created) = ops(router, "POST", "/internal/v1/columns", &body).await;
    assert_eq!(status, 201, "{created}");
    created["column_id"].as_str().unwrap().to_owned()
}

async fn put(router: &Router, id: &str, body: &Value) {
    let (status, answered) = ops(router, "PUT", &format!("/internal/v1/columns/{id}"), body).await;
    assert_eq!(status, 200, "{answered}");
}

/// `(severity, subject)` of every `classifier_degraded` alert, oldest first.
async fn degraded(pool: &PgPool) -> Vec<(String, Value)> {
    maverick_store::alerts::list_alerts_by_kind(pool, "classifier_degraded")
        .await
        .unwrap()
        .into_iter()
        .map(|alert| {
            let subject = serde_json::from_str(alert.subject.get()).unwrap();
            (alert.severity, subject)
        })
        .collect()
}

macro_rules! db {
    () => {{
        let Some(db) = maverick_testdb::database(&SCHEMA).await else {
            return;
        };
        db
    }};
}

#[tokio::test]
async fn a_write_that_takes_the_classifier_columns_zero_evidence_away_alerts_once() {
    let db = db!();
    let pool = db.pool().await;
    let notified = Arc::new(Notified::default());
    let router = seam(&pool, Some(ZERO), &notified);
    let _ = parity_support::divergence!("D2");
    let id = create(&router, "zr-classifier").await;
    let member = create(&router, "zr-member").await;
    assert!(degraded(&pool).await.is_empty());

    // Another zero column's downgrade is the G4 simulation's concern, not this one's.
    put(&router, &member, &json!({"retention_mode": "standard"})).await;
    assert!(degraded(&pool).await.is_empty());

    // The classifier column's downgrade writes one critical alert that names the column.
    put(&router, &id, &json!({"retention_mode": "standard"})).await;
    let alerts = degraded(&pool).await;
    assert_eq!(alerts.len(), 1);
    assert_eq!(alerts[0].0, "critical");
    assert_eq!(
        alerts[0].1,
        json!({
            "classifier_column": CLASSIFIER,
            "column_id": id,
            "reason": LOST,
            "zero_retention_column": ZERO,
        })
    );
    let notified_now = notified.0.lock().unwrap().clone();
    assert_eq!(notified_now.len(), 1);
    assert_eq!(
        notified_now[0].message,
        format!(
            "zero-retention classifier column {ZERO} (column {id}) has no valid \
             zero-retention evidence (effective retention standard); zero-retention \
             tenants' prompt-form requests get 422 zero_retention_requires_declared"
        )
    );

    // Another write while the evidence is still missing writes nothing more.
    put(&router, &id, &json!({"retention_mode": "standard"})).await;
    assert_eq!(degraded(&pool).await.len(), 1);

    // Restoring the evidence writes one info alert; a repeat writes nothing.
    put(&router, &id, &evidence()).await;
    put(&router, &id, &evidence()).await;
    let alerts = degraded(&pool).await;
    assert_eq!(alerts.len(), 2);
    assert_eq!(alerts[1].0, "info");
    assert_eq!(alerts[1].1["reason"], RESTORED);
    assert_eq!(alerts[1].1["column_id"], id.as_str());
    assert_eq!(notified.0.lock().unwrap().len(), 2);
    db.close().await;
}

#[tokio::test]
async fn an_unset_or_unmatched_zero_retention_column_alerts_nothing() {
    let db = db!();
    let pool = db.pool().await;
    let notified = Arc::new(Notified::default());
    // Unset: zero-retention tenants are refused by configuration, so nothing changes.
    let unset = seam(&pool, None, &notified);
    let id = create(&unset, "zr-classifier").await;
    put(&unset, &id, &json!({"retention_mode": "standard"})).await;
    // A ref that matches no column is D41's `unresolved` alert, not this one.
    let unmatched = seam(&pool, Some("absent-model/direct"), &notified);
    put(&unmatched, &id, &json!({"retention_mode": "standard"})).await;
    assert!(degraded(&pool).await.is_empty());
    assert!(notified.0.lock().unwrap().is_empty());
    db.close().await;
}

/// The check alone, at hand-picked times, as the D41 loop runs it: evidence that
/// expires with no write alerts once, and renewed evidence restores it.
#[tokio::test]
async fn evidence_that_expires_without_a_write_alerts_from_the_check() {
    let db = db!();
    let pool = db.pool().await;
    let valid_until: DateTime<Utc> = Utc.with_ymd_and_hms(2030, 3, 1, 0, 0, 0).unwrap();
    let empty = JsonRawValue::from_string("{}".to_owned()).unwrap();
    catalog::create_model_column(
        &pool,
        &NewModelColumn {
            id: "col_zr",
            provider: "anthropic",
            model_id: "zr-classifier",
            endpoint: "direct",
            settings: &empty,
            status: "active",
            pricing: &empty,
            quality_source_column_id: None,
            cold_start_until: None,
            deprecation_date: None,
            execution_aliases: None,
            retention_mode: Some("zero"),
            retention_verified_at: Some(valid_until - Duration::days(80)),
            retention_valid_until: Some(valid_until),
            retention_source: Some("https://example.com/zdr"),
            execution_alias_retention: None,
            param_constraints: None,
        },
    )
    .await
    .unwrap();
    let columns = catalog::list_model_columns(&pool).await.unwrap();
    let settings = settings(Some(ZERO));
    let check = |now: DateTime<Utc>| {
        let pool = pool.clone();
        let settings = settings.clone();
        let columns = columns.clone();
        async move {
            classifier_evidence::alert_on_change(&pool, None, &settings, &columns, now)
                .await
                .unwrap()
                .map(|alert| (alert.severity, alert.subject.get().to_owned()))
        }
    };

    assert_eq!(check(valid_until - Duration::days(1)).await, None);
    let lost = check(valid_until + Duration::days(1)).await.expect("lost");
    assert_eq!(lost.0, "critical");
    let subject: Value = serde_json::from_str(&lost.1).unwrap();
    assert_eq!(subject["reason"], LOST);
    assert_eq!(subject["column_id"], "col_zr");
    assert_eq!(check(valid_until + Duration::days(2)).await, None);
    let restored = check(valid_until - Duration::days(1))
        .await
        .expect("restored");
    assert_eq!(restored.0, "info");
    assert_eq!(degraded(&pool).await.len(), 2);
    db.close().await;
}
