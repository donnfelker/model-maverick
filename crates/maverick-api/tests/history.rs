//! `GET /v1/history/*` on real Postgres (bead 5.8; reference Part 5 §6.9 to §6.13 and
//! §13.8, Part 4 §6; spec §9.6). Every `history_test.go` case has a test here, and every
//! recorded error exchange of those tests (`parity/blackbox/api-recorded.jsonl`) is
//! replayed byte for byte.

use std::sync::{Arc, LazyLock};

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use chrono::{FixedOffset, NaiveDate, Utc};
use maverick_api::{
    ApiState, AuthState, HealthState, Identity, RateLimiter, StaticAuthBackend, public_router_with,
};
use maverick_domain::history::{KIND_OUTCOME, KIND_RECOMMENDATION, encode_cursor};
use maverick_domain::keys;
use maverick_store::catalog::{self, NewModelColumn};
use maverick_store::decisions::{self, NewRecommendation};
use maverick_store::measurement::{self, AggregateUpsert};
use maverick_store::{JsonRawValue, MIGRATOR, alerts, corpus, tenancy};
use maverick_testdb::{Schema, TestDatabase};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

const TENANT_A: &str = "ten_hist_a";
const TENANT_B: &str = "ten_hist_b";
const CORPUS: &str = "2026-07-01";
const COLUMN: &str = "col_01SERIES";

fn router(pool: Option<&PgPool>, backend: StaticAuthBackend) -> Router {
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

fn as_tenant(tenant: &str) -> StaticAuthBackend {
    StaticAuthBackend::authenticated(Identity {
        tenant_id: tenant.to_owned(),
        api_key_id: format!("key_{tenant}"),
    })
}

async fn send(router: Router, method: Method, path: &str, body: &str) -> (StatusCode, String) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, "Bearer mk_live_test")
        .body(Body::from(body.to_owned()))
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

async fn get_as(pool: &PgPool, tenant: &str, path: &str) -> (StatusCode, String) {
    send(router(Some(pool), as_tenant(tenant)), Method::GET, path, "").await
}

async fn get(pool: &PgPool, path: &str) -> (StatusCode, String) {
    get_as(pool, TENANT_A, path).await
}

/// A 200 body, parsed.
async fn ok(pool: &PgPool, path: &str) -> Value {
    let (status, text) = get(pool, path).await;
    assert_eq!(status, StatusCode::OK, "{path}: {text}");
    parse(&text)
}

fn parse(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("not JSON ({e}): {text}"))
}

fn raw(text: &str) -> Box<JsonRawValue> {
    JsonRawValue::from_string(text.to_owned()).unwrap()
}

async fn database() -> Option<(TestDatabase, PgPool)> {
    let db = maverick_testdb::database(&SCHEMA).await?;
    let pool = db.pool().await;
    for tenant in [TENANT_A, TENANT_B] {
        tenancy::create_tenant(&pool, tenant, "Acme", &[1; 32])
            .await
            .unwrap();
        tenancy::create_api_key(
            &pool,
            &format!("key_{tenant}"),
            tenant,
            tenant.as_bytes(),
            "abcd",
            "",
        )
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

/// A seeded recommendation, shaped as the Go test's `seedRecommendation` defaults.
struct Seed<'a> {
    capability: &'a str,
    difficulty: u8,
    pool: &'a str,
    mode: &'a str,
    pick: Option<(&'a str, &'a str, &'a str, &'a str, Option<f64>)>,
    clears: bool,
    constraints_met: bool,
    measured: bool,
    prompt_hash: Option<&'a str>,
}

impl Default for Seed<'_> {
    fn default() -> Self {
        Self {
            capability: "structured_extraction",
            difficulty: 3,
            pool: "default",
            mode: "balanced",
            pick: Some((
                "anthropic",
                "claude-sonnet-4-6",
                "direct",
                "col_01SEED",
                Some(94.1),
            )),
            clears: true,
            constraints_met: true,
            measured: true,
            prompt_hash: None,
        }
    }
}

/// Seeds one recommendation for `tenant` and returns its id (monotonic, so id order is
/// seed order).
async fn seed(pool: &PgPool, tenant: &str, seed: &Seed<'_>) -> String {
    let id = maverick_domain::ids::new(maverick_domain::ids::prefix::RECOMMENDATION);
    let classification = json!({
        "capability": seed.capability, "confidence": 1, "difficulty": seed.difficulty,
        "domain_tags": [], "high_stakes": false, "method": "declared",
    });
    let recommendations = match seed.pick {
        Some((provider, model, endpoint, column_id, score)) => json!([{
            "clears_difficulty": seed.clears, "column_id": column_id, "endpoint": endpoint,
            "model": model, "provider": provider, "reason": "seeded", "score": score,
        }]),
        None => json!([]),
    };
    let response = json!({
        "check_spec": {"ref": "checks/extraction/v1", "type": "json_schema"},
        "classification": classification,
        "constraints_met": seed.constraints_met,
        "matrix_version": CORPUS,
        "measured": seed.measured,
        "mode": seed.mode,
        "pool": seed.pool,
        "recommendation_id": id,
        "recommendations": recommendations,
        "violations": [],
    });
    let candidates = json!([{"column_id": "col_01SEED", "provider": "anthropic",
                             "model": "claude-sonnet-4-6", "endpoint": "direct",
                             "status": "survivor", "rank": 0}]);
    let request = json!({"workload": {"capability": seed.capability}});
    decisions::create_recommendation(
        pool,
        &NewRecommendation {
            id: &id,
            tenant_id: tenant,
            api_key_id: &format!("key_{tenant}"),
            prompt_hash: seed.prompt_hash,
            request: &raw(&request.to_string()),
            classification: &raw(&classification.to_string()),
            candidates: &raw(&candidates.to_string()),
            response: &raw(&response.to_string()),
            matrix_version: CORPUS,
            affinity: None,
        },
    )
    .await
    .unwrap();
    id
}

async fn seed_default(pool: &PgPool, tenant: &str) -> String {
    seed(pool, tenant, &Seed::default()).await
}

fn ids(body: &Value, key: &str, id_key: &str) -> Vec<String> {
    body[key]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry[id_key].as_str().unwrap().to_owned())
        .collect()
}

fn keys_of(value: &Value) -> Vec<&str> {
    value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect()
}

/// The keys of the first object after `marker` in `text`, in wire order.
fn wire_keys(text: &str, marker: &str) -> Vec<String> {
    let start = text.find(marker).unwrap() + marker.len();
    let mut keys = Vec::new();
    let mut depth = 0;
    let mut chars = text[start..].char_indices().peekable();
    let mut expect_key = true;
    while let Some((_, c)) = chars.next() {
        match c {
            '{' | '[' => depth += 1,
            '}' | ']' => {
                if depth == 1 {
                    break;
                }
                depth -= 1;
            }
            ',' if depth == 1 => expect_key = true,
            '"' => {
                let mut literal = String::new();
                for (_, c) in chars.by_ref() {
                    if c == '"' {
                        break;
                    }
                    literal.push(c);
                }
                if depth == 1 && expect_key {
                    keys.push(literal);
                    expect_key = false;
                }
            }
            _ => {}
        }
    }
    keys
}

// ---- auth ----

// parity: api.TestHistoryRequiresAuth
#[tokio::test]
async fn every_history_path_requires_auth() {
    for path in [
        "/v1/history/recommendations",
        "/v1/history/outcomes",
        "/v1/history/calibration",
        "/v1/history/models/col_01X",
        "/v1/history/recommendations/rec_01X",
    ] {
        let (status, body) = send(
            router(None, StaticAuthBackend::missing()),
            Method::GET,
            path,
            "",
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}");
        assert_eq!(
            body,
            "{\"error\":{\"code\":\"unauthorized\",\"message\":\"missing or invalid API key\",\"details\":{}}}\n"
        );
    }
}

// ---- recommendations ----

// parity: api.TestHistoryRecommendationsSummaries
#[tokio::test]
async fn recommendation_summaries_are_exact() {
    let Some((db, pool)) = database().await else {
        return;
    };
    let older = seed_default(&pool, TENANT_A).await;
    let newer = seed(
        &pool,
        TENANT_A,
        &Seed {
            capability: "code_generation",
            difficulty: 4,
            pool: "vertex-only",
            mode: "quality",
            pick: Some(("openai", "gpt-5.5", "direct", "col_01PICK", Some(96.3))),
            clears: false,
            constraints_met: false,
            measured: false,
            ..Seed::default()
        },
    )
    .await;
    let (status, text) = get(&pool, "/v1/history/recommendations").await;
    assert_eq!(status, StatusCode::OK);
    let body = parse(&text);
    assert_eq!(
        keys_of(&body),
        ["matrix_version", "next_cursor", "recommendations"]
    );
    assert_eq!(body["matrix_version"], CORPUS);
    assert_eq!(body["next_cursor"], Value::Null);
    assert_eq!(
        ids(&body, "recommendations", "id"),
        [newer.as_str(), older.as_str()]
    );
    assert_eq!(
        wire_keys(&text, "\"recommendations\":["),
        [
            "id",
            "created_at",
            "capability",
            "difficulty",
            "pick",
            "mode",
            "pool",
            "flags"
        ]
    );
    assert!(text.ends_with("],\"next_cursor\":null,\"matrix_version\":\"2026-07-01\"}\n"));
    let first = &body["recommendations"][0];
    assert!(first["created_at"].as_str().unwrap().ends_with('Z'));
    let mut summary = first.clone();
    summary.as_object_mut().unwrap().remove("created_at");
    assert_eq!(
        summary,
        json!({"id": newer, "capability": "code_generation", "difficulty": 4,
               "pick": {"provider": "openai", "model": "gpt-5.5", "endpoint": "direct",
                        "column_id": "col_01PICK", "score": 96.3},
               "mode": "quality", "pool": "vertex-only",
               "flags": {"constraints_met": false, "measured": false,
                         "clears_difficulty": false}})
    );
    assert!(text.contains(&format!(
        "\"id\":\"{newer}\",\"created_at\":\"{}\",\"capability\":\"code_generation\",\"difficulty\":4,\"pick\":{{\"provider\":\"openai\",\"model\":\"gpt-5.5\",\"endpoint\":\"direct\",\"column_id\":\"col_01PICK\",\"score\":96.3}},\"mode\":\"quality\",\"pool\":\"vertex-only\",\"flags\":{{\"constraints_met\":false,\"measured\":false,\"clears_difficulty\":false}}}}",
        first["created_at"].as_str().unwrap()
    )));
    let second = &body["recommendations"][1];
    assert_eq!(
        second["pick"],
        json!({"provider": "anthropic", "model": "claude-sonnet-4-6", "endpoint": "direct",
               "column_id": "col_01SEED", "score": 94.1})
    );
    assert_eq!(
        second["flags"],
        json!({"constraints_met": true, "measured": true, "clears_difficulty": true})
    );
    db.close().await;
}

/// A recommendation without a pick has `pick: null` and `clears_difficulty: false`; an
/// entry without a score has `score: null`.
#[tokio::test]
async fn summaries_without_a_pick_or_score() {
    let Some((db, pool)) = database().await else {
        return;
    };
    seed(
        &pool,
        TENANT_A,
        &Seed {
            pick: None,
            ..Seed::default()
        },
    )
    .await;
    seed(
        &pool,
        TENANT_A,
        &Seed {
            pick: Some(("anthropic", "m", "direct", "col_x", None)),
            ..Seed::default()
        },
    )
    .await;
    let body = ok(&pool, "/v1/history/recommendations").await;
    assert_eq!(body["recommendations"][0]["pick"]["score"], Value::Null);
    assert_eq!(body["recommendations"][1]["pick"], Value::Null);
    assert_eq!(
        body["recommendations"][1]["flags"]["clears_difficulty"],
        false
    );
    db.close().await;
}

// parity: api.TestHistoryRecommendationsFilters
#[tokio::test]
async fn recommendation_filters() {
    let Some((db, pool)) = database().await else {
        return;
    };
    let hashed = seed(
        &pool,
        TENANT_A,
        &Seed {
            prompt_hash: Some("hmac_sha256:9f2c"),
            ..Seed::default()
        },
    )
    .await;
    let code = seed(
        &pool,
        TENANT_A,
        &Seed {
            capability: "code_generation",
            ..Seed::default()
        },
    )
    .await;
    let plain = seed_default(&pool, TENANT_A).await;

    let body = ok(
        &pool,
        "/v1/history/recommendations?prompt_hash=hmac_sha256%3A9f2c",
    )
    .await;
    assert_eq!(ids(&body, "recommendations", "id"), [hashed.as_str()]);
    let body = ok(
        &pool,
        "/v1/history/recommendations?capability=code_generation",
    )
    .await;
    assert_eq!(ids(&body, "recommendations", "id"), [code.as_str()]);

    // `since` is inclusive, and an offset timestamp names the same instant.
    let boundary = decisions::get_recommendation_for_tenant(&pool, &plain, TENANT_A)
        .await
        .unwrap()
        .created_at;
    let offset = boundary.with_timezone(&FixedOffset::west_opt(4 * 3600).unwrap());
    let since = offset
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, false)
        .replace(':', "%3A")
        .replace('+', "%2B");
    let body = ok(&pool, &format!("/v1/history/recommendations?since={since}")).await;
    assert_eq!(ids(&body, "recommendations", "id"), [plain.as_str()]);
    // A date is UTC midnight: every row of today.
    let today = Utc::now().date_naive();
    let body = ok(&pool, &format!("/v1/history/recommendations?since={today}")).await;
    assert_eq!(ids(&body, "recommendations", "id").len(), 3);

    assert_eq!(
        get(&pool, "/v1/history/recommendations?capability=nope").await,
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            "{\"error\":{\"code\":\"invalid_request\",\"message\":\"capability \\\"nope\\\" is not a v1 capability\",\"details\":{\"capability\":\"nope\"}}}\n".to_owned()
        )
    );
    db.close().await;
}

// parity: api.TestHistoryRecommendationsPaginationWalk
#[tokio::test]
async fn recommendation_pages_walk_newest_first() {
    let Some((db, pool)) = database().await else {
        return;
    };
    let mut seeded = Vec::new();
    for _ in 0..5 {
        seeded.push(seed_default(&pool, TENANT_A).await);
    }
    seeded.reverse();
    let mut seen = Vec::new();
    let mut sizes = Vec::new();
    let mut path = "/v1/history/recommendations?limit=2".to_owned();
    loop {
        let body = ok(&pool, &path).await;
        let page = ids(&body, "recommendations", "id");
        sizes.push(page.len());
        seen.extend(page.clone());
        match body["next_cursor"].as_str() {
            Some(cursor) => {
                assert_eq!(
                    cursor,
                    encode_cursor(KIND_RECOMMENDATION, &[page.last().unwrap()])
                );
                path = format!("/v1/history/recommendations?limit=2&cursor={cursor}");
            }
            None => break,
        }
    }
    assert_eq!(sizes, [2, 2, 1]);
    assert_eq!(seen, seeded, "newest first, no repeats");
    db.close().await;
}

// parity: api.TestHistoryRecommendationsPaginationExactBoundary
#[tokio::test]
async fn an_exact_multiple_ends_with_a_null_cursor() {
    let Some((db, pool)) = database().await else {
        return;
    };
    for _ in 0..4 {
        seed_default(&pool, TENANT_A).await;
    }
    let first = ok(&pool, "/v1/history/recommendations?limit=2").await;
    let cursor = first["next_cursor"].as_str().expect("page 1 has a cursor");
    let second = ok(
        &pool,
        &format!("/v1/history/recommendations?limit=2&cursor={cursor}"),
    )
    .await;
    assert_eq!(ids(&second, "recommendations", "id").len(), 2);
    assert_eq!(second["next_cursor"], Value::Null);
    db.close().await;
}

// parity: api.TestHistoryRecommendationsEmptyPage
// parity: api.TestHistoryLimitBoundsAccepted
#[tokio::test]
async fn an_empty_page_and_the_limit_bounds() {
    let Some((db, pool)) = database().await else {
        return;
    };
    let empty = "{\"recommendations\":[],\"next_cursor\":null,\"matrix_version\":\"2026-07-01\"}\n";
    for path in [
        "/v1/history/recommendations",
        "/v1/history/recommendations?limit=1",
        "/v1/history/recommendations?limit=200",
    ] {
        assert_eq!(get(&pool, path).await, (StatusCode::OK, empty.to_owned()));
    }
    // No active corpus: `matrix_version` is empty.
    sqlx::query("UPDATE corpus_versions SET active = false")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        get(&pool, "/v1/history/recommendations").await.1,
        "{\"recommendations\":[],\"next_cursor\":null,\"matrix_version\":\"\"}\n"
    );
    db.close().await;
}

// parity: api.TestHistoryRecommendationsCursorStableUnderInserts
#[tokio::test]
async fn a_cursor_is_stable_under_inserts() {
    let Some((db, pool)) = database().await else {
        return;
    };
    let mut original = Vec::new();
    for _ in 0..4 {
        original.push(seed_default(&pool, TENANT_A).await);
    }
    let first = ok(&pool, "/v1/history/recommendations?limit=2").await;
    let cursor = first["next_cursor"].as_str().unwrap().to_owned();
    seed_default(&pool, TENANT_A).await;
    let second = ok(
        &pool,
        &format!("/v1/history/recommendations?limit=2&cursor={cursor}"),
    )
    .await;
    assert_eq!(
        ids(&second, "recommendations", "id"),
        [original[1].as_str(), original[0].as_str()]
    );
    db.close().await;
}

// parity: api.TestHistoryRecommendationsCrossTenantIsolation
#[tokio::test]
async fn another_tenant_sees_none_of_the_log() {
    let Some((db, pool)) = database().await else {
        return;
    };
    seed_default(&pool, TENANT_A).await;
    let (_, text) = get_as(&pool, TENANT_B, "/v1/history/recommendations").await;
    assert_eq!(parse(&text)["recommendations"], json!([]));
    db.close().await;
}

// ---- parameters ----

// parity: api.TestHistoryInvalidCursor422
// parity: api.TestHistoryForeignCursor422
#[tokio::test]
async fn bad_and_foreign_cursors_are_422() {
    let Some((db, pool)) = database().await else {
        return;
    };
    let rec = seed_default(&pool, TENANT_A).await;
    let bad = "{\"error\":{\"code\":\"invalid_request\",\"message\":\"cursor is not a valid pagination token\",\"details\":{\"parameter\":\"cursor\"}}}\n";
    let recommendations_cursor = encode_cursor(KIND_RECOMMENDATION, &[&rec]);
    let outcomes_cursor = encode_cursor(KIND_OUTCOME, &["out_1"]);
    for path in [
        "/v1/history/recommendations?cursor=%21%21garbage".to_owned(),
        "/v1/history/outcomes?cursor=%21%21garbage".to_owned(),
        "/v1/history/calibration?cursor=%21%21garbage".to_owned(),
        format!("/v1/history/models/{COLUMN}?cursor=%21%21garbage"),
        format!("/v1/history/outcomes?cursor={recommendations_cursor}"),
        format!("/v1/history/recommendations?cursor={outcomes_cursor}"),
        format!("/v1/history/calibration?cursor={recommendations_cursor}"),
        format!("/v1/history/models/{COLUMN}?cursor={recommendations_cursor}"),
        // Padded, and a field left empty.
        format!("/v1/history/recommendations?cursor={recommendations_cursor}%3D"),
        format!(
            "/v1/history/recommendations?cursor={}",
            encode_cursor(KIND_RECOMMENDATION, &[""])
        ),
    ] {
        assert_eq!(
            get(&pool, &path).await,
            (StatusCode::UNPROCESSABLE_ENTITY, bad.to_owned()),
            "{path}"
        );
    }
    db.close().await;
}

// parity: api.TestHistoryInvalidLimit422
// parity: api.TestHistoryUnknownParam422
// parity: api.TestHistoryInvalidSince422
#[tokio::test]
async fn bad_parameters_are_422_in_go_order() {
    let Some((db, pool)) = database().await else {
        return;
    };
    for raw in ["0", "-1", "201", "abc", "2.5"] {
        assert_eq!(
            get(&pool, &format!("/v1/history/recommendations?limit={raw}")).await,
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                format!(
                    "{{\"error\":{{\"code\":\"invalid_request\",\"message\":\"limit \\\"{raw}\\\" must be an integer between 1 and 200\",\"details\":{{\"parameter\":\"limit\",\"value\":\"{raw}\"}}}}}}\n"
                )
            ),
            "{raw}"
        );
    }
    let unknown = "{\"error\":{\"code\":\"invalid_request\",\"message\":\"unknown query parameter \\\"offset\\\"\",\"details\":{\"parameter\":\"offset\"}}}\n";
    assert_eq!(
        get(&pool, "/v1/history/recommendations?offset=10").await,
        (StatusCode::UNPROCESSABLE_ENTITY, unknown.to_owned())
    );
    let since = "{\"error\":{\"code\":\"invalid_request\",\"message\":\"since \\\"yesterday\\\" must be an RFC 3339 timestamp or YYYY-MM-DD date\",\"details\":{\"parameter\":\"since\",\"value\":\"yesterday\"}}}\n";
    assert_eq!(
        get(&pool, "/v1/history/recommendations?since=yesterday").await,
        (StatusCode::UNPROCESSABLE_ENTITY, since.to_owned())
    );
    // Order: an unknown name (wherever it is), then limit, then since, then the endpoint's
    // checks, then the cursor.
    assert_eq!(
        get(
            &pool,
            "/v1/history/recommendations?since=x&limit=0&capability=nope&offset=10"
        )
        .await
        .1,
        unknown
    );
    let (_, text) = get(
        &pool,
        "/v1/history/recommendations?capability=nope&since=x&limit=0",
    )
    .await;
    assert!(text.contains("limit \\\"0\\\""), "{text}");
    let (_, text) = get(&pool, "/v1/history/recommendations?capability=nope&since=x").await;
    assert!(text.contains("since \\\"x\\\""), "{text}");
    let (_, text) = get(
        &pool,
        "/v1/history/recommendations?cursor=%21%21&capability=nope",
    )
    .await;
    assert!(text.contains("capability \\\"nope\\\""), "{text}");
    // A repeated name keeps its first value; an empty value is not supplied.
    let (status, _) = get(&pool, "/v1/history/recommendations?limit=5&limit=abc").await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = get(
        &pool,
        "/v1/history/recommendations?limit=&since=&cursor=&capability=",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    db.close().await;
}

// ---- one decision trace ----

// parity: api.TestHistoryTraceFullDecision
// parity: api.TestHistoryTraceNullPromptHashOnDeclared
#[tokio::test]
async fn a_trace_returns_the_stored_decision() {
    let Some((db, pool)) = database().await else {
        return;
    };
    let hashed = seed(
        &pool,
        TENANT_A,
        &Seed {
            prompt_hash: Some("hmac_sha256:9f2c"),
            ..Seed::default()
        },
    )
    .await;
    let declared = seed_default(&pool, TENANT_A).await;
    let stored = decisions::get_recommendation_for_tenant(&pool, &hashed, TENANT_A)
        .await
        .unwrap();

    let (status, text) = get(&pool, &format!("/v1/history/recommendations/{hashed}")).await;
    assert_eq!(status, StatusCode::OK);
    let body = parse(&text);
    assert_eq!(
        wire_keys(&format!("x{text}"), "x"),
        [
            "id",
            "api_key_id",
            "prompt_hash",
            "request",
            "classification",
            "candidates",
            "response",
            "matrix_version",
            "created_at"
        ]
    );
    assert_eq!(body["id"], hashed.as_str());
    assert_eq!(body["api_key_id"], format!("key_{TENANT_A}"));
    assert_eq!(body["prompt_hash"], "hmac_sha256:9f2c");
    assert_eq!(body["matrix_version"], CORPUS);
    assert!(!body["created_at"].as_str().unwrap().is_empty());
    for (key, column) in [
        ("request", &stored.request),
        ("classification", &stored.classification),
        ("candidates", &stored.candidates),
        ("response", &stored.response),
    ] {
        let column: Value = serde_json::from_str(column.get()).unwrap();
        assert_eq!(body[key], column, "{key}");
    }
    // The jsonb as Postgres stores it: its key order, compacted.
    assert!(
        text.contains("\"request\":{\"workload\":{\"capability\":\"structured_extraction\"}},\"classification\":{\"method\":\"declared\",\"capability\":\"structured_extraction\",\"confidence\":1,\"difficulty\":3,\"domain_tags\":[],\"high_stakes\":false},"),
        "{text}"
    );

    let body = parse(
        &get(&pool, &format!("/v1/history/recommendations/{declared}"))
            .await
            .1,
    );
    assert_eq!(body["prompt_hash"], Value::Null);
    db.close().await;
}

// parity: api.TestHistoryTraceCrossTenant404Oracle
#[tokio::test]
async fn another_tenants_trace_is_the_missing_404() {
    let Some((db, pool)) = database().await else {
        return;
    };
    let theirs = seed_default(&pool, TENANT_A).await;
    let foreign = get_as(
        &pool,
        TENANT_B,
        &format!("/v1/history/recommendations/{theirs}"),
    )
    .await;
    let missing = get_as(
        &pool,
        TENANT_B,
        "/v1/history/recommendations/rec_01NEVEREXISTED",
    )
    .await;
    assert_eq!(foreign, missing);
    assert_eq!(
        foreign,
        (
            StatusCode::NOT_FOUND,
            "{\"error\":{\"code\":\"not_found\",\"message\":\"resource not found\",\"details\":{}}}\n"
                .to_owned()
        )
    );
    db.close().await;
}

// ---- outcomes ----

async fn telemetry(pool: &PgPool, tenant: &str, item: Value) {
    let (status, body) = send(
        router(Some(pool), as_tenant(tenant)),
        Method::POST,
        "/v1/telemetry",
        &json!({"items": [item]}).to_string(),
    )
    .await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::OK, "{\"accepted\":1,\"dropped\":0}\n")
    );
}

// parity: api.TestHistoryOutcomesListAndFilters
#[tokio::test]
async fn outcomes_list_newest_first_and_filter() {
    let Some((db, pool)) = database().await else {
        return;
    };
    let rec_a = seed_default(&pool, TENANT_A).await;
    let rec_b = seed_default(&pool, TENANT_A).await;
    telemetry(
        &pool,
        TENANT_A,
        json!({"recommendation_id": rec_a, "model_used": "claude-sonnet-4-6",
               "status": 200, "check_pass": false}),
    )
    .await;
    // D16: Go's unchecked 2xx was `success`; here the check passed.
    telemetry(
        &pool,
        TENANT_A,
        json!({"recommendation_id": rec_b, "model_used": "claude-opus-4-8",
               "status": 200, "check_pass": true}),
    )
    .await;

    let (status, text) = get(&pool, "/v1/history/outcomes").await;
    assert_eq!(status, StatusCode::OK);
    let body = parse(&text);
    assert_eq!(
        keys_of(&body),
        ["matrix_version", "next_cursor", "outcomes"]
    );
    let recs: Vec<String> = ids(&body, "outcomes", "recommendation_id");
    assert_eq!(recs, [rec_b.as_str(), rec_a.as_str()]);
    let a = &body["outcomes"][1];
    assert_eq!(
        wire_keys(&text, "\"outcomes\":["),
        [
            "id",
            "recommendation_id",
            "model_used",
            "outcome",
            "escalated_to",
            "details",
            "endpoint_used",
            "created_at"
        ]
    );
    assert!(a["id"].as_str().unwrap().starts_with("out_"));
    assert_eq!(a["model_used"], "claude-sonnet-4-6");
    assert_eq!(a["outcome"], "parse_failure");
    assert_eq!(a["escalated_to"], Value::Null);
    assert_eq!(a["endpoint_used"], "direct");
    // jsonb key order: shorter keys first.
    assert!(
        text.contains("\"details\":{\"status\":200,\"check_pass\":false}"),
        "{text}"
    );

    let only_a = ok(
        &pool,
        &format!("/v1/history/outcomes?recommendation_id={rec_a}"),
    )
    .await;
    assert_eq!(
        ids(&only_a, "outcomes", "recommendation_id"),
        [rec_a.as_str()]
    );
    let success = ok(&pool, "/v1/history/outcomes?outcome=success").await;
    assert_eq!(
        ids(&success, "outcomes", "recommendation_id"),
        [rec_b.as_str()]
    );
    assert_eq!(
        get(&pool, "/v1/history/outcomes?outcome=shrug").await,
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            "{\"error\":{\"code\":\"invalid_request\",\"message\":\"outcome \\\"shrug\\\" is not one of [success parse_failure quality_insufficient latency_exceeded provider_error completed_unchecked other]\",\"details\":{\"field\":\"outcome\",\"value\":\"shrug\"}}}\n".to_owned()
        )
    );
    db.close().await;
}

/// DIVERGENCE: D16 and D40. An unchecked 2xx is `completed_unchecked` (D16), listed like any
/// outcome and accepted by the `outcome` filter (D40).
#[tokio::test]
async fn completed_unchecked_outcomes_are_listed_and_filterable() {
    let d16 = parity_support::divergence!("D16");
    assert_eq!(d16.id(), "D16");
    let d40 = parity_support::divergence!("D40");
    assert_eq!(d40.id(), "D40");
    let Some((db, pool)) = database().await else {
        return;
    };
    let rec = seed_default(&pool, TENANT_A).await;
    telemetry(
        &pool,
        TENANT_A,
        json!({"recommendation_id": rec, "model_used": "m", "status": 200}),
    )
    .await;
    let body = ok(&pool, "/v1/history/outcomes?outcome=completed_unchecked").await;
    assert_eq!(body["outcomes"][0]["outcome"], "completed_unchecked");
    assert_eq!(body["outcomes"][0]["details"], json!({"status": 200}));
    let body = ok(&pool, "/v1/history/outcomes?outcome=success").await;
    assert_eq!(body["outcomes"], json!([]));
    db.close().await;
}

// parity: api.TestHistoryOutcomesCrossTenantIsolation
#[tokio::test]
async fn another_tenants_outcomes_are_invisible() {
    let Some((db, pool)) = database().await else {
        return;
    };
    let rec = seed_default(&pool, TENANT_A).await;
    telemetry(
        &pool,
        TENANT_A,
        json!({"recommendation_id": rec, "model_used": "claude-sonnet-4-6", "status": 200,
               "check_pass": true}),
    )
    .await;
    let own = ok(
        &pool,
        &format!("/v1/history/outcomes?recommendation_id={rec}"),
    )
    .await;
    assert_eq!(own["outcomes"].as_array().unwrap().len(), 1);
    let empty = "{\"outcomes\":[],\"next_cursor\":null,\"matrix_version\":\"2026-07-01\"}\n";
    assert_eq!(
        get_as(&pool, TENANT_B, "/v1/history/outcomes").await,
        (StatusCode::OK, empty.to_owned())
    );
    assert_eq!(
        get_as(
            &pool,
            TENANT_B,
            &format!("/v1/history/outcomes?recommendation_id={rec}")
        )
        .await,
        (StatusCode::OK, empty.to_owned())
    );
    db.close().await;
}

// parity: api.TestHistoryOutcomesPaginationWalk
#[tokio::test]
async fn outcome_pages_walk_newest_first() {
    let Some((db, pool)) = database().await else {
        return;
    };
    let rec = seed_default(&pool, TENANT_A).await;
    for _ in 0..3 {
        telemetry(
            &pool,
            TENANT_A,
            json!({"recommendation_id": rec, "model_used": "claude-sonnet-4-6", "status": 500}),
        )
        .await;
    }
    let all = ids(
        &ok(&pool, "/v1/history/outcomes?limit=200").await,
        "outcomes",
        "id",
    );
    assert_eq!(all.len(), 3);
    let first = ok(&pool, "/v1/history/outcomes?limit=2").await;
    assert_eq!(
        ids(&first, "outcomes", "id"),
        [all[0].as_str(), all[1].as_str()]
    );
    let cursor = first["next_cursor"].as_str().unwrap();
    assert_eq!(cursor, encode_cursor(KIND_OUTCOME, &[&all[1]]));
    let second = ok(
        &pool,
        &format!("/v1/history/outcomes?limit=2&cursor={cursor}"),
    )
    .await;
    assert_eq!(ids(&second, "outcomes", "id"), [all[2].as_str()]);
    assert_eq!(second["next_cursor"], Value::Null);
    assert_eq!(second["outcomes"][0]["outcome"], "provider_error");
    db.close().await;
}

// ---- model series ----

async fn column(pool: &PgPool, id: &str) {
    let empty = raw("{}");
    catalog::create_model_column(
        pool,
        &NewModelColumn {
            id,
            provider: "anthropic",
            model_id: "claude-series",
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

fn day(text: &str) -> NaiveDate {
    NaiveDate::parse_from_str(text, "%Y-%m-%d").unwrap()
}

async fn aggregate(
    pool: &PgPool,
    capability: &str,
    tier: i16,
    dimension: &str,
    ewma: f32,
    on: &str,
) {
    measurement::upsert_aggregate(
        pool,
        &AggregateUpsert {
            model_column_id: COLUMN,
            capability,
            difficulty_tier: tier,
            dimension,
            ewma,
            variance: 0.0,
            n: 3,
            measured: true,
            matrix_version: CORPUS,
            as_of: day(on),
        },
    )
    .await
    .unwrap();
}

// parity: api.TestHistoryModelSeriesShapeAndOrder
#[tokio::test]
async fn a_series_is_ascending_with_exact_points() {
    let Some((db, pool)) = database().await else {
        return;
    };
    column(&pool, COLUMN).await;
    aggregate(
        &pool,
        "structured_extraction",
        3,
        "quality",
        0.94,
        "2026-06-30",
    )
    .await;
    aggregate(
        &pool,
        "structured_extraction",
        3,
        "quality",
        0.91,
        "2026-06-29",
    )
    .await;
    aggregate(
        &pool,
        "structured_extraction",
        3,
        "quality",
        0.95,
        "2026-07-01",
    )
    .await;
    aggregate(
        &pool,
        "structured_extraction",
        3,
        "cost_per_run_usd",
        0.004,
        "2026-07-01",
    )
    .await;
    let (status, text) = get(&pool, &format!("/v1/history/models/{COLUMN}")).await;
    assert_eq!(status, StatusCode::OK);
    let point = |on: &str, ewma: &str| {
        format!(
            "{{\"as_of\":\"{on}\",\"capability\":\"structured_extraction\",\"difficulty\":3,\"ewma\":{ewma},\"variance\":0,\"n\":3,\"measured\":true,\"matrix_version\":\"2026-07-01\",\"regression\":false}}"
        )
    };
    assert_eq!(
        text,
        format!(
            "{{\"column_id\":\"{COLUMN}\",\"dimension\":\"quality\",\"series\":[{},{},{}],\"next_cursor\":null,\"matrix_version\":\"2026-07-01\"}}\n",
            point("2026-06-29", "0.91"),
            point("2026-06-30", "0.94"),
            point("2026-07-01", "0.95"),
        )
    );
    db.close().await;
}

async fn regression_alert(pool: &PgPool, subject: Value) {
    alerts::insert_alert(
        pool,
        &maverick_domain::ids::new(maverick_domain::ids::prefix::ALERT),
        "regression",
        "warning",
        &raw(&subject.to_string()),
        "regression",
    )
    .await
    .unwrap();
}

// parity: api.TestHistoryModelSeriesRegressionMarker
#[tokio::test]
async fn regression_alerts_mark_their_points() {
    let Some((db, pool)) = database().await else {
        return;
    };
    column(&pool, COLUMN).await;
    aggregate(
        &pool,
        "structured_extraction",
        3,
        "quality",
        0.94,
        "2026-06-29",
    )
    .await;
    aggregate(
        &pool,
        "structured_extraction",
        3,
        "quality",
        0.85,
        "2026-06-30",
    )
    .await;
    regression_alert(
        &pool,
        json!({"model_column_id": COLUMN, "capability": "structured_extraction",
               "difficulty_tier": 3, "dimension": "quality", "as_of": "2026-06-30"}),
    )
    .await;
    // Other cells, another dimension, no dimension (quality), and subjects Go could not
    // decode: none marks the 06-29 point.
    for subject in [
        json!({"model_column_id": COLUMN, "capability": "code_generation",
               "difficulty_tier": 3, "as_of": "2026-06-29"}),
        json!({"model_column_id": COLUMN, "capability": "structured_extraction",
               "difficulty_tier": 4, "as_of": "2026-06-29"}),
        json!({"model_column_id": COLUMN, "capability": "structured_extraction",
               "difficulty_tier": 3, "dimension": "pass_rate", "as_of": "2026-06-29"}),
        json!({"model_column_id": COLUMN, "capability": "structured_extraction",
               "difficulty_tier": "3", "as_of": "2026-06-29"}),
        json!({"model_column_id": COLUMN, "capability": "structured_extraction",
               "difficulty_tier": 3.5, "as_of": "2026-06-29"}),
        json!({"model_column_id": COLUMN, "capability": "structured_extraction",
               "difficulty_tier": 3, "as_of": ""}),
        json!({"model_column_id": "col_other", "capability": "structured_extraction",
               "difficulty_tier": 3, "as_of": "2026-06-29"}),
    ] {
        regression_alert(&pool, subject).await;
    }
    let body = ok(&pool, &format!("/v1/history/models/{COLUMN}")).await;
    let marks: Vec<(&str, bool)> = body["series"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["as_of"].as_str().unwrap(),
                p["regression"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(marks, [("2026-06-29", false), ("2026-06-30", true)]);
    db.close().await;
}

// parity: api.TestHistoryModelSeriesFilters
#[tokio::test]
async fn series_filters_and_their_errors() {
    let Some((db, pool)) = database().await else {
        return;
    };
    column(&pool, COLUMN).await;
    aggregate(
        &pool,
        "structured_extraction",
        3,
        "quality",
        0.93,
        "2026-07-01",
    )
    .await;
    aggregate(&pool, "code_generation", 3, "quality", 0.9, "2026-07-01").await;
    aggregate(
        &pool,
        "structured_extraction",
        4,
        "quality",
        0.88,
        "2026-07-01",
    )
    .await;
    aggregate(
        &pool,
        "structured_extraction",
        3,
        "latency_p95_ms",
        2100.0,
        "2026-07-01",
    )
    .await;
    aggregate(
        &pool,
        "structured_extraction",
        3,
        "quality",
        0.95,
        "2026-07-03",
    )
    .await;
    let base = format!("/v1/history/models/{COLUMN}");

    let body = ok(&pool, &format!("{base}?capability=code_generation")).await;
    assert_eq!(body["series"].as_array().unwrap().len(), 1);
    assert_eq!(body["series"][0]["capability"], "code_generation");
    let body = ok(&pool, &format!("{base}?difficulty=4")).await;
    assert_eq!(body["series"][0]["difficulty"], 4);
    assert_eq!(body["series"].as_array().unwrap().len(), 1);
    let (_, text) = get(&pool, &format!("{base}?dimension=latency_p95_ms")).await;
    assert!(
        text.contains("\"dimension\":\"latency_p95_ms\",\"series\":[{\"as_of\":\"2026-07-01\",\"capability\":\"structured_extraction\",\"difficulty\":3,\"ewma\":2100,"),
        "{text}"
    );
    let body = ok(&pool, &format!("{base}?since=2026-07-02")).await;
    assert_eq!(body["series"].as_array().unwrap().len(), 1);
    assert_eq!(body["series"][0]["as_of"], "2026-07-03");
    // A timestamp keeps its own calendar date.
    let body = ok(
        &pool,
        &format!("{base}?since=2026-07-02T23%3A00%3A00-04%3A00"),
    )
    .await;
    assert_eq!(body["series"][0]["as_of"], "2026-07-03");

    let error = |message: &str, details: &str| {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                "{{\"error\":{{\"code\":\"invalid_request\",\"message\":\"{message}\",\"details\":{details}}}}}\n"
            ),
        )
    };
    for (query, expected) in [
        (
            "capability=nope",
            error(
                "capability \\\"nope\\\" is not a v1 capability",
                "{\"capability\":\"nope\"}",
            ),
        ),
        (
            "tier=3",
            error(
                "unknown query parameter \\\"tier\\\"",
                "{\"parameter\":\"tier\"}",
            ),
        ),
        (
            "dimension=vibes",
            error(
                "dimension \\\"vibes\\\" is not one of [quality pass_rate ambiguity latency_p50_ms latency_p95_ms cost_per_run_usd]",
                "{\"field\":\"dimension\",\"value\":\"vibes\"}",
            ),
        ),
        (
            "difficulty=6",
            error("difficulty 6 is out of range 1-5", "{\"difficulty\":6}"),
        ),
        (
            "difficulty=three",
            error(
                "difficulty \\\"three\\\" must be an integer difficulty tier",
                "{\"parameter\":\"difficulty\",\"value\":\"three\"}",
            ),
        ),
    ] {
        assert_eq!(
            get(&pool, &format!("{base}?{query}")).await,
            expected,
            "{query}"
        );
        // The checks come before the column lookup.
        assert_eq!(
            get(&pool, &format!("/v1/history/models/col_01NOPE?{query}")).await,
            expected,
            "{query}"
        );
    }
    db.close().await;
}

// parity: api.TestHistoryModelSeriesPaginationWalk
#[tokio::test]
async fn series_pages_walk_in_cell_order() {
    let Some((db, pool)) = database().await else {
        return;
    };
    column(&pool, COLUMN).await;
    for on in ["2026-07-01", "2026-07-02", "2026-07-03"] {
        aggregate(&pool, "code_generation", 2, "quality", 0.9, on).await;
        aggregate(&pool, "structured_extraction", 3, "quality", 0.94, on).await;
    }
    let first = ok(&pool, &format!("/v1/history/models/{COLUMN}?limit=4")).await;
    // Byte-identical to the recorded Go cursor: the same cell.
    assert_eq!(
        first["next_cursor"],
        "djF8YWdnfDIwMjYtMDctMDJ8c3RydWN0dXJlZF9leHRyYWN0aW9ufDM"
    );
    let second = ok(
        &pool,
        &format!(
            "/v1/history/models/{COLUMN}?limit=4&cursor={}",
            first["next_cursor"].as_str().unwrap()
        ),
    )
    .await;
    assert_eq!(second["next_cursor"], Value::Null);
    let cells: Vec<(String, String)> = [&first, &second]
        .iter()
        .flat_map(|page| page["series"].as_array().unwrap().clone())
        .map(|p| {
            (
                p["as_of"].as_str().unwrap().to_owned(),
                p["capability"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    let expected: Vec<(String, String)> = ["2026-07-01", "2026-07-02", "2026-07-03"]
        .iter()
        .flat_map(|on| {
            ["code_generation", "structured_extraction"]
                .map(|capability| ((*on).to_owned(), capability.to_owned()))
        })
        .collect();
    assert_eq!(cells, expected);
    db.close().await;
}

// parity: api.TestHistoryModelSeriesUnknownColumn404
// parity: api.TestHistoryModelSeriesEmptyForKnownColumn
#[tokio::test]
async fn unknown_columns_404_and_known_empty_columns_are_empty() {
    let Some((db, pool)) = database().await else {
        return;
    };
    assert_eq!(
        get(&pool, "/v1/history/models/col_01NOPE").await,
        (
            StatusCode::NOT_FOUND,
            "{\"error\":{\"code\":\"not_found\",\"message\":\"resource not found\",\"details\":{}}}\n"
                .to_owned()
        )
    );
    column(&pool, COLUMN).await;
    assert_eq!(
        get(&pool, &format!("/v1/history/models/{COLUMN}")).await,
        (
            StatusCode::OK,
            format!(
                "{{\"column_id\":\"{COLUMN}\",\"dimension\":\"quality\",\"series\":[],\"next_cursor\":null,\"matrix_version\":\"2026-07-01\"}}\n"
            )
        )
    );
    // Columns are global: another tenant reads the same series.
    assert_eq!(
        get_as(&pool, TENANT_B, &format!("/v1/history/models/{COLUMN}"))
            .await
            .0,
        StatusCode::OK
    );
    db.close().await;
}

// ---- calibration ----

// parity: api.TestHistoryCalibrationListShape
#[tokio::test]
async fn calibration_is_newest_batch_first() {
    let Some((db, pool)) = database().await else {
        return;
    };
    let first = measurement::upsert_calibration(
        &pool,
        "bat_01OLD",
        "structured_extraction",
        0.12,
        0.03,
        150,
    )
    .await
    .unwrap();
    measurement::upsert_calibration(&pool, "bat_02NEW", "code_generation", 0.09, 0.03, 150)
        .await
        .unwrap();
    measurement::upsert_calibration(&pool, "bat_02NEW", "structured_extraction", 0.11, 0.03, 150)
        .await
        .unwrap();
    let since = first
        .scored_at
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let (status, text) = get(&pool, &format!("/v1/history/calibration?since={since}")).await;
    assert_eq!(status, StatusCode::OK);
    let body = parse(&text);
    let order: Vec<(&str, &str)> = body["calibration"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["batch_id"].as_str().unwrap(),
                c["capability"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        order,
        [
            ("bat_02NEW", "code_generation"),
            ("bat_02NEW", "structured_extraction"),
            ("bat_01OLD", "structured_extraction"),
        ]
    );
    assert_eq!(
        wire_keys(&text, "\"calibration\":["),
        [
            "batch_id",
            "capability",
            "brier_score",
            "overconfidence",
            "n",
            "scored_at"
        ]
    );
    assert!(
        text.starts_with("{\"calibration\":[{\"batch_id\":\"bat_02NEW\",\"capability\":\"code_generation\",\"brier_score\":0.09,\"overconfidence\":0.03,\"n\":150,\"scored_at\":\""),
        "{text}"
    );
    assert!(text.ends_with("],\"next_cursor\":null,\"matrix_version\":\"2026-07-01\"}\n"));
    db.close().await;
}

// parity: api.TestHistoryCalibrationFiltersAndPagination
#[tokio::test]
async fn calibration_filters_and_pages() {
    let Some((db, pool)) = database().await else {
        return;
    };
    for batch in ["bat_01", "bat_02", "bat_03"] {
        measurement::upsert_calibration(&pool, batch, "structured_extraction", 0.1, 0.03, 150)
            .await
            .unwrap();
        measurement::upsert_calibration(&pool, batch, "factual_qa", 0.2, 0.03, 150)
            .await
            .unwrap();
    }
    let body = ok(&pool, "/v1/history/calibration?capability=factual_qa").await;
    assert_eq!(
        ids(&body, "calibration", "batch_id"),
        ["bat_03", "bat_02", "bat_01"]
    );
    assert_eq!(
        get(&pool, "/v1/history/calibration?capability=nope")
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let first = ok(&pool, "/v1/history/calibration?limit=4").await;
    let cursor = first["next_cursor"].as_str().unwrap();
    assert_eq!(
        cursor,
        encode_cursor("cal", &["bat_02", "structured_extraction"])
    );
    let second = ok(
        &pool,
        &format!("/v1/history/calibration?limit=4&cursor={cursor}"),
    )
    .await;
    assert_eq!(second["next_cursor"], Value::Null);
    let rows: Vec<(String, String)> = [&first, &second]
        .iter()
        .flat_map(|page| page["calibration"].as_array().unwrap().clone())
        .map(|c| {
            (
                c["batch_id"].as_str().unwrap().to_owned(),
                c["capability"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    let expected: Vec<(String, String)> = ["bat_03", "bat_02", "bat_01"]
        .iter()
        .flat_map(|batch| {
            ["factual_qa", "structured_extraction"]
                .map(|capability| ((*batch).to_owned(), capability.to_owned()))
        })
        .collect();
    assert_eq!(rows, expected, "all six once, no repeats");
    // Calibration is global.
    let other = parse(&get_as(&pool, TENANT_B, "/v1/history/calibration").await.1);
    assert_eq!(other["calibration"].as_array().unwrap().len(), 6);
    db.close().await;
}

// ---- recorded Go exchanges ----

/// Every recorded non-200 history exchange, replayed against an empty database: the
/// status and body are Go's bytes. The one exception is D40's widened outcome list.
#[tokio::test]
async fn recorded_error_exchanges_match_go_bytes() {
    let Some((db, pool)) = database().await else {
        return;
    };
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../parity/blackbox/api-recorded.jsonl"
    );
    let mut replayed = 0;
    for line in std::fs::read_to_string(path).unwrap().lines() {
        let row: Value = serde_json::from_str(line).unwrap();
        let test = row["test"].as_str().unwrap();
        let status = row["response"]["status"].as_u64().unwrap();
        if !test.starts_with("TestHistory") || status == 200 {
            continue;
        }
        let request = &row["request"];
        let target = request["path"].as_str().unwrap();
        let backend = if request["auth_kind"] == "none" {
            StaticAuthBackend::missing()
        } else {
            as_tenant(TENANT_A)
        };
        let (got_status, got_body) =
            send(router(Some(&pool), backend), Method::GET, target, "").await;
        let mut want = row["response"]["body"].as_str().unwrap().to_owned();
        if target.contains("outcome=shrug") {
            let _d40 = parity_support::divergence!("D40");
            want = want.replace(
                "provider_error other",
                "provider_error completed_unchecked other",
            );
        }
        assert_eq!(
            got_status.as_u16(),
            u16::try_from(status).unwrap(),
            "{test} {target}"
        );
        assert_eq!(got_body, want, "{test} {target}");
        replayed += 1;
    }
    assert_eq!(replayed, 28, "every recorded history error exchange");
    db.close().await;
}
