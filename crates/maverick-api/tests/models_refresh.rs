//! DIVERGENCE: D51. `POST /internal/v1/models/refresh` (spec §4.6, §13; docs/api.md) on
//! real Postgres, with scripted model lists from the fake provider, so no test calls a
//! real provider. One contract test per error code: 401, 413, each 422 case, 500, and 404
//! for another method.

use std::sync::{Arc, LazyLock, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use chrono::{DateTime, TimeZone, Utc};
use http_body_util::BodyExt;
use maverick_alerts::AlertNotifier;
use maverick_api::{
    ApiState, AuthState, HealthState, InternalSettings, PgAuthBackend, RateLimiter,
    RecommendConfig, internal_router_with,
};
use maverick_domain::keys;
use maverick_providers::ProviderErrorKind;
use maverick_providers::fake::FakeModelLister;
use maverick_providers::models::{ListedModel, ModelLister};
use maverick_store::alerts::{Alert, list_alerts_by_kind};
use maverick_store::catalog::{NewModelColumn, create_model_column, list_model_columns};
use maverick_store::{JsonRawValue, MIGRATOR};
use maverick_testdb::Schema;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

const OPS: &str = "ops:alpha";
const PATH: &str = "/internal/v1/models/refresh";
const SONNET: &str = "claude-sonnet-5-5-20260930";
const PRICED_SONNET: &str = r#"{"enroll":[{"provider":"anthropic","model_id":"claude-sonnet-5-5-20260930","pricing":{"input_per_mtok":3,"output_per_mtok":15}}]}"#;

#[derive(Default)]
struct Notified(Mutex<Vec<Alert>>);

impl AlertNotifier for Notified {
    fn notify(&self, alert: &Alert) {
        self.0.lock().unwrap().push(alert.clone());
    }
}

/// 12:00 UTC on 2026-07-06, so the next nightly batch (03:00) is the night of 07-07.
fn clock() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 7, 6, 12, 0, 0).unwrap()
}

fn listed(provider: &str, model_id: &str, display_name: Option<&str>) -> ListedModel {
    ListedModel {
        provider: provider.to_owned(),
        model_id: model_id.to_owned(),
        display_name: display_name.map(str::to_owned),
        created_at: None,
    }
}

/// Anthropic lists two snapshots of a new base and one of the cataloged haiku; OpenAI
/// fails; Google and xAI have no key.
fn listers() -> Vec<Arc<dyn ModelLister>> {
    vec![
        Arc::new(FakeModelLister::new(
            "anthropic",
            vec![
                listed("anthropic", SONNET, Some("Claude Sonnet 5.5")),
                listed("anthropic", "claude-sonnet-5-5-20260815", None),
                listed("anthropic", "claude-haiku-4-5-20251001", None),
            ],
        )),
        Arc::new(FakeModelLister::failing(
            "openai",
            ProviderErrorKind::ProviderError,
        )),
    ]
}

struct Seam {
    router: Router,
    pool: PgPool,
    notified: Arc<Notified>,
}

fn api(
    pool: Option<&PgPool>,
    notified: &Arc<Notified>,
    listers: Vec<Arc<dyn ModelLister>>,
) -> ApiState {
    let recommend = RecommendConfig {
        alerts: Some(Arc::clone(notified) as Arc<dyn AlertNotifier>),
        ..RecommendConfig::default()
    };
    let state = match pool {
        Some(pool) => ApiState::with_health(pool.clone(), HealthState::detached()),
        None => ApiState::detached(),
    };
    state
        .with_recommend_config(recommend)
        .with_internal(InternalSettings {
            cold_start_nights: 5,
            clock: Arc::new(clock),
            ..InternalSettings::default()
        })
        .with_model_listers(listers)
}

fn auth(pool: &PgPool) -> AuthState {
    AuthState::new(
        Arc::new(PgAuthBackend::new(pool.clone())),
        keys::parse_internal_tokens(OPS).unwrap(),
        RateLimiter::new(1000),
    )
}

async fn seam(pool: &PgPool, listers: Vec<Arc<dyn ModelLister>>) -> Seam {
    let empty = JsonRawValue::from_string("{}".to_owned()).unwrap();
    create_model_column(
        pool,
        &NewModelColumn {
            id: "col_haiku",
            provider: "anthropic",
            model_id: "claude-haiku-4-5",
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
    let notified = Arc::new(Notified::default());
    Seam {
        router: internal_router_with(auth(pool), api(Some(pool), &notified, listers)),
        pool: pool.clone(),
        notified,
    }
}

async fn send(router: &Router, method: &str, token: Option<&str>, body: Vec<u8>) -> (u16, String) {
    let mut request = Request::builder().method(method).uri(PATH);
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

impl Seam {
    async fn post(&self, body: &str) -> (u16, Value) {
        let (status, text) = send(&self.router, "POST", Some(OPS), body.as_bytes().to_vec()).await;
        assert!(text.ends_with('\n'), "{text}");
        (status, serde_json::from_str(&text).expect("json"))
    }

    /// A 422 `invalid_request`: returns its details.
    async fn rejected(&self, body: &str) -> Value {
        let (status, response) = self.post(body).await;
        assert_eq!(status, 422, "{body}: {response}");
        assert_eq!(response["error"]["code"], "invalid_request", "{response}");
        response["error"]["details"].clone()
    }

    async fn column_ids(&self) -> Vec<String> {
        list_model_columns(&self.pool)
            .await
            .unwrap()
            .into_iter()
            .map(|column| column.id)
            .collect()
    }
}

#[tokio::test]
async fn a_report_lists_every_provider_and_writes_the_job_alerts() {
    let _ = parity_support::divergence!("D51");
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let seam = seam(&db.pool().await, listers()).await;

    let (status, text) = send(&seam.router, "POST", Some(OPS), Vec::new()).await;

    assert_eq!(status, 200, "{text}");
    let keys = [
        "\"ran_at\"",
        "\"providers\"",
        "\"new\"",
        "\"withdrawn\"",
        "\"enrolled\"",
        "\"spend_preview\"",
    ];
    let positions: Vec<usize> = keys.iter().map(|key| text.find(key).expect(key)).collect();
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]), "{text}");
    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["ran_at"], "2026-07-06T12:00:00Z");
    assert_eq!(
        body["providers"],
        json!([
            {"provider": "anthropic", "status": "ok", "listed": 3},
            {"provider": "google", "status": "skipped_no_key"},
            {"provider": "openai", "status": "failed", "error": "provider_error (status 500)"},
            {"provider": "xai", "status": "skipped_no_key"},
        ])
    );
    assert_eq!(
        body["new"],
        json!([{
            "provider": "anthropic",
            "model_id": SONNET,
            "base_model_id": "claude-sonnet-5-5",
            "display_name": "Claude Sonnet 5.5",
        }])
    );
    assert_eq!(body["withdrawn"], json!([]));
    assert_eq!(body["enrolled"], json!([]));
    assert_eq!(body["spend_preview"], Value::Null);
    let alerts = list_alerts_by_kind(&seam.pool, "new_model_available")
        .await
        .unwrap();
    assert_eq!(alerts.len(), 1);
    assert_eq!(
        seam.notified.0.lock().unwrap().len(),
        1,
        "delivered like the job's"
    );

    // `{}` reports too, and the alert is not written twice.
    let (status, _) = seam.post("{}").await;
    assert_eq!(status, 200);
    assert_eq!(
        list_alerts_by_kind(&seam.pool, "new_model_available")
            .await
            .unwrap()
            .len(),
        1
    );
    db.close().await;
}

#[tokio::test]
async fn enrolling_creates_cold_start_columns_and_previews_the_next_night() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let seam = seam(&db.pool().await, listers()).await;

    let (status, body) = seam.post(PRICED_SONNET).await;

    assert_eq!(status, 200, "{body}");
    let enrolled = body["enrolled"].as_array().expect("enrolled");
    assert_eq!(enrolled.len(), 1);
    assert_eq!(enrolled[0]["model_id"], SONNET);
    assert_eq!(enrolled[0]["endpoint"], "direct", "the default endpoint");
    assert_eq!(enrolled[0]["status"], "cold_start");
    assert_eq!(enrolled[0]["cold_start_until"], "2026-07-11");
    assert_eq!(
        enrolled[0]["pricing"],
        json!({"input_per_mtok": 3, "output_per_mtok": 15})
    );
    let preview = &body["spend_preview"];
    assert_eq!(preview["job"], "nightly_smoke");
    assert_eq!(preview["night"], "2026-07-07");
    assert_eq!(preview["decision"], "allow", "{preview}");
    for field in [
        "runs",
        "projected_usd",
        "night_spent_usd",
        "nightly_cap_usd",
        "week_spent_usd",
        "weekly_cap_usd",
    ] {
        assert!(preview[field].is_number(), "{field}: {preview}");
    }
    let column_id = enrolled[0]["column_id"].as_str().expect("id");
    assert!(seam.column_ids().await.iter().any(|id| id == column_id));
    // The model it just enrolled gets no "new model" alert.
    assert!(
        list_alerts_by_kind(&seam.pool, "new_model_available")
            .await
            .unwrap()
            .is_empty()
    );
    db.close().await;
}

#[tokio::test]
async fn another_listed_snapshot_of_a_new_base_may_enroll() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let seam = seam(&db.pool().await, listers()).await;

    let (status, body) = seam
        .post(r#"{"enroll":[{"provider":"anthropic","model_id":"claude-sonnet-5-5-20260815","endpoint":"direct","pricing":{"input_per_mtok":3,"output_per_mtok":15}}]}"#)
        .await;

    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["enrolled"][0]["model_id"],
        "claude-sonnet-5-5-20260815"
    );
    db.close().await;
}

#[tokio::test]
async fn a_bad_body_is_422() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let seam = seam(&db.pool().await, listers()).await;
    for body in [
        "{",
        r#"{"refresh":true}"#,
        r#"{"enroll":1}"#,
        r#"{"enroll":[{"provider":"anthropic","model":"x"}]}"#,
    ] {
        seam.rejected(body).await;
    }
    assert_eq!(seam.column_ids().await, ["col_haiku"]);
    db.close().await;
}

#[tokio::test]
async fn an_entry_needs_pricing_and_a_valid_column() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let seam = seam(&db.pool().await, listers()).await;

    let details = seam
        .rejected(&format!(
            r#"{{"enroll":[{{"provider":"anthropic","model_id":"{SONNET}"}}]}}"#
        ))
        .await;
    assert_eq!(details, json!({"field": "pricing"}));

    let details = seam
        .rejected(&format!(
            r#"{{"enroll":[{{"provider":"anthropic","model_id":"{SONNET}","endpoint":"moon","pricing":{{"input_per_mtok":3,"output_per_mtok":15}}}}]}}"#
        ))
        .await;
    assert_eq!(details["field"], "endpoint");
    db.close().await;
}

#[tokio::test]
async fn an_entry_for_a_provider_that_did_not_answer_is_422() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let seam = seam(&db.pool().await, listers()).await;
    for (provider, status) in [("google", "skipped_no_key"), ("openai", "failed")] {
        let details = seam
            .rejected(&format!(
                r#"{{"enroll":[{{"provider":"{provider}","model_id":"m-1","pricing":{{"input_per_mtok":1,"output_per_mtok":2}}}}]}}"#
            ))
            .await;
        assert_eq!(
            details,
            json!({"provider": provider, "provider_status": status})
        );
    }
    db.close().await;
}

#[tokio::test]
async fn an_entry_that_is_not_new_is_422() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let seam = seam(&db.pool().await, listers()).await;
    for model_id in ["claude-opus-9", "claude-haiku-4-5-20251001"] {
        let details = seam
            .rejected(&format!(
                r#"{{"enroll":[{{"provider":"anthropic","model_id":"{model_id}","pricing":{{"input_per_mtok":1,"output_per_mtok":2}}}}]}}"#
            ))
            .await;
        assert_eq!(
            details,
            json!({"provider": "anthropic", "model_id": model_id})
        );
    }
    db.close().await;
}

/// Two entries for one column, or one bad entry next to a good one: nothing is created.
#[tokio::test]
async fn enrollment_is_all_or_nothing() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let seam = seam(&db.pool().await, listers()).await;
    let entry = format!(
        r#"{{"provider":"anthropic","model_id":"{SONNET}","pricing":{{"input_per_mtok":3,"output_per_mtok":15}}}}"#
    );

    let details = seam
        .rejected(&format!(r#"{{"enroll":[{entry},{entry}]}}"#))
        .await;
    assert_eq!(
        details,
        json!({"provider": "anthropic", "model_id": SONNET, "endpoint": "direct"})
    );

    seam.rejected(&format!(
        r#"{{"enroll":[{entry},{{"provider":"anthropic","model_id":"claude-opus-9","pricing":{{"input_per_mtok":1,"output_per_mtok":2}}}}]}}"#
    ))
    .await;
    assert_eq!(seam.column_ids().await, ["col_haiku"]);
    db.close().await;
}

#[tokio::test]
async fn a_run_where_no_provider_answers_is_still_200() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let failing: Vec<Arc<dyn ModelLister>> = vec![Arc::new(FakeModelLister::failing(
        "xai",
        ProviderErrorKind::RateLimited,
    ))];
    let seam = seam(&db.pool().await, failing).await;

    let (status, body) = seam.post("").await;

    assert_eq!(status, 200, "{body}");
    let statuses: Vec<&str> = body["providers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|report| report["status"].as_str().unwrap())
        .collect();
    assert_eq!(
        statuses,
        [
            "skipped_no_key",
            "skipped_no_key",
            "skipped_no_key",
            "failed"
        ]
    );
    assert_eq!(body["new"], json!([]));
    db.close().await;
}

#[tokio::test]
async fn auth_size_store_and_method_errors() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let seam = seam(&pool, listers()).await;

    let (status, body) = send(&seam.router, "POST", None, Vec::new()).await;
    assert_eq!(status, 401, "{body}");
    assert!(body.contains("\"unauthorized\""), "{body}");

    let mut big = br#"{"enroll":[{"provider":"anthropic","model_id":""#.to_vec();
    big.resize(2_097_153, b'a');
    let (status, body) = send(&seam.router, "POST", Some(OPS), big).await;
    assert_eq!(status, 413, "{body}");
    assert!(body.contains("\"payload_too_large\""), "{body}");

    let (status, body) = send(&seam.router, "GET", Some(OPS), Vec::new()).await;
    assert_eq!(status, 404, "{body}");

    // A store failure is 500: here, a state with no database.
    let notified = Arc::new(Notified::default());
    let detached = internal_router_with(auth(&pool), api(None, &notified, listers()));
    let (status, body) = send(&detached, "POST", Some(OPS), Vec::new()).await;
    assert_eq!(status, 500, "{body}");
    assert!(body.contains("\"internal_error\""), "{body}");
    db.close().await;
}
