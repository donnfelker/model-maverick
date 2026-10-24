//! The hot-path latency harness (bead 9.3; master plan §3.2 "Latency target"; spec §2.3;
//! D3, D20). M6 gates on these three numbers:
//!
//! - **Declared path:** `POST /v1/recommend` with a declared workload, p95 < 50 ms
//!   intra-region. It reads precomputed aggregates, inserts one trace row, and calls no
//!   provider.
//! - **Uncached prompt path:** a prompt-form recommend whose prompt is new, so the
//!   classifier runs. p95 < 1.8 s (D20), inside the SDK's 2 s timeout. Here the classifier is
//!   the fake provider with a configurable latency, which checks the budget arithmetic. The
//!   real classifier latency is measured in M6 with owner keys. One extra call over the 1.5 s
//!   classifier deadline (D3) must fall back to `fallback_default` within that deadline.
//! - **Proxy added time to first byte:** a chat request through `maverick-proxy` against the
//!   same request sent straight to the fake upstream. The plan sets no number; it is
//!   reported.
//!
//! The engine and the proxy are the real binaries as processes (`--api-only`), on Postgres,
//! with a realistic catalog: 12 active columns across the four providers, and a week of
//! aggregates for 10 capabilities × 5 tiers × 5 dimensions. Run it with
//! `scripts/latency.sh`, which builds everything in release. By default it only reports;
//! `MAVERICK_LATENCY_ASSERT=1` also asserts the targets (the M6 gate run, on a quiet host).
//! The report prints as a table and goes to `target/latency/<ts>/report.json`.
//!
//! To read the numbers on a shared host, the report also has an environment floor (Postgres
//! `select 1`, a single-row insert and commit, and the API's `/up`), the API's CPU time per
//! request for `/up`, `GET /v1/models`, and the declared recommend (from
//! `/proc/<pid>/stat`), and the database transactions per declared recommend (from
//! `pg_stat_database`).
//!
//! Knobs (environment): `MAVERICK_LATENCY_DECLARED` (500 requests),
//! `MAVERICK_LATENCY_PROMPT` (100), `MAVERICK_LATENCY_PROXY` (200),
//! `MAVERICK_LATENCY_WARMUP` (30), `MAVERICK_LATENCY_CONCURRENCY` (4),
//! `MAVERICK_LATENCY_CLASSIFIER_MS` (600).

use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use maverick_domain::classify::fence;
use maverick_domain::scoring::observe::dimension;
use maverick_domain::tasks::CAPABILITIES;
use maverick_e2e::FakeProviderServer;
use maverick_e2e::harness::{Steps, binaries, expect, http, workspace};
use maverick_e2e::process::{Artifacts, FailureReport, Process, free_port, wait_ready};
use maverick_providers::fake::{FakeProvider, FixtureSpec};
use maverick_store::catalog::{self, NewModelColumn};
use maverick_store::{JsonRawValue, corpus};
use maverick_testdb::Schema;
use serde_json::{Value, json};
use sqlx::PgPool;

static EMPTY: LazyLock<Schema> = LazyLock::new(Schema::empty);

const INTERNAL_TOKEN: &str = "ops:latency-internal-secret";
const CORPUS: &str = "latency-2026-09";
/// The default classifier column (`MAVERICK_CLASSIFIER_COLUMN` is left unset).
const CLASSIFIER_MODEL: &str = "claude-haiku-4-5";
const PROXY_PROMPT: &str = "latency probe: reply ok";
const PROXY_OUTPUT: &str = "ok";
/// A prompt whose classifier call outlasts the 1.5 s deadline.
const SLOW_PROMPT: &str = "latency probe over the classifier deadline";
/// Requests per environment-floor measurement.
const FLOOR: usize = 200;

/// The realistic catalog: `(provider, model, endpoint, input $/Mtok, output $/Mtok)`.
const CATALOG: [(&str, &str, &str, f64, f64); 12] = [
    ("anthropic", "claude-haiku-4-5", "direct", 1.0, 5.0),
    ("anthropic", "claude-sonnet-4-6", "direct", 3.0, 15.0),
    ("anthropic", "claude-opus-4-8", "direct", 15.0, 75.0),
    ("anthropic", "claude-sonnet-4-6", "vertex", 3.3, 16.5),
    ("openai", "gpt-5.5", "direct", 2.5, 10.0),
    ("openai", "gpt-5-mini", "direct", 0.25, 2.0),
    ("openai", "gpt-5.5", "azure", 2.75, 11.0),
    ("google", "gemini-3.1-pro", "direct", 2.0, 12.0),
    ("google", "gemini-3.1-flash", "direct", 0.3, 2.5),
    ("google", "gemini-3.1-pro", "vertex", 2.2, 13.2),
    ("xai", "grok-4", "direct", 3.0, 15.0),
    ("xai", "grok-4-fast", "direct", 0.2, 0.5),
];

/// The run's sizes, from the environment.
struct Knobs {
    declared: usize,
    prompt: usize,
    proxy: usize,
    warmup: usize,
    concurrency: usize,
    classifier_ms: u64,
    assert_targets: bool,
}

impl Knobs {
    fn from_env() -> Self {
        fn var<T: std::str::FromStr>(name: &str, default: T) -> T {
            std::env::var(name)
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(default)
        }
        Self {
            declared: var("MAVERICK_LATENCY_DECLARED", 500),
            prompt: var("MAVERICK_LATENCY_PROMPT", 100),
            proxy: var("MAVERICK_LATENCY_PROXY", 200),
            warmup: var("MAVERICK_LATENCY_WARMUP", 30),
            concurrency: var("MAVERICK_LATENCY_CONCURRENCY", 4_usize).max(1),
            classifier_ms: var("MAVERICK_LATENCY_CLASSIFIER_MS", 600),
            assert_targets: std::env::var("MAVERICK_LATENCY_ASSERT").as_deref() == Ok("1"),
        }
    }
}

fn raw(value: &Value) -> Box<JsonRawValue> {
    JsonRawValue::from_string(value.to_string()).expect("json")
}

fn spec(model: &str, prompt: &str, output: &str, latency_ms: u64) -> FixtureSpec {
    serde_json::from_value(json!({
        "model": model, "prompt": prompt, "output": output,
        "tokens_in": 420, "tokens_out": 40, "latency_ms": latency_ms,
    }))
    .expect("fixture")
}

/// The prompt of uncached request `i` (warmups use their own range).
fn prompt(i: usize) -> String {
    format!("latency probe {i}: summarize the attached contract clause in one sentence.")
}

/// Every fixture: one classifier answer per distinct prompt (the classifier latency), the
/// over-deadline prompt, and the proxy's chat request for each OpenAI model.
fn fixtures(knobs: &Knobs) -> Vec<FixtureSpec> {
    let mut specs = Vec::new();
    for i in 0..knobs.prompt + knobs.warmup {
        let capability = CAPABILITIES[i % CAPABILITIES.len()];
        let classification = json!({"capability": capability, "difficulty": 1 + i % 5,
                                    "domain_tags": [], "confidence": 0.9});
        specs.push(spec(
            CLASSIFIER_MODEL,
            &fence(&prompt(i)),
            &classification.to_string(),
            knobs.classifier_ms,
        ));
    }
    specs.push(spec(
        CLASSIFIER_MODEL,
        &fence(SLOW_PROMPT),
        r#"{"capability":"factual_qa","difficulty":2,"domain_tags":[],"confidence":0.9}"#,
        2_500,
    ));
    // One per OpenAI model: a model on two endpoints shares its fixture.
    let mut openai: Vec<&str> = CATALOG
        .iter()
        .filter(|(provider, ..)| *provider == "openai")
        .map(|(_, model, ..)| *model)
        .collect();
    openai.sort_unstable();
    openai.dedup();
    for model in openai {
        specs.push(spec(model, PROXY_PROMPT, PROXY_OUTPUT, 0));
    }
    specs
}

/// The catalog, an active corpus, and a week of aggregates for every cell.
async fn seed(pool: &PgPool) -> Vec<String> {
    corpus::upsert_corpus_version(pool, CORPUS, "latency harness", 0, false)
        .await
        .expect("corpus");
    corpus::activate_corpus_version(pool, CORPUS)
        .await
        .expect("activate");
    let settings = raw(&json!({}));
    let mut ids = Vec::new();
    for (provider, model, endpoint, input, output) in CATALOG {
        let id = maverick_domain::ids::new(maverick_domain::ids::prefix::MODEL_COLUMN);
        let pricing = raw(&json!({"input_per_mtok": input, "output_per_mtok": output,
                                  "currency": "USD", "as_of": "2026-09-01", "source": "list"}));
        catalog::create_model_column(
            pool,
            &NewModelColumn {
                id: &id,
                provider,
                model_id: model,
                endpoint,
                settings: &settings,
                status: "active",
                pricing: &pricing,
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
        ids.push(id);
    }
    // 12 columns × 10 capabilities × 5 tiers × 5 dimensions × 7 nights, in one statement.
    let dimensions = [
        dimension::QUALITY,
        dimension::PASS_RATE,
        dimension::COST_PER_RUN_USD,
        dimension::LATENCY_P95_MS,
        dimension::AVAILABILITY,
    ]
    .map(str::to_owned);
    let inserted = sqlx::query(
        "insert into aggregates (model_column_id, capability, difficulty_tier, dimension,
                                 ewma, variance, n, measured, matrix_version, as_of)
         select c.id, cap.name, t.tier, d.dim,
                (case d.dim
                   when 'quality' then 0.55 + ((c.ord * 7 + t.tier * 3 + cap.ord) % 10) * 0.04
                   when 'pass_rate' then 0.50 + ((c.ord * 5 + t.tier + cap.ord * 3) % 10) * 0.05
                   when 'cost_per_run_usd' then 0.0004 * c.ord * t.tier
                   when 'latency_p95_ms' then 600 + 150 * c.ord + 40 * t.tier
                   else 0.97 + (c.ord % 3) * 0.01
                 end)::real,
                0.001, 5 + night.n, true, $3, current_date - night.n::int
         from unnest($1::text[]) with ordinality as c(id, ord)
         cross join unnest($2::text[]) with ordinality as cap(name, ord)
         cross join generate_series(1, 5) as t(tier)
         cross join unnest($4::text[]) as d(dim)
         cross join generate_series(0, 6) as night(n)",
    )
    .bind(&ids)
    .bind(CAPABILITIES.map(str::to_owned).to_vec())
    .bind(CORPUS)
    .bind(dimensions.to_vec())
    .execute(pool)
    .await
    .expect("aggregates")
    .rows_affected();
    assert_eq!(inserted, 12 * 10 * 5 * 5 * 7);
    ids
}

/// One timed request.
struct Sample {
    /// Until the response headers arrived.
    ttfb: Duration,
    /// Until the whole body was read.
    total: Duration,
    status: u16,
    body: String,
}

/// `n` requests from `concurrency` sequential clients; request `i` comes from `make(i)`.
async fn load<F>(n: usize, concurrency: usize, make: F) -> Vec<Sample>
where
    F: Fn(usize) -> reqwest::RequestBuilder + Send + Sync + 'static,
{
    let make = Arc::new(make);
    let mut workers = Vec::new();
    for worker in 0..concurrency.min(n.max(1)) {
        let make = Arc::clone(&make);
        workers.push(tokio::spawn(async move {
            let mut samples = Vec::new();
            for i in (worker..n).step_by(concurrency) {
                let started = Instant::now();
                let response = make(i).send().await.expect("request");
                let ttfb = started.elapsed();
                let status = response.status().as_u16();
                let body = response.text().await.expect("body");
                samples.push(Sample {
                    ttfb,
                    total: started.elapsed(),
                    status,
                    body,
                });
            }
            samples
        }));
    }
    let mut samples = Vec::with_capacity(n);
    for worker in workers {
        samples.extend(worker.await.expect("worker"));
    }
    samples
}

/// p50, p95, p99, and max of a set of durations (nearest rank), in milliseconds.
#[derive(Debug, Clone, Copy)]
struct Stats {
    n: usize,
    p50: f64,
    p95: f64,
    p99: f64,
    max: f64,
}

impl Stats {
    fn of(durations: impl IntoIterator<Item = Duration>) -> Self {
        let mut ms: Vec<f64> = durations
            .into_iter()
            .map(|d| d.as_secs_f64() * 1000.0)
            .collect();
        ms.sort_by(f64::total_cmp);
        let rank = |p: f64| {
            let index = ((p * ms.len() as f64).ceil() as usize).clamp(1, ms.len()) - 1;
            ms[index]
        };
        Self {
            n: ms.len(),
            p50: rank(0.50),
            p95: rank(0.95),
            p99: rank(0.99),
            max: *ms.last().expect("samples"),
        }
    }

    fn json(self) -> Value {
        json!({"n": self.n, "p50_ms": round(self.p50), "p95_ms": round(self.p95),
               "p99_ms": round(self.p99), "max_ms": round(self.max)})
    }
}

fn ms_or_unknown(ms: Option<f64>) -> String {
    ms.map_or("?".to_owned(), |ms| format!("{ms:.2}"))
}

fn round(ms: f64) -> f64 {
    (ms * 100.0).round() / 100.0
}

fn engine_env(db: &str, fake: &str, public: u16, internal: u16) -> Vec<(String, String)> {
    [
        ("MAVERICK_ROLE", "origin".to_owned()),
        ("MAVERICK_DATABASE_URL", db.to_owned()),
        ("MAVERICK_INTERNAL_TOKENS", INTERNAL_TOKEN.to_owned()),
        ("MAVERICK_CAPTURE_MASTER_KEY", "c4".repeat(32)),
        ("MAVERICK_PUBLIC_ADDR", format!("127.0.0.1:{public}")),
        ("MAVERICK_INTERNAL_ADDR", format!("127.0.0.1:{internal}")),
        ("MAVERICK_RATELIMIT_RPM", "1000000".to_owned()),
        // D4's budget would turn a long uncached run into fallbacks.
        ("MAVERICK_CLASSIFIER_DAILY_QUOTA", "1000000".to_owned()),
        ("MAVERICK_CLASSIFIER_DAILY_SPEND_CAP_USD", "1000".to_owned()),
        (
            "MAVERICK_ANTHROPIC_API_KEY",
            "fake-anthropic-key".to_owned(),
        ),
        ("MAVERICK_OPENAI_API_KEY", "fake-openai-key".to_owned()),
        ("MAVERICK_GOOGLE_API_KEY", "fake-google-key".to_owned()),
        ("MAVERICK_XAI_API_KEY", "fake-xai-key".to_owned()),
        ("MAVERICK_ANTHROPIC_BASE_URL", fake.to_owned()),
        ("MAVERICK_OPENAI_BASE_URL", format!("{fake}/openai")),
        ("MAVERICK_GOOGLE_BASE_URL", fake.to_owned()),
        ("MAVERICK_XAI_BASE_URL", format!("{fake}/xai")),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value))
    .collect()
}

fn proxy_env(engine: &str, fake: &str, port: u16, tenant_key: &str) -> Vec<(String, String)> {
    let closed = "http://127.0.0.1:1";
    [
        ("MAVERICK_API_KEY", tenant_key),
        ("MAVERICK_ENGINE_URL", engine),
        ("MAVERICK_PROXY_ADDR", &format!("127.0.0.1:{port}")),
        ("MAVERICK_PROXY_TELEMETRY", "on"),
        ("MAVERICK_PROXY_RATELIMIT_RPM", "1000000"),
        ("MAVERICK_UPSTREAM_OPENAI", fake),
        ("MAVERICK_UPSTREAM_ANTHROPIC", closed),
        ("MAVERICK_UPSTREAM_GOOGLE", closed),
        ("MAVERICK_UPSTREAM_XAI", closed),
        ("MAVERICK_UPSTREAM_OPENROUTER", closed),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .collect()
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(64)
        .build()
        .expect("http client")
}

/// The declared body of request `i`: every capability, tier, and mode in turn.
fn declared_body(i: usize) -> String {
    let modes = ["balanced", "quality", "cost"];
    json!({"workload": {"capability": CAPABILITIES[i % CAPABILITIES.len()],
                        "difficulty": 1 + (i / CAPABILITIES.len()) % 5},
           "mode": modes[i % modes.len()]})
    .to_string()
}

fn all_ok(samples: &[Sample], what: &str) {
    for sample in samples {
        assert_eq!(sample.status, 200, "{what}: {}", sample.body);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "latency harness: needs the built binaries and Docker; run scripts/latency.sh"]
async fn hot_path_latency() {
    let knobs = Knobs::from_env();
    let mut steps = Steps::new();
    let artifacts = Artifacts::create_named(&workspace(), "latency").expect("artifact dir");
    let report = FailureReport::new(&artifacts);
    eprintln!("latency artifacts: {}", artifacts.dir().display());
    let (engine_bin, proxy_bin) = binaries();
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    eprintln!("binaries: {} ({profile} test build)", engine_bin.display());

    steps.begin("Postgres 16 (testcontainers), empty database; maverick migrate");
    let Some(db) = maverick_testdb::database(&EMPTY).await else {
        eprintln!("SKIP: Docker is not available (set MAVERICK_TEST_REQUIRE_DB=1 to fail)");
        return;
    };
    let db_url = db.url().to_owned();
    let pool = db.pool().await;
    let fake = Arc::new(
        FakeProviderServer::start(FakeProvider::from_fixtures(&fixtures(&knobs)).expect("fake"))
            .await
            .expect("fake provider"),
    );
    {
        let fake = Arc::clone(&fake);
        report.note(move || format!("fake provider misses: {:#?}", fake.misses()));
    }
    let (public, internal) = (free_port(), free_port());
    let env = engine_env(&db_url, fake.url(), public, internal);
    let mut migrate = Process::spawn(&artifacts, "migrate", &engine_bin, &["migrate"], &env)
        .expect("spawn migrate");
    let status = migrate.wait(Duration::from_secs(120)).await.expect("exit");
    assert!(status.success(), "migrate: {status}\n{}", migrate.log());

    steps.begin("Seed a realistic catalog: 12 columns, a week of aggregates per cell");
    let columns = seed(&pool).await;
    steps.note(format!(
        "{} columns, {} aggregate rows",
        columns.len(),
        12 * 10 * 5 * 5 * 7
    ));

    steps.begin("maverick serve --api-only; a tenant whose default pool is the catalog");
    let mut api = Process::spawn(
        &artifacts,
        "api",
        &engine_bin,
        &["serve", "--api-only"],
        &env,
    )
    .expect("spawn api");
    let public_url = format!("http://127.0.0.1:{public}");
    let internal_url = format!("http://127.0.0.1:{internal}");
    wait_ready(&format!("{public_url}/up"), Duration::from_secs(60))
        .await
        .expect("api /up");
    let ops = format!("Bearer {INTERNAL_TOKEN}");
    let created = http(
        &steps,
        "POST",
        &format!("{internal_url}/internal/v1/tenants"),
        &[("authorization", ops.as_str())],
        Some(&json!({"name": "latency-tenant"})),
    )
    .await;
    expect(&created, 201, "tenant");
    let tenant = created.json();
    assert_eq!(
        tenant["pools"][0]["model_column_ids"]
            .as_array()
            .map(Vec::len),
        Some(12),
        "the default pool holds every active column"
    );
    let tenant_id = tenant["tenant"]["id"].as_str().unwrap().to_owned();
    let key = http(
        &steps,
        "POST",
        &format!("{internal_url}/internal/v1/tenants/{tenant_id}/api-keys"),
        &[("authorization", ops.as_str())],
        Some(&json!({"name": "latency"})),
    )
    .await;
    expect(&key, 201, "api key");
    let bearer = format!("Bearer {}", key.json()["key"].as_str().unwrap());

    let http_client = client();
    let recommend_url = format!("{public_url}/v1/recommend");
    let declared = |bearer: String, url: String, client: reqwest::Client, offset: usize| {
        move |i: usize| {
            client
                .post(&url)
                .header("authorization", &bearer)
                .header("content-type", "application/json")
                .body(declared_body(i + offset))
        }
    };

    steps.begin(&format!(
        "Warm up: {} declared and {} prompt-form requests",
        knobs.warmup, knobs.warmup
    ));
    let warm = load(
        knobs.warmup,
        1,
        declared(
            bearer.clone(),
            recommend_url.clone(),
            http_client.clone(),
            7,
        ),
    )
    .await;
    all_ok(&warm, "declared warmup");
    let warm = {
        let (client, url, bearer) = (http_client.clone(), recommend_url.clone(), bearer.clone());
        let base = knobs.prompt;
        load(knobs.warmup, 1, move |i| {
            client
                .post(&url)
                .header("authorization", &bearer)
                .header("content-type", "application/json")
                .body(json!({"workload": {"prompt": prompt(base + i)}}).to_string())
        })
        .await
    };
    all_ok(&warm, "prompt warmup");

    // What this host costs before the engine does anything: the report reads the engine's
    // numbers against these.
    steps.begin(&format!(
        "Environment floor, 1 client: Postgres select 1, insert + commit, API /up ({FLOOR} each)"
    ));
    let mut select_one = Vec::with_capacity(FLOOR);
    for _ in 0..FLOOR {
        let started = Instant::now();
        sqlx::query("select 1")
            .execute(&pool)
            .await
            .expect("select 1");
        select_one.push(started.elapsed());
    }
    sqlx::query("create table latency_floor (id bigserial primary key, note text not null)")
        .execute(&pool)
        .await
        .expect("floor table");
    let mut commits = Vec::with_capacity(FLOOR);
    for i in 0..FLOOR {
        let started = Instant::now();
        sqlx::query("insert into latency_floor (note) values ($1)")
            .bind(format!("floor {i}"))
            .execute(&pool)
            .await
            .expect("insert");
        commits.push(started.elapsed());
    }
    let cpu_start = api.cpu_seconds();
    let up = {
        let (client, url) = (http_client.clone(), format!("{public_url}/up"));
        load(FLOOR, 1, move |_| client.get(&url)).await
    };
    all_ok(&up, "/up");
    let cpu_up = api.cpu_seconds();
    // An authenticated read: the public middleware (auth, rate limit, request log) plus a
    // few queries, to tell middleware cost from recommend cost.
    let models = {
        let (client, url, bearer) = (
            http_client.clone(),
            format!("{public_url}/v1/models"),
            bearer.clone(),
        );
        load(FLOOR, 1, move |_| {
            client.get(&url).header("authorization", &bearer)
        })
        .await
    };
    all_ok(&models, "/v1/models");
    let cpu_models = api.cpu_seconds();
    #[allow(clippy::cast_precision_loss)]
    let per = |before: Option<f64>, after: Option<f64>| {
        before
            .zip(after)
            .map(|(before, after)| (after - before) * 1000.0 / FLOOR as f64)
    };
    let (up_cpu_ms, models_cpu_ms) = (per(cpu_start, cpu_up), per(cpu_up, cpu_models));
    let models_stats = Stats::of(models.iter().map(|s| s.total));
    steps.note(format!(
        "api CPU per request: /up {up_cpu_ms:?} ms, /v1/models {models_cpu_ms:?} ms"
    ));
    let select_stats = Stats::of(select_one);
    let commit_stats = Stats::of(commits);
    let up_stats = Stats::of(up.iter().map(|s| s.total));
    steps.note(format!("select 1: {select_stats:?}"));
    steps.note(format!("insert + commit: {commit_stats:?}"));
    steps.note(format!("/up: {up_stats:?}"));

    let serial_n = knobs.declared.min(FLOOR);
    steps.begin(&format!("Declared path, 1 client: {serial_n} requests"));
    let transactions = async || -> i64 {
        // Postgres publishes these counters with a short delay.
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        sqlx::query_scalar(
            "select xact_commit + xact_rollback from pg_stat_database
             where datname = current_database()",
        )
        .fetch_one(&pool)
        .await
        .expect("pg_stat_database")
    };
    let (cpu_before, xact_before) = (api.cpu_seconds(), transactions().await);
    let serial = load(
        serial_n,
        1,
        declared(
            bearer.clone(),
            recommend_url.clone(),
            http_client.clone(),
            3,
        ),
    )
    .await;
    all_ok(&serial, "declared, 1 client");
    let serial_stats = Stats::of(serial.iter().map(|s| s.total));
    let (cpu_after, xact_after) = (api.cpu_seconds(), transactions().await);
    // The harness's own two counter reads are transactions too.
    #[allow(clippy::cast_precision_loss)]
    let xact_per_request = (xact_after - xact_before - 1) as f64 / serial_n as f64;
    #[allow(clippy::cast_precision_loss)]
    let cpu_ms_per_request = cpu_before
        .zip(cpu_after)
        .map(|(before, after)| (after - before) * 1000.0 / serial_n as f64);
    steps.note(format!("{serial_stats:?}"));
    steps.note(format!(
        "per request: api CPU {} ms, {xact_per_request:.1} database transactions",
        cpu_ms_per_request.map_or("?".to_owned(), |ms| format!("{ms:.2}"))
    ));

    steps.begin(&format!(
        "Declared path: {} requests, {} clients",
        knobs.declared, knobs.concurrency
    ));
    let samples = load(
        knobs.declared,
        knobs.concurrency,
        declared(
            bearer.clone(),
            recommend_url.clone(),
            http_client.clone(),
            0,
        ),
    )
    .await;
    all_ok(&samples, "declared");
    for sample in &samples {
        let body: Value = serde_json::from_str(&sample.body).expect("json");
        assert_eq!(body["classification"]["method"], "declared");
        assert_eq!(
            body["recommendations"].as_array().map(Vec::len),
            Some(12),
            "the chain is the whole pool"
        );
    }
    let declared_stats = Stats::of(samples.iter().map(|s| s.total));
    steps.note(format!("{declared_stats:?}"));

    steps.begin(&format!(
        "Uncached prompt path: {} distinct prompts, classifier at {} ms, {} clients",
        knobs.prompt, knobs.classifier_ms, knobs.concurrency
    ));
    let samples = {
        let (client, url, bearer) = (http_client.clone(), recommend_url.clone(), bearer.clone());
        load(knobs.prompt, knobs.concurrency, move |i| {
            client
                .post(&url)
                .header("authorization", &bearer)
                .header("content-type", "application/json")
                .body(json!({"workload": {"prompt": prompt(i)}}).to_string())
        })
        .await
    };
    all_ok(&samples, "prompt");
    for sample in &samples {
        let body: Value = serde_json::from_str(&sample.body).expect("json");
        assert_eq!(
            body["classification"]["method"], "classified",
            "every prompt is new, so none is cached: {body}"
        );
    }
    let prompt_stats = Stats::of(samples.iter().map(|s| s.total));
    steps.note(format!("{prompt_stats:?}"));

    steps.begin("Classifier over the 1.5 s deadline (D3): falls back in time");
    let started = Instant::now();
    let slow = http(
        &steps,
        "POST",
        &recommend_url,
        &[("authorization", bearer.as_str())],
        Some(&json!({"workload": {"prompt": SLOW_PROMPT}})),
    )
    .await;
    let slow_ms = started.elapsed().as_secs_f64() * 1000.0;
    expect(&slow, 200, "over-deadline prompt");
    assert_eq!(slow.json()["classification"]["method"], "fallback_default");
    steps.note(format!(
        "fallback_default after {slow_ms:.0} ms (fake classifier sleeps 2500 ms)"
    ));

    steps.begin(&format!(
        "Proxy added time to first byte: {} chat requests through maverick-proxy vs direct",
        knobs.proxy
    ));
    let port = free_port();
    let mut proxy = Process::spawn(
        &artifacts,
        "proxy",
        &proxy_bin,
        &[],
        &proxy_env(
            &public_url,
            fake.url(),
            port,
            bearer.trim_start_matches("Bearer "),
        ),
    )
    .expect("spawn proxy");
    let proxy_url = format!("http://127.0.0.1:{port}");
    wait_ready(&format!("{proxy_url}/up"), Duration::from_secs(30))
        .await
        .expect("proxy /up");
    let chat = |url: String, client: reqwest::Client, model: &'static str, proxied: bool| {
        move |_: usize| {
            let mut request = client
                .post(&url)
                .header("authorization", "Bearer sk-latency-caller")
                .header("content-type", "application/json")
                .body(
                    json!({"model": model,
                           "messages": [{"role": "user", "content": PROXY_PROMPT}]})
                    .to_string(),
                );
            if proxied {
                request = request.header("x-maverick-workload", "code_generation:3");
            }
            request
        }
    };
    let proxied_url = format!("{proxy_url}/v1/chat/completions");
    let direct_url = format!("{}/v1/chat/completions", fake.url());
    all_ok(
        &load(
            knobs.warmup,
            1,
            chat(
                proxied_url.clone(),
                http_client.clone(),
                "maverick/auto",
                true,
            ),
        )
        .await,
        "proxy warmup",
    );
    let proxied = load(
        knobs.proxy,
        knobs.concurrency,
        chat(proxied_url, http_client.clone(), "maverick/auto", true),
    )
    .await;
    all_ok(&proxied, "proxied chat");
    for sample in &proxied {
        let body: Value = serde_json::from_str(&sample.body).expect("json");
        assert_eq!(body["choices"][0]["message"]["content"], PROXY_OUTPUT);
    }
    let direct = load(
        knobs.proxy,
        knobs.concurrency,
        chat(direct_url, http_client.clone(), "gpt-5.5", false),
    )
    .await;
    all_ok(&direct, "direct chat");
    let proxy_stats = Stats::of(proxied.iter().map(|s| s.ttfb));
    let direct_stats = Stats::of(direct.iter().map(|s| s.ttfb));

    steps.begin("Report");
    let proxy_added = Stats {
        n: proxy_stats.n,
        p50: proxy_stats.p50 - direct_stats.p50,
        p95: proxy_stats.p95 - direct_stats.p95,
        p99: proxy_stats.p99 - direct_stats.p99,
        max: f64::NAN,
    };
    let rows = [
        (
            "postgres select 1 (floor)",
            Some(select_stats),
            String::new(),
        ),
        (
            "postgres insert + commit (floor)",
            Some(commit_stats),
            String::new(),
        ),
        (
            "api GET /up (floor)",
            Some(up_stats),
            format!("api CPU {} ms per request", ms_or_unknown(up_cpu_ms)),
        ),
        (
            "api GET /v1/models (auth + reads)",
            Some(models_stats),
            format!("api CPU {} ms per request", ms_or_unknown(models_cpu_ms)),
        ),
        (
            "declared recommend, 1 client",
            Some(serial_stats),
            format!(
                "p95 < 50 ms (api CPU {} ms, {xact_per_request:.1} db transactions per request)",
                ms_or_unknown(cpu_ms_per_request)
            ),
        ),
        (
            "declared recommend, concurrent",
            Some(declared_stats),
            "p95 < 50 ms".to_owned(),
        ),
        (
            "uncached prompt recommend",
            Some(prompt_stats),
            format!("p95 < 1800 ms (classifier {} ms)", knobs.classifier_ms),
        ),
        (
            "classifier over deadline",
            None,
            format!("{slow_ms:.2} ms; fallback within the 1500 ms deadline"),
        ),
        ("proxy chat TTFB", Some(proxy_stats), String::new()),
        (
            "upstream chat TTFB (direct)",
            Some(direct_stats),
            String::new(),
        ),
        (
            "proxy added TTFB (difference)",
            Some(proxy_added),
            "reported (no fixed number)".to_owned(),
        ),
    ];
    let cell = |value: f64| {
        if value.is_nan() {
            "-".to_owned()
        } else {
            format!("{value:.2}")
        }
    };
    let mut table = format!(
        "{:<34} {:>5} {:>9} {:>9} {:>9} {:>9}  target\n",
        "path (ms)", "n", "p50", "p95", "p99", "max"
    );
    for (name, stats, target) in &rows {
        let line = match stats {
            Some(s) => format!(
                "{name:<34} {:>5} {:>9} {:>9} {:>9} {:>9}  {target}\n",
                s.n,
                cell(s.p50),
                cell(s.p95),
                cell(s.p99),
                cell(s.max)
            ),
            None => format!(
                "{name:<34} {:>5} {:>9} {:>9} {:>9} {:>9}  {target}\n",
                1, "", "", "", ""
            ),
        };
        table.push_str(&line);
    }
    eprintln!("\n{table}");
    let report_json = json!({
        "build": profile,
        "knobs": {"declared": knobs.declared, "prompt": knobs.prompt, "proxy": knobs.proxy,
                  "warmup": knobs.warmup, "concurrency": knobs.concurrency,
                  "classifier_ms": knobs.classifier_ms},
        "catalog": {"columns": 12, "capabilities": 10, "tiers": 5, "dimensions": 5, "nights": 7},
        "floor": {"postgres_select_1": select_stats.json(),
                  "postgres_insert_commit": commit_stats.json(),
                  "api_up": up_stats.json(),
                  "api_up_cpu_ms": up_cpu_ms.map(round),
                  "api_models": models_stats.json(),
                  "api_models_cpu_ms": models_cpu_ms.map(round)},
        "declared_recommend_one_client": serial_stats.json(),
        "declared_per_request": {"api_cpu_ms": cpu_ms_per_request.map(round),
                                 "db_transactions": round(xact_per_request)},
        "declared_recommend": declared_stats.json(),
        "uncached_prompt_recommend": prompt_stats.json(),
        "classifier_over_deadline_ms": round(slow_ms),
        "proxy_chat_ttfb": proxy_stats.json(),
        "upstream_chat_ttfb": direct_stats.json(),
        "proxy_added_ttfb_ms": {
            "p50": round(proxy_added.p50),
            "p95": round(proxy_added.p95),
            "p99": round(proxy_added.p99),
        },
    });
    let path = artifacts
        .write(
            "report.json",
            &serde_json::to_string_pretty(&report_json).expect("json"),
        )
        .expect("report");
    artifacts.write("report.txt", &table).expect("table");
    steps.note(format!("report: {}", path.display()));
    assert!(
        fake.misses().is_empty(),
        "every provider call had a fixture"
    );

    if knobs.assert_targets {
        assert!(
            serial_stats.p95 < 50.0 && declared_stats.p95 < 50.0,
            "declared p95 {:.2} ms (1 client) / {:.2} ms (concurrent) over the 50 ms target",
            serial_stats.p95,
            declared_stats.p95
        );
        assert!(
            prompt_stats.p95 < 1800.0,
            "uncached prompt p95 {:.2} ms over the 1.8 s target",
            prompt_stats.p95
        );
        assert!(
            slow_ms < 1500.0 + 250.0,
            "the over-deadline call took {slow_ms:.0} ms; the classifier deadline is 1.5 s"
        );
    } else {
        eprintln!("(targets not asserted: set MAVERICK_LATENCY_ASSERT=1 with a release build)");
    }

    for process in [&mut proxy, &mut api] {
        let status = process
            .terminate(Duration::from_secs(30))
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        assert!(status.success(), "{}: {status}", process.name());
    }
    drop(report);
    pool.close().await;
    db.close().await;
}
