//! `GET /v1/policies` on real Postgres (bead 5.5; reference Part 5 §6.4 and §13.3, Part 4
//! §2.5). Error bodies are the exact ones in `parity/fixtures/api/error_catalog_observed.json`.

use std::sync::{Arc, LazyLock};

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use chrono::{NaiveDate, Utc};
use maverick_api::{
    ApiState, AuthState, HealthState, Identity, RateLimiter, StaticAuthBackend, public_router_with,
};
use maverick_domain::keys;
use maverick_store::catalog::{self, NewModelColumn};
use maverick_store::measurement::{self, AggregateUpsert};
use maverick_store::{JsonRawValue, MIGRATOR, corpus, tenancy};
use maverick_testdb::{Schema, TestDatabase};
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

const TENANT: &str = "ten_policies";
const OTHER: &str = "ten_policies_b";
const CORPUS: &str = "2026-07-01";

fn router(pool: Option<&PgPool>, tenant: &str, backend: Option<StaticAuthBackend>) -> Router {
    let backend = backend.unwrap_or_else(|| {
        StaticAuthBackend::authenticated(Identity {
            tenant_id: tenant.to_owned(),
            api_key_id: format!("key_{tenant}"),
        })
    });
    let auth = AuthState::new(
        Arc::new(backend),
        keys::parse_internal_tokens("ops:alpha").unwrap(),
        RateLimiter::new(10_000),
    );
    let api = match pool {
        Some(pool) => ApiState::with_health(
            pool.clone(),
            HealthState::with_providers(pool.clone(), Vec::new()),
        ),
        None => ApiState::detached(),
    };
    public_router_with(auth, api)
}

async fn get(router: Router, path: &str) -> (StatusCode, String) {
    let request = Request::builder()
        .method(Method::GET)
        .uri(path)
        .header(header::AUTHORIZATION, "Bearer mk_live_test")
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    eprintln!("GET {path} -> {status} {text}");
    (status, text)
}

async fn policies(pool: &PgPool, query: &str) -> (StatusCode, String) {
    get(
        router(Some(pool), TENANT, None),
        &format!("/v1/policies{query}"),
    )
    .await
}

fn parse(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("not JSON ({e}): {text}"))
}

async fn database() -> Option<(TestDatabase, PgPool)> {
    let db = maverick_testdb::database(&SCHEMA).await?;
    let pool = db.pool().await;
    for tenant in [TENANT, OTHER] {
        tenancy::create_tenant(&pool, tenant, tenant, &[1; 32])
            .await
            .unwrap();
    }
    corpus::upsert_corpus_version(&pool, CORPUS, "", 0, false)
        .await
        .unwrap();
    corpus::activate_corpus_version(&pool, CORPUS)
        .await
        .unwrap();
    Some((db, pool))
}

async fn column(
    pool: &PgPool,
    id: &str,
    endpoint: &str,
    status: &str,
    quality_source: Option<&str>,
) {
    let empty = JsonRawValue::from_string("{}".to_owned()).unwrap();
    catalog::create_model_column(
        pool,
        &NewModelColumn {
            id,
            provider: "anthropic",
            model_id: &format!("model-{id}"),
            endpoint,
            settings: &empty,
            status,
            pricing: &empty,
            quality_source_column_id: quality_source,
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

async fn pool_of(pool: &PgPool, tenant: &str, name: &str, ids: &[&str], default: bool) {
    let ids: Vec<String> = ids.iter().map(|id| (*id).to_owned()).collect();
    tenancy::create_pool(
        pool,
        &format!("pool_{tenant}_{name}"),
        tenant,
        name,
        &ids,
        default,
    )
    .await
    .unwrap();
}

fn night() -> NaiveDate {
    Utc::now().date_naive()
}

async fn aggregate(pool: &PgPool, column: &str, capability: &str, tier: i16, dim: &str, ewma: f32) {
    measurement::upsert_aggregate(
        pool,
        &AggregateUpsert {
            model_column_id: column,
            capability,
            difficulty_tier: tier,
            dimension: dim,
            ewma,
            variance: 0.0,
            n: 20,
            measured: true,
            matrix_version: CORPUS,
            as_of: night(),
        },
    )
    .await
    .unwrap();
}

/// Pass rates for tiers 1.. and one flat cost at every tier.
async fn cells(pool: &PgPool, column: &str, capability: &str, rates: &[f32], cost: Option<f32>) {
    for (tier, rate) in (1..).zip(rates) {
        aggregate(pool, column, capability, tier, "pass_rate", *rate).await;
    }
    if let Some(cost) = cost {
        for tier in 1..=5 {
            aggregate(pool, column, capability, tier, "cost_per_run_usd", cost).await;
        }
    }
}

/// `(column_id, ceiling, clears_band)` of each band.
fn bands(policy: &Value) -> Vec<(String, u64, bool)> {
    policy["bands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|band| {
            (
                band["column"]["column_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                band["ceiling"].as_u64().unwrap(),
                band["clears_band"].as_bool().unwrap(),
            )
        })
        .collect()
}

// parity: api.TestPoliciesRequiresAuth
#[tokio::test]
async fn policies_require_auth() {
    let router = router(None, TENANT, Some(StaticAuthBackend::missing()));
    let (status, body) = get(router, "/v1/policies").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        body,
        "{\"error\":{\"code\":\"unauthorized\",\"message\":\"missing or invalid API key\",\"details\":{}}}\n"
    );
}

// parity: api.TestPoliciesResponseShapeByteExact
#[tokio::test]
async fn response_shape_is_byte_exact() {
    let Some((db, pool)) = database().await else {
        return;
    };
    for id in ["col_cheap", "col_mid", "col_top"] {
        column(&pool, id, "direct", "active", None).await;
    }
    pool_of(
        &pool,
        TENANT,
        "default",
        &["col_cheap", "col_mid", "col_top"],
        true,
    )
    .await;
    let capability = "structured_extraction";
    cells(
        &pool,
        "col_cheap",
        capability,
        &[0.99, 0.9, 0.7, 0.6, 0.5],
        Some(0.001),
    )
    .await;
    cells(
        &pool,
        "col_mid",
        capability,
        &[0.98, 0.95, 0.92, 0.88, 0.8],
        Some(0.004),
    )
    .await;
    cells(
        &pool,
        "col_top",
        capability,
        &[0.99, 0.98, 0.96, 0.93, 0.9],
        Some(0.02),
    )
    .await;
    let (status, body) = policies(&pool, "?capability=structured_extraction").await;
    assert_eq!(status, StatusCode::OK);
    let column = |id: &str| {
        format!(
            r#"{{"provider":"anthropic","model":"model-{id}","endpoint":"direct","column_id":"{id}"}}"#
        )
    };
    let want = format!(
        r#"{{"pool":"default","quality_bar":0.8,"buffer":0.05,"policies":[{{"capability":"structured_extraction","bands":[{{"band":"easy","tiers":[1,2],"column":{},"ceiling":2,"pass_rate":0.9,"clears_band":true}},{{"band":"medium","tiers":[3],"column":{},"ceiling":4,"pass_rate":0.92,"clears_band":true}},{{"band":"hard","tiers":[4,5],"column":{},"ceiling":5,"pass_rate":0.9,"clears_band":true}}]}}],"matrix_version":"2026-07-01"}}
"#,
        column("col_cheap"),
        column("col_mid"),
        column("col_top")
    );
    assert_eq!(body, want);
    db.close().await;
}

// parity: api.TestPoliciesAllCapabilitiesInOrder
#[tokio::test]
async fn all_capabilities_in_the_fixed_order() {
    let Some((db, pool)) = database().await else {
        return;
    };
    column(&pool, "col_one", "direct", "active", None).await;
    pool_of(&pool, TENANT, "default", &["col_one"], true).await;
    cells(
        &pool,
        "col_one",
        "factual_qa",
        &[0.95, 0.9, 0.88, 0.86, 0.86],
        None,
    )
    .await;
    let (status, body) = policies(&pool, "").await;
    assert_eq!(status, StatusCode::OK);
    let body = parse(&body);
    assert_eq!(body["quality_bar"], 0.8);
    let policies = body["policies"].as_array().unwrap();
    let order: Vec<&str> = policies
        .iter()
        .map(|p| p["capability"].as_str().unwrap())
        .collect();
    assert_eq!(
        order,
        [
            "code_generation",
            "structured_extraction",
            "summarization_synthesis",
            "multi_step_reasoning",
            "instruction_schema_following",
            "long_context_qa",
            "factual_qa",
            "rewriting_editing",
            "translation_multilingual",
            "critique_and_judging"
        ]
    );
    for (index, policy) in policies.iter().enumerate() {
        let want = if index == 6 { (5, true) } else { (0, false) };
        for band in bands(policy) {
            assert_eq!(band.0, "col_one", "G1: a band always names the column");
            assert_eq!((band.1, band.2), want, "{}", policy["capability"]);
        }
    }
    db.close().await;
}

// parity: api.TestPoliciesClearsBandFalseFallback
#[tokio::test]
async fn a_band_nothing_clears_falls_back_to_the_highest_ceiling() {
    let Some((db, pool)) = database().await else {
        return;
    };
    column(&pool, "col_weak", "direct", "active", None).await;
    column(&pool, "col_weaker", "direct", "active", None).await;
    pool_of(&pool, TENANT, "default", &["col_weak", "col_weaker"], true).await;
    let capability = "multi_step_reasoning";
    cells(
        &pool,
        "col_weak",
        capability,
        &[0.9, 0.88, 0.7, 0.5, 0.4],
        Some(0.01),
    )
    .await;
    cells(
        &pool,
        "col_weaker",
        capability,
        &[0.6, 0.5, 0.4, 0.3, 0.2],
        Some(0.001),
    )
    .await;
    let (_, body) = policies(&pool, "?capability=multi_step_reasoning").await;
    let body = parse(&body);
    assert_eq!(
        bands(&body["policies"][0]),
        [
            ("col_weak".to_owned(), 2, true),
            ("col_weak".to_owned(), 2, false),
            ("col_weak".to_owned(), 2, false)
        ]
    );
    db.close().await;
}

// parity: api.TestPoliciesQualityBarOverride
#[tokio::test]
async fn the_tenant_bar_overrides_the_default() {
    let Some((db, pool)) = database().await else {
        return;
    };
    column(&pool, "col_code", "direct", "active", None).await;
    pool_of(&pool, TENANT, "default", &["col_code"], true).await;
    cells(&pool, "col_code", "code_generation", &[0.9; 5], None).await;
    tenancy::create_tenant_weight(&pool, TENANT, "code_generation", 0.5, 0.3, 0.2, 0.9)
        .await
        .unwrap();
    let (_, body) = policies(&pool, "?capability=code_generation").await;
    let body = parse(&body);
    assert_eq!(body["quality_bar"], 0.9);
    for band in bands(&body["policies"][0]) {
        assert_eq!((band.1, band.2), (0, false));
    }
    // QUIRK: the full table always shows the default bar, though the ceilings use 0.9.
    let (_, all) = policies(&pool, "").await;
    let all = parse(&all);
    assert_eq!(all["quality_bar"], 0.8);
    for band in bands(&all["policies"][0]) {
        assert_eq!((band.1, band.2), (0, false));
    }
    db.close().await;
}

// parity: api.TestPoliciesQualitySourceColumn
#[tokio::test]
async fn pass_rates_come_from_the_quality_source() {
    let Some((db, pool)) = database().await else {
        return;
    };
    column(&pool, "col_source", "direct", "active", None).await;
    column(&pool, "col_derived", "vertex", "active", Some("col_source")).await;
    pool_of(&pool, TENANT, "default", &["col_derived"], true).await;
    let capability = "long_context_qa";
    cells(
        &pool,
        "col_source",
        capability,
        &[0.95, 0.92, 0.9, 0.88, 0.87],
        None,
    )
    .await;
    for tier in 1..=5 {
        aggregate(
            &pool,
            "col_derived",
            capability,
            tier,
            "cost_per_run_usd",
            0.003,
        )
        .await;
    }
    let (_, body) = policies(&pool, "?capability=long_context_qa").await;
    let body = parse(&body);
    for band in bands(&body["policies"][0]) {
        assert_eq!(band, ("col_derived".to_owned(), 5, true));
    }
    assert_eq!(body["policies"][0]["bands"][2]["pass_rate"], 0.87);
    db.close().await;
}

// parity: api.TestPoliciesExcludesRetiredColumns
#[tokio::test]
async fn retired_columns_are_never_candidates() {
    let Some((db, pool)) = database().await else {
        return;
    };
    column(&pool, "col_retired", "direct", "retired", None).await;
    column(&pool, "col_active", "direct", "active", None).await;
    pool_of(
        &pool,
        TENANT,
        "default",
        &["col_retired", "col_active"],
        true,
    )
    .await;
    cells(&pool, "col_retired", "factual_qa", &[0.99; 5], Some(0.0001)).await;
    cells(&pool, "col_active", "factual_qa", &[0.9; 5], Some(0.01)).await;
    let (_, body) = policies(&pool, "?capability=factual_qa").await;
    for band in bands(&parse(&body)["policies"][0]) {
        assert_eq!(band.0, "col_active");
    }
    db.close().await;
}

/// Sets a column's stored retention evidence.
async fn retention(pool: &PgPool, column: &str, mode: &str, valid_for_days: i64) {
    sqlx::query(
        "UPDATE model_columns SET retention_mode = $2, retention_verified_at = now(),
                retention_valid_until = now() + make_interval(days => $3) WHERE id = $1",
    )
    .bind(column)
    .bind(mode)
    .bind(i32::try_from(valid_for_days).unwrap())
    .execute(pool)
    .await
    .unwrap();
}

/// DIVERGENCE: D42. For a zero-retention tenant, only columns with valid zero-retention
/// evidence are candidates, as `/v1/recommend` routes: a cheaper standard column, an
/// expired zero column, and a column with no evidence never name a band. Another tenant
/// with the same pool still sees every column (Go's behavior).
#[tokio::test]
async fn a_zero_retention_tenant_sees_only_zero_retention_columns() {
    let d42 = parity_support::divergence!("D42");
    assert_eq!(d42.id(), "D42");
    let Some((db, pool)) = database().await else {
        return;
    };
    for id in ["col_zero", "col_standard", "col_expired", "col_unknown"] {
        column(&pool, id, "direct", "active", None).await;
    }
    retention(&pool, "col_zero", "zero", 30).await;
    retention(&pool, "col_standard", "standard", 30).await;
    retention(&pool, "col_expired", "zero", -1).await;
    // The non-compliant columns are cheaper and clear more, so without the filter they win.
    cells(&pool, "col_zero", "factual_qa", &[0.9; 5], Some(0.05)).await;
    for id in ["col_standard", "col_expired", "col_unknown"] {
        cells(&pool, id, "factual_qa", &[0.99; 5], Some(0.001)).await;
    }
    let ids = ["col_standard", "col_expired", "col_unknown", "col_zero"];
    pool_of(&pool, TENANT, "default", &ids, true).await;
    pool_of(&pool, OTHER, "default", &ids, true).await;
    tenancy::upsert_tenant_constraints(&pool, TENANT, None, None, true, false, false)
        .await
        .unwrap();

    let (status, body) = policies(&pool, "?capability=factual_qa").await;
    assert_eq!(status, StatusCode::OK);
    for band in bands(&parse(&body)["policies"][0]) {
        assert_eq!(band.0, "col_zero", "{body}");
    }
    // Go's behavior for a tenant without the constraint.
    let (_, other) = get(
        router(Some(&pool), OTHER, None),
        "/v1/policies?capability=factual_qa",
    )
    .await;
    for band in bands(&parse(&other)["policies"][0]) {
        assert_ne!(band.0, "col_zero", "{other}");
    }

    // With no compliant column left, every band names none and clears nothing.
    retention(&pool, "col_zero", "zero", -1).await;
    let (status, body) = policies(&pool, "?capability=factual_qa").await;
    assert_eq!(status, StatusCode::OK);
    for band in parse(&body)["policies"][0]["bands"].as_array().unwrap() {
        assert_eq!(band["column"], Value::Null, "{body}");
        assert_eq!(band["clears_band"], false, "{body}");
    }
    db.close().await;
}

// parity: api.TestPoliciesNamedPoolSelection
#[tokio::test]
async fn a_named_pool_is_used() {
    let Some((db, pool)) = database().await else {
        return;
    };
    column(&pool, "col_direct", "direct", "active", None).await;
    column(&pool, "col_vertex", "vertex", "active", None).await;
    pool_of(&pool, TENANT, "default", &["col_direct"], true).await;
    pool_of(&pool, TENANT, "vertex-only", &["col_vertex"], false).await;
    let (status, body) = policies(&pool, "?capability=factual_qa&pool=vertex-only").await;
    assert_eq!(status, StatusCode::OK);
    let body = parse(&body);
    assert_eq!(body["pool"], "vertex-only");
    for band in bands(&body["policies"][0]) {
        assert_eq!(band.0, "col_vertex");
    }
    db.close().await;
}

// parity: api.TestPoliciesUnknownCapability
// parity: api.TestPoliciesUnknownQueryParameter
// parity: api.TestPoliciesUnknownPool
#[tokio::test]
async fn request_errors_are_exact() {
    let Some((db, pool)) = database().await else {
        return;
    };
    column(&pool, "col_one", "direct", "active", None).await;
    pool_of(&pool, TENANT, "default", &["col_one"], true).await;
    for (query, want) in [
        (
            "?capability=poetry",
            "{\"error\":{\"code\":\"invalid_request\",\"message\":\"capability \\\"poetry\\\" is not a v1 capability\",\"details\":{\"capability\":\"poetry\"}}}\n",
        ),
        (
            "?mode=cost",
            "{\"error\":{\"code\":\"invalid_request\",\"message\":\"unknown query parameter \\\"mode\\\"\",\"details\":{\"parameter\":\"mode\"}}}\n",
        ),
        (
            "?pool=nope",
            "{\"error\":{\"code\":\"unknown_pool\",\"message\":\"pool \\\"nope\\\" is not configured for this tenant\",\"details\":{\"pool\":\"nope\"}}}\n",
        ),
    ] {
        let (status, body) = policies(&pool, query).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{query}");
        assert_eq!(body, want, "{query}");
    }
    // Repeated names keep the first value; an empty value is not supplied.
    let (status, body) = policies(&pool, "?capability=factual_qa&capability=poetry").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(parse(&body)["policies"].as_array().unwrap().len(), 1);
    let (status, body) = policies(&pool, "?capability=&pool=").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(parse(&body)["policies"].as_array().unwrap().len(), 10);
    // A wrong method is the 404 envelope.
    let request = Request::builder()
        .method(Method::POST)
        .uri("/v1/policies")
        .header(header::AUTHORIZATION, "Bearer mk_live_test")
        .body(Body::empty())
        .unwrap();
    let response = router(Some(&pool), TENANT, None)
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    db.close().await;
}

// parity: api.TestPoliciesNoDefaultPool
#[tokio::test]
async fn a_tenant_without_a_default_pool_is_refused() {
    let Some((db, pool)) = database().await else {
        return;
    };
    let (status, body) = policies(&pool, "").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        body,
        "{\"error\":{\"code\":\"unknown_pool\",\"message\":\"tenant has no default pool\",\"details\":{\"pool\":\"\"}}}\n"
    );
    db.close().await;
}

// parity: api.TestPoliciesCrossTenantIsolation
#[tokio::test]
async fn another_tenants_pool_is_unknown() {
    let Some((db, pool)) = database().await else {
        return;
    };
    column(&pool, "col_a", "direct", "active", None).await;
    column(&pool, "col_b", "direct", "active", None).await;
    pool_of(&pool, TENANT, "team-a", &["col_a"], true).await;
    pool_of(&pool, OTHER, "team-b", &["col_b"], true).await;
    let as_b = |query: &'static str| get(router(Some(&pool), OTHER, None), query);
    let (status, body) = as_b("/v1/policies?pool=team-a").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        body,
        "{\"error\":{\"code\":\"unknown_pool\",\"message\":\"pool \\\"team-a\\\" is not configured for this tenant\",\"details\":{\"pool\":\"team-a\"}}}\n"
    );
    let (_, body) = as_b("/v1/policies?capability=factual_qa").await;
    let body = parse(&body);
    assert_eq!(body["pool"], "team-b");
    for band in bands(&body["policies"][0]) {
        assert_eq!(band.0, "col_b");
    }
    db.close().await;
}
