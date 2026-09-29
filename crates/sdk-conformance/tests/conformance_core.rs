//! Bead model-maverick-5fs (core-direct path): run the frozen conformance scenarios against
//! maverick-core directly.
//!
//! The proxy runner (`conformance.rs`) covers the groups the proxy serves faithfully. This
//! runner covers the groups the proxy is exempt from by position, by driving maverick-core's
//! own pieces the way an SDK core does: `recommend::RecommendClient` for the chain,
//! `chain::ChainExecutor` for the walk, and `telemetry::Telemetry` for the batcher. It reuses
//! maverick-proxy's `rewrite_attempt`, `resolve_key`, `attempt_headers`, `Upstreams`, and the
//! shared `openrouter` rules for the per-attempt request, and it makes the SDK's own
//! decisions that the proxy does not:
//!
//! - On chain exhaustion it relays the last upstream error unchanged (group 5), where the
//!   proxy answers `502 no_executable_candidates`.
//! - It runs OpenRouter execution (D15) from an `sk-or-` caller key without the proxy's
//!   hosted-mode gating, so the tenant key stays `mk_test_conformance` (groups 10 to 12).
//! - A concrete model name passes through untouched, with no recommend call (S09a).
//!
//! This phase covers groups 5, 10, 11, and 12 and S09a. The fail-open ladder (group 6) is
//! the next phase on this bead.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{Loaded, assert_recorded, config_ms, load_scenarios, refused_url, upstream_for};
use maverick_core::alias::{Alias, AliasError, SettingsMode, parse_alias};
use maverick_core::chain::{ChainExecutor, ChainResult, LastFailure, PreparedAttempt};
use maverick_core::openrouter::{self, Eligibility, Verdict};
use maverick_core::recommend::{
    ChainEntry, RecommendClient, RecommendClientConfig, RecommendError, RecommendRequest,
    Recommendation, TenantKey, TraceHeaders, Workload,
};
use maverick_core::telemetry::{
    BatcherConfig, HttpSink, Telemetry, TelemetryItem, UsageShape, parse_usage,
};
use maverick_proxy::chain::Surface;
use maverick_proxy::config::load_config;
use maverick_proxy::credentials::{CallerCredential, EnvKeys, attempt_headers, resolve_key};
use maverick_proxy::rewrite::{AttemptParams, rewrite_attempt, wire_format};
use maverick_proxy::surface::Upstreams;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
use sdk_conformance::{Harness, Scenario};
use serde_json::{Value, json};

/// The tenant key every scenario uses unless it overrides `config.api_key`.
const DEFAULT_TENANT_KEY: &str = "mk_test_conformance";

/// The groups this phase serves. Group 6 (fail-open) is the next phase.
const CORE_GROUPS: [u64; 4] = [5, 10, 11, 12];

/// Scenarios in other groups that are core only and covered here. S09a is a concrete-model
/// passthrough (group 9).
const CORE_EXTRA: [&str; 1] = ["S09a"];

/// Per-scenario wiring shared by every step.
struct Ctx {
    tenant: String,
    caller_key: &'static str,
    openrouter_mode: bool,
    upstreams: Upstreams,
    env_keys: EnvKeys,
    executor: ChainExecutor,
    recommend: RecommendClient,
    telemetry: Telemetry,
    http: reqwest::Client,
}

/// The base upstream headers for a synthesized SDK request: JSON content type only.
fn base_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers
}

async fn run_scenario_core(loaded: &Loaded) -> Result<(), String> {
    let doc = &loaded.doc;
    let config = doc.get("config").cloned().unwrap_or_else(|| json!({}));

    let harness = Harness::start()
        .await
        .map_err(|e| format!("harness: {e}"))?;
    harness.load(Scenario::from_json(&loaded.bytes).map_err(|e| format!("scenario: {e}"))?);
    let engine_url = harness.engine_base_url();
    let provider_url = harness.provider_base_url();
    let refused = refused_url();

    let tenant = config
        .get("api_key")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_TENANT_KEY)
        .to_owned();
    let openrouter_mode = config.get("execution").and_then(Value::as_str) == Some("openrouter");
    let caller_key = if openrouter_mode {
        "sk-or-caller"
    } else {
        "sk-caller-openai"
    };
    let telemetry_off = config.get("telemetry").and_then(Value::as_str) == Some("off");

    // Build a proxy Config so the upstream table and the provider env keys come from the same
    // machinery the proxy uses. Every upstream points at the mock (a refused address where the
    // scenario marks one). The provider env keys let cross-provider entries be attempted; every
    // upstream is a mock, so no key reaches a real provider.
    let lookup = {
        let (engine_url, provider_url, refused, config, tenant) = (
            engine_url.clone(),
            provider_url.clone(),
            refused.clone(),
            config.clone(),
            tenant.clone(),
        );
        move |name: &str| -> Option<String> {
            match name {
                "MAVERICK_PROXY_MODE" => Some("self-hosted".to_owned()),
                "MAVERICK_API_KEY" => Some(tenant.clone()),
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
                "ANTHROPIC_API_KEY" => Some("sk-env-anthropic".to_owned()),
                "GEMINI_API_KEY" => Some("sk-env-google".to_owned()),
                "XAI_API_KEY" => Some("sk-env-xai".to_owned()),
                _ => None,
            }
        }
    };
    let proxy_config = load_config(lookup).map_err(|e| format!("config: {e}"))?;

    let telemetry = if telemetry_off {
        Telemetry::off()
    } else {
        let interval = config_ms(&config, "telemetry_flush_interval_ms")
            .map(Duration::from_millis)
            .unwrap_or(BatcherConfig::default().interval);
        let max_batch = config
            .get("telemetry_max_batch")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(BatcherConfig::default().max_batch);
        let sink = Arc::new(HttpSink::new(&engine_url).map_err(|e| format!("sink: {e}"))?);
        Telemetry::start(
            sink,
            BatcherConfig {
                interval,
                max_batch,
                shutdown_flush: BatcherConfig::default().shutdown_flush,
            },
        )
    };

    let http = reqwest::Client::builder()
        .no_proxy()
        .pool_max_idle_per_host(2)
        .build()
        .map_err(|e| format!("http: {e}"))?;
    let executor = match config_ms(&config, "attempt_timeout_ms") {
        Some(ms) => ChainExecutor::new(http.clone()).with_header_timeout(Duration::from_millis(ms)),
        None => ChainExecutor::new(http.clone()),
    };
    let recommend = RecommendClient::new(RecommendClientConfig::new(engine_url.clone()))
        .map_err(|e| format!("recommend client: {e}"))?;

    let ctx = Ctx {
        tenant,
        caller_key,
        openrouter_mode,
        upstreams: Upstreams::from_config(&proxy_config),
        env_keys: EnvKeys::from_config(&proxy_config),
        executor,
        recommend,
        telemetry,
        http,
    };

    let steps = doc["steps"].as_array().expect("steps");
    let mut shut_down = false;
    for (i, step) in steps.iter().enumerate() {
        let where_ = format!("step {i}");
        if let Some(call) = step.get("call") {
            run_call(&ctx, call, &step["expect"], &where_).await?;
        } else if let Some(wait) = step.get("wait_ms").and_then(Value::as_u64) {
            tokio::time::sleep(Duration::from_millis(wait)).await;
        } else if let Some(check) = step.get("check_recorded") {
            assert_recorded(&harness.recorded(), check).map_err(|e| format!("{where_}: {e}"))?;
        } else if step.get("shutdown").is_some() {
            ctx.telemetry.shutdown().await;
            shut_down = true;
        } else {
            return Err(format!("{where_}: unknown step {step}"));
        }
    }
    if !shut_down {
        ctx.telemetry.shutdown().await;
    }
    assert_recorded(&harness.recorded(), &doc["recorded"]).map_err(|e| format!("recorded: {e}"))
}

async fn run_call(ctx: &Ctx, call: &Value, expect: &Value, where_: &str) -> Result<(), String> {
    let model = call["model"].as_str().ok_or("call.model")?;
    let mut body_value = call["body"].clone();
    body_value["model"] = json!(model);
    let body = serde_json::to_vec(&body_value).map_err(|e| format!("{where_}: body: {e}"))?;

    let alias = match parse_alias(model) {
        Ok(alias) => alias,
        // A concrete model is not routed: the body reaches the provider untouched (S09a).
        Err(AliasError::NotMaverickAlias) => return passthrough(ctx, &body, expect, where_).await,
        Err(AliasError::MalformedAlias) => {
            return Err(format!("{where_}: malformed alias {model}"));
        }
    };

    let workload_header = call.get("workload").and_then(Value::as_str);
    let workload = Workload::for_chat_request(&body, workload_header)
        .map_err(|e| format!("{where_}: workload: {e}"))?;
    let request = RecommendRequest::for_alias(&alias, workload);
    let tenant_key = TenantKey::new(ctx.tenant.clone());
    let trace = TraceHeaders {
        traceparent: None,
        tracestate: None,
    };

    // This phase has no fail-open, so a recommend failure other than a terminal privacy error
    // is a runner error, not a fall-open.
    let recommendation = match ctx
        .recommend
        .recommend_traced(&tenant_key, &request, &trace)
        .await
    {
        Ok(recommendation) => recommendation,
        Err(RecommendError::ZeroRetentionRequiresDeclared { .. }) => {
            return assert_error(expect, "zero_retention_requires_declared", where_);
        }
        Err(RecommendError::NoCompliantColumns { .. }) => {
            return assert_error(expect, "no_compliant_columns", where_);
        }
        Err(RecommendError::Engine(failure)) => {
            return Err(format!("{where_}: engine recommend failed: {failure}"));
        }
    };

    run_chain(ctx, &alias, &body, &recommendation, expect, where_).await
}

async fn run_chain(
    ctx: &Ctx,
    alias: &Alias,
    body: &[u8],
    recommendation: &Recommendation,
    expect: &Value,
    where_: &str,
) -> Result<(), String> {
    let zero_retention = recommendation.zero_retention;
    let recommendation_id = recommendation.recommendation_id.clone();
    let chain = &recommendation.chain;

    // D15: a zero-retention chain with no verified-zero alias is refused before any attempt.
    if ctx.openrouter_mode
        && openrouter::verdict(chain, zero_retention) == Verdict::NoCompliantColumns
    {
        return assert_error(expect, "no_compliant_columns", where_);
    }

    let format = wire_format(Surface::Chat);
    let credential = if ctx.openrouter_mode {
        CallerCredential::OpenRouter(ctx.caller_key.to_owned())
    } else {
        CallerCredential::Native(ctx.caller_key.to_owned())
    };
    let upstream_headers = base_headers();
    let prepare = |_index: usize, entry: &ChainEntry| -> Option<PreparedAttempt> {
        let (model, key, url, endpoint_used, zdr): (&str, &str, String, &str, bool) =
            if ctx.openrouter_mode {
                let slug = match openrouter::eligibility(entry, zero_retention) {
                    Eligibility::Slug(slug) => slug,
                    _ => return None,
                };
                (
                    slug,
                    ctx.caller_key,
                    ctx.upstreams.openrouter_url(""),
                    openrouter::ENDPOINT_USED,
                    zero_retention,
                )
            } else {
                let key = resolve_key(&entry.provider, Surface::Chat, &credential, &ctx.env_keys)?;
                let url = ctx.upstreams.url(Surface::Chat, &entry.provider, "")?;
                (entry.model.as_str(), key, url, "direct", false)
            };
        let settings = match alias.settings_mode {
            SettingsMode::Apply => Some(&entry.settings),
            SettingsMode::Caller => None,
        };
        let rewritten = rewrite_attempt(
            body,
            model,
            &AttemptParams {
                format,
                settings,
                constraints: &entry.param_constraints,
                zdr,
            },
        )
        .ok()?;
        let headers = attempt_headers(Surface::Chat, &upstream_headers, key)?;
        let url = reqwest::Url::parse(&url).ok()?;
        let mut request = reqwest::Request::new(reqwest::Method::POST, url);
        *request.headers_mut() = headers;
        *request.body_mut() = Some(rewritten.body.into());
        Some(PreparedAttempt {
            request,
            settings_applied: rewritten.settings_applied,
            endpoint_used: endpoint_used.to_owned(),
        })
    };

    let outcome = ctx
        .executor
        .execute(chain, prepare, None, std::future::pending())
        .await;
    let mut items: Vec<TelemetryItem> = outcome
        .attempts
        .iter()
        .map(|record| record.telemetry_item(&recommendation_id))
        .collect();

    let response = match outcome.result {
        ChainResult::Relayed { response, .. } => {
            let status = response.status();
            let bytes = response
                .bytes()
                .await
                .map_err(|e| format!("{where_}: relay read: {e}"))?
                .to_vec();
            if status.is_success() {
                let (tokens_in, tokens_out) = parse_usage(UsageShape::Chat, &bytes);
                if let Some(last) = items.last_mut() {
                    last.tokens_in = tokens_in;
                    last.tokens_out = tokens_out;
                }
            }
            Some((status.as_u16(), bytes))
        }
        // An SDK core surfaces the last provider error unchanged (S05).
        ChainResult::Exhausted { last_failure } => match last_failure {
            Some(LastFailure::Response(failed)) => Some((failed.status.as_u16(), failed.body)),
            Some(LastFailure::Transport(_)) => None,
            None => None,
        },
    };

    if !recommendation.telemetry_opt_out {
        ctx.telemetry.record(&ctx.tenant, items);
    }

    match response {
        Some((status, bytes)) => assert_response(expect, status, &bytes, where_),
        // Nothing settled and there was no last response: the chain had no runnable entry.
        None => assert_error(expect, "no_executable_candidates", where_),
    }
}

/// Send the inbound body untouched to the native provider (openai) with the caller key. No
/// recommend call and no telemetry: a concrete model is not routed (S09a).
async fn passthrough(ctx: &Ctx, body: &[u8], expect: &Value, where_: &str) -> Result<(), String> {
    let url = ctx
        .upstreams
        .url(Surface::Chat, "openai", "")
        .ok_or_else(|| format!("{where_}: no openai upstream"))?;
    let headers = attempt_headers(Surface::Chat, &base_headers(), ctx.caller_key)
        .ok_or_else(|| format!("{where_}: bad caller key"))?;
    let response = ctx
        .http
        .post(url)
        .headers(headers)
        .body(body.to_vec())
        .send()
        .await
        .map_err(|e| format!("{where_}: passthrough send: {e}"))?;
    let status = response.status().as_u16();
    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("{where_}: passthrough read: {e}"))?
        .to_vec();
    assert_response(expect, status, &bytes, where_)
}

fn assert_response(expect: &Value, status: u16, bytes: &[u8], where_: &str) -> Result<(), String> {
    match expect["outcome"].as_str() {
        Some("response") => {
            let want = expect["status"].as_u64().unwrap_or(200) as u16;
            if status != want {
                return Err(format!(
                    "{where_}: status {status}, want {want}; body {}",
                    String::from_utf8_lossy(bytes)
                ));
            }
            if let Some(expected_body) = expect.get("body") {
                let got: Value = serde_json::from_slice(bytes).unwrap_or(Value::Null);
                if &got != expected_body {
                    return Err(format!("{where_}: body {got} != {expected_body}"));
                }
            }
            Ok(())
        }
        Some("error") => Err(format!(
            "{where_}: got a {status} response, but the scenario expects error {}",
            expect.get("code").and_then(Value::as_str).unwrap_or("?")
        )),
        other => Err(format!("{where_}: unknown outcome {other:?}")),
    }
}

fn assert_error(expect: &Value, code: &str, where_: &str) -> Result<(), String> {
    match expect["outcome"].as_str() {
        Some("error") => {
            let want = expect.get("code").and_then(Value::as_str);
            if want == Some(code) {
                Ok(())
            } else {
                Err(format!("{where_}: error code {code}, want {want:?}"))
            }
        }
        Some("response") => Err(format!(
            "{where_}: got error {code}, but the scenario expects a response"
        )),
        other => Err(format!("{where_}: unknown outcome {other:?}")),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn core_conformance_scenarios() {
    let scenarios = load_scenarios();
    assert!(!scenarios.is_empty(), "no scenarios found");

    let mut ran = 0;
    let mut failures = Vec::new();
    for loaded in &scenarios {
        let covered =
            CORE_GROUPS.contains(&loaded.group) || CORE_EXTRA.contains(&loaded.id.as_str());
        if !covered {
            continue;
        }
        match run_scenario_core(loaded).await {
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
    println!("core conformance (phase 1): {ran} passed");

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
    assert!(ran > 0, "no core scenarios ran");
}
