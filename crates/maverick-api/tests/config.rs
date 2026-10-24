//! `GET|PUT /v1/config/{pools,weights,constraints}` on real Postgres (reference Part 5
//! §6.1 to §6.3, §13.2; Part 4 §1.4, §1.5). Error bodies follow
//! `parity/fixtures/api/error_catalog_observed.json`; success bodies follow the recorded
//! exchanges in `parity/blackbox/api-recorded.jsonl`.

use std::sync::{Arc, LazyLock};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{Method, Request, StatusCode, header};
use chrono::{Duration, NaiveDate, Utc};
use maverick_api::{
    ApiState, AuthState, HealthState, Identity, RateLimiter, StaticAuthBackend, public_router_with,
};
use maverick_domain::keys;
use maverick_store::catalog::{self, NewModelColumn};
use maverick_store::measurement::{self, AggregateUpsert};
use maverick_store::{JsonRawValue, MIGRATOR, tenancy};
use maverick_testdb::{Schema, TestDatabase};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

const TENANT: &str = "ten_a";
const OTHER: &str = "ten_b";

fn router_as(api: ApiState, backend: StaticAuthBackend) -> Router {
    let auth = AuthState::new(
        Arc::new(backend),
        keys::parse_internal_tokens("ops:alpha").unwrap(),
        RateLimiter::new(10_000),
    );
    public_router_with(auth, api)
}

fn key_of(tenant: &str) -> StaticAuthBackend {
    StaticAuthBackend::authenticated(Identity {
        tenant_id: tenant.to_owned(),
        api_key_id: format!("key_{tenant}"),
    })
}

fn router_for(pool: &PgPool, tenant: &str) -> Router {
    let health = HealthState::with_providers(pool.clone(), Vec::new());
    router_as(ApiState::with_health(pool.clone(), health), key_of(tenant))
}

async fn send(router: Router, method: Method, path: &str, body: Body) -> (StatusCode, String) {
    let request = Request::builder()
        .method(method.clone())
        .uri(path)
        .header(header::AUTHORIZATION, "Bearer mk_live_test")
        .body(body)
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    eprintln!("{method} {path} -> {status} {text}");
    (status, text)
}

async fn get_as(pool: &PgPool, tenant: &str, path: &str) -> (StatusCode, String) {
    send(router_for(pool, tenant), Method::GET, path, Body::empty()).await
}

async fn put_as(pool: &PgPool, tenant: &str, path: &str, body: &str) -> (StatusCode, String) {
    send(
        router_for(pool, tenant),
        Method::PUT,
        path,
        Body::from(body.to_owned()),
    )
    .await
}

async fn get(pool: &PgPool, path: &str) -> (StatusCode, String) {
    get_as(pool, TENANT, path).await
}

async fn put(pool: &PgPool, path: &str, body: &str) -> (StatusCode, String) {
    put_as(pool, TENANT, path, body).await
}

fn parse(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("not JSON ({e}): {text}"))
}

/// Asserts a 422 with this code, message, and details.
fn assert_422(response: (StatusCode, String), code: &str, message: &str, details: Value) {
    let (status, text) = response;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{text}");
    let error = &parse(&text)["error"];
    assert_eq!(error["code"], code, "{text}");
    assert_eq!(error["message"], message, "{text}");
    assert_eq!(error["details"], details, "{text}");
}

async fn database() -> Option<(TestDatabase, PgPool)> {
    let db = maverick_testdb::database(&SCHEMA).await?;
    let pool = db.pool().await;
    for tenant in [TENANT, OTHER] {
        tenancy::create_tenant(&pool, tenant, "Acme", &[1; 32])
            .await
            .unwrap();
    }
    Some((db, pool))
}

async fn column(pool: &PgPool, id: &str, status: &str) {
    let empty = JsonRawValue::from_string("{}".to_owned()).unwrap();
    catalog::create_model_column(
        pool,
        &NewModelColumn {
            id,
            provider: "anthropic",
            model_id: id,
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
    .unwrap();
}

async fn aggregate(
    pool: &PgPool,
    column: &str,
    capability: &str,
    tier: i16,
    dimension: &str,
    ewma: f32,
    as_of: NaiveDate,
) {
    measurement::upsert_aggregate(
        pool,
        &AggregateUpsert {
            model_column_id: column,
            capability,
            difficulty_tier: tier,
            dimension,
            ewma,
            variance: 0.0,
            n: 20,
            measured: true,
            matrix_version: "2026-07-01",
            as_of,
        },
    )
    .await
    .unwrap();
}

fn day(text: &str) -> NaiveDate {
    NaiveDate::parse_from_str(text, "%Y-%m-%d").unwrap()
}

const POOLS: &str = "/v1/config/pools";
const WEIGHTS: &str = "/v1/config/weights";
const CONSTRAINTS: &str = "/v1/config/constraints";
/// Go's three keys, then D39's `telemetry_opt_out` and D44's `affinity_cost_aware`.
const NO_CONSTRAINTS: &str = "{\"constraints\":{\"max_cost_per_run\":null,\"max_latency_ms\":null,\"zero_retention\":false,\"telemetry_opt_out\":false,\"affinity_cost_aware\":false}}\n";

// parity: api.TestConfigPoolsRoundTrip
#[tokio::test]
async fn pools_round_trip() {
    let Some((db, pool)) = database().await else {
        return;
    };
    column(&pool, "col_a", "active").await;
    column(&pool, "col_b", "active").await;
    assert_eq!(
        get(&pool, POOLS).await,
        (StatusCode::OK, "{\"pools\":[]}\n".to_owned())
    );
    let body = r#"{"pools":[{"name":"vertex-only","model_column_ids":["col_b"]},{"name":"default","model_column_ids":["col_a","col_b"],"is_default":true}]}"#;
    let stored = "{\"pools\":[{\"name\":\"default\",\"model_column_ids\":[\"col_a\",\"col_b\"],\
                  \"is_default\":true},{\"name\":\"vertex-only\",\"model_column_ids\":[\"col_b\"],\
                  \"is_default\":false}]}\n";
    assert_eq!(
        put(&pool, POOLS, body).await,
        (StatusCode::OK, stored.to_owned())
    );
    assert_eq!(get(&pool, POOLS).await, (StatusCode::OK, stored.to_owned()));
    db.close().await;
}

// parity: api.TestConfigPoolsFullReplace
#[tokio::test]
async fn pools_full_replace() {
    let Some((db, pool)) = database().await else {
        return;
    };
    column(&pool, "col_a", "active").await;
    let (status, _) = put(
        &pool,
        POOLS,
        r#"{"pools":[{"name":"alpha","model_column_ids":["col_a"],"is_default":true},{"name":"beta","model_column_ids":["col_a"]}]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let gamma = r#"{"pools":[{"name":"gamma","model_column_ids":["col_a"],"is_default":true}]}"#;
    assert_eq!(
        put(&pool, POOLS, gamma).await,
        (StatusCode::OK, format!("{gamma}\n"))
    );
    assert_eq!(
        get(&pool, POOLS).await,
        (StatusCode::OK, format!("{gamma}\n"))
    );
    db.close().await;
}

// parity: api.TestConfigPoolsUnknownModel
#[tokio::test]
async fn pools_unknown_model() {
    let Some((db, pool)) = database().await else {
        return;
    };
    column(&pool, "col_a", "active").await;
    assert_422(
        put(
            &pool,
            POOLS,
            r#"{"pools":[{"name":"default","model_column_ids":["col_a","col_does_not_exist"],"is_default":true}]}"#,
        )
        .await,
        "unknown_model",
        "model_column_ids entry \"col_does_not_exist\" in pool \"default\" is not in the model catalog",
        json!({"entry": "col_does_not_exist", "pool": "default"}),
    );
    assert_eq!(get(&pool, POOLS).await.1, "{\"pools\":[]}\n");
    db.close().await;
}

// parity: api.TestConfigPoolsDefaultRule
// parity: api.TestConfigPoolsDuplicateName
#[tokio::test]
async fn pools_shape_rules() {
    let Some((db, pool)) = database().await else {
        return;
    };
    column(&pool, "col_a", "active").await;
    for (body, got) in [
        (
            r#"{"pools":[{"name":"a","model_column_ids":["col_a"]}]}"#,
            0,
        ),
        (
            r#"{"pools":[{"name":"a","model_column_ids":["col_a"],"is_default":true},{"name":"b","model_column_ids":["col_a"],"is_default":true}]}"#,
            2,
        ),
        (r#"{"pools":[]}"#, 0),
    ] {
        assert_422(
            put(&pool, POOLS, body).await,
            "invalid_request",
            &format!("exactly one pool must be is_default: true (got {got})"),
            json!({"default_count": got}),
        );
    }
    assert_422(
        put(
            &pool,
            POOLS,
            r#"{"pools":[{"name":"same","model_column_ids":["col_a"],"is_default":true},{"name":"same","model_column_ids":["col_a"]}]}"#,
        )
        .await,
        "invalid_request",
        "pool name \"same\" appears more than once",
        json!({"pool": "same"}),
    );
    assert_eq!(get(&pool, POOLS).await.1, "{\"pools\":[]}\n");
    db.close().await;
}

/// DIVERGENCE D45 (spec §9.7): the model grammar reserves the pool name `auto` and the `:`
/// character. The check runs after the shape rules and before the catalog and G4 checks, and
/// nothing is written. Go accepted both names.
#[tokio::test]
async fn pools_named_auto_or_with_a_colon_are_refused() {
    let Some((db, pool)) = database().await else {
        return;
    };
    column(&pool, "col_a", "active").await;
    assert_422(
        put(
            &pool,
            POOLS,
            r#"{"pools":[{"name":"team/a","model_column_ids":["col_a"],"is_default":true},{"name":"auto","model_column_ids":["col_a"]}]}"#,
        )
        .await,
        "invalid_request",
        "pool name \"auto\" is reserved",
        json!({"pool": "auto"}),
    );
    // The first offending pool in request order is named; a missing column is not reached.
    assert_422(
        put(
            &pool,
            POOLS,
            r#"{"pools":[{"name":"prod:cost","model_column_ids":["col_missing"],"is_default":true},{"name":"auto","model_column_ids":["col_a"]}]}"#,
        )
        .await,
        "invalid_request",
        "pool name \"prod:cost\" must not contain ':'",
        json!({"pool": "prod:cost"}),
    );
    // A shape error still comes first, as in Go.
    assert_422(
        put(
            &pool,
            POOLS,
            r#"{"pools":[{"name":"auto","model_column_ids":["col_a"]}]}"#,
        )
        .await,
        "invalid_request",
        "exactly one pool must be is_default: true (got 0)",
        json!({"default_count": 0}),
    );
    assert_eq!(get(&pool, POOLS).await.1, "{\"pools\":[]}\n");
    // Names with `/` and other case are routable, so they are stored.
    let body = r#"{"pools":[{"name":"AUTO","model_column_ids":["col_a"]},{"name":"team/a","model_column_ids":["col_a"],"is_default":true}]}"#;
    assert_eq!(put(&pool, POOLS, body).await.0, StatusCode::OK);
    db.close().await;
}

// parity: api.TestConfigPoolsAllRetired
#[tokio::test]
async fn pools_of_only_retired_columns_would_be_empty() {
    let Some((db, pool)) = database().await else {
        return;
    };
    column(&pool, "col_live", "active").await;
    column(&pool, "col_dead", "retired").await;
    assert_422(
        put(
            &pool,
            POOLS,
            r#"{"pools":[{"name":"default","model_column_ids":["col_live"],"is_default":true},{"name":"graveyard","model_column_ids":["col_dead"]}]}"#,
        )
        .await,
        "pool_would_be_empty",
        "pool \"graveyard\" would have no usable columns: every column is retired",
        json!({"constraint": "status_retired", "pool": "graveyard"}),
    );
    db.close().await;
}

// parity: api.TestConfigPoolsRejectedByStoredConstraint
#[tokio::test]
async fn pools_are_simulated_against_the_stored_constraints() {
    let Some((db, pool)) = database().await else {
        return;
    };
    let today = Utc::now().date_naive();
    for (id, cost) in [("col_cheap", 0.005f32), ("col_pricey", 0.02)] {
        column(&pool, id, "active").await;
        aggregate(
            &pool,
            id,
            "structured_extraction",
            3,
            "cost_per_run_usd",
            cost,
            today,
        )
        .await;
    }
    let first =
        r#"{"pools":[{"name":"default","model_column_ids":["col_cheap"],"is_default":true}]}"#;
    assert_eq!(put(&pool, POOLS, first).await.0, StatusCode::OK);
    let (status, _) = put(
        &pool,
        CONSTRAINTS,
        r#"{"constraints":{"max_cost_per_run":0.01}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_422(
        put(
            &pool,
            POOLS,
            r#"{"pools":[{"name":"pricey-only","model_column_ids":["col_pricey"]},{"name":"default","model_column_ids":["col_cheap"],"is_default":true}]}"#,
        )
        .await,
        "pool_would_be_empty",
        "constraint \"max_cost_per_run\" would leave pool \"pricey-only\" with zero candidates for \
         structured_extraction tier 3",
        json!({"capability": "structured_extraction", "constraint": "max_cost_per_run",
               "difficulty_tier": 3, "pool": "pricey-only"}),
    );
    assert_eq!(get(&pool, POOLS).await.1, format!("{first}\n"));
    db.close().await;
}

// parity: api.TestConfigConstraintsRoundTrip
#[tokio::test]
async fn constraints_round_trip() {
    let Some((db, pool)) = database().await else {
        return;
    };
    assert_eq!(
        get(&pool, CONSTRAINTS).await,
        (StatusCode::OK, NO_CONSTRAINTS.to_owned())
    );
    let full = "{\"constraints\":{\"max_cost_per_run\":0.25,\"max_latency_ms\":4000,\
                \"zero_retention\":true,\"telemetry_opt_out\":false,\"affinity_cost_aware\":false}}\n";
    assert_eq!(
        put(
            &pool,
            CONSTRAINTS,
            r#"{"constraints":{"max_cost_per_run":0.25,"max_latency_ms":4000,"zero_retention":true}}"#
        )
        .await,
        (StatusCode::OK, full.to_owned())
    );
    assert_eq!(
        get(&pool, CONSTRAINTS).await,
        (StatusCode::OK, full.to_owned())
    );
    assert_eq!(
        put(&pool, CONSTRAINTS, r#"{"constraints":{}}"#).await,
        (StatusCode::OK, NO_CONSTRAINTS.to_owned())
    );
    assert_eq!(get(&pool, CONSTRAINTS).await.1, NO_CONSTRAINTS);
    db.close().await;
}

// parity: api.TestConfigConstraintsWouldEmptyPool
// parity: api.TestConfigConstraintsLatencyWouldEmptyPool
// parity: api.TestConfigConstraintsNameTheEmptiedPool
#[tokio::test]
async fn constraints_that_empty_a_pool_are_refused() {
    let Some((db, pool)) = database().await else {
        return;
    };
    let today = Utc::now().date_naive();
    column(&pool, "col_cheap", "active").await;
    column(&pool, "col_pricey", "active").await;
    column(&pool, "col_slow", "active").await;
    aggregate(
        &pool,
        "col_cheap",
        "code_generation",
        3,
        "cost_per_run_usd",
        0.005,
        today,
    )
    .await;
    aggregate(
        &pool,
        "col_pricey",
        "code_generation",
        3,
        "cost_per_run_usd",
        0.02,
        today,
    )
    .await;
    aggregate(
        &pool,
        "col_pricey",
        "code_generation",
        4,
        "cost_per_run_usd",
        0.02,
        today,
    )
    .await;
    aggregate(
        &pool,
        "col_slow",
        "long_context_qa",
        5,
        "latency_p95_ms",
        9000.0,
        today,
    )
    .await;

    // Only pricey (code_generation t3 and t4 at 0.02): the first cell in sorted order names t3.
    tenancy::create_pool(
        &pool,
        "pool_1",
        TENANT,
        "default",
        &["col_pricey".to_owned()],
        true,
    )
    .await
    .unwrap();
    assert_422(
        put(
            &pool,
            CONSTRAINTS,
            r#"{"constraints":{"max_cost_per_run":0.01}}"#,
        )
        .await,
        "pool_would_be_empty",
        "constraint \"max_cost_per_run\" would leave pool \"default\" with zero candidates for \
         code_generation tier 3",
        json!({"capability": "code_generation", "constraint": "max_cost_per_run",
               "difficulty_tier": 3, "pool": "default"}),
    );
    assert_eq!(
        get(&pool, CONSTRAINTS).await.1,
        NO_CONSTRAINTS,
        "nothing written"
    );

    tenancy::delete_pools_by_tenant(&pool, TENANT)
        .await
        .unwrap();
    tenancy::create_pool(
        &pool,
        "pool_2",
        TENANT,
        "default",
        &["col_slow".to_owned()],
        true,
    )
    .await
    .unwrap();
    assert_422(
        put(
            &pool,
            CONSTRAINTS,
            r#"{"constraints":{"max_latency_ms":2000}}"#,
        )
        .await,
        "pool_would_be_empty",
        "constraint \"max_latency_ms\" would leave pool \"default\" with zero candidates for \
         long_context_qa tier 5",
        json!({"capability": "long_context_qa", "constraint": "max_latency_ms",
               "difficulty_tier": 5, "pool": "default"}),
    );

    tenancy::delete_pools_by_tenant(&pool, TENANT)
        .await
        .unwrap();
    tenancy::create_pool(
        &pool,
        "pool_3",
        TENANT,
        "default",
        &["col_cheap".to_owned()],
        true,
    )
    .await
    .unwrap();
    tenancy::create_pool(
        &pool,
        "pool_4",
        TENANT,
        "expensive",
        &["col_pricey".to_owned()],
        false,
    )
    .await
    .unwrap();
    assert_422(
        put(
            &pool,
            CONSTRAINTS,
            r#"{"constraints":{"max_cost_per_run":0.01}}"#,
        )
        .await,
        "pool_would_be_empty",
        "constraint \"max_cost_per_run\" would leave pool \"expensive\" with zero candidates for \
         code_generation tier 3",
        json!({"capability": "code_generation", "constraint": "max_cost_per_run",
               "difficulty_tier": 3, "pool": "expensive"}),
    );
    db.close().await;
}

// parity: api.TestConfigConstraintsUnmeasuredColumnsSurvive
// parity: api.TestConfigConstraintsUseLatestAggregate
#[tokio::test]
async fn unmeasured_columns_survive_and_only_the_latest_aggregate_counts() {
    let Some((db, pool)) = database().await else {
        return;
    };
    column(&pool, "col_cold", "cold_start").await;
    tenancy::create_pool(
        &pool,
        "pool_1",
        TENANT,
        "default",
        &["col_cold".to_owned()],
        true,
    )
    .await
    .unwrap();
    assert_eq!(
        put(
            &pool,
            CONSTRAINTS,
            r#"{"constraints":{"max_cost_per_run":0.000001}}"#
        )
        .await,
        (
            StatusCode::OK,
            "{\"constraints\":{\"max_cost_per_run\":0.000001,\"max_latency_ms\":null,\
             \"zero_retention\":false,\"telemetry_opt_out\":false,\"affinity_cost_aware\":false}}\n"
                .to_owned()
        )
    );

    column(&pool, "col_qa", "active").await;
    aggregate(
        &pool,
        "col_qa",
        "factual_qa",
        2,
        "cost_per_run_usd",
        0.02,
        day("2026-06-01"),
    )
    .await;
    aggregate(
        &pool,
        "col_qa",
        "factual_qa",
        2,
        "cost_per_run_usd",
        0.005,
        day("2026-07-01"),
    )
    .await;
    tenancy::delete_pools_by_tenant(&pool, TENANT)
        .await
        .unwrap();
    tenancy::create_pool(
        &pool,
        "pool_2",
        TENANT,
        "default",
        &["col_qa".to_owned()],
        true,
    )
    .await
    .unwrap();
    let (status, _) = put(
        &pool,
        CONSTRAINTS,
        r#"{"constraints":{"max_cost_per_run":0.01}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    db.close().await;
}

/// D1: zero retention is simulated with the constraints, so a pool without valid
/// zero-retention evidence refuses the change. D39: the PUT is a full replace (spec
/// §9.7), so a payload without `telemetry_opt_out` stores `false`.
#[tokio::test]
async fn zero_retention_is_simulated_and_telemetry_opt_out_is_replaced() {
    let Some((db, pool)) = database().await else {
        return;
    };
    column(&pool, "col_std", "active").await;
    tenancy::create_pool(
        &pool,
        "pool_1",
        TENANT,
        "default",
        &["col_std".to_owned()],
        true,
    )
    .await
    .unwrap();
    let (status, text) = put(
        &pool,
        CONSTRAINTS,
        r#"{"constraints":{"zero_retention":true}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let error = &parse(&text)["error"];
    assert_eq!(error["code"], "pool_would_be_empty");
    assert_eq!(error["details"]["constraint"], "zero_retention");
    assert_eq!(error["details"]["pool"], "default");

    sqlx::query("UPDATE model_columns SET retention_mode = 'zero', retention_valid_until = $1")
        .bind(Utc::now() + Duration::days(30))
        .execute(&pool)
        .await
        .unwrap();
    tenancy::upsert_tenant_constraints(&pool, TENANT, None, None, false, true, false)
        .await
        .unwrap();
    let (status, _) = put(
        &pool,
        CONSTRAINTS,
        r#"{"constraints":{"zero_retention":true}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let row = tenancy::get_tenant_constraints(&pool, TENANT)
        .await
        .unwrap();
    assert!(row.zero_retention);
    assert!(
        !row.telemetry_opt_out,
        "D39: a full-replace PUT without the key clears the opt-out"
    );
    db.close().await;
}

/// DIVERGENCE: D39. `telemetry_opt_out` is written and read through the constraints
/// payload as the last key. It filters no column, so it never trips G4: an opt-out PUT
/// succeeds for a pool that the other constraints leave intact.
#[tokio::test]
async fn telemetry_opt_out_round_trips() {
    let d39 = parity_support::divergence!("D39");
    assert_eq!(d39.id(), "D39");
    let Some((db, pool)) = database().await else {
        return;
    };
    column(&pool, "col_std", "active").await;
    tenancy::create_pool(
        &pool,
        "pool_1",
        TENANT,
        "default",
        &["col_std".to_owned()],
        true,
    )
    .await
    .unwrap();
    let opted_out = "{\"constraints\":{\"max_cost_per_run\":null,\"max_latency_ms\":null,\
                     \"zero_retention\":false,\"telemetry_opt_out\":true,\"affinity_cost_aware\":false}}\n";
    assert_eq!(
        put(
            &pool,
            CONSTRAINTS,
            r#"{"constraints":{"telemetry_opt_out":true}}"#
        )
        .await,
        (StatusCode::OK, opted_out.to_owned())
    );
    assert_eq!(
        get(&pool, CONSTRAINTS).await,
        (StatusCode::OK, opted_out.to_owned())
    );
    let row = tenancy::get_tenant_constraints(&pool, TENANT)
        .await
        .unwrap();
    assert!(row.telemetry_opt_out && !row.zero_retention);

    // Another tenant is unaffected.
    assert_eq!(
        get_as(&pool, "tenant_other", CONSTRAINTS).await,
        (StatusCode::OK, NO_CONSTRAINTS.to_owned())
    );

    // A wrongly typed value is the Go-strict decode error, and nothing is written.
    let (status, text) = put(
        &pool,
        CONSTRAINTS,
        r#"{"constraints":{"telemetry_opt_out":"no"}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(parse(&text)["error"]["code"], "invalid_request");
    assert_eq!(
        parse(&text)["error"]["message"],
        "malformed request body: json: cannot unmarshal string into Go struct field \
         ConstraintsInput.constraints.telemetry_opt_out of type bool"
    );
    assert_eq!(get(&pool, CONSTRAINTS).await.1, opted_out);

    // Opting back in.
    assert_eq!(
        put(
            &pool,
            CONSTRAINTS,
            r#"{"constraints":{"telemetry_opt_out":false}}"#
        )
        .await,
        (StatusCode::OK, NO_CONSTRAINTS.to_owned())
    );
    db.close().await;
}

// parity: api.TestConfigWeightsRoundTrip
// parity: api.TestConfigWeightsFullReplace
#[tokio::test]
async fn weights_round_trip_and_full_replace() {
    let Some((db, pool)) = database().await else {
        return;
    };
    assert_eq!(
        get(&pool, WEIGHTS).await,
        (StatusCode::OK, "{\"weights\":[]}\n".to_owned())
    );
    let body = "{\"weights\": [\n\t\t{\"capability\": \"structured_extraction\", \"w_quality\": 0.5, \
                \"w_cost\": 0.35, \"w_latency\": 0.15, \"quality_bar\": 0.9},\n\t\t{\"capability\": \
                \"code_generation\", \"w_quality\": 0.8, \"w_cost\": 0.1, \"w_latency\": 0.1}\n\t]}";
    let stored = "{\"weights\":[{\"capability\":\"code_generation\",\"w_quality\":0.8,\"w_cost\":0.1,\
                  \"w_latency\":0.1,\"quality_bar\":0.8},{\"capability\":\"structured_extraction\",\
                  \"w_quality\":0.5,\"w_cost\":0.35,\"w_latency\":0.15,\"quality_bar\":0.9}]}\n";
    assert_eq!(
        put(&pool, WEIGHTS, body).await,
        (StatusCode::OK, stored.to_owned())
    );
    assert_eq!(
        get(&pool, WEIGHTS).await,
        (StatusCode::OK, stored.to_owned())
    );
    let only = "{\"weights\":[{\"capability\":\"factual_qa\",\"w_quality\":0.5,\"w_cost\":0.35,\
                \"w_latency\":0.15,\"quality_bar\":0.8}]}\n";
    assert_eq!(
        put(
            &pool,
            WEIGHTS,
            r#"{"weights": [{"capability": "factual_qa", "w_quality": 0.5, "w_cost": 0.35, "w_latency": 0.15}]}"#
        )
        .await,
        (StatusCode::OK, only.to_owned())
    );
    assert_eq!(get(&pool, WEIGHTS).await.1, only);
    db.close().await;
}

// parity: api.TestConfigWeightsValidation
// parity: api.TestConfigWeightsSumTolerance
#[tokio::test]
async fn weights_validation() {
    let Some((db, pool)) = database().await else {
        return;
    };
    let row = |q: &str, c: &str, l: &str, bar: &str| {
        format!(
            r#"{{"weights":[{{"capability":"code_generation","w_quality":{q},"w_cost":{c},"w_latency":{l}{bar}}}]}}"#
        )
    };
    let cases = [
        (
            row("0.5", "0.3", "0.1", ""),
            "weights for \"code_generation\" must sum to 1 (got 0.9000)",
            json!({"capability": "code_generation", "sum": 0.9000000134110451}),
        ),
        (
            row("0.6", "0.4", "0.2", ""),
            "weights for \"code_generation\" must sum to 1 (got 1.2000)",
            // The widened float32 sum (QUIRK), computed here for these inputs.
            json!({"capability": "code_generation",
                   "sum": f64::from(0.6f32) + f64::from(0.4f32) + f64::from(0.2f32)}),
        ),
        (
            row("-0.1", "0.6", "0.5", ""),
            "w_quality for \"code_generation\" must be between 0 and 1",
            json!({"capability": "code_generation", "field": "w_quality"}),
        ),
        (
            row("1.1", "0.0", "-0.1", ""),
            "w_quality for \"code_generation\" must be between 0 and 1",
            json!({"capability": "code_generation", "field": "w_quality"}),
        ),
        (
            row("0.5", "0.35", "0.15", r#","quality_bar":0"#),
            "quality_bar for \"code_generation\" must be greater than 0 and at most 1",
            json!({"capability": "code_generation", "field": "quality_bar"}),
        ),
        (
            row("0.5", "0.35", "0.15", r#","quality_bar":1.5"#),
            "quality_bar for \"code_generation\" must be greater than 0 and at most 1",
            json!({"capability": "code_generation", "field": "quality_bar"}),
        ),
        (
            r#"{"weights":[{"capability":"telepathy","w_quality":0.5,"w_cost":0.35,"w_latency":0.15}]}"#.to_owned(),
            "capability \"telepathy\" is not a v1 capability",
            json!({"capability": "telepathy"}),
        ),
        (
            r#"{"weights":[{"capability":"code_generation","w_quality":0.5,"w_cost":0.35,"w_latency":0.15},{"capability":"code_generation","w_quality":0.5,"w_cost":0.35,"w_latency":0.15}]}"#.to_owned(),
            "capability \"code_generation\" appears more than once",
            json!({"capability": "code_generation"}),
        ),
        (
            r#"{"weights":[{"capability":"code_generation","w_quality":0.8,"w_cost":0.1}]}"#.to_owned(),
            "weights[0].w_latency is required",
            json!({"field": "weights[0].w_latency"}),
        ),
        (
            row("0.5", "0.3", "0.18", ""),
            "weights for \"code_generation\" must sum to 1 (got 0.9800)",
            json!({"capability": "code_generation", "sum": 0.9800000190734863}),
        ),
    ];
    for (body, message, details) in cases {
        assert_422(
            put(&pool, WEIGHTS, &body).await,
            "invalid_request",
            message,
            details,
        );
    }
    assert_eq!(
        get(&pool, WEIGHTS).await.1,
        "{\"weights\":[]}\n",
        "nothing written"
    );
    // 0.5 + 0.35 + 0.1505 = 1.0005 is inside the 0.001 tolerance.
    let (status, text) = put(&pool, WEIGHTS, &row("0.5", "0.35", "0.1505", "")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(text.contains("\"w_latency\":0.1505"), "{text}");
    db.close().await;
}

// parity: api.TestConfigStrictDecoding
#[tokio::test]
async fn strict_decoding() {
    let Some((db, pool)) = database().await else {
        return;
    };
    let cases = [
        (
            POOLS,
            r#"{"pools":[],"surprise":1}"#,
            "malformed request body: json: unknown field \"surprise\"",
        ),
        (
            POOLS,
            r#"{"pools":[{"name":"a","model_column_ids":["x"],"color":"red"}]}"#,
            "malformed request body: json: unknown field \"color\"",
        ),
        (
            POOLS,
            r#"{"pools":["#,
            "malformed request body: unexpected EOF",
        ),
        (POOLS, "", "request body is required"),
        (
            POOLS,
            r#"{"pools":[]}{"x":1}"#,
            "request body has trailing content",
        ),
        (POOLS, "{}", "pools is required"),
        (WEIGHTS, "{}", "weights is required"),
        (CONSTRAINTS, "{}", "constraints is required"),
        (
            POOLS,
            r#"{"pools":[{"model_column_ids":["x"],"is_default":true}]}"#,
            "pools[0].name is required",
        ),
        (
            POOLS,
            r#"{"pools":[{"name":"a","is_default":true}]}"#,
            "pools[0].model_column_ids is required",
        ),
        (
            CONSTRAINTS,
            r#"{"constraints":{"max_cost_per_run":-1}}"#,
            "max_cost_per_run must be greater than 0",
        ),
        (
            CONSTRAINTS,
            r#"{"constraints":{"max_latency_ms":0}}"#,
            "max_latency_ms must be greater than 0",
        ),
    ];
    for (path, body, message) in cases {
        let (status, text) = put(&pool, path, body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{path} {body}");
        let error = &parse(&text)["error"];
        assert_eq!(error["code"], "invalid_request", "{path} {body}");
        assert_eq!(error["message"], message, "{path} {body}");
    }
    assert_eq!(get(&pool, POOLS).await.1, "{\"pools\":[]}\n");
    assert_eq!(get(&pool, CONSTRAINTS).await.1, NO_CONSTRAINTS);
    db.close().await;
}

// parity: api.TestConfigCrossTenantIsolation
#[tokio::test]
async fn tenants_are_isolated() {
    let Some((db, pool)) = database().await else {
        return;
    };
    column(&pool, "col_a", "active").await;
    let a = r#"{"pools":[{"name":"a-pool","model_column_ids":["col_a"],"is_default":true}]}"#;
    let b = r#"{"pools":[{"name":"b-pool","model_column_ids":["col_a"],"is_default":true}]}"#;
    assert_eq!(put_as(&pool, TENANT, POOLS, a).await.0, StatusCode::OK);
    assert_eq!(get_as(&pool, OTHER, POOLS).await.1, "{\"pools\":[]}\n");
    assert_eq!(put_as(&pool, OTHER, POOLS, b).await.0, StatusCode::OK);
    assert_eq!(get_as(&pool, TENANT, POOLS).await.1, format!("{a}\n"));
    assert_eq!(get_as(&pool, OTHER, POOLS).await.1, format!("{b}\n"));
    db.close().await;
}

// parity: api.TestConfigOversizeBodyDeclaredLength413
// parity: api.TestConfigOversizeBodyChunked413
#[tokio::test]
async fn oversize_bodies_are_413() {
    let router = router_as(ApiState::detached(), key_of(TENANT));
    let too_large = "{\"error\":{\"code\":\"payload_too_large\",\"message\":\"request body exceeds the \
         limit\",\"details\":{\"limit_bytes\":2097152}}}\n";
    let big = vec![b' '; 3 << 20];
    let declared = Request::builder()
        .method(Method::PUT)
        .uri(POOLS)
        .header(header::AUTHORIZATION, "Bearer mk_live_test")
        .header(header::CONTENT_LENGTH, big.len())
        .body(Body::from(big.clone()))
        .unwrap();
    let response = router.clone().oneshot(declared).await.unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(bytes, too_large.as_bytes());

    let chunks = big
        .chunks(1 << 20)
        .map(|chunk| Ok::<_, std::io::Error>(Bytes::copy_from_slice(chunk)))
        .collect::<Vec<_>>();
    let (status, text) = send(
        router,
        Method::PUT,
        POOLS,
        Body::from_stream(futures_util::stream::iter(chunks)),
    )
    .await;
    assert_eq!(
        (status, text.as_str()),
        (StatusCode::PAYLOAD_TOO_LARGE, too_large)
    );
}

// parity: api.TestConfigWrongMethod
// parity: api.TestConfigRequiresAuth
#[tokio::test]
async fn wrong_methods_and_missing_keys() {
    let not_found =
        "{\"error\":{\"code\":\"not_found\",\"message\":\"resource not found\",\"details\":{}}}\n";
    let unauthorized = "{\"error\":{\"code\":\"unauthorized\",\"message\":\"missing or invalid \
                        API key\",\"details\":{}}}\n";
    let router = router_as(ApiState::detached(), key_of(TENANT));
    let (status, text) = send(router, Method::POST, POOLS, Body::from("{}")).await;
    assert_eq!((status, text.as_str()), (StatusCode::NOT_FOUND, not_found));
    for path in [POOLS, WEIGHTS, CONSTRAINTS] {
        let router = router_as(ApiState::detached(), StaticAuthBackend::missing());
        let (status, text) = send(router, Method::GET, path, Body::empty()).await;
        assert_eq!(
            (status, text.as_str()),
            (StatusCode::UNAUTHORIZED, unauthorized),
            "{path}"
        );
    }
}
