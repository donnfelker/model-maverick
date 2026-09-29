//! `POST /v1/telemetry` on real Postgres (reference Part 5 §6.8, §13.7; fixture
//! `parity/fixtures/api/telemetry.json`).

use std::sync::{Arc, LazyLock};

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use maverick_api::{
    ApiState, AuthState, HealthState, Identity, RateLimiter, StaticAuthBackend, public_router_with,
};
use maverick_domain::keys;
use maverick_store::decisions::{self, NewRecommendation};
use maverick_store::{JsonRawValue, MIGRATOR, tenancy};
use maverick_testdb::Schema;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

fn fixture() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../parity/fixtures/api/telemetry.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// A router authenticated as `tenant`.
fn router(pool: &PgPool, tenant: &str) -> Router {
    let auth = AuthState::new(
        Arc::new(StaticAuthBackend::authenticated(Identity {
            tenant_id: tenant.to_owned(),
            api_key_id: format!("key_{tenant}"),
        })),
        keys::parse_internal_tokens("ops:alpha").unwrap(),
        RateLimiter::new(10_000),
    );
    let health = HealthState::with_providers(pool.clone(), Vec::new());
    public_router_with(auth, ApiState::with_health(pool.clone(), health))
}

async fn post(
    router: Router,
    path: &str,
    body: impl Into<Body>,
    key: bool,
) -> (StatusCode, String) {
    let mut request = Request::builder().method(Method::POST).uri(path);
    if key {
        request = request.header(header::AUTHORIZATION, "Bearer mk_live_test");
    }
    let response = router
        .oneshot(request.body(body.into()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

async fn send(pool: &PgPool, tenant: &str, body: Value) -> (StatusCode, String) {
    post(
        router(pool, tenant),
        "/v1/telemetry",
        body.to_string(),
        true,
    )
    .await
}

/// Seeds a tenant with one API key.
async fn tenant(pool: &PgPool, id: &str) {
    tenancy::create_tenant(pool, id, "Acme", &[1; 32])
        .await
        .unwrap();
    tenancy::create_api_key(pool, &format!("key_{id}"), id, id.as_bytes(), "abcd", "")
        .await
        .unwrap();
}

async fn recommendation(pool: &PgPool, id: &str, tenant: &str) {
    let empty = JsonRawValue::from_string("{}".to_owned()).unwrap();
    decisions::create_recommendation(
        pool,
        &NewRecommendation {
            id,
            tenant_id: tenant,
            api_key_id: &format!("key_{tenant}"),
            prompt_hash: None,
            request: &empty,
            classification: &empty,
            candidates: &empty,
            response: &empty,
            matrix_version: "2026-07-01",
        },
    )
    .await
    .unwrap();
}

/// (outcome, endpoint_used, details, escalated_to) of every outcome row of a recommendation.
async fn outcomes(pool: &PgPool, rec: &str) -> Vec<(String, String, Value, Option<String>)> {
    let rows: Vec<(String, String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT outcome, endpoint_used, details::text, escalated_to FROM outcomes
         WHERE recommendation_id = $1 ORDER BY id",
    )
    .bind(rec)
    .fetch_all(pool)
    .await
    .unwrap();
    rows.into_iter()
        .map(|(o, e, d, x)| {
            (
                o,
                e,
                d.map_or(Value::Null, |d| serde_json::from_str(&d).unwrap()),
                x,
            )
        })
        .collect()
}

// parity: api.TestTelemetryRequiresAuth
#[tokio::test]
async fn telemetry_requires_auth() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let (status, body) = post(
        router(&pool, "ten_a"),
        "/v1/telemetry",
        r#"{"items":[]}"#,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    pool.close().await;
    db.close().await;
}

// parity: api.TestOutcomesRouteRemoved
#[tokio::test]
async fn post_outcomes_does_not_exist() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let (status, body) = post(router(&pool, "ten_a"), "/v1/outcomes", "{}", true).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    pool.close().await;
    db.close().await;
}

// parity: api.TestTelemetryEmptyItemsNoOp
#[tokio::test]
async fn empty_items_are_a_no_op() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let (status, body) = send(&pool, "ten_a", json!({"items": []})).await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::OK, "{\"accepted\":0,\"dropped\":0}\n")
    );
    pool.close().await;
    db.close().await;
}

// parity: api.TestTelemetryEnvelopeInvalidRequest422
// parity: api.TestTelemetryEnvelopeInvalidRequest422/empty_body
// parity: api.TestTelemetryEnvelopeInvalidRequest422/malformed_json
// parity: api.TestTelemetryEnvelopeInvalidRequest422/non-object_body_(array)
// parity: api.TestTelemetryEnvelopeInvalidRequest422/non-object_body_(string)
// parity: api.TestTelemetryEnvelopeInvalidRequest422/missing_items_key
// parity: api.TestTelemetryEnvelopeInvalidRequest422/null_items
// parity: api.TestTelemetryEnvelopeInvalidRequest422/non-array_items_(string)
// parity: api.TestTelemetryEnvelopeInvalidRequest422/non-array_items_(object)
// parity: api.TestTelemetryEnvelopeInvalidRequest422/trailing_content
// parity: api.TestTelemetryEnvelopeInvalidRequest422/501_items
// D25: for `malformed request body:` cases only the prefix, status, and code match Go.
#[tokio::test]
async fn a_bad_envelope_is_422_invalid_request() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    for case in fixture()["envelope_422"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        let body = match case["body"].as_str() {
            None => String::new(),
            Some("{\"items\":[null x 501]}") => {
                json!({"items": vec![Value::Null; 501]}).to_string()
            }
            Some(body) => body.to_owned(),
        };
        let (status, text) = post(router(&pool, "ten_a"), "/v1/telemetry", body, true).await;
        let expected = &case["expected"];
        assert_eq!(
            status.as_u16(),
            expected["status"].as_u64().unwrap() as u16,
            "{id}: {text}"
        );
        let got: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(got["error"]["code"], expected["code"], "{id}: {text}");
        assert_eq!(got["error"]["details"], expected["details"], "{id}: {text}");
        let want = expected["message"].as_str().unwrap();
        let message = got["error"]["message"].as_str().unwrap();
        if want.starts_with("malformed request body: ") {
            assert!(
                message.starts_with("malformed request body: "),
                "{id}: {text}"
            );
        } else {
            assert_eq!(message, want, "{id}: {text}");
        }
    }
    pool.close().await;
    db.close().await;
}

// parity: api.TestTelemetryDerivationTable
// parity: api.TestTelemetryDerivationTable/timeout_beats_a_5xx_status
// parity: api.TestTelemetryDerivationTable/timeout_beats_status_0
// parity: api.TestTelemetryDerivationTable/connect_error_beats_a_2xx_status
// parity: api.TestTelemetryDerivationTable/provider_error_error_class
// parity: api.TestTelemetryDerivationTable/status_0_(transport_failure)
// parity: api.TestTelemetryDerivationTable/status_429
// parity: api.TestTelemetryDerivationTable/status_500
// parity: api.TestTelemetryDerivationTable/status_503
// parity: api.TestTelemetryDerivationTable/2xx_with_check_pass_false
// parity: api.TestTelemetryDerivationTable/201_with_check_pass_false
// parity: api.TestTelemetryDerivationTable/2xx_success (D16: completed_unchecked)
// parity: api.TestTelemetryDerivationTable/2xx_with_check_pass_true
// parity: api.TestTelemetryDerivationTable/semantic_4xx_is_other
// parity: api.TestTelemetryDerivationTable/3xx_is_other
#[tokio::test]
async fn each_item_derives_its_outcome() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    tenant(&pool, "ten_a").await;
    for case in fixture()["derivation"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        let rec = format!("rec_{}", id.replace('-', "_"));
        recommendation(&pool, &rec, "ten_a").await;
        let mut item = case["item"].clone();
        item["recommendation_id"] = json!(rec);
        item["model_used"] = json!("claude-sonnet-4-6");
        let (status, body) = send(&pool, "ten_a", json!({"items": [item]})).await;
        assert_eq!(
            (status, body.as_str()),
            (StatusCode::OK, "{\"accepted\":1,\"dropped\":0}\n"),
            "{id}"
        );
        let mut expected = case["expected_outcome"].as_str().unwrap();
        if expected == "success" && case["item"].get("check_pass").is_none() {
            expected = "completed_unchecked"; // D16: a 2xx with no check is not quality evidence
        }
        let stored = outcomes(&pool, &rec).await;
        assert_eq!(stored.len(), 1, "{id}");
        assert_eq!(stored[0].0, expected, "{id}");
    }
    pool.close().await;
    db.close().await;
}

// parity: api.TestTelemetryAcceptedDroppedCounts
#[tokio::test]
async fn foreign_unknown_and_invalid_items_are_dropped_without_an_oracle() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    tenant(&pool, "ten_a").await;
    tenant(&pool, "ten_b").await;
    recommendation(&pool, "rec_a", "ten_a").await;
    recommendation(&pool, "rec_b", "ten_b").await;
    let item = |rec: &str| json!({"recommendation_id": rec, "model_used": "m", "status": 200});
    let batch = json!({"items": [
        item("rec_a"),
        item("rec_b"),
        item("rec_01NEVEREXISTEDXXXXXXXXXX"),
        {"recommendation_id": "rec_a", "status": 200},
    ]});
    let (status, body) = send(&pool, "ten_a", batch).await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::OK, "{\"accepted\":1,\"dropped\":3}\n")
    );
    assert_eq!(outcomes(&pool, "rec_a").await.len(), 1);
    assert!(
        outcomes(&pool, "rec_b").await.is_empty(),
        "tenant B's recommendation is untouched"
    );
    pool.close().await;
    db.close().await;
}

// parity: api.TestTelemetryDropsInvalidItems
// parity: api.TestTelemetryDropsInvalidItems/missing_recommendation_id
// parity: api.TestTelemetryDropsInvalidItems/empty_recommendation_id
// parity: api.TestTelemetryDropsInvalidItems/missing_model_used
// parity: api.TestTelemetryDropsInvalidItems/missing_status
// parity: api.TestTelemetryDropsInvalidItems/mistyped_status_(string)
// parity: api.TestTelemetryDropsInvalidItems/negative_status
// parity: api.TestTelemetryDropsInvalidItems/negative_latency_ms
// parity: api.TestTelemetryDropsInvalidItems/negative_tokens_in
// parity: api.TestTelemetryDropsInvalidItems/negative_tokens_out
// parity: api.TestTelemetryDropsInvalidItems/invalid_error_class
// parity: api.TestTelemetryDropsInvalidItems/invalid_endpoint_used
// parity: api.TestTelemetryDropsInvalidItems/mistyped_check_pass
// parity: api.TestTelemetryDropsInvalidItems/item_is_not_an_object
#[tokio::test]
async fn invalid_items_are_dropped_one_by_one() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    tenant(&pool, "ten_a").await;
    recommendation(&pool, "rec_seeded", "ten_a").await;
    for case in fixture()["drops"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        let item: Value = serde_json::from_str(
            &case["item"]
                .to_string()
                .replace("<seeded rec id>", "rec_seeded"),
        )
        .unwrap();
        let (status, body) = send(&pool, "ten_a", json!({"items": [item]})).await;
        assert_eq!(status, StatusCode::OK, "{id}");
        assert_eq!(
            body,
            case["expected_response_body_exact"].as_str().unwrap(),
            "{id}"
        );
    }
    assert!(outcomes(&pool, "rec_seeded").await.is_empty());
    pool.close().await;
    db.close().await;
}

// parity: api.TestTelemetryEndpointUsedDefaultAndStored
#[tokio::test]
async fn endpoint_used_defaults_to_direct_and_is_stored() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    tenant(&pool, "ten_a").await;
    recommendation(&pool, "rec_1", "ten_a").await;
    let batch = json!({"items": [
        {"recommendation_id": "rec_1", "model_used": "m", "status": 200},
        {"recommendation_id": "rec_1", "model_used": "m", "status": 200, "endpoint_used": "openrouter"},
    ]});
    send(&pool, "ten_a", batch).await;
    let endpoints: Vec<String> = outcomes(&pool, "rec_1")
        .await
        .into_iter()
        .map(|o| o.1)
        .collect();
    assert_eq!(endpoints, ["direct", "openrouter"]);
    pool.close().await;
    db.close().await;
}

// parity: api.TestTelemetryDetailsContainsExactSuppliedFacts
#[tokio::test]
async fn details_hold_exactly_the_supplied_facts() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    tenant(&pool, "ten_a").await;
    recommendation(&pool, "rec_1", "ten_a").await;
    let batch = json!({"items": [
        {"recommendation_id": "rec_1", "model_used": "m", "status": 200},
        {"recommendation_id": "rec_1", "model_used": "m", "status": 200, "latency_ms": 1234,
         "tokens_in": 10, "tokens_out": 20, "finish_reason": "stop", "error_class": "other",
         "check_pass": true, "endpoint_used": "direct"},
    ]});
    send(&pool, "ten_a", batch).await;
    let details: Vec<Value> = outcomes(&pool, "rec_1")
        .await
        .into_iter()
        .map(|o| o.2)
        .collect();
    assert_eq!(details[0], json!({"status": 200}));
    assert_eq!(
        details[1],
        json!({"status": 200, "latency_ms": 1234, "tokens_in": 10, "tokens_out": 20,
               "finish_reason": "stop", "error_class": "other", "check_pass": true})
    );
    pool.close().await;
    db.close().await;
}

// parity: api.TestTelemetryMultipleItemsPerRecommendationAppended
#[tokio::test]
async fn several_items_for_one_recommendation_are_appended() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    tenant(&pool, "ten_a").await;
    recommendation(&pool, "rec_1", "ten_a").await;
    let item = json!({"recommendation_id": "rec_1", "model_used": "m", "status": 200});
    let (_, body) = send(
        &pool,
        "ten_a",
        json!({"items": [item.clone(), item.clone(), item]}),
    )
    .await;
    assert_eq!(body, "{\"accepted\":3,\"dropped\":0}\n");
    let stored = outcomes(&pool, "rec_1").await;
    assert_eq!(stored.len(), 3);
    assert!(
        stored.iter().all(|o| o.3.is_none()),
        "escalated_to stays null"
    );
    pool.close().await;
    db.close().await;
}

/// DIVERGENCE: D39. An opted-out tenant's items are all dropped and counted, valid or not,
/// and no `outcomes` row is written; the response has the usual shape. A malformed
/// envelope is still a 422. Another tenant's batch still writes rows, and opting back in
/// restores writes.
#[tokio::test]
async fn an_opted_out_tenants_items_are_dropped() {
    let d39 = parity_support::divergence!("D39");
    assert_eq!(d39.id(), "D39");
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    tenant(&pool, "ten_a").await;
    tenant(&pool, "ten_b").await;
    recommendation(&pool, "rec_a", "ten_a").await;
    recommendation(&pool, "rec_b", "ten_b").await;
    tenancy::upsert_tenant_constraints(&pool, "ten_a", None, None, false, true)
        .await
        .unwrap();
    let item = |rec: &str| json!({"recommendation_id": rec, "model_used": "m", "status": 200});

    let batch = json!({"items": [item("rec_a"), item("rec_a"), {"status": 200}]});
    let (status, body) = send(&pool, "ten_a", batch).await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::OK, "{\"accepted\":0,\"dropped\":3}\n")
    );
    assert!(outcomes(&pool, "rec_a").await.is_empty(), "no outcomes row");
    let (status, _) = send(&pool, "ten_a", json!({"items": null})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let (_, body) = send(&pool, "ten_b", json!({"items": [item("rec_b")]})).await;
    assert_eq!(body, "{\"accepted\":1,\"dropped\":0}\n");
    assert_eq!(outcomes(&pool, "rec_b").await.len(), 1);

    tenancy::upsert_tenant_constraints(&pool, "ten_a", None, None, false, false)
        .await
        .unwrap();
    let (_, body) = send(&pool, "ten_a", json!({"items": [item("rec_a")]})).await;
    assert_eq!(body, "{\"accepted\":1,\"dropped\":0}\n");
    assert_eq!(outcomes(&pool, "rec_a").await.len(), 1);
    pool.close().await;
    db.close().await;
}
