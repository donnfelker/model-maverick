//! Acceptance for the production proxy (production-proxy plan §11; D45, D46): the real
//! engine and the real hosted proxy, with recording provider mocks. No real provider is
//! called.
//!
//! - D45: the model name picks the route and the variant, a named pool stays a hard
//!   boundary (G2), a direct model never reaches the engine, and `X-Maverick-Settings:
//!   caller` keeps the caller's own settings.
//! - D46: with the engine down, the proxy serves the route's last good list, then the
//!   surface's default model, then `503 engine_unavailable`; a zero-retention tenant falls
//!   open only to zero-retention models.

use maverick_acceptance::{
    CLAUDE, GEMINI, GPT, OPUS, Reply, RunningProxy, Stack, Tenant, Upstreams,
};
use maverick_store::measurement::{self, AggregateUpsert};
use maverick_store::tenancy;
use serde_json::{Value, json};

/// The caller's own Anthropic key: hosted mode runs Anthropic entries with it only.
const CALLER_KEY: &str = "sk-ant-acceptance-caller";

/// The declared workload the seeded measurements cover, so no classifier is called.
const WORKLOAD: &str = "structured_extraction:3";

fn body(model: &str) -> Value {
    json!({
        "model": model,
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "Reply with OK."}]
    })
}

async fn send(proxy: &RunningProxy, tenant: &Tenant, model: &str, extra: &[(&str, &str)]) -> Reply {
    let mut headers = vec![("x-api-key", CALLER_KEY), ("x-maverick-workload", WORKLOAD)];
    headers.extend_from_slice(extra);
    proxy
        .post("/v1/messages", tenant, &headers, &body(model))
        .await
}

/// The model the Anthropic mock received last.
fn last_model(upstreams: &Upstreams) -> Value {
    upstreams
        .anthropic
        .seen()
        .last()
        .expect("an Anthropic request")
        .json()["model"]
        .clone()
}

/// Costs that order the pool apart by mode. Quality: Opus (97) leads alone. Cost: GPT is
/// far the cheapest inside the 5.5-point band, then Sonnet, then Opus.
async fn priced(stack: &Stack) {
    for (column, cost) in [(OPUS, 0.10), (CLAUDE, 0.05), (GPT, 0.001), (GEMINI, 0.005)] {
        measurement::upsert_aggregate(
            &stack.pool,
            &AggregateUpsert {
                model_column_id: column,
                capability: "structured_extraction",
                difficulty_tier: 3,
                dimension: "cost_per_run_usd",
                ewma: cost,
                variance: 0.0,
                n: 20,
                measured: true,
                matrix_version: "2026-07-01",
                as_of: chrono::Utc::now().date_naive(),
            },
        )
        .await
        .expect("cost");
    }
}

/// D45: `maverick/auto` is the quality pick, `:cost` the cost pick, and a named pool never
/// widens. The Messages surface runs Anthropic entries only, so the cost route reaches
/// the first Anthropic entry after GPT.
#[tokio::test]
async fn the_model_name_picks_the_route_and_the_variant() {
    let Some(stack) = Stack::start(Default::default()).await else {
        return;
    };
    priced(&stack).await;
    let tenant = stack
        .tenant("routes", &[OPUS, CLAUDE, GPT, GEMINI], false, false)
        .await;
    let team: Vec<String> = [CLAUDE, GPT].iter().map(|id| (*id).to_owned()).collect();
    tenancy::create_pool(
        &stack.pool,
        "pool_routes_team",
        &tenant.id,
        "team/a",
        &team,
        false,
    )
    .await
    .expect("team pool");
    let upstreams = Upstreams::start("OK").await;
    let proxy = stack.proxy(&upstreams, &[]).await;
    for (model, want) in [
        ("maverick/auto", "claude-opus-4-6"),
        ("maverick/auto:quality", "claude-opus-4-6"),
        ("maverick/auto:cost", "claude-sonnet-4-6"),
        ("maverick/team/a:balanced", "claude-sonnet-4-6"),
        ("maverick/team/a", "claude-sonnet-4-6"),
    ] {
        let reply = send(&proxy, &tenant, model, &[]).await;
        assert_eq!(reply.status, 200, "{model}: {}", reply.body);
        assert_eq!(reply.header("x-maverick-model-used"), Some(want), "{model}");
        assert_eq!(last_model(&upstreams), json!(want), "{model}");
        assert_eq!(reply.header("x-maverick-fallback"), None, "{model}");
    }
    proxy.stop().await;
}

/// D45: a direct model goes to its provider with the native author prefix removed, and
/// it makes no engine call and sends no telemetry.
#[tokio::test]
async fn a_direct_model_never_reaches_the_engine() {
    let Some(stack) = Stack::start(Default::default()).await else {
        return;
    };
    let tenant = stack.tenant("direct", &[OPUS, CLAUDE], false, false).await;
    let upstreams = Upstreams::start("OK").await;
    let proxy = stack.proxy(&upstreams, &[]).await;
    let before = stack.hits().len();
    let reply = proxy
        .post(
            "/v1/messages",
            &tenant,
            &[("x-api-key", CALLER_KEY)],
            &body("anthropic/claude-opus-5-5"),
        )
        .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(last_model(&upstreams), json!("claude-opus-5-5"));
    assert_eq!(reply.header("x-maverick-recommendation-id"), None);
    // Stopping flushes telemetry: still nothing reached the engine.
    proxy.stop().await;
    assert_eq!(stack.hits().len(), before, "{:?}", stack.hits());
}

/// D14, D45: the recommended entry's settings are applied by default, and
/// `X-Maverick-Settings: caller` keeps the caller's own value. The header never reaches
/// the provider.
#[tokio::test]
async fn the_settings_header_keeps_the_callers_effort() {
    let Some(stack) = Stack::start(Default::default()).await else {
        return;
    };
    sqlx::query("update model_columns set settings = $1::jsonb where id = $2")
        .bind(r#"{"reasoning":"low"}"#)
        .bind(OPUS)
        .execute(&stack.pool)
        .await
        .expect("settings");
    let tenant = stack.tenant("settings", &[OPUS], false, false).await;
    let upstreams = Upstreams::start("OK").await;
    let proxy = stack.proxy(&upstreams, &[]).await;
    let mut with_effort = body("maverick/auto");
    with_effort["output_config"] = json!({"effort": "high"});
    let headers = |caller: bool| {
        let mut headers = vec![("x-api-key", CALLER_KEY), ("x-maverick-workload", WORKLOAD)];
        if caller {
            headers.push(("x-maverick-settings", "caller"));
        }
        headers
    };
    for (caller, effort) in [(false, "low"), (true, "high")] {
        let reply = proxy
            .post("/v1/messages", &tenant, &headers(caller), &with_effort)
            .await;
        assert_eq!(reply.status, 200, "{}", reply.body);
        let seen = upstreams.anthropic.seen().pop().expect("a request");
        assert_eq!(
            seen.json()["output_config"]["effort"],
            json!(effort),
            "{caller}"
        );
        assert_eq!(seen.header("x-maverick-settings"), None, "{caller}");
    }
    proxy.stop().await;
}

/// D46: with the engine down, the same agent keeps working from its last good list (with
/// the cached recommendation id); a cold route gets the configured default model; with
/// no default it gets `503`. Once the engine is back, routing resumes.
#[tokio::test]
async fn an_engine_outage_falls_open() {
    let Some(stack) = Stack::start(Default::default()).await else {
        return;
    };
    let tenant = stack
        .tenant("outage", &[OPUS, CLAUDE, GPT], false, false)
        .await;
    let upstreams = Upstreams::start("OK").await;
    let with_default = stack
        .proxy(
            &upstreams,
            &[("MAVERICK_PROXY_DEFAULT_MODEL_MESSAGES", "claude-haiku-4-5")],
        )
        .await;
    let bare = stack.proxy(&upstreams, &[]).await;
    let warm = send(&with_default, &tenant, "maverick/auto", &[]).await;
    assert_eq!(warm.status, 200, "{}", warm.body);
    let id = warm
        .header("x-maverick-recommendation-id")
        .expect("an id")
        .to_owned();

    stack.set_engine_down(true);
    let cached = send(&with_default, &tenant, "maverick/auto", &[]).await;
    assert_eq!(cached.status, 200, "{}", cached.body);
    assert_eq!(
        cached.header("x-maverick-fallback"),
        Some("last_known_good")
    );
    assert_eq!(
        cached.header("x-maverick-recommendation-id"),
        Some(id.as_str())
    );
    assert_eq!(last_model(&upstreams), json!("claude-opus-4-6"));

    let cold = send(&with_default, &tenant, "maverick/auto:cost", &[]).await;
    assert_eq!(cold.status, 200, "{}", cold.body);
    assert_eq!(cold.header("x-maverick-fallback"), Some("default_model"));
    assert_eq!(last_model(&upstreams), json!("claude-haiku-4-5"));

    let none = send(&bare, &tenant, "maverick/auto", &[]).await;
    assert_eq!(none.status, 503, "{}", none.body);
    assert_eq!(none.json()["error"]["code"], "engine_unavailable");
    assert_eq!(none.header("retry-after"), Some("5"));

    stack.set_engine_down(false);
    let back = send(&with_default, &tenant, "maverick/auto", &[]).await;
    assert_eq!(back.status, 200, "{}", back.body);
    assert_eq!(back.header("x-maverick-fallback"), None);
    with_default.stop().await;
    bare.stop().await;
}

/// D46, G14: a zero-retention tenant falls open only to `zero` entries, or to a default
/// model the operator marked verified zero retention; otherwise it gets `503`.
#[tokio::test]
async fn a_zero_retention_tenant_falls_open_only_to_compliant_models() {
    let Some(stack) = Stack::start(Default::default()).await else {
        return;
    };
    // GPT has standard evidence only, so the engine leaves it out for this tenant.
    let tenant = stack.tenant("zr", &[GPT, OPUS, CLAUDE], true, false).await;
    let upstreams = Upstreams::start("OK").await;
    let default = [("MAVERICK_PROXY_DEFAULT_MODEL_MESSAGES", "claude-haiku-4-5")];
    let unverified = stack.proxy(&upstreams, &default).await;
    let verified = stack
        .proxy(
            &upstreams,
            &[
                default[0],
                ("MAVERICK_PROXY_DEFAULT_MODEL_ZERO_RETENTION", "true"),
            ],
        )
        .await;
    for proxy in [&unverified, &verified] {
        let warm = send(proxy, &tenant, "maverick/auto", &[]).await;
        assert_eq!(warm.status, 200, "{}", warm.body);
    }

    stack.set_engine_down(true);
    let cached = send(&unverified, &tenant, "maverick/auto", &[]).await;
    assert_eq!(
        cached.header("x-maverick-fallback"),
        Some("last_known_good")
    );
    assert_eq!(last_model(&upstreams), json!("claude-opus-4-6"));
    let refused = send(&unverified, &tenant, "maverick/auto:cost", &[]).await;
    assert_eq!(refused.status, 503, "{}", refused.body);
    let allowed = send(&verified, &tenant, "maverick/auto:cost", &[]).await;
    assert_eq!(allowed.status, 200, "{}", allowed.body);
    assert_eq!(allowed.header("x-maverick-fallback"), Some("default_model"));
    assert_eq!(last_model(&upstreams), json!("claude-haiku-4-5"));
    stack.set_engine_down(false);
    unverified.stop().await;
    verified.stop().await;
}
