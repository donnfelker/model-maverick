//! Bead 8.3 (proxy path): run the frozen conformance scenarios against the real proxy.
//!
//! Each scenario scripts a mock engine and a mock provider (the [`Harness`]). This runner
//! points a real [`Proxy`] (maverick-proxy on maverick-core) at that harness, replays the
//! scenario steps as chat requests, and asserts two things: the caller responses match the
//! scenario `expect` blocks, and the engine, provider, and telemetry traffic the harness
//! recorded matches the scenario `recorded` block. Master plan §7.2 N5: the proxy runs on
//! `maverick-core` and must pass the same conformance scenarios.
//!
//! # Classification: core, proxy, or both
//!
//! This runner covers the scenarios the proxy serves faithfully: direct execution
//! (self-hosted mode) that ends in a relay. The other groups need the SDK core's own
//! behavior, which the proxy does not have by position, and run in a separate core-direct
//! runner (bead model-maverick-5fs):
//!
//! - Group 5: chain exhaustion relays the last upstream error unchanged. The proxy answers
//!   `502 no_executable_candidates` instead (reference part 6 §4), so this is core only.
//! - Group 6: the fail-open ladder (last-known-good, default model, original model). The
//!   proxy is exempt from fail-open today (G14): it maps a failed recommend to `502
//!   engine_error`. Core only. `plans/production-proxy-plan.md` §4
//!   changes this.
//! - Groups 10 to 12: OpenRouter execution (D15) needs an `sk-or-` caller key, which the
//!   proxy recognizes only in hosted mode. Hosted mode reads the tenant key from
//!   `X-Maverick-API-Key`, which must start with `mk_live_`, but these scenarios expect the
//!   tenant key `mk_test_conformance`. Core only.
//! - S09a (`concrete_model_passes_through`): the SDK core sends a concrete model straight to
//!   the provider without contacting MM. The proxy routes only `maverick/*` aliases and
//!   answers `400 invalid_model_alias` for a concrete model, so this one scenario in group 9
//!   is core only.
//!
//! The covered groups are 1, 2, 3, 4, 7, 8, and 9: declared and projected workloads, the
//! advance rules (429 with Retry-After, timeouts, connection refused), settings and
//! normalization on the relayed attempt, telemetry batching and opt-out, and the zero
//! retention request shape.

mod common;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{Loaded, assert_recorded, config_ms, load_scenarios, refused_url, upstream_for};
use maverick_core::chain::ChainExecutor;
use maverick_core::telemetry::{BatcherConfig, HttpSink, Telemetry};
use maverick_proxy::chain::ChainRunner;
use maverick_proxy::config::load_config;
use maverick_proxy::reports::ProxyReports;
use maverick_proxy::server::Proxy;
use maverick_proxy::surface::{ExecutorRunner, MAX_IDLE_PER_HOST, ReportSink, Upstreams};
use sdk_conformance::{Harness, Scenario};
use serde_json::{Value, json};
use tokio::net::TcpListener;

/// The tenant key every scenario uses unless it overrides `config.api_key`.
const DEFAULT_TENANT_KEY: &str = "mk_test_conformance";

/// The caller's provider key for direct execution: an OpenAI-shaped key on the chat surface.
const CALLER_KEY: &str = "sk-caller-openai";

/// The groups this proxy runner serves. The rest are core only (see the module docs).
const PROXY_GROUPS: [u64; 7] = [1, 2, 3, 4, 7, 8, 9];

/// Scenarios inside a covered group that are still core only. S09a is a concrete-model
/// passthrough, which the proxy answers with `400 invalid_model_alias` by design.
const CORE_ONLY: [&str; 1] = ["S09a"];

/// Run one scenario through the proxy. Returns the first assertion failure, if any.
async fn run_scenario(loaded: &Loaded) -> Result<(), String> {
    let doc = &loaded.doc;
    let config = doc.get("config").cloned().unwrap_or_else(|| json!({}));

    let harness = Harness::start()
        .await
        .map_err(|e| format!("harness: {e}"))?;
    let scenario = Scenario::from_json(&loaded.bytes).map_err(|e| format!("scenario: {e}"))?;
    harness.load(scenario);
    let engine_url = harness.engine_base_url();
    let provider_url = harness.provider_base_url();
    let refused = refused_url();

    let api_key = config
        .get("api_key")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_TENANT_KEY)
        .to_owned();
    let telemetry_off = config.get("telemetry").and_then(Value::as_str) == Some("off");

    let lookup = {
        let engine_url = engine_url.clone();
        let provider_url = provider_url.clone();
        let refused = refused.clone();
        let config = config.clone();
        let api_key = api_key.clone();
        move |name: &str| -> Option<String> {
            match name {
                "MAVERICK_PROXY_MODE" => Some("self-hosted".to_owned()),
                "MAVERICK_API_KEY" => Some(api_key.clone()),
                "MAVERICK_ENGINE_URL" => Some(engine_url.clone()),
                "MAVERICK_UPSTREAM_OPENAI" => {
                    Some(upstream_for(&config, "openai", &provider_url, &refused))
                }
                "MAVERICK_UPSTREAM_ANTHROPIC" => {
                    Some(upstream_for(&config, "anthropic", &provider_url, &refused))
                }
                "MAVERICK_UPSTREAM_GOOGLE" => {
                    Some(upstream_for(&config, "google", &provider_url, &refused))
                }
                "MAVERICK_UPSTREAM_XAI" => {
                    Some(upstream_for(&config, "xai", &provider_url, &refused))
                }
                "MAVERICK_UPSTREAM_OPENROUTER" => {
                    Some(upstream_for(&config, "openrouter", &provider_url, &refused))
                }
                // A self-hosted proxy configures every provider (D12 fallback keys). The
                // chat surface's native provider (openai) uses the caller key, so these are
                // the keys for cross-provider entries, such as S03c's xai attempt. Every
                // upstream points at the mock, so a key never reaches a real provider.
                "ANTHROPIC_API_KEY" => Some("sk-env-anthropic".to_owned()),
                "GEMINI_API_KEY" => Some("sk-env-google".to_owned()),
                "XAI_API_KEY" => Some("sk-env-xai".to_owned()),
                "MAVERICK_PROXY_TELEMETRY" => telemetry_off.then(|| "off".to_owned()),
                _ => None,
            }
        }
    };
    let proxy_config = load_config(lookup).map_err(|e| format!("config: {e}"))?;

    // Telemetry: build the batcher with the scenario's timing so the flush and shutdown
    // steps behave, and keep a handle to flush it before the recorded assertions.
    let telemetry = if proxy_config.telemetry {
        let interval = config_ms(&config, "telemetry_flush_interval_ms")
            .map(Duration::from_millis)
            .unwrap_or(BatcherConfig::default().interval);
        let max_batch = config
            .get("telemetry_max_batch")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(BatcherConfig::default().max_batch);
        let sink =
            Arc::new(HttpSink::new(&proxy_config.engine_url).map_err(|e| format!("sink: {e}"))?);
        Telemetry::start(
            sink,
            BatcherConfig {
                interval,
                max_batch,
                shutdown_flush: BatcherConfig::default().shutdown_flush,
            },
        )
    } else {
        Telemetry::off()
    };
    let reports = ProxyReports::new(telemetry);

    // The chain runner: the real executor, with the scenario's per-attempt header timeout.
    let http = reqwest::Client::builder()
        .no_proxy()
        .pool_max_idle_per_host(MAX_IDLE_PER_HOST)
        .build()
        .map_err(|e| format!("http: {e}"))?;
    let executor = match config_ms(&config, "attempt_timeout_ms") {
        Some(ms) => ChainExecutor::new(http).with_header_timeout(Duration::from_millis(ms)),
        None => ChainExecutor::new(http),
    };
    let sink: Arc<dyn ReportSink> = Arc::new(reports.clone());
    let runner =
        ExecutorRunner::with_executor(executor, Upstreams::from_config(&proxy_config), sink);
    let proxy = Proxy::new(proxy_config, Arc::new(runner) as Arc<dyn ChainRunner>)
        .map_err(|e| format!("proxy: {e}"))?;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("proxy bind: {e}"))?;
    let addr = listener
        .local_addr()
        .map_err(|e| format!("proxy addr: {e}"))?;
    let app = proxy.router();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let client = reqwest::Client::new();
    let url = format!("http://{addr}/v1/chat/completions");
    let mut shut_down = false;
    let result = run_steps(
        doc,
        &config,
        &client,
        &url,
        &harness,
        &reports,
        &mut shut_down,
    )
    .await;

    // Flush any queued telemetry, then check the whole recorded picture.
    if !shut_down {
        reports.shutdown().await;
    }
    let result = result.and_then(|()| {
        assert_recorded(&harness.recorded(), &doc["recorded"]).map_err(|e| format!("recorded: {e}"))
    });

    server.abort();
    result
}

#[allow(clippy::too_many_arguments)]
async fn run_steps(
    doc: &Value,
    config: &Value,
    client: &reqwest::Client,
    url: &str,
    harness: &Harness,
    reports: &ProxyReports,
    shut_down: &mut bool,
) -> Result<(), String> {
    let steps = doc["steps"].as_array().expect("steps");
    for (i, step) in steps.iter().enumerate() {
        let where_ = format!("step {i}");
        if let Some(call) = step.get("call") {
            run_call(config, client, url, call, &step["expect"], &where_).await?;
        } else if let Some(wait) = step.get("wait_ms").and_then(Value::as_u64) {
            tokio::time::sleep(Duration::from_millis(wait)).await;
        } else if let Some(check) = step.get("check_recorded") {
            assert_recorded(&harness.recorded(), check).map_err(|e| format!("{where_}: {e}"))?;
        } else if step.get("shutdown").is_some() {
            reports.shutdown().await;
            *shut_down = true;
        } else {
            return Err(format!("{where_}: unknown step {step}"));
        }
    }
    Ok(())
}

async fn run_call(
    config: &Value,
    client: &reqwest::Client,
    url: &str,
    call: &Value,
    expect: &Value,
    where_: &str,
) -> Result<(), String> {
    // The runner sets the body's `model` from `call.model`; the frozen body carries none.
    let mut body = call["body"].clone();
    let model = call["model"].as_str().ok_or("call.model")?;
    body["model"] = json!(model);
    let caller_key = if config.get("execution").and_then(Value::as_str) == Some("openrouter") {
        "sk-or-caller"
    } else {
        CALLER_KEY
    };

    let mut request = client
        .post(url)
        .header("authorization", format!("Bearer {caller_key}"))
        .header("content-type", "application/json");
    if let Some(settings) = call.get("settings").and_then(Value::as_str) {
        request = request.header("x-maverick-settings", settings);
    }
    if let Some(workload) = call.get("workload").and_then(Value::as_str) {
        request = request.header("x-maverick-workload", workload);
    }
    let payload = serde_json::to_vec(&body).map_err(|e| format!("{where_}: body: {e}"))?;

    let started = Instant::now();
    let response = request
        .body(payload)
        .send()
        .await
        .map_err(|e| format!("{where_}: send: {e}"))?;
    let status = response.status().as_u16();
    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("{where_}: read: {e}"))?;
    let elapsed = started.elapsed();

    match expect["outcome"].as_str() {
        Some("response") => {
            let want = expect["status"].as_u64().unwrap_or(200) as u16;
            if status != want {
                return Err(format!(
                    "{where_}: status {status}, want {want}; body {}",
                    String::from_utf8_lossy(&bytes)
                ));
            }
            if let Some(expected_body) = expect.get("body") {
                let got: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                if &got != expected_body {
                    return Err(format!("{where_}: body {got} != {expected_body}"));
                }
            }
            check_elapsed(expect, elapsed, where_)?;
        }
        Some("error") => {
            if let Some(want) = expect.get("status").and_then(Value::as_u64) {
                if u64::from(status) != want {
                    return Err(format!("{where_}: error status {status}, want {want}"));
                }
            }
            if let Some(code) = expect.get("code").and_then(Value::as_str) {
                let got: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                let got_code = got.pointer("/error/code").and_then(Value::as_str);
                if got_code != Some(code) {
                    return Err(format!("{where_}: error code {got_code:?}, want {code}"));
                }
            }
        }
        other => return Err(format!("{where_}: unknown outcome {other:?}")),
    }
    Ok(())
}

fn check_elapsed(expect: &Value, elapsed: Duration, where_: &str) -> Result<(), String> {
    let ms = elapsed.as_millis() as u64;
    if let Some(min) = expect.get("min_elapsed_ms").and_then(Value::as_u64) {
        if ms < min {
            return Err(format!("{where_}: elapsed {ms}ms < min {min}ms"));
        }
    }
    if let Some(max) = expect.get("max_elapsed_ms").and_then(Value::as_u64) {
        if ms > max {
            return Err(format!("{where_}: elapsed {ms}ms > max {max}ms"));
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_conformance_scenarios() {
    let scenarios = load_scenarios();
    assert!(!scenarios.is_empty(), "no scenarios found");

    let covered: BTreeSet<u64> = PROXY_GROUPS.into_iter().collect();
    let mut ran = 0;
    let mut failures = Vec::new();
    let mut skipped = Vec::new();

    for loaded in &scenarios {
        if !covered.contains(&loaded.group) || CORE_ONLY.contains(&loaded.id.as_str()) {
            skipped.push((loaded.id.clone(), loaded.group));
            continue;
        }
        match run_scenario(loaded).await {
            Ok(()) => {
                ran += 1;
                println!("PASS {} (group {})", loaded.id, loaded.group);
            }
            Err(error) => {
                println!("FAIL {} (group {}): {error}", loaded.id, loaded.group);
                failures.push((loaded.id.clone(), error));
            }
        }
    }

    println!(
        "proxy conformance: {ran} passed, {} skipped (core only)",
        skipped.len()
    );
    for (id, group) in &skipped {
        println!("  SKIP {id} (group {group}): core-direct behavior, bead model-maverick-5fs");
    }

    assert!(
        failures.is_empty(),
        "{} scenario(s) failed:\n{}",
        failures.len(),
        failures
            .iter()
            .map(|(id, error)| format!("  {id}: {error}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(ran > 0, "no proxy scenarios ran");
}
