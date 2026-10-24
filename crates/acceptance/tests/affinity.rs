//! Acceptance for conversation affinity (spec §7.6, §8.1, §9.10; D44): a multi-turn
//! conversation through the real hosted proxy and the real engine, with recording
//! provider mocks. No real provider is called.
//!
//! The pool is Opus (97) and Sonnet (95), both Anthropic, so the caller's Anthropic key
//! runs either. Each turn declares its workload, so no classifier is called. A freshly
//! started proxy sends `affinity` only after the engine has advertised it (D50), so every
//! proxy first serves one turn without `X-Maverick-Conversation`, which must behave
//! exactly as before.

use maverick_acceptance::{CLAUDE, OPUS, Reply, RunningProxy, Stack, Tenant, Upstreams};
use maverick_store::measurement::{self, AggregateUpsert};
use serde_json::{Value, json};

const CALLER_KEY: &str = "sk-ant-acceptance-caller";
const WORKLOAD: &str = "structured_extraction:3";
const OPUS_MODEL: &str = "claude-opus-4-6";
const SONNET_MODEL: &str = "claude-sonnet-4-6";

fn body() -> Value {
    json!({
        "model": "maverick/auto",
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "Reply with OK."}]
    })
}

async fn send(proxy: &RunningProxy, tenant: &Tenant, conversation: Option<&str>) -> Reply {
    let mut headers = vec![("x-api-key", CALLER_KEY), ("x-maverick-workload", WORKLOAD)];
    if let Some(conversation) = conversation {
        headers.push(("x-maverick-conversation", conversation));
    }
    let reply = proxy.post("/v1/messages", tenant, &headers, &body()).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    reply
}

/// The model the Anthropic mock answered last with a success.
fn served(upstreams: &Upstreams) -> String {
    upstreams
        .anthropic
        .seen()
        .last()
        .expect("an Anthropic request")
        .json()["model"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn cell(stack: &Stack, column: &str, tier: i16, dimension: &str, ewma: f32) {
    measurement::upsert_aggregate(
        &stack.pool,
        &AggregateUpsert {
            model_column_id: column,
            capability: "structured_extraction",
            difficulty_tier: tier,
            dimension,
            ewma,
            variance: 0.0,
            n: 20,
            measured: true,
            matrix_version: "2026-07-01",
            as_of: chrono::Utc::now().date_naive(),
        },
    )
    .await
    .expect("aggregate");
}

/// The latest recommendation's affinity status and reason from its trace block, or
/// `None` when it stored no block.
async fn traced(stack: &Stack, tenant: &Tenant) -> Option<(String, Option<String>)> {
    let row: (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT affinity->>'status', affinity->>'reason' FROM recommendations
         WHERE tenant_id = $1 ORDER BY created_at DESC, id DESC LIMIT 1",
    )
    .bind(&tenant.id)
    .fetch_one(&stack.pool)
    .await
    .expect("a trace row");
    row.0.map(|status| (status, row.1))
}

async fn pinned(stack: &Stack, tenant: &Tenant) -> String {
    sqlx::query_scalar("SELECT column_id FROM conversation_affinity WHERE tenant_id = $1")
        .bind(&tenant.id)
        .fetch_one(&stack.pool)
        .await
        .expect("the conversation's row")
}

async fn engine_ok(stack: &Stack, tenant: &Tenant, method: &str, path: &str, body: Value) {
    let reply = stack
        .engine(method, path, &tenant.key, &body.to_string())
        .await;
    assert_eq!(reply.status, 200, "{path}: {}", reply.body);
}

/// The first turn of a new proxy: no conversation, so no affinity, and the pick is the
/// plain quality pick.
async fn warm_up(proxy: &RunningProxy, stack: &Stack, tenant: &Tenant, upstreams: &Upstreams) {
    let reply = send(proxy, tenant, None).await;
    assert_eq!(reply.header("x-maverick-affinity"), None);
    assert_eq!(traced(stack, tenant).await, None);
    assert_eq!(served(upstreams), OPUS_MODEL);
}

fn status(reply: &Reply) -> &str {
    reply
        .header("x-maverick-affinity")
        .expect("X-Maverick-Affinity")
}

/// DIVERGENCE: D44. Cost-aware off (the default): the pick is always the best model for
/// the turn, and affinity only reports it: `new`, then `kept`, then `switched` with
/// `best_model` when the measurements move under the conversation.
#[tokio::test]
async fn by_default_affinity_reports_and_never_changes_the_pick() {
    let Some(stack) = Stack::start(Default::default()).await else {
        return;
    };
    let tenant = stack.tenant("plain", &[OPUS, CLAUDE], false, false).await;
    let upstreams = Upstreams::start("OK").await;
    let proxy = stack.proxy(&upstreams, &[]).await;
    warm_up(&proxy, &stack, &tenant, &upstreams).await;

    let first = send(&proxy, &tenant, Some("conv-plain")).await;
    assert_eq!(status(&first), "new");
    assert_eq!(served(&upstreams), OPUS_MODEL);
    let second = send(&proxy, &tenant, Some("conv-plain")).await;
    assert_eq!(status(&second), "kept");
    assert_eq!(traced(&stack, &tenant).await, Some(("kept".into(), None)));

    // Sonnet now measures 99 against Opus's 97: the best model wins.
    cell(&stack, CLAUDE, 3, "quality", 0.99).await;
    let third = send(&proxy, &tenant, Some("conv-plain")).await;
    assert_eq!(status(&third), "switched");
    assert_eq!(served(&upstreams), SONNET_MODEL);
    assert_eq!(
        traced(&stack, &tenant).await,
        Some(("switched".into(), Some("best_model".into())))
    );
    assert_eq!(pinned(&stack, &tenant).await, CLAUDE);

    // The header never reaches a provider.
    assert!(
        upstreams
            .seen()
            .iter()
            .all(|seen| seen.header("x-maverick-conversation").is_none())
    );
    proxy.stop().await;
}

/// DIVERGENCE: D44. Cost-aware on (the tenant setting): a close rival stays behind the
/// incumbent bonus (`kept`), a tier the pinned column no longer clears moves the
/// conversation (`switched`, `escalation`), and a pool change that drops the pin is
/// `broken` (`not_in_pool`).
#[tokio::test]
async fn cost_aware_affinity_keeps_escalates_and_breaks() {
    let Some(stack) = Stack::start(Default::default()).await else {
        return;
    };
    let tenant = stack.tenant("aware", &[OPUS, CLAUDE], false, false).await;
    engine_ok(
        &stack,
        &tenant,
        "PUT",
        "/v1/config/constraints",
        json!({"constraints": {"affinity_cost_aware": true}}),
    )
    .await;
    let upstreams = Upstreams::start("OK").await;
    let proxy = stack.proxy(&upstreams, &[]).await;
    warm_up(&proxy, &stack, &tenant, &upstreams).await;

    let first = send(&proxy, &tenant, Some("conv-aware")).await;
    assert_eq!(status(&first), "new");
    assert_eq!(served(&upstreams), OPUS_MODEL);

    // Sonnet 98 against the pinned Opus 97 plus the 1.5 bonus: kept.
    cell(&stack, CLAUDE, 3, "quality", 0.98).await;
    let kept = send(&proxy, &tenant, Some("conv-aware")).await;
    assert_eq!(status(&kept), "kept");
    assert_eq!(served(&upstreams), OPUS_MODEL);

    // Opus no longer clears tier 3 (pass rate 0.5 from tier 3 up) and Sonnet does.
    for tier in 3..=5 {
        cell(&stack, OPUS, tier, "pass_rate", 0.5).await;
    }
    let escalated = send(&proxy, &tenant, Some("conv-aware")).await;
    assert_eq!(status(&escalated), "switched");
    assert_eq!(served(&upstreams), SONNET_MODEL);
    assert_eq!(
        traced(&stack, &tenant).await,
        Some(("switched".into(), Some("escalation".into())))
    );

    // The pool drops Sonnet, the pinned column now.
    engine_ok(
        &stack,
        &tenant,
        "PUT",
        "/v1/config/pools",
        json!({"pools": [{"name": "default", "model_column_ids": [OPUS], "is_default": true}]}),
    )
    .await;
    let broken = send(&proxy, &tenant, Some("conv-aware")).await;
    assert_eq!(status(&broken), "broken");
    assert_eq!(served(&upstreams), OPUS_MODEL);
    assert_eq!(
        traced(&stack, &tenant).await,
        Some(("broken".into(), Some("not_in_pool".into())))
    );
    proxy.stop().await;
}

/// DIVERGENCE: D44. A 429 on the pick lets the next entry serve the turn. The proxy's
/// telemetry moves the conversation to that entry, so the next turn finds it pinned
/// there and, with cost-aware affinity, keeps it.
#[tokio::test]
async fn telemetry_moves_the_conversation_to_the_entry_that_served() {
    let Some(stack) = Stack::start(Default::default()).await else {
        return;
    };
    let tenant = stack.tenant("follow", &[OPUS, CLAUDE], false, false).await;
    engine_ok(
        &stack,
        &tenant,
        "PUT",
        "/v1/config/constraints",
        json!({"constraints": {"affinity_cost_aware": true}}),
    )
    .await;
    // Sonnet 96.5: behind Opus alone, ahead of it once pinned (96.5 + 1.5 > 97).
    cell(&stack, CLAUDE, 3, "quality", 0.965).await;
    let upstreams = Upstreams::start("OK").await;
    let proxy = stack.proxy(&upstreams, &[]).await;
    warm_up(&proxy, &stack, &tenant, &upstreams).await;

    upstreams.anthropic.fail(OPUS_MODEL, 429);
    let first = send(&proxy, &tenant, Some("conv-follow")).await;
    assert_eq!(status(&first), "new");
    assert_eq!(served(&upstreams), SONNET_MODEL);
    assert_eq!(pinned(&stack, &tenant).await, OPUS);
    // Stopping the proxy flushes its telemetry: the 429 item and the 200 item.
    proxy.stop().await;
    assert_eq!(pinned(&stack, &tenant).await, CLAUDE);

    upstreams.anthropic.clear_failures();
    let proxy = stack.proxy(&upstreams, &[]).await;
    warm_up(&proxy, &stack, &tenant, &upstreams).await;
    let next = send(&proxy, &tenant, Some("conv-follow")).await;
    assert_eq!(status(&next), "kept");
    assert_eq!(served(&upstreams), SONNET_MODEL);
    proxy.stop().await;
}
