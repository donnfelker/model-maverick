//! `GET /v1/models` on real Postgres (bead 5.5; reference Part 5 §6.7 and §13.6; spec §9.4
//! and §4.5; openrouter spec §2.3). The Go keys come first and the D1/D15 retention fields
//! follow them.

use std::sync::{Arc, LazyLock};

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use chrono::{DateTime, Duration, NaiveDate, Utc};
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

const TENANT: &str = "ten_models";
const CORPUS: &str = "2026-07-01";

fn router(pool: Option<&PgPool>, backend: Option<StaticAuthBackend>) -> Router {
    let backend = backend.unwrap_or_else(|| {
        StaticAuthBackend::authenticated(Identity {
            tenant_id: TENANT.to_owned(),
            api_key_id: "key_models".to_owned(),
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

async fn call(router: Router, method: Method, path: &str) -> (StatusCode, String) {
    let request = Request::builder()
        .method(method)
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
    eprintln!("{path} -> {status} {text}");
    (status, text)
}

async fn models(pool: &PgPool) -> (StatusCode, String) {
    call(router(Some(pool), None), Method::GET, "/v1/models").await
}

fn parse(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("not JSON ({e}): {text}"))
}

async fn database() -> Option<(TestDatabase, PgPool)> {
    let db = maverick_testdb::database(&SCHEMA).await?;
    let pool = db.pool().await;
    tenancy::create_tenant(&pool, TENANT, "Acme", &[1; 32])
        .await
        .unwrap();
    corpus::upsert_corpus_version(&pool, CORPUS, "", 0, false)
        .await
        .unwrap();
    corpus::activate_corpus_version(&pool, CORPUS)
        .await
        .unwrap();
    Some((db, pool))
}

#[derive(Default)]
struct Col<'a> {
    id: &'a str,
    model: &'a str,
    endpoint: &'a str,
    status: &'a str,
    settings: &'a str,
    pricing: &'a str,
    quality_source: Option<&'a str>,
    deprecation: Option<NaiveDate>,
    aliases: Option<&'a str>,
    alias_retention: Option<&'a str>,
    retention: Option<(&'a str, DateTime<Utc>, DateTime<Utc>, &'a str)>,
}

async fn column(pool: &PgPool, col: Col<'_>) {
    let raw = |text: &str| JsonRawValue::from_string(text.to_owned()).unwrap();
    let settings = raw(if col.settings.is_empty() {
        "{}"
    } else {
        col.settings
    });
    let pricing = raw(if col.pricing.is_empty() {
        "{}"
    } else {
        col.pricing
    });
    let aliases = col.aliases.map(raw);
    let alias_retention = col.alias_retention.map(raw);
    catalog::create_model_column(
        pool,
        &NewModelColumn {
            id: col.id,
            provider: "anthropic",
            model_id: col.model,
            endpoint: col.endpoint,
            settings: &settings,
            status: col.status,
            pricing: &pricing,
            quality_source_column_id: col.quality_source,
            cold_start_until: None,
            deprecation_date: col.deprecation,
            execution_aliases: aliases.as_deref(),
            retention_mode: col.retention.map(|r| r.0),
            retention_verified_at: col.retention.map(|r| r.1),
            retention_valid_until: col.retention.map(|r| r.2),
            retention_source: col.retention.map(|r| r.3),
            execution_alias_retention: alias_retention.as_deref(),
            param_constraints: None,
        },
    )
    .await
    .unwrap();
}

async fn aggregate(pool: &PgPool, column: &str, measured: bool) {
    measurement::upsert_aggregate(
        pool,
        &AggregateUpsert {
            model_column_id: column,
            capability: "structured_extraction",
            difficulty_tier: 3,
            dimension: "quality",
            ewma: 0.9,
            variance: 0.0,
            n: 20,
            measured,
            matrix_version: CORPUS,
            as_of: Utc::now().date_naive(),
        },
    )
    .await
    .unwrap();
}

fn entries(body: &Value) -> Vec<&Value> {
    body["models"].as_array().unwrap().iter().collect()
}

// parity: api.TestModelsRequiresAuth
// parity: api.TestModelsWrongMethod
#[tokio::test]
async fn models_require_auth_and_get() {
    let (status, body) = call(
        router(None, Some(StaticAuthBackend::missing())),
        Method::GET,
        "/v1/models",
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body.contains("\"unauthorized\""));
    let (status, body) = call(router(None, None), Method::POST, "/v1/models").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        body,
        "{\"error\":{\"code\":\"not_found\",\"message\":\"resource not found\",\"details\":{}}}\n"
    );
}

// parity: api.TestModelsCatalogFieldExact
#[tokio::test]
async fn the_catalog_fields_are_exact() {
    let Some((db, pool)) = database().await else {
        return;
    };
    let pricing = r#"{"input_per_mtok": 3.0, "output_per_mtok": 15.0, "currency": "USD", "as_of": "2026-07-01", "source": "list"}"#;
    column(
        &pool,
        Col {
            id: "col_1_active",
            model: "claude-models-contract",
            endpoint: "direct",
            status: "active",
            settings: r#"{"thinking": true}"#,
            pricing,
            ..Col::default()
        },
    )
    .await;
    column(
        &pool,
        Col {
            id: "col_2_deprecated",
            model: "claude-models-contract",
            endpoint: "vertex",
            status: "deprecated",
            quality_source: Some("col_1_active"),
            deprecation: NaiveDate::from_ymd_opt(2026, 12, 31),
            ..Col::default()
        },
    )
    .await;
    column(
        &pool,
        Col {
            id: "col_3_cold",
            model: "claude-cold",
            endpoint: "direct",
            status: "cold_start",
            ..Col::default()
        },
    )
    .await;
    column(
        &pool,
        Col {
            id: "col_4_retired",
            model: "claude-old",
            endpoint: "direct",
            status: "retired",
            ..Col::default()
        },
    )
    .await;
    aggregate(&pool, "col_1_active", true).await;
    aggregate(&pool, "col_3_cold", false).await;

    let (status, text) = models(&pool).await;
    assert_eq!(status, StatusCode::OK);
    let body = parse(&text);
    assert_eq!(body["matrix_version"], CORPUS);
    let entries = entries(&body);
    let ids: Vec<&str> = entries
        .iter()
        .map(|e| e["column_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["col_1_active", "col_2_deprecated", "col_3_cold"]);
    let (active, deprecated, cold) = (entries[0], entries[1], entries[2]);
    assert_eq!(active["settings"], serde_json::json!({"thinking": true}));
    assert!(
        text.contains(r#""input_per_mtok":3.0"#) && text.contains(r#""output_per_mtok":15.0"#),
        "stored number spelling: {text}"
    );
    assert_eq!(active["pricing"].as_object().unwrap().len(), 5);
    assert_eq!(active["deprecation_date"], Value::Null);
    assert_eq!(active["quality_source_column_id"], Value::Null);
    assert_eq!(active["measured"], true);
    assert_eq!(active["execution_aliases"], serde_json::json!({}));
    assert_eq!(active["status"], "active");
    assert_eq!(deprecated["status"], "deprecated");
    assert_eq!(deprecated["endpoint"], "vertex");
    assert_eq!(deprecated["deprecation_date"], "2026-12-31");
    assert_eq!(deprecated["quality_source_column_id"], "col_1_active");
    assert_eq!(deprecated["measured"], false);
    assert_eq!(cold["status"], "cold_start");
    assert_eq!(cold["measured"], false);
    // Query parameters are ignored (QUIRK Q13).
    let (status, again) = call(router(Some(&pool), None), Method::GET, "/v1/models?x=1").await;
    assert_eq!((status, again), (StatusCode::OK, text));
    db.close().await;
}

/// DIVERGENCE: D1, D15. Go's eleven keys come first, in order; the retention fields follow.
/// No score of any kind.
// parity: api.TestModelsEntryKeySetExact
#[tokio::test]
async fn the_entry_keys_are_exact() {
    let d1 = parity_support::divergence!("D1");
    let d15 = parity_support::divergence!("D15");
    assert_eq!((d1.id(), d15.id()), ("D1", "D15"));
    let Some((db, pool)) = database().await else {
        return;
    };
    column(
        &pool,
        Col {
            id: "col_keys",
            model: "claude-keys",
            endpoint: "direct",
            status: "active",
            ..Col::default()
        },
    )
    .await;
    let (_, text) = models(&pool).await;
    assert!(
        text.starts_with(
            r#"{"models":[{"column_id":"col_keys","provider":"anthropic","model_id":"claude-keys","endpoint":"direct","settings":{},"execution_aliases":{},"status":"active","pricing":{},"deprecation_date":null,"measured":false,"quality_source_column_id":null,"retention_mode":"unknown","retention_verified_at":null,"retention_valid_until":null,"retention_source":null,"execution_alias_retention":{}}],"matrix_version":"#
        ),
        "{text}"
    );
    for forbidden in ["score", "quality\"", "pass_rate", "ewma"] {
        assert!(!text.contains(forbidden), "{forbidden}: {text}");
    }
    db.close().await;
}

// parity: api.TestModelsCarriesExecutionAliases
#[tokio::test]
async fn execution_aliases_are_returned_verbatim() {
    let Some((db, pool)) = database().await else {
        return;
    };
    column(
        &pool,
        Col {
            id: "col_or",
            model: "claude-sonnet-4-6",
            endpoint: "openrouter",
            status: "active",
            aliases: Some(r#"{"openrouter": "anthropic/claude-sonnet-4-6"}"#),
            ..Col::default()
        },
    )
    .await;
    let (_, text) = models(&pool).await;
    let body = parse(&text);
    assert_eq!(
        entries(&body)[0]["execution_aliases"],
        serde_json::json!({"openrouter": "anthropic/claude-sonnet-4-6"})
    );
    db.close().await;
}

/// DIVERGENCE: D1, D15. `retention_mode` is effective after the expiry rule, and so is each
/// alias's mode in `execution_alias_retention`; a stored `zero` past `retention_valid_until`,
/// or an alias with no evidence, reads as `unknown`. The column's evidence fields are shown
/// as stored.
#[tokio::test]
async fn retention_fields_are_effective_after_expiry() {
    let _d1 = parity_support::divergence!("D1");
    let _d15 = parity_support::divergence!("D15");
    let Some((db, pool)) = database().await else {
        return;
    };
    let verified = DateTime::parse_from_rfc3339("2026-07-01T10:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let future = Utc::now() + Duration::days(30);
    let past = Utc::now() - Duration::days(1);
    column(&pool, Col {
        id: "col_a_zero",
        model: "claude-zero",
        endpoint: "direct",
        status: "active",
        retention: Some(("zero", verified, future, "https://example.test/zdr")),
        aliases: Some(r#"{"openrouter": "anthropic/claude-zero", "bedrock": "x"}"#),
        alias_retention: Some(&format!(
            r#"{{"openrouter": {{"retention_mode": "zero", "retention_verified_at": "2026-07-01T10:00:00Z", "retention_valid_until": "{}", "retention_source": "https://openrouter.test/zdr"}}}}"#,
            future.to_rfc3339()
        )),
        ..Col::default()
    })
    .await;
    column(&pool, Col {
        id: "col_b_expired",
        model: "claude-expired",
        endpoint: "direct",
        status: "active",
        retention: Some(("zero", verified, past, "https://example.test/old")),
        aliases: Some(r#"{"openrouter": "anthropic/claude-expired"}"#),
        alias_retention: Some(&format!(
            r#"{{"openrouter": {{"retention_mode": "zero", "retention_verified_at": "2026-07-01T10:00:00Z", "retention_valid_until": "{}"}}}}"#,
            past.to_rfc3339()
        )),
        ..Col::default()
    })
    .await;
    let (_, text) = models(&pool).await;
    let body = parse(&text);
    let entries = entries(&body);
    let zero = entries[0];
    assert_eq!(zero["retention_mode"], "zero");
    assert_eq!(zero["retention_verified_at"], "2026-07-01T10:00:00Z");
    assert_eq!(zero["retention_source"], "https://example.test/zdr");
    assert!(zero["retention_valid_until"].as_str().is_some());
    assert_eq!(
        zero["execution_alias_retention"],
        serde_json::json!({"bedrock": "unknown", "openrouter": "zero"}),
        "an alias with no evidence reads as unknown"
    );
    assert!(
        text.contains(r#""execution_alias_retention":{"bedrock":"unknown","openrouter":"zero"}"#),
        "the execution_aliases key order (jsonb: shorter keys first): {text}"
    );
    let expired = entries[1];
    assert_eq!(expired["retention_mode"], "unknown");
    assert_eq!(expired["retention_verified_at"], "2026-07-01T10:00:00Z");
    assert_eq!(
        expired["execution_alias_retention"],
        serde_json::json!({"openrouter": "unknown"})
    );
    db.close().await;
}
