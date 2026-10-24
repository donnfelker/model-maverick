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
//! - On an MM outage it runs the fail-open ladder (G14, group 6): the last-known-good chain
//!   for the workload, then the configured default model, then the caller's own model, with
//!   the zero-retention rules that override G14. The proxy is exempt from fail-open by
//!   position (an engine failure is its `502 engine_error`).
//! - On chain exhaustion it relays the last upstream error unchanged (group 5), where the
//!   proxy answers `502 no_executable_candidates`.
//! - It runs OpenRouter execution (D15) from an `sk-or-` caller key without the proxy's
//!   hosted-mode gating, so the tenant key stays `mk_test_conformance` (groups 10 to 12).
//! - A concrete model name passes through untouched, with no recommend call (S09a).
//!
//! This covers every core-only scenario: groups 5, 6, 10, 11, and 12, and S09a.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{Loaded, assert_recorded, config_ms, load_scenarios, refused_url, upstream_for};
use maverick_core::alias::{AliasError, ParsedModel, parse_alias};
use maverick_core::chain::{ChainExecutor, ChainResult, LastFailure, PreparedAttempt};
use maverick_core::failopen::{FailOpen, FailOpenConfig, LadderError, Plan, Privacy};
use maverick_core::openrouter::{self, Eligibility, Verdict};
use maverick_core::recommend::{
    ChainEntry, RecommendClient, RecommendClientConfig, RecommendError, RecommendRequest,
    TenantKey, TraceHeaders, Workload,
};
use maverick_core::settings::SettingsMode;
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

/// The groups this runner serves: the fail-open ladder (6), exhaustion relay (5), and
/// OpenRouter (10 to 12).
const CORE_GROUPS: [u64; 5] = [5, 6, 10, 11, 12];

/// Scenarios in other groups that are core only and covered here. S09a is a concrete-model
/// passthrough (group 9).
const CORE_EXTRA: [&str; 1] = ["S09a"];

/// Scenarios in a covered group that are still deferred. Empty: the runner covers every
/// core-only scenario, including S06j (the zero-retention workload-form switch).
const CORE_DEFER: [&str; 0] = [];

/// Per-scenario wiring shared by every step.
struct Ctx {
    tenant: String,
    caller_key: &'static str,
    openrouter_mode: bool,
    privacy: Privacy,
    upstreams: Upstreams,
    env_keys: EnvKeys,
    executor: ChainExecutor,
    recommend: RecommendClient,
    failopen: FailOpen,
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

    let privacy = match config.get("privacy").and_then(Value::as_str) {
        Some("off") => Privacy::Off,
        Some("declared") => Privacy::Declared,
        // The SDK default sends the prompt for classification unless told otherwise.
        _ => Privacy::Prompt,
    };
    let failopen = FailOpen::new(FailOpenConfig {
        default_model: config
            .get("default_model")
            .and_then(Value::as_str)
            .map(str::to_owned),
        zero_retention_verified: config
            .get("zero_retention_verified")
            .and_then(Value::as_array)
            .map(|models| {
                models
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
    });

    let ctx = Ctx {
        tenant,
        caller_key,
        openrouter_mode,
        privacy,
        upstreams: Upstreams::from_config(&proxy_config),
        env_keys: EnvKeys::from_config(&proxy_config),
        executor,
        recommend,
        failopen,
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
        Ok(ParsedModel::Route(alias)) => alias,
        // A concrete model is not routed: the body reaches the provider untouched (S09a).
        Ok(ParsedModel::Direct { .. }) => return passthrough(ctx, &body, expect, where_).await,
        Err(AliasError::MalformedAlias) => {
            return Err(format!("{where_}: malformed alias {model}"));
        }
    };

    let settings_mode = SettingsMode::from_header(call.get("settings").and_then(Value::as_str))
        .map_err(|e| format!("{where_}: {e}"))?;

    // A declared header is always honored. Otherwise the prompt is projected only when
    // privacy allows it and the tenant is not known zero retention; a zero-retention tenant
    // sends the declared fallback so nothing about the prompt leaves the process (S06j).
    let workload_header = call.get("workload").and_then(Value::as_str);
    let workload = if workload_header.is_some() {
        Workload::for_chat_request(&body, workload_header)
            .map_err(|e| format!("{where_}: workload: {e}"))?
    } else if ctx.privacy == Privacy::Prompt && !ctx.failopen.is_zero_retention(&ctx.tenant) {
        Workload::for_chat_request(&body, None).map_err(|e| format!("{where_}: workload: {e}"))?
    } else {
        Workload::declared_fallback()
    };
    let request = RecommendRequest::for_alias(&alias, workload);
    let tenant_key = TenantKey::new(ctx.tenant.clone());
    let trace = TraceHeaders {
        traceparent: None,
        tracestate: None,
    };

    // The fail-open ladder (G14): the last-known-good chain for this workload, then the
    // default model, then the caller's own concrete model. The cache key is stable per
    // workload, so a later outage reuses the earlier chain.
    let workload_key = call
        .get("workload")
        .and_then(Value::as_str)
        .unwrap_or("prompt");
    let original_model = call.get("original_model").and_then(Value::as_str);
    let plan = ctx
        .failopen
        .plan(
            &ctx.tenant,
            workload_key,
            ctx.privacy,
            original_model,
            || {
                ctx.recommend
                    .recommend_traced(&tenant_key, &request, &trace)
            },
        )
        .await;

    match plan {
        Ok(Plan::Recommended(recommendation)) => {
            run_chain(
                ctx,
                settings_mode,
                &body,
                &recommendation.chain,
                recommendation.zero_retention,
                &recommendation.recommendation_id,
                recommendation.telemetry_opt_out,
                expect,
                where_,
            )
            .await
        }
        Ok(Plan::LastKnownGood(chain)) => {
            let zero_retention = ctx.failopen.is_zero_retention(&ctx.tenant);
            run_chain(
                ctx,
                settings_mode,
                &body,
                &chain,
                zero_retention,
                "",
                false,
                expect,
                where_,
            )
            .await
        }
        Ok(Plan::DefaultModel(model) | Plan::OriginalModel(model)) => {
            let chain = [synth_entry(&model)];
            run_chain(
                ctx,
                settings_mode,
                &body,
                &chain,
                false,
                "",
                false,
                expect,
                where_,
            )
            .await
        }
        Err(LadderError::Terminal(RecommendError::ZeroRetentionRequiresDeclared { .. })) => {
            assert_error(expect, "zero_retention_requires_declared", where_)
        }
        Err(LadderError::Terminal(RecommendError::NoCompliantColumns { .. })) => {
            assert_error(expect, "no_compliant_columns", where_)
        }
        Err(LadderError::NoCompliantColumns) => {
            assert_error(expect, "no_compliant_columns", where_)
        }
        Err(LadderError::Terminal(other)) => {
            Err(format!("{where_}: terminal recommend error {other}"))
        }
        Err(LadderError::Unavailable(failure)) => Err(format!(
            "{where_}: maverick unavailable and no fallback: {failure}"
        )),
        Err(LadderError::NoModel) => Err(format!("{where_}: no model to call")),
    }
}

/// A single direct chain entry for a fall-open model: the native chat provider, that model,
/// no settings or constraints.
fn synth_entry(model: &str) -> ChainEntry {
    ChainEntry {
        provider: "openai".to_owned(),
        model: model.to_owned(),
        ..ChainEntry::default()
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_chain(
    ctx: &Ctx,
    settings_mode: SettingsMode,
    body: &[u8],
    chain: &[ChainEntry],
    zero_retention: bool,
    recommendation_id: &str,
    telemetry_opt_out: bool,
    expect: &Value,
    where_: &str,
) -> Result<(), String> {
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
        let settings = match settings_mode {
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
        .map(|record| record.telemetry_item(recommendation_id))
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

    // Privacy off never contacts MM, telemetry included (S06d). Otherwise a tenant that has
    // not opted out (D39) reports every dispatched attempt.
    if ctx.privacy != Privacy::Off && !telemetry_opt_out {
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
    let mut deferred = Vec::new();
    for loaded in &scenarios {
        let covered =
            CORE_GROUPS.contains(&loaded.group) || CORE_EXTRA.contains(&loaded.id.as_str());
        if !covered {
            continue;
        }
        if CORE_DEFER.contains(&loaded.id.as_str()) {
            deferred.push(loaded.id.clone());
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
    println!(
        "core conformance: {ran} passed, {} deferred",
        deferred.len()
    );
    for id in &deferred {
        println!("  DEFER {id}: workload-form switch (next phase)");
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
    assert!(ran > 0, "no core scenarios ran");
}
