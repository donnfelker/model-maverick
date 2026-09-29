//! The end-to-end test (reference Part 5 §13.14 `e2e_phase2_test.go`; spec §15 "Fake
//! providers"): the synthetic mini-corpus, three nights of the smoke batch and the
//! aggregate rebuild with the fake provider, then `PUT /v1/config/pools` and a declared
//! `POST /v1/recommend`. It is the one test that crosses measurement, aggregates, and
//! decision in one process. No network: the fake provider answers every call.

use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use maverick_api::{
    ApiState, AuthState, HealthState, Identity, RateLimiter, StaticAuthBackend, public_router_with,
};
use maverick_domain::graders::{GoldenAnswer, JsonSchema, KeyFacts, Registry, RuleScan};
use maverick_domain::keys;
use maverick_domain::spend::SpendCaps;
use maverick_domain::tasks;
use maverick_jobs::batch::{BatchConfig, BatchOutcome, run_batch};
use maverick_jobs::rebuild::{RebuildConfig, RebuildOutcome, aggregate_rebuild};
use maverick_jobs::{
    BatchFollowupArgs, BatchLeaseConfig, BatchLeaseManager, JobArgs, NightlySmokeArgs,
};
use maverick_providers::fake::{FakeProvider, FixtureSpec};
use maverick_runner::{Cancellation, Clients, RunnerConfig};
use maverick_store::catalog::{self, NewModelColumn};
use maverick_store::health::GRAPHILE_WORKER_SCHEMA;
use maverick_store::measurement::{self, CellDimension};
use maverick_store::{JsonRawValue, corpus, decisions, tenancy};
use maverick_testdb::Schema;
use serde_json::{Value, json};
use sqlx::PgPool;

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| {
    Schema::custom("engine + graphile_worker", |url| async move {
        maverick_store::migrate::migrate(&url).await?;
        Ok(())
    })
});

const TENANT: &str = "ten_e2e";
const KEY: &str = "key_e2e";
const VERSION: &str = "e2e-phase2-2026-07-06";
const GOOD_MODEL: &str = "claude-e2e-good";
const BAD_MODEL: &str = "gpt-e2e-bad";
const BAD_OUTPUT: &str = "I cannot process this document.";
const NIGHTS: [&str; 3] = ["2026-07-01", "2026-07-02", "2026-07-03"];

fn raw(text: &str) -> Box<JsonRawValue> {
    JsonRawValue::from_string(text.to_owned()).expect("json")
}

fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../parity/fixtures/api/testdata/e2e_corpus")
}

/// The good column's correct answer per task.
fn good_output(task_id: &str) -> &'static str {
    match task_id {
        "task_e2ex_001" => r#"{"order_id": "ORD-7"}"#,
        "task_e2ex_002" => r#"{"invoice_number": "INV-2041", "total_due": 1842.50}"#,
        "task_e2ex_003" => r#"{"effective_date": "2025-03-01", "monthly_fee_usd": 12500}"#,
        "task_e2ex_004" => {
            r#"{"subsidiaries": [{"name": "Alpha GmbH", "ownership": "80%", "jurisdiction": "Germany"}, {"name": "Beta KK", "ownership": "55%", "jurisdiction": "Japan"}]}"#
        }
        other => panic!("unexpected task {other}"),
    }
}

async fn column(pool: &PgPool, id: &str, provider: &str, model: &str, pricing: &str) {
    catalog::create_model_column(
        pool,
        &NewModelColumn {
            id,
            provider,
            model_id: model,
            endpoint: "direct",
            settings: &raw("{}"),
            status: "active",
            pricing: &raw(pricing),
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
}

fn lease() -> BatchLeaseManager {
    BatchLeaseManager::new(BatchLeaseConfig {
        lease_ttl: Duration::from_secs(600),
        heartbeat_interval: Duration::from_secs(200),
        owner_stale_after: Duration::from_secs(300),
        graphile_schema: GRAPHILE_WORKER_SCHEMA.to_string(),
    })
}

/// Serves the public router on a loopback port and returns its base URL.
async fn serve(pool: &PgPool) -> String {
    let auth = AuthState::new(
        Arc::new(StaticAuthBackend::authenticated(Identity {
            tenant_id: TENANT.to_owned(),
            api_key_id: KEY.to_owned(),
        })),
        keys::parse_internal_tokens("ops:alpha").expect("tokens"),
        RateLimiter::new(10_000),
    );
    let health = HealthState::with_providers(pool.clone(), Vec::new());
    let router = public_router_with(auth, ApiState::with_health(pool.clone(), health));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve");
    });
    base
}

async fn call(method: reqwest::Method, url: &str, body: &str) -> (u16, Value) {
    let response = reqwest::Client::new()
        .request(method, url)
        .header("Authorization", "Bearer mk_live_test")
        .header("Content-Type", "application/json")
        .body(body.to_owned())
        .send()
        .await
        .expect("request");
    let status = response.status().as_u16();
    let text = response.text().await.expect("body");
    eprintln!("{url} -> {status} {text}");
    (
        status,
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("not JSON ({e}): {text}")),
    )
}

async fn latest(
    pool: &PgPool,
    column: &str,
    tier: i16,
    dimension: &str,
) -> Option<(f32, i32, bool)> {
    measurement::get_latest_aggregate(
        pool,
        &CellDimension {
            model_column_id: column,
            capability: "structured_extraction",
            difficulty_tier: tier,
            dimension,
        },
    )
    .await
    .ok()
    .map(|row| (row.ewma, row.n, row.measured))
}

fn close(value: &Value, expected: f64, tolerance: f64) -> bool {
    value
        .as_f64()
        .is_some_and(|v| (v - expected).abs() <= tolerance)
}

/// parity: api.TestPhase2ExitEndToEndRecommend
#[tokio::test]
async fn phase2_exit_end_to_end_recommend() {
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

    // The mini-corpus, stored and activated.
    let loaded = tasks::load(&corpus_dir()).expect("the mini-corpus loads");
    assert_eq!(loaded.tasks.len(), 4);
    corpus::store_corpus(&pool, VERSION, "e2e", &loaded)
        .await
        .expect("store");
    corpus::activate_corpus(&pool, VERSION)
        .await
        .expect("activate");

    column(
        &pool,
        "col_good",
        "anthropic",
        GOOD_MODEL,
        r#"{"input_per_mtok": 3.0, "output_per_mtok": 15.0}"#,
    )
    .await;
    column(
        &pool,
        "col_bad",
        "openai",
        BAD_MODEL,
        r#"{"input_per_mtok": 2.5, "output_per_mtok": 10.0}"#,
    )
    .await;

    // The fake provider: the good column answers each task correctly, the bad one refuses.
    let mut specs = Vec::new();
    for task in corpus::list_active_tasks_by_corpus_version(&pool, VERSION)
        .await
        .expect("tasks")
    {
        let attachments = tasks::parse_attachments(task.attachments.get()).expect("attachments");
        let prompt = tasks::prompt_text(&task.prompt, &attachments);
        for (model, output, tokens_in, tokens_out) in [
            (GOOD_MODEL, good_output(&task.id), 400, 60),
            (BAD_MODEL, BAD_OUTPUT, 300, 40),
        ] {
            specs.push(
                serde_json::from_value::<FixtureSpec>(json!({
                    "model": model,
                    "prompt": prompt,
                    "output": output,
                    "tokens_in": tokens_in,
                    "tokens_out": tokens_out,
                }))
                .expect("fixture"),
            );
        }
    }
    let fake = Arc::new(FakeProvider::from_fixtures(&specs).expect("fake"));
    let mut registry = Registry::new();
    registry.must_register(Arc::new(GoldenAnswer));
    registry.must_register(Arc::new(KeyFacts));
    registry.must_register(Arc::new(RuleScan));
    registry.must_register(Arc::new(JsonSchema));
    let batch = BatchConfig {
        repeats: 3,
        caps: SpendCaps::default(),
        lease: lease(),
        runner: RunnerConfig::default(),
        clients: Clients::new()
            .with("anthropic", Arc::clone(&fake) as _)
            .with("openai", fake),
        graders: Arc::new(registry),
        notifier: None,
    };
    let rebuild = RebuildConfig {
        alpha: 0.3,
        lambda_var: 0.5,
        lambda_amb: 0.25,
        min_nights: 3,
        runner_timeout: Duration::from_secs(120),
        lease: lease(),
    };

    // Three nights of the smoke batch, each followed by its rebuild.
    for night in NIGHTS {
        let job = JobArgs::NightlySmoke(NightlySmokeArgs {
            night: night.to_owned(),
        });
        let out = match run_batch(&pool, &job, &batch, "worker-e2e", &Cancellation::new())
            .await
            .expect("batch")
        {
            BatchOutcome::Ran(out) => out,
            other => panic!("{night}: not run: {other:?}"),
        };
        // 3 smoke tasks x 2 columns x 3 repeats.
        assert_eq!((out.planned_runs, out.completed_runs), (18, 18), "{night}");
        assert!((out.completion_pct - 1.0).abs() < f64::EPSILON, "{night}");
        assert!(out.actual_usd > 0.0, "{night}");
        let rebuilt = aggregate_rebuild(
            &pool,
            &BatchFollowupArgs {
                night: night.to_owned(),
                batch_id: out.batch_id.clone(),
            },
            &rebuild,
            "worker-e2e",
        )
        .await
        .expect("rebuild");
        assert!(
            matches!(rebuilt, RebuildOutcome::Rebuilt(_)),
            "{night}: {rebuilt:?}"
        );
    }

    // The aggregates the decision reads.
    let (ewma, n, measured) = latest(&pool, "col_good", 2, "quality")
        .await
        .expect("good t2");
    assert!(
        n == 3 && measured && ewma >= 0.999,
        "good t2 quality {ewma} n {n}"
    );
    for tier in [1, 2, 4] {
        let (ewma, _, measured) = latest(&pool, "col_good", tier, "pass_rate")
            .await
            .expect("good pass_rate");
        assert!(measured && ewma >= 0.999, "good t{tier} pass_rate {ewma}");
    }
    let (ewma, n, measured) = latest(&pool, "col_bad", 2, "quality")
        .await
        .expect("bad t2");
    assert!(
        n == 3 && measured && ewma <= 0.001,
        "bad t2 quality {ewma} n {n}"
    );
    let (pass, _, _) = latest(&pool, "col_bad", 2, "pass_rate")
        .await
        .expect("bad pass");
    assert_eq!(pass, 0.0);
    assert!(
        latest(&pool, "col_good", 3, "quality").await.is_none(),
        "tier 3 is full-corpus-only"
    );

    // The HTTP calls.
    let base = serve(&pool).await;
    let (status, _) = call(
        reqwest::Method::PUT,
        &format!("{base}/v1/config/pools"),
        r#"{"pools":[{"name":"default","model_column_ids":["col_good","col_bad"],"is_default":true}]}"#,
    )
    .await;
    assert_eq!(status, 200);
    let (status, response) = call(
        reqwest::Method::POST,
        &format!("{base}/v1/recommend"),
        r#"{"workload":{"capability":"structured_extraction","difficulty":2},"mode":"balanced"}"#,
    )
    .await;
    assert_eq!(status, 200);
    let classification = &response["classification"];
    assert_eq!(classification["method"], "declared");
    assert_eq!(classification["capability"], "structured_extraction");
    assert_eq!(classification["difficulty"], 2);
    assert_eq!(classification["confidence"], 1);
    assert_eq!(response["measured"], true);
    assert_eq!(response["matrix_version"], VERSION);
    assert_eq!(response["pool"], "default");
    assert_eq!(response["mode"], "balanced");
    assert_eq!(
        response["check_spec"],
        json!({"type": "json_schema", "ref": "checks/extraction/v1"})
    );
    assert_eq!(response["constraints_met"], true);
    assert!(response.get("prompt_hash").is_none());
    let entries = response["recommendations"].as_array().expect("entries");
    assert_eq!(entries.len(), 2);
    let (good, bad) = (&entries[0], &entries[1]);
    assert_eq!(
        (
            &good["provider"],
            &good["model"],
            &good["endpoint"],
            &good["column_id"]
        ),
        (
            &json!("anthropic"),
            &json!(GOOD_MODEL),
            &json!("direct"),
            &json!("col_good")
        )
    );
    assert!(
        close(&good["score"], 99.95, 0.05),
        "good score {}",
        good["score"]
    );
    assert_eq!(good["clears_difficulty"], true);
    assert_eq!(bad["column_id"], "col_bad");
    assert_eq!(bad["clears_difficulty"], false);
    assert!(
        bad["score"].as_f64().is_some_and(|s| s <= 1.0),
        "bad score {}",
        bad["score"]
    );
    // D9: each entry also carries `settings` and `retention_mode`.
    for entry in entries {
        assert!(entry.get("settings").is_some() && entry.get("retention_mode").is_some());
    }

    // The trace.
    let id = response["recommendation_id"].as_str().expect("id");
    let row = decisions::get_recommendation(&pool, id)
        .await
        .expect("trace");
    assert_eq!(row.prompt_hash, None);
    let candidates: Value = serde_json::from_str(row.candidates.get()).expect("candidates");
    let candidates = candidates.as_array().expect("candidates");
    assert_eq!(candidates.len(), 2);
    let (good, bad) = (&candidates[0], &candidates[1]);
    assert_eq!(
        (&good["column_id"], &good["status"], &good["rank"]),
        (&json!("col_good"), &json!("survivor"), &json!(0))
    );
    assert!(
        close(&good["quality"], 100.0, 0.1),
        "good quality {}",
        good["quality"]
    );
    assert!(close(&good["pass_rate"], 1.0, 1e-9));
    // 400 x 3 / 1e6 + 60 x 15 / 1e6.
    assert!(
        close(&good["cost_per_run_usd"], 0.0021, 1e-5),
        "good cost {}",
        good["cost_per_run_usd"]
    );
    assert!(good["latency_p95_ms"].is_number() && good["v"].is_number());
    for flag in ["leader", "measured", "clears_difficulty", "prefs_satisfied"] {
        assert_eq!(good[flag], true, "good {flag}");
    }
    assert_eq!(
        (&bad["column_id"], &bad["status"], &bad["rank"]),
        (&json!("col_bad"), &json!("survivor"), &json!(1))
    );
    assert!(
        bad["quality"].as_f64().is_some_and(|q| q <= 1.0),
        "bad quality {}",
        bad["quality"]
    );
    assert!(close(&bad["pass_rate"], 0.0, 1e-9));
    // 300 x 2.5 / 1e6 + 40 x 10 / 1e6.
    assert!(
        close(&bad["cost_per_run_usd"], 0.00115, 1e-5),
        "bad cost {}",
        bad["cost_per_run_usd"]
    );
    assert_eq!(bad["measured"], true);
    assert_eq!(bad["clears_difficulty"], false);
    assert_eq!(bad["leader"], false);
    // D8: both columns answered every run, so neither is under the availability floor.
    assert!(
        candidates
            .iter()
            .all(|c| c.get("low_availability") != Some(&json!(true)))
    );

    pool.close().await;
    db.close().await;
}
