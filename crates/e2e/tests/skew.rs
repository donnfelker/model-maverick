//! DIVERGENCE: D50. The skew matrix (spec §14.5; self-hosted plan §7.6 and §7.7; bead
//! xq7v.22): one engine release and one proxy release, as separate processes on Postgres,
//! against the fake provider. `scripts/skew.sh` runs it for every row of
//! `crates/e2e/skew-releases.txt` twice: that release's engine with the current proxy (and
//! so the current SDK core inside it), and the current engine with that release's proxy.
//!
//! The scenario uses only what every supported release serves, so it fails only on a skew
//! bug. The engine decodes strictly (spec §9.2), so a proxy that sends a field the engine
//! did not advertise in `X-Maverick-API-Features` gets `422` and the request fails. A
//! direct probe checks the other half: the engine accepts `affinity` exactly when it
//! advertises it.
//!
//! `MAVERICK_SKEW_ENGINE` and `MAVERICK_SKEW_PROXY` name the binaries (default: the
//! current build, as in `scripts/e2e.sh`), and `MAVERICK_SKEW_LABEL` names the pair.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use maverick_e2e::FakeProviderServer;
use maverick_e2e::harness::{Steps, binaries, count, eventually, expect, http, workspace};
use maverick_e2e::process::{Artifacts, FailureReport, Process, free_port, wait_ready};
use maverick_providers::fake::{FakeProvider, FixtureSpec};
use maverick_testdb::Schema;
use serde_json::{Value, json};

const MODEL: &str = "claude-skew";
const INTERNAL_TOKEN: &str = "ops:skew-internal-secret";
const PROMPT: &str = "skew: say ok";
const OUTPUT: &str = "ok from the skew run";
const WORKLOAD: (&str, &str) = ("x-maverick-workload", "structured_extraction:2");
const CONVERSATION: (&str, &str) = ("x-maverick-conversation", "conv-skew-1");

/// A binary named by `var`, else the current build's.
fn binary(var: &str, current: PathBuf) -> PathBuf {
    std::env::var_os(var).map_or(current, PathBuf::from)
}

fn engine_env(db: &str, fake: &str, public: u16, internal: u16) -> Vec<(String, String)> {
    [
        ("MAVERICK_ROLE", "origin".to_owned()),
        ("MAVERICK_DATABASE_URL", db.to_owned()),
        ("MAVERICK_INTERNAL_TOKENS", INTERNAL_TOKEN.to_owned()),
        ("MAVERICK_CAPTURE_MASTER_KEY", "5e".repeat(32)),
        ("MAVERICK_PUBLIC_ADDR", format!("127.0.0.1:{public}")),
        ("MAVERICK_INTERNAL_ADDR", format!("127.0.0.1:{internal}")),
        (
            "MAVERICK_ANTHROPIC_API_KEY",
            "fake-anthropic-key".to_owned(),
        ),
        ("MAVERICK_ANTHROPIC_BASE_URL", fake.to_owned()),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value))
    .collect()
}

fn proxy_env(engine: &str, fake: &str, port: u16, api_key: &str) -> Vec<(String, String)> {
    let closed = "http://127.0.0.1:1";
    [
        ("MAVERICK_ENGINE_URL", engine),
        ("MAVERICK_API_KEY", api_key),
        ("MAVERICK_PROXY_ADDR", &format!("127.0.0.1:{port}")),
        ("MAVERICK_PROXY_TELEMETRY", "on"),
        ("MAVERICK_UPSTREAM_ANTHROPIC", fake),
        ("MAVERICK_UPSTREAM_OPENAI", closed),
        ("MAVERICK_UPSTREAM_GOOGLE", closed),
        ("MAVERICK_UPSTREAM_XAI", closed),
        ("MAVERICK_UPSTREAM_OPENROUTER", closed),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .collect()
}

fn fixtures() -> Vec<FixtureSpec> {
    vec![
        serde_json::from_value(json!({
            "model": MODEL, "prompt": PROMPT, "output": OUTPUT,
            "tokens_in": 40, "tokens_out": 6,
        }))
        .expect("fixture"),
    ]
}

/// The engine's `/v1/recommend` request lines, as `(status, line)`.
fn recommend_statuses(log: &str) -> Vec<u16> {
    log.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| line["msg"] == "request" && line["path"] == "/v1/recommend")
        .filter_map(|line| line["status"].as_u64())
        .filter_map(|status| u16::try_from(status).ok())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "skew matrix: needs the built binaries and Docker; run scripts/skew.sh"]
async fn an_engine_and_a_proxy_from_different_releases_work_together() {
    let label = std::env::var("MAVERICK_SKEW_LABEL").unwrap_or_else(|_| "current".to_owned());
    let mut steps = Steps::new();
    let artifacts = Artifacts::create_named(&workspace(), "skew").expect("artifact dir");
    let report = FailureReport::new(&artifacts);
    let (current_engine, current_proxy) = binaries();
    let engine_bin = binary("MAVERICK_SKEW_ENGINE", current_engine);
    let proxy_bin = binary("MAVERICK_SKEW_PROXY", current_proxy);
    eprintln!(
        "skew {label}: engine {}, proxy {}; artifacts {}",
        engine_bin.display(),
        proxy_bin.display(),
        artifacts.dir().display()
    );

    steps.begin("Postgres, the fake provider, and `maverick migrate`");
    let Some(db) = maverick_testdb::database(&Schema::empty()).await else {
        eprintln!("SKIP: Docker is not available (set MAVERICK_TEST_REQUIRE_DB=1 to fail)");
        return;
    };
    let pool = db.pool().await;
    let fake = Arc::new(
        FakeProviderServer::start(FakeProvider::from_fixtures(&fixtures()).expect("fixtures"))
            .await
            .expect("fake provider"),
    );
    {
        let fake = Arc::clone(&fake);
        report.note(move || format!("fake provider misses: {:#?}", fake.misses()));
    }
    let (public, internal) = (free_port(), free_port());
    let env = engine_env(db.url(), fake.url(), public, internal);
    let mut migrate = Process::spawn(&artifacts, "migrate", &engine_bin, &["migrate"], &env)
        .expect("spawn migrate");
    let status = migrate
        .wait(Duration::from_secs(120))
        .await
        .expect("migrate exits");
    assert!(status.success(), "migrate: {status}\n{}", migrate.log());

    steps.begin("maverick serve");
    let engine =
        Process::spawn(&artifacts, "engine", &engine_bin, &["serve"], &env).expect("spawn serve");
    let public_url = format!("http://127.0.0.1:{public}");
    let internal_url = format!("http://127.0.0.1:{internal}");
    wait_ready(&format!("{public_url}/up"), Duration::from_secs(60))
        .await
        .expect("engine /up");
    let up = http(&steps, "GET", &format!("{public_url}/up"), &[], None).await;
    let features: Vec<String> = up
        .header("x-maverick-api-features")
        .unwrap_or_default()
        .split(',')
        .map(|name| name.trim_matches([' ', '\t']).to_owned())
        .filter(|name| !name.is_empty())
        .collect();
    let affinity_served = features.iter().any(|name| name == "affinity");
    steps.note(format!(
        "engine {} advertises {features:?}",
        up.header("x-maverick-engine-version")
            .unwrap_or("(no version)")
    ));

    steps.begin("Internal seam: one column, one tenant, one key, the default pool");
    let ops = format!("Bearer {INTERNAL_TOKEN}");
    let ops_headers = [("authorization", ops.as_str())];
    let column = http(
        &steps,
        "POST",
        &format!("{internal_url}/internal/v1/columns"),
        &ops_headers,
        Some(
            &json!({"provider": "anthropic", "model_id": MODEL, "endpoint": "direct",
                     "pricing": {"input_per_mtok": 3.0, "output_per_mtok": 15.0,
                                 "currency": "USD", "as_of": "2026-07-01", "source": "list"}}),
        ),
    )
    .await;
    expect(&column, 201, "column");
    let column_id = column.json()["column_id"].as_str().unwrap().to_owned();
    let tenant = http(
        &steps,
        "POST",
        &format!("{internal_url}/internal/v1/tenants"),
        &ops_headers,
        Some(&json!({"name": "skew-tenant"})),
    )
    .await;
    expect(&tenant, 201, "tenant");
    let tenant_id = tenant.json()["tenant"]["id"].as_str().unwrap().to_owned();
    let key = http(
        &steps,
        "POST",
        &format!("{internal_url}/internal/v1/tenants/{tenant_id}/api-keys"),
        &ops_headers,
        Some(&json!({"name": "skew"})),
    )
    .await;
    expect(&key, 201, "api key");
    let api_key = key.json()["key"].as_str().unwrap().to_owned();
    let pools = http(
        &steps,
        "PUT",
        &format!("{internal_url}/internal/v1/tenants/{tenant_id}/pools"),
        &ops_headers,
        Some(&json!({"pools": [{"name": "default", "is_default": true,
                                "model_column_ids": [&column_id]}]})),
    )
    .await;
    expect(&pools, 200, "pools");

    steps.begin("maverick-proxy (self-hosted): messages, with and without a conversation");
    let port = free_port();
    let mut proxy = Process::spawn(
        &artifacts,
        "proxy",
        &proxy_bin,
        &[],
        &proxy_env(&public_url, fake.url(), port, &api_key),
    )
    .expect("spawn proxy");
    let proxy_url = format!("http://127.0.0.1:{port}");
    wait_ready(&format!("{proxy_url}/up"), Duration::from_secs(30))
        .await
        .expect("proxy /up");
    let body = json!({"model": "maverick/auto", "max_tokens": 16,
                      "messages": [{"role": "user", "content": PROMPT}]});
    let mut last_affinity = None;
    // The first request carries no optional field (the proxy has seen no engine response
    // yet); the later ones carry what the engine advertised.
    for (turn, conversation) in [(1, true), (2, false), (3, true), (4, true)] {
        let mut headers = vec![("x-api-key", "sk-ant-skew-caller"), WORKLOAD];
        if conversation {
            headers.push(CONVERSATION);
        }
        let reply = http(
            &steps,
            "POST",
            &format!("{proxy_url}/v1/messages"),
            &headers,
            Some(&body),
        )
        .await;
        expect(&reply, 200, &format!("turn {turn}"));
        assert_eq!(reply.json()["content"][0]["text"], OUTPUT, "turn {turn}");
        assert_eq!(
            reply.header("x-maverick-model-used"),
            Some(MODEL),
            "turn {turn}"
        );
        // The engine answered: the proxy did not fall open (D46).
        assert_eq!(reply.header("x-maverick-fallback"), None, "turn {turn}");
        assert!(
            reply.header("x-maverick-recommendation-id").is_some(),
            "turn {turn}"
        );
        last_affinity = reply.header("x-maverick-affinity").map(str::to_owned);
    }
    // After the first response, the proxy sends affinity exactly when the engine offers it.
    assert_eq!(
        last_affinity.is_some(),
        affinity_served,
        "x-maverick-affinity {last_affinity:?} with features {features:?}"
    );
    let status = proxy
        .terminate(Duration::from_secs(30))
        .await
        .expect("proxy stops");
    assert!(status.success(), "proxy: {status}");

    steps.begin("Telemetry from the proxy arrived as outcome rows");
    let arrived = eventually(Duration::from_secs(30), || {
        let pool = pool.clone();
        async move { count(&pool, "select count(*) from outcomes").await >= 4 }
    })
    .await;
    assert!(arrived, "telemetry: {}", engine.log());

    steps.begin("The engine never rejected the proxy's recommend request");
    let statuses = recommend_statuses(&engine.log());
    assert_eq!(statuses.len(), 4, "{statuses:?}");
    assert!(statuses.iter().all(|status| *status == 200), "{statuses:?}");

    steps.begin("The engine accepts `affinity` exactly when it advertises it");
    let bearer = format!("Bearer {api_key}");
    let probe = http(
        &steps,
        "POST",
        &format!("{public_url}/v1/recommend"),
        &[("authorization", bearer.as_str())],
        Some(
            &json!({"affinity": {"key": "conv-skew-probe"}, "mode": "quality",
                     "workload": {"capability": "structured_extraction", "difficulty": 2}}),
        ),
    )
    .await;
    if affinity_served {
        expect(&probe, 200, "affinity on an engine that advertises it");
    } else {
        expect(
            &probe,
            422,
            "affinity on an engine that does not advertise it",
        );
        assert_eq!(probe.json()["error"]["code"], "invalid_request");
    }
    steps.note(format!("skew {label}: passed in {:?}", steps.elapsed()));
    db.close().await;
}
