//! The full-stack end-to-end run (bead 9.10): the real `maverick` and `maverick-proxy`
//! binaries as separate processes, the deploy shape, on Postgres, against the fake
//! provider over HTTP. No real provider and no network beyond localhost.
//!
//! Run it with `scripts/e2e.sh`, which builds the binaries first. It is `#[ignore]`d in a
//! plain `cargo test` because it needs those binaries and Docker. Every step prints a
//! numbered header, what it sent, the status, and the elapsed time. Process logs and the
//! fake provider's request log (prompt hashes only) go to `target/e2e/<timestamp>/`, and a
//! failure prints the last 200 lines of every process log. `MAVERICK_E2E_BREAK=<step>`
//! fails that step on purpose, to check the report.

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use chrono::{Duration as Days, Utc};
use maverick_domain::classify::fence;
use maverick_e2e::FakeProviderServer;
use maverick_e2e::process::{Artifacts, FailureReport, Process, free_port, wait_ready};
use maverick_jobs::rebuild::enqueue_in;
use maverick_jobs::{JobArgs, NightlySmokeArgs, PredictionLockArgs};
use maverick_providers::fake::{FakeProvider, FixtureSpec};
use maverick_testdb::Schema;
use reqwest::header::HeaderMap;
use serde_json::{Value, json};
use sqlx::PgPool;

static EMPTY: LazyLock<Schema> = LazyLock::new(Schema::empty);

const NIGHT: &str = "2026-07-01";
const CORPUS_VERSION: &str = "e2e-fullstack-2026-07-01";
const GOOD: &str = "claude-e2e-good";
const BAD: &str = "gpt-e2e-bad";
const ZR_MODEL: &str = "claude-e2e-zr";
const BAD_OUTPUT: &str = "I cannot process this document.";
const INTERNAL_TOKEN: &str = "ops:e2e-internal-secret";
/// Sent inside prompts and request bodies; it must appear in no process log (G5).
const CANARY: &str = "E2E-CANARY-7f3a91c4";
const RECOMMEND_PROMPT: &str =
    "E2E-CANARY-7f3a91c4 Extract the order id from: order ORD-7 shipped.";
const CHAT_PROMPT: &str = "E2E-CANARY-7f3a91c4 chat: say ok";
const MESSAGES_PROMPT: &str = "E2E-CANARY-7f3a91c4 messages: say ok";
const CHAT_OUTPUT: &str = "ok from the chat entry";
const MESSAGES_OUTPUT: &str = "ok from the messages entry";
const CLASSIFICATION: &str =
    r#"{"capability":"structured_extraction","difficulty":2,"domain_tags":[],"confidence":0.93}"#;

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the workspace root")
        .to_path_buf()
}

fn corpus_dir() -> PathBuf {
    workspace().join("parity/fixtures/api/testdata/e2e_corpus")
}

/// The binaries: `MAVERICK_E2E_BIN_DIR` (set by `scripts/e2e.sh` after building them), or
/// the directory above this test binary.
fn binaries() -> (PathBuf, PathBuf) {
    let dir = std::env::var_os("MAVERICK_E2E_BIN_DIR").map_or_else(
        || {
            std::env::current_exe()
                .expect("test binary path")
                .parent()
                .and_then(Path::parent)
                .expect("target dir")
                .to_path_buf()
        },
        PathBuf::from,
    );
    let engine = dir.join("maverick");
    let proxy = dir.join("maverick-proxy");
    for bin in [&engine, &proxy] {
        assert!(
            bin.exists(),
            "{} is missing: run scripts/e2e.sh, or `cargo build -p maverick -p maverick-proxy` first",
            bin.display()
        );
    }
    (engine, proxy)
}

/// The good answer for each mini-corpus task (as in the 5.16 e2e).
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

fn spec(model: &str, prompt: &str, output: &str) -> FixtureSpec {
    serde_json::from_value(json!({
        "model": model, "prompt": prompt, "output": output,
        "tokens_in": 400, "tokens_out": 60,
    }))
    .expect("fixture")
}

/// Every fixture the run needs: each task for each benchmarked model, the classifier
/// call, and the proxy's two requests.
fn fixtures() -> Vec<FixtureSpec> {
    let loaded = maverick_domain::tasks::load(&corpus_dir()).expect("the mini-corpus loads");
    let mut specs = Vec::new();
    for task in &loaded.tasks {
        let prompt = task.prompt_text();
        specs.push(spec(GOOD, &prompt, good_output(&task.id)));
        specs.push(spec(ZR_MODEL, &prompt, good_output(&task.id)));
        specs.push(spec(BAD, &prompt, BAD_OUTPUT));
    }
    specs.push(spec(GOOD, &fence(RECOMMEND_PROMPT), CLASSIFICATION));
    specs.push(spec(BAD, CHAT_PROMPT, CHAT_OUTPUT));
    specs.push(spec(GOOD, MESSAGES_PROMPT, MESSAGES_OUTPUT));
    specs
}

/// Numbered steps with elapsed times.
struct Steps {
    number: usize,
    started: Instant,
    step_started: Instant,
    broken: Option<usize>,
}

impl Steps {
    fn new() -> Self {
        Self {
            number: 0,
            started: Instant::now(),
            step_started: Instant::now(),
            broken: std::env::var("MAVERICK_E2E_BREAK")
                .ok()
                .and_then(|step| step.parse().ok()),
        }
    }

    fn begin(&mut self, title: &str) {
        if self.number > 0 {
            eprintln!(
                "    ok in {:.2}s",
                self.step_started.elapsed().as_secs_f64()
            );
        }
        self.number += 1;
        self.step_started = Instant::now();
        eprintln!(
            "\n=== [{:02}] {title}  (t+{:.1}s)",
            self.number,
            self.started.elapsed().as_secs_f64()
        );
        assert!(
            self.broken != Some(self.number),
            "step {} broken on purpose (MAVERICK_E2E_BREAK)",
            self.number
        );
    }

    fn note(&self, text: impl AsRef<str>) {
        eprintln!("    {}", text.as_ref());
    }
}

/// One HTTP exchange, logged.
struct Reply {
    status: u16,
    headers: HeaderMap,
    body: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|error| panic!("not JSON ({error}): {}", self.body))
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }

    /// The JSON-RPC message of an MCP reply (one SSE frame).
    fn frame(&self) -> Value {
        let data = self
            .body
            .strip_prefix("event: message\ndata: ")
            .and_then(|rest| rest.strip_suffix("\n\n"))
            .unwrap_or_else(|| panic!("not one SSE frame: {:?}", self.body));
        serde_json::from_str(data).expect("SSE JSON")
    }
}

async fn http(
    steps: &Steps,
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<&Value>,
) -> Reply {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("http client");
    let mut request = client.request(method.parse().expect("method"), url);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    if let Some(body) = body {
        request = request
            .header("content-type", "application/json")
            .body(body.to_string());
    }
    let started = Instant::now();
    let response = request
        .send()
        .await
        .unwrap_or_else(|error| panic!("{method} {url}: {error}"));
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let body = response.text().await.expect("body");
    steps.note(format!(
        "{method} {url} -> {status} in {} ms",
        started.elapsed().as_millis()
    ));
    Reply {
        status,
        headers,
        body,
    }
}

fn expect(reply: &Reply, status: u16, what: &str) {
    assert_eq!(reply.status, status, "{what}: {}", reply.body);
}

/// The engine's environment: only what `maverick` reads.
fn engine_env(db: &str, fake: &str, public: u16, internal: u16) -> Vec<(String, String)> {
    [
        ("MAVERICK_DATABASE_URL", db.to_owned()),
        ("MAVERICK_INTERNAL_TOKENS", INTERNAL_TOKEN.to_owned()),
        ("MAVERICK_CAPTURE_MASTER_KEY", "c4".repeat(32)),
        ("MAVERICK_PUBLIC_ADDR", format!("127.0.0.1:{public}")),
        ("MAVERICK_INTERNAL_ADDR", format!("127.0.0.1:{internal}")),
        (
            "MAVERICK_ANTHROPIC_API_KEY",
            "fake-anthropic-key".to_owned(),
        ),
        ("MAVERICK_OPENAI_API_KEY", "fake-openai-key".to_owned()),
        ("MAVERICK_ANTHROPIC_BASE_URL", fake.to_owned()),
        ("MAVERICK_OPENAI_BASE_URL", format!("{fake}/openai")),
        ("MAVERICK_CLASSIFIER_COLUMN", format!("{GOOD}/direct")),
        ("MAVERICK_RUNNER_REPEATS", "1".to_owned()),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value))
    .collect()
}

/// The proxy's environment. Upstreams no step uses point at a closed port.
fn proxy_env(engine: &str, fake: &str, port: u16, extra: &[(&str, &str)]) -> Vec<(String, String)> {
    let closed = "http://127.0.0.1:1";
    let mut env: Vec<(String, String)> = [
        ("MAVERICK_ENGINE_URL", engine),
        ("MAVERICK_PROXY_ADDR", &format!("127.0.0.1:{port}")),
        ("MAVERICK_PROXY_TELEMETRY", "on"),
        ("MAVERICK_UPSTREAM_OPENAI", fake),
        ("MAVERICK_UPSTREAM_ANTHROPIC", fake),
        ("MAVERICK_UPSTREAM_GOOGLE", closed),
        ("MAVERICK_UPSTREAM_XAI", closed),
        ("MAVERICK_UPSTREAM_OPENROUTER", closed),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .collect();
    env.extend(
        extra
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned())),
    );
    env
}

async fn count(pool: &PgPool, sql: &str) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .fetch_one(pool)
        .await
        .unwrap_or_else(|error| panic!("{sql}: {error}"))
}

/// Polls `check` until it holds or `timeout` passes.
async fn eventually<F, Fut>(timeout: Duration, mut check: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if check().await {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "full-stack e2e: needs the built binaries and Docker; run scripts/e2e.sh"]
async fn the_whole_stack_runs_as_one_system() {
    let mut steps = Steps::new();
    let artifacts = Artifacts::create(&workspace()).expect("artifact dir");
    let report = FailureReport::new(&artifacts);
    eprintln!("e2e artifacts: {}", artifacts.dir().display());
    let (engine_bin, proxy_bin) = binaries();

    // ---- 1. Postgres ----
    steps.begin("Start Postgres 16 (testcontainers) with an empty database");
    let Some(db) = maverick_testdb::database(&EMPTY).await else {
        eprintln!("SKIP: Docker is not available (set MAVERICK_TEST_REQUIRE_DB=1 to fail)");
        return;
    };
    let db_url = db.url().to_owned();
    steps.note(format!("database {}", db.name()));
    let pool = db.pool().await;

    // ---- 2. migrate ----
    steps.begin("maverick migrate");
    let fake = Arc::new(
        FakeProviderServer::start(FakeProvider::from_fixtures(&fixtures()).expect("fixtures"))
            .await
            .expect("fake provider"),
    );
    {
        let fake = Arc::clone(&fake);
        report.note(move || {
            format!(
                "fake provider misses (by prompt hash): {:#?}",
                fake.misses()
            )
        });
    }
    let (public, api_internal, worker_internal) = (free_port(), free_port(), free_port());
    let env = engine_env(&db_url, fake.url(), public, api_internal);
    let mut migrate = Process::spawn(&artifacts, "migrate", &engine_bin, &["migrate"], &env)
        .expect("spawn migrate");
    let status = migrate
        .wait(Duration::from_secs(120))
        .await
        .expect("migrate exits");
    assert!(status.success(), "migrate: {status}\n{}", migrate.log());
    assert!(
        migrate.log().contains("migrations complete"),
        "{}",
        migrate.log()
    );

    // ---- 3. fake provider ----
    steps.begin("Fake provider over HTTP");
    steps.note(format!(
        "{} ({} fixtures; anthropic {0}, openai {0}/openai)",
        fake.url(),
        fixtures().len()
    ));

    // ---- 4. serve --worker-only ----
    // The worker's internal listener serves the seam, so the catalog is provisioned
    // before the API starts: the API resolves its classifier column and judge pool
    // against the catalog at boot.
    steps.begin("maverick serve --worker-only (its internal listener serves the seam)");
    let worker_env = engine_env(&db_url, fake.url(), free_port(), worker_internal);
    let mut worker = Process::spawn(
        &artifacts,
        "worker",
        &engine_bin,
        &["serve", "--worker-only"],
        &worker_env,
    )
    .expect("spawn worker");
    let internal_url = format!("http://127.0.0.1:{worker_internal}");
    let waited = wait_ready(
        &format!("{internal_url}/health/detail"),
        Duration::from_secs(60),
    )
    .await
    .expect("worker internal listener");
    steps.note(format!("worker internal listener after {waited:?}"));

    // ---- 5. seam: catalog, tenants, keys, config ----
    steps.begin("Internal seam: columns, tenants, API keys, pools, weights, constraints");
    let ops = format!("Bearer {INTERNAL_TOKEN}");
    let ops_headers = [("authorization", ops.as_str())];
    let pricing = |input: f64, output: f64| {
        json!({"input_per_mtok": input, "output_per_mtok": output, "currency": "USD",
               "as_of": "2026-07-01", "source": "list"})
    };
    let mut columns = Vec::new();
    let now = Utc::now();
    for body in [
        json!({"provider": "anthropic", "model_id": GOOD, "endpoint": "direct",
               "pricing": pricing(3.0, 15.0)}),
        json!({"provider": "anthropic", "model_id": GOOD, "endpoint": "vertex",
               "pricing": pricing(3.3, 16.5)}),
        json!({"provider": "openai", "model_id": BAD, "endpoint": "direct",
               "pricing": pricing(2.5, 10.0)}),
        json!({"provider": "anthropic", "model_id": ZR_MODEL, "endpoint": "direct",
               "pricing": pricing(3.0, 15.0), "retention_mode": "zero",
               "retention_verified_at": now.to_rfc3339(),
               "retention_valid_until": (now + Days::days(30)).to_rfc3339(),
               "retention_source": "https://example.test/zdr"}),
    ] {
        let created = http(
            &steps,
            "POST",
            &format!("{internal_url}/internal/v1/columns"),
            &ops_headers,
            Some(&body),
        )
        .await;
        expect(&created, 201, "column");
        columns.push(created.json()["column_id"].as_str().unwrap().to_owned());
    }
    let [good_col, vertex_col, bad_col, zr_col] = <[String; 4]>::try_from(columns).unwrap();

    let tenant = |name: &'static str| {
        let steps = &steps;
        let internal_url = &internal_url;
        let ops_headers = &ops_headers;
        async move {
            let created = http(
                steps,
                "POST",
                &format!("{internal_url}/internal/v1/tenants"),
                ops_headers,
                Some(&json!({"name": name})),
            )
            .await;
            expect(&created, 201, "tenant");
            let id = created.json()["tenant"]["id"].as_str().unwrap().to_owned();
            let key = http(
                steps,
                "POST",
                &format!("{internal_url}/internal/v1/tenants/{id}/api-keys"),
                ops_headers,
                Some(&json!({"name": "e2e"})),
            )
            .await;
            expect(&key, 201, "api key");
            (id, key.json()["key"].as_str().unwrap().to_owned())
        }
    };
    let (tenant_id, tenant_key) = tenant("e2e-tenant").await;
    let (zr_id, zr_key) = tenant("e2e-zero-retention").await;
    for (id, resource, body) in [
        (
            &tenant_id,
            "pools",
            json!({"pools": [{"name": "default", "is_default": true,
                              "model_column_ids": [&good_col, &vertex_col, &bad_col]}]}),
        ),
        (
            &tenant_id,
            "weights",
            json!({"weights": [{"capability": "structured_extraction", "w_quality": 0.6,
                                "w_cost": 0.3, "w_latency": 0.1, "quality_bar": 0.8}]}),
        ),
        (
            &tenant_id,
            "constraints",
            json!({"constraints": {"max_cost_per_run": null, "max_latency_ms": null}}),
        ),
        (
            &zr_id,
            "pools",
            json!({"pools": [{"name": "default", "is_default": true,
                              "model_column_ids": [&zr_col]}]}),
        ),
        (
            &zr_id,
            "constraints",
            json!({"constraints": {"zero_retention": true}}),
        ),
    ] {
        let put = http(
            &steps,
            "PUT",
            &format!("{internal_url}/internal/v1/tenants/{id}/{resource}"),
            &ops_headers,
            Some(&body),
        )
        .await;
        expect(&put, 200, resource);
    }

    // ---- 6. corpus ----
    steps.begin("maverick corpus load + activate (the synthetic mini-corpus)");
    let dir = corpus_dir();
    let dir = dir.to_str().expect("utf-8 path");
    for (name, args) in [
        (
            "corpus-load",
            vec!["corpus", "load", "--version", CORPUS_VERSION, "--dir", dir],
        ),
        (
            "corpus-activate",
            vec!["corpus", "activate", "--version", CORPUS_VERSION],
        ),
    ] {
        let mut cli = Process::spawn(&artifacts, name, &engine_bin, &args, &env).expect(name);
        let status = cli.wait(Duration::from_secs(60)).await.expect(name);
        assert!(status.success(), "{name}: {status}\n{}", cli.log());
        steps.note(format!(
            "{name}: {}",
            cli.log().lines().last().unwrap_or_default()
        ));
    }

    // ---- 7. one night of jobs ----
    steps.begin(&format!(
        "Enqueue prediction_lock and nightly_smoke for {NIGHT}; wait for the follow-ups"
    ));
    {
        let mut conn = pool.acquire().await.expect("connection");
        for job in [
            JobArgs::PredictionLock(PredictionLockArgs {
                night: NIGHT.to_owned(),
            }),
            JobArgs::NightlySmoke(NightlySmokeArgs {
                night: NIGHT.to_owned(),
            }),
        ] {
            enqueue_in(&mut conn, &job).await.expect("enqueue");
            steps.note(format!("enqueued {:?}", job.kind()));
        }
    }
    let settled = eventually(Duration::from_secs(180), || {
        let pool = pool.clone();
        async move {
            count(&pool, "select count(*) from graphile_worker._private_jobs").await == 0
                && count(
                    &pool,
                    "select count(*) from batches where kind in
                       ('nightly_smoke','aggregate_rebuild','regression_scan','calibration_score')
                       and status = 'completed'",
                )
                .await
                    == 4
        }
    })
    .await;
    let batches: Vec<(String, String, String)> =
        sqlx::query_as("select kind, batch_window, status from batches order by kind")
            .fetch_all(&pool)
            .await
            .expect("batches");
    steps.note(format!("batches: {batches:?}"));
    assert!(settled, "the night did not settle: {batches:?}");
    for kind in [
        "nightly_smoke",
        "aggregate_rebuild",
        "regression_scan",
        "calibration_score",
    ] {
        assert_eq!(
            batches.iter().filter(|(k, _, _)| k == kind).count(),
            1,
            "{kind} runs exactly once"
        );
    }
    let runs = count(&pool, "select count(*) from runs").await;
    let aggregates = count(
        &pool,
        &format!("select count(*) from aggregates where as_of = '{NIGHT}'"),
    )
    .await;
    steps.note(format!("runs {runs}, aggregates at {NIGHT}: {aggregates}"));
    assert!(runs > 0 && aggregates > 0);
    assert!(fake.misses().is_empty(), "every batch call had a fixture");

    // ---- 8. recommend ----
    // ---- serve --api-only ----
    steps.begin("maverick serve --api-only (a separate process, after provisioning)");
    let mut api = Process::spawn(
        &artifacts,
        "api",
        &engine_bin,
        &["serve", "--api-only"],
        &env,
    )
    .expect("spawn api");
    let public_url = format!("http://127.0.0.1:{public}");
    let waited = wait_ready(&format!("{public_url}/up"), Duration::from_secs(60))
        .await
        .expect("api /up");
    steps.note(format!("api /up after {waited:?}"));
    let up = http(&steps, "GET", &format!("{public_url}/up"), &[], None).await;
    expect(&up, 200, "/up");
    let api_internal_url = format!("http://127.0.0.1:{api_internal}");
    let health = http(
        &steps,
        "GET",
        &format!("{api_internal_url}/health/detail"),
        &ops_headers,
        None,
    )
    .await;
    expect(&health, 200, "/health/detail");
    let health = health.json();
    steps.note(format!("api /health/detail: {health}"));
    assert_eq!(health["status"], "ok", "{health}");
    assert_eq!(health["active_matrix_version"], CORPUS_VERSION);
    assert_eq!(health["components"]["db"]["status"], "ok");

    // ---- recommend ----
    steps.begin("POST /v1/recommend: declared and prompt forms");
    let bearer = format!("Bearer {tenant_key}");
    let tenant_headers = [("authorization", bearer.as_str())];
    let traces_before = count(&pool, "select count(*) from recommendations").await;
    let declared = http(
        &steps,
        "POST",
        &format!("{public_url}/v1/recommend"),
        &tenant_headers,
        Some(&json!({"workload": {"capability": "structured_extraction", "difficulty": 2}})),
    )
    .await;
    expect(&declared, 200, "declared recommend");
    let body = declared.json();
    let chain: Vec<&str> = body["recommendations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["column_id"].as_str().unwrap())
        .collect();
    steps.note(format!(
        "pick {} ({}), chain {chain:?}, measured {}",
        body["recommendations"][0]["model"],
        body["recommendations"][0]["endpoint"],
        body["measured"]
    ));
    assert_eq!(body["classification"]["method"], "declared");
    assert_eq!(chain.len(), 3, "the whole pool is the chain");
    for column in [&good_col, &vertex_col, &bad_col] {
        assert!(chain.contains(&column.as_str()), "{column} in {chain:?}");
    }
    assert_eq!(
        body["recommendations"][0]["model"], GOOD,
        "the correct column leads"
    );
    assert_eq!(
        body["measured"], false,
        "one night is not measured (3 needed)"
    );
    assert_eq!(body["telemetry"], true);
    assert_eq!(body["zero_retention"], false);

    let prompted = http(
        &steps,
        "POST",
        &format!("{public_url}/v1/recommend"),
        &tenant_headers,
        Some(&json!({"workload": {"prompt": RECOMMEND_PROMPT}})),
    )
    .await;
    expect(&prompted, 200, "prompt recommend");
    let body = prompted.json();
    assert_eq!(body["classification"]["method"], "classified", "{body}");
    assert_eq!(
        body["classification"]["capability"],
        "structured_extraction"
    );
    assert!(
        body["prompt_hash"]
            .as_str()
            .unwrap()
            .starts_with("hmac_sha256:")
    );
    assert_eq!(
        count(&pool, "select count(*) from recommendations").await,
        traces_before + 2,
        "one trace row per call"
    );
    assert_eq!(
        count(
            &pool,
            &format!("select count(*) from recommendations where request::text like '%{CANARY}%'")
        )
        .await,
        0,
        "the prompt is never stored"
    );

    // ---- 9. proxy, self-hosted and hosted ----
    let mut telemetry_ids = Vec::new();
    for (mode, extra) in [
        (
            "self-hosted",
            vec![("MAVERICK_API_KEY", tenant_key.as_str())],
        ),
        ("hosted", vec![("MAVERICK_PROXY_MODE", "hosted")]),
    ] {
        steps.begin(&format!(
            "maverick-proxy ({mode}): one chat and one messages request"
        ));
        let port = free_port();
        let mut proxy = Process::spawn(
            &artifacts,
            &format!("proxy-{mode}"),
            &proxy_bin,
            &[],
            &proxy_env(&public_url, fake.url(), port, &extra),
        )
        .expect("spawn proxy");
        let proxy_url = format!("http://127.0.0.1:{port}");
        wait_ready(&format!("{proxy_url}/up"), Duration::from_secs(30))
            .await
            .expect("proxy /up");
        let tenant_header = [("x-maverick-api-key", tenant_key.as_str())];
        let extra_headers: &[(&str, &str)] = if mode == "hosted" {
            &tenant_header
        } else {
            &[]
        };
        let workload = ("x-maverick-workload", "structured_extraction:2");

        let mut chat_headers = vec![("authorization", "Bearer sk-e2e-caller-openai"), workload];
        chat_headers.extend_from_slice(extra_headers);
        let chat = http(
            &steps,
            "POST",
            &format!("{proxy_url}/v1/chat/completions"),
            &chat_headers,
            Some(&json!({"model": "maverick/auto",
                         "messages": [{"role": "user", "content": CHAT_PROMPT}]})),
        )
        .await;
        expect(&chat, 200, "proxy chat");
        assert_eq!(chat.json()["choices"][0]["message"]["content"], CHAT_OUTPUT);
        assert_eq!(chat.header("x-maverick-model-used"), Some(BAD));
        let chat_id = chat
            .header("x-maverick-recommendation-id")
            .expect("correlation header")
            .to_owned();

        let mut messages_headers = vec![("x-api-key", "sk-ant-e2e-caller"), workload];
        messages_headers.extend_from_slice(extra_headers);
        let messages = http(
            &steps,
            "POST",
            &format!("{proxy_url}/v1/messages"),
            &messages_headers,
            Some(&json!({"model": "maverick/auto", "max_tokens": 64,
                         "messages": [{"role": "user", "content": MESSAGES_PROMPT}]})),
        )
        .await;
        expect(&messages, 200, "proxy messages");
        assert_eq!(messages.json()["content"][0]["text"], MESSAGES_OUTPUT);
        assert_eq!(messages.header("x-maverick-model-used"), Some(GOOD));
        let messages_id = messages
            .header("x-maverick-recommendation-id")
            .expect("correlation header")
            .to_owned();

        // SIGTERM flushes the telemetry batch.
        let status = proxy
            .terminate(Duration::from_secs(30))
            .await
            .expect("proxy stops");
        assert!(status.success(), "proxy {mode}: {status}");
        telemetry_ids.extend([chat_id, messages_id]);
    }
    steps.begin("Proxy telemetry arrived as outcome rows");
    for id in &telemetry_ids {
        let arrived = eventually(Duration::from_secs(30), || {
            let pool = pool.clone();
            let id = id.clone();
            async move {
                count(
                    &pool,
                    &format!("select count(*) from outcomes where recommendation_id = '{id}'"),
                )
                .await
                    >= 1
            }
        })
        .await;
        assert!(arrived, "telemetry for {id}");
        steps.note(format!("outcome for {id}"));
    }

    // ---- 10. privacy ----
    steps.begin("Privacy: zero-retention prompt form is refused");
    let zr_bearer = format!("Bearer {zr_key}");
    let refused = http(
        &steps,
        "POST",
        &format!("{public_url}/v1/recommend"),
        &[("authorization", zr_bearer.as_str())],
        Some(&json!({"workload": {"prompt": RECOMMEND_PROMPT}})),
    )
    .await;
    expect(&refused, 422, "zero-retention prompt form");
    assert_eq!(
        refused.json()["error"]["code"],
        "zero_retention_requires_declared"
    );
    let declared_zr = http(
        &steps,
        "POST",
        &format!("{public_url}/v1/recommend"),
        &[("authorization", zr_bearer.as_str())],
        Some(&json!({"workload": {"capability": "structured_extraction", "difficulty": 2}})),
    )
    .await;
    expect(&declared_zr, 200, "zero-retention declared form");
    let body = declared_zr.json();
    assert_eq!(body["zero_retention"], true);
    assert_eq!(body["recommendations"][0]["column_id"], zr_col.as_str());

    // ---- 11. MCP ----
    steps.begin("MCP over /mcp: initialize, tools/list, recommend_model");
    let mut mcp_headers = vec![
        ("authorization", bearer.as_str()),
        ("accept", "application/json, text/event-stream"),
    ];
    let init = http(
        &steps,
        "POST",
        &format!("{public_url}/mcp"),
        &mcp_headers,
        Some(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                     "params": {"clientInfo": {"name": "e2e", "version": "v1"},
                                "protocolVersion": "2025-11-25", "capabilities": {}}})),
    )
    .await;
    expect(&init, 200, "initialize");
    let session = init
        .header("mcp-session-id")
        .expect("session id")
        .to_owned();
    mcp_headers.push(("mcp-session-id", session.as_str()));
    let ack = http(
        &steps,
        "POST",
        &format!("{public_url}/mcp"),
        &mcp_headers,
        Some(&json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}})),
    )
    .await;
    expect(&ack, 202, "initialized");
    let tools = http(
        &steps,
        "POST",
        &format!("{public_url}/mcp"),
        &mcp_headers,
        Some(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})),
    )
    .await;
    expect(&tools, 200, "tools/list");
    let names: Vec<String> = tools.frame()["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names, ["get_policy", "list_models", "recommend_model"]);
    let called = http(
        &steps,
        "POST",
        &format!("{public_url}/mcp"),
        &mcp_headers,
        Some(&json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
                     "params": {"name": "recommend_model", "arguments":
                        {"workload": {"capability": "structured_extraction", "difficulty": 2}}}})),
    )
    .await;
    expect(&called, 200, "recommend_model");
    let result = &called.frame()["result"];
    assert_ne!(result["isError"], true, "{result}");
    assert_eq!(
        result["structuredContent"]["recommendations"][0]["model"],
        GOOD
    );

    // ---- 12. shutdown ----
    steps.begin("SIGTERM the api and the worker; clean exits");
    for process in [&mut api, &mut worker] {
        let status = process
            .terminate(Duration::from_secs(45))
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        assert!(status.success(), "{}: {status}", process.name());
        assert!(process.log().contains("\"stopped\""), "{}", process.name());
    }
    let failed_jobs = count(
        &pool,
        "select count(*) from alerts where kind = 'job_failure' or severity = 'critical'",
    )
    .await;
    assert_eq!(failed_jobs, 0, "no job failure or critical alert");

    steps.begin("No canary in any process log; fake provider log saved");
    for (name, log) in artifacts.logs() {
        assert!(!log.contains(CANARY), "{name}.log carries the canary");
    }
    let served = fake.served();
    artifacts
        .write("fake-provider.txt", &format!("{served:#?}"))
        .expect("fake log");
    steps.note(format!(
        "{} provider calls, {} misses",
        served.len(),
        fake.misses().len()
    ));
    assert!(
        fake.misses().is_empty(),
        "every provider call had a fixture"
    );
    eprintln!(
        "\n=== e2e passed in {:.1}s; artifacts in {}",
        steps.started.elapsed().as_secs_f64(),
        artifacts.dir().display()
    );
    drop(report);
    pool.close().await;
    db.close().await;
}
