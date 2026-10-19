//! The privacy acceptance suite, tests 2 to 12 (bead 9.2; spec §15 "Privacy tests",
//! §14.3, §9.10; D1, D2, D12, D15, D30, D39; G5, G13, G14). Test 1, the canary, runs alone
//! in `tests/canary.rs` because it owns the process's global subscriber.
//!
//! Each test runs the real engine on real Postgres and, where the rule concerns the proxy,
//! the real proxy in hosted mode. Every provider is a recording fake, so each test
//! shows where a prompt went and where it did not.

use maverick_acceptance::{
    CLAUDE, DECLARED, GEMINI, GPT, OPUS, Options, Stack, Tenant, Upstreams, eventually,
};
use serde_json::{Value, json};

const COMPLETION: &str = "the model's answer";

fn chat_body(prompt: &str) -> Value {
    json!({"model": "maverick/auto", "messages": [{"role": "user", "content": prompt}]})
}

fn messages_body(prompt: &str) -> Value {
    json!({
        "model": "maverick/auto",
        "max_tokens": 64,
        "messages": [{"role": "user", "content": prompt}]
    })
}

/// The column ids of a recommend response's chain, in order.
fn chain(body: &Value) -> Vec<String> {
    body["recommendations"]
        .as_array()
        .unwrap_or_else(|| panic!("no chain: {body}"))
        .iter()
        .map(|entry| entry["column_id"].as_str().unwrap().to_owned())
        .collect()
}

/// The error code of an error envelope.
fn code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or_default()
}

/// The trace row's exclusion reason for each excluded column.
async fn exclusions(stack: &Stack, recommendation_id: &str) -> Vec<(String, String)> {
    let candidates: Value =
        sqlx::query_scalar("select candidates from recommendations where id = $1")
            .bind(recommendation_id)
            .fetch_one(&stack.pool)
            .await
            .expect("trace row");
    candidates
        .as_array()
        .expect("candidates")
        .iter()
        .filter(|candidate| candidate["status"] == "excluded")
        .map(|candidate| {
            (
                candidate["column_id"].as_str().unwrap().to_owned(),
                candidate["exclusion_reason"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

/// Outcome rows written for the tenant's recommendations.
async fn outcomes(stack: &Stack, tenant: &Tenant) -> i64 {
    sqlx::query_scalar(
        "select count(*) from outcomes o
         join recommendations r on r.id = o.recommendation_id
         where r.tenant_id = $1",
    )
    .bind(&tenant.id)
    .fetch_one(&stack.pool)
    .await
    .expect("outcomes")
}

/// Test 2 (D1): a zero-retention tenant's declared request ranks only columns with valid
/// zero evidence. A request cannot widen that, and a pool without such a column is 409.
#[tokio::test]
async fn a_declared_request_never_selects_an_unverified_column() {
    let Some(stack) = Stack::start(Options::default()).await else {
        return;
    };
    let tenant = stack
        .tenant("zr", &[OPUS, CLAUDE, GPT, GEMINI], true, false)
        .await;
    let reply = stack
        .engine("POST", "/v1/recommend", &tenant.key, DECLARED)
        .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let body = reply.json();
    assert_eq!(chain(&body), [OPUS, CLAUDE]);
    assert_eq!(body["zero_retention"], true);
    let excluded = exclusions(&stack, body["recommendation_id"].as_str().unwrap()).await;
    for column in [GPT, GEMINI] {
        assert!(
            excluded.contains(&(
                column.to_owned(),
                "standing_constraint:zero_retention".to_owned()
            )),
            "{column}: {excluded:?}"
        );
    }

    // `allowed_models` narrows; it cannot bring an unverified column back.
    let only_gpt = json!({
        "workload": {"capability": "structured_extraction", "difficulty": 3},
        "allowed_models": ["gpt-5"]
    });
    let reply = stack
        .engine("POST", "/v1/recommend", &tenant.key, &only_gpt.to_string())
        .await;
    assert_eq!(reply.status, 409, "{}", reply.body);
    assert_eq!(code(&reply.json()), "no_compliant_columns");

    let unverified = stack
        .tenant("zr_unverified", &[GPT, GEMINI], true, false)
        .await;
    let reply = stack
        .engine("POST", "/v1/recommend", &unverified.key, DECLARED)
        .await;
    assert_eq!(reply.status, 409, "{}", reply.body);
    assert_eq!(code(&reply.json()), "no_compliant_columns");
}

/// Test 3 (D1, D2): a zero-retention tenant's prompt goes only to the verified
/// zero-retention classifier, and its picks only to verified endpoints, through the
/// engine and through the proxy.
#[tokio::test]
async fn a_zero_retention_prompt_and_picks_reach_only_verified_endpoints() {
    let Some(stack) = Stack::start(Options {
        zero_retention_classifier: true,
    })
    .await
    else {
        return;
    };
    let tenant = stack
        .tenant("zr", &[OPUS, CLAUDE, GPT, GEMINI], true, false)
        .await;
    let reply = stack.recommend_prompt(&tenant, "a private contract").await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(chain(&reply.json()), [OPUS, CLAUDE]);
    assert!(
        stack.classifier.prompts().is_empty(),
        "no unverified column"
    );
    let classified = stack.zr_classifier.prompts();
    assert_eq!(classified.len(), 1);
    assert!(
        classified[0].contains("a private contract"),
        "{classified:?}"
    );

    let upstreams = Upstreams::start(COMPLETION).await;
    let proxy = stack.proxy(&upstreams, &[]).await;
    let reply = proxy
        .messages(
            &tenant,
            Some("sk-ant-caller"),
            &messages_body("a private memo"),
        )
        .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let anthropic: Vec<Value> = upstreams
        .anthropic
        .seen()
        .iter()
        .map(|seen| seen.json()["model"].clone())
        .collect();
    assert_eq!(anthropic, [json!("claude-opus-4-6")]);
    // The chat surface's only provider is OpenAI, and no OpenAI column is verified.
    let reply = proxy
        .chat(
            &tenant,
            Some("sk-proj-caller"),
            &chat_body("a private memo"),
        )
        .await;
    assert_ne!(reply.status, 200, "{}", reply.body);
    for (name, upstream) in upstreams.all() {
        if name != "anthropic" {
            assert!(upstream.seen().is_empty(), "{name} was called");
        }
    }
    assert!(stack.classifier.prompts().is_empty());
    proxy.stop().await;
}

/// Test 4 (D1, D15): expired evidence is never stale compliance. An expired column is
/// 409 at the engine; an expired alias reads `unknown`, and the proxy's OpenRouter mode
/// answers 409 without any upstream request. (The proxy sends the prompt form, so the
/// engine has a verified zero-retention classifier.)
#[tokio::test]
async fn expired_evidence_answers_409() {
    let Some(stack) = Stack::start(Options {
        zero_retention_classifier: true,
    })
    .await
    else {
        return;
    };
    let tenant = stack.tenant("zr", &[CLAUDE], true, false).await;
    let recommend = || stack.engine("POST", "/v1/recommend", &tenant.key, DECLARED);
    assert_eq!(recommend().await.status, 200);

    sqlx::query(
        "update model_columns set retention_valid_until = now() - interval '1 hour' where id = $1",
    )
    .bind(CLAUDE)
    .execute(&stack.pool)
    .await
    .unwrap();
    let reply = recommend().await;
    assert_eq!(reply.status, 409, "{}", reply.body);
    assert_eq!(code(&reply.json()), "no_compliant_columns");

    sqlx::query(
        "update model_columns
         set retention_valid_until = now() + interval '30 days',
             execution_alias_retention =
               '{\"openrouter\":{\"retention_mode\":\"zero\",\"retention_valid_until\":\"2020-01-01T00:00:00Z\"}}'
         where id = $1",
    )
    .bind(CLAUDE)
    .execute(&stack.pool)
    .await
    .unwrap();
    let reply = recommend().await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(
        reply.json()["recommendations"][0]["execution_alias_retention"]["openrouter"],
        "unknown"
    );

    let upstreams = Upstreams::start(COMPLETION).await;
    let proxy = stack.proxy(&upstreams, &[]).await;
    let reply = proxy
        .chat(&tenant, Some("sk-or-v1-caller"), &chat_body("hello"))
        .await;
    assert_eq!(reply.status, 409, "{}", reply.body);
    assert_eq!(code(&reply.json()), "no_compliant_columns");
    assert!(upstreams.seen().is_empty(), "{:?}", upstreams.seen());
    proxy.stop().await;
}

/// Test 5 (D2): without a zero-retention classifier column, a zero-retention tenant's
/// prompt form is refused before any classifier call, and nothing is stored.
#[tokio::test]
async fn a_zero_retention_tenant_without_a_zero_retention_classifier_must_declare() {
    let Some(stack) = Stack::start(Options::default()).await else {
        return;
    };
    let tenant = stack.tenant("zr", &[CLAUDE], true, false).await;
    let reply = stack.recommend_prompt(&tenant, "a private contract").await;
    assert_eq!(reply.status, 422, "{}", reply.body);
    assert_eq!(code(&reply.json()), "zero_retention_requires_declared");
    assert!(stack.classifier.prompts().is_empty());
    assert!(stack.zr_classifier.prompts().is_empty());
    for table in ["classification_cache", "debug_captures", "recommendations"] {
        assert_eq!(stack.count(table).await, 0, "{table}");
    }
}

/// Test 6 (G5): debug capture is refused for a zero-retention tenant at window creation
/// (409) and at request time, even with a window present.
#[tokio::test]
async fn debug_capture_is_refused_for_zero_retention_tenants() {
    let Some(stack) = Stack::start(Options {
        zero_retention_classifier: true,
    })
    .await
    else {
        return;
    };
    let window = r#"{"duration_minutes":60}"#;
    let tenant = stack.tenant("zr", &[CLAUDE], true, false).await;
    let reply = stack
        .engine("POST", "/v1/debug/capture-windows", &tenant.key, window)
        .await;
    assert_eq!(reply.status, 409, "{}", reply.body);
    assert_eq!(code(&reply.json()), "zero_retention_conflict");
    assert_eq!(stack.count("capture_windows").await, 0);

    // A window stored directly still captures nothing.
    let now = chrono::Utc::now();
    maverick_store::capture::create_capture_window(
        &stack.pool,
        "cw_direct",
        &tenant.id,
        None,
        None,
        now - chrono::Duration::minutes(1),
        now + chrono::Duration::minutes(60),
    )
    .await
    .unwrap();
    let reply = stack.recommend_prompt(&tenant, "a private contract").await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(stack.count("debug_captures").await, 0);

    // The control: an ordinary tenant with a window is captured.
    let other = stack.tenant("plain", &[CLAUDE], false, false).await;
    let reply = stack
        .engine("POST", "/v1/debug/capture-windows", &other.key, window)
        .await;
    assert_eq!(reply.status, 201, "{}", reply.body);
    assert_eq!(
        stack
            .recommend_prompt(&other, "an ordinary prompt")
            .await
            .status,
        200
    );
    assert_eq!(stack.count("debug_captures").await, 1);
}

/// Test 7 (D12): a hosted proxy given only an OpenAI key never calls Anthropic, and it
/// never looks up a provider env key, even when one is set.
#[tokio::test]
async fn the_hosted_proxy_uses_only_the_callers_credential() {
    let Some(stack) = Stack::start(Options::default()).await else {
        return;
    };
    let tenant = stack
        .tenant("plain", &[OPUS, CLAUDE, GPT, GEMINI], false, false)
        .await;
    let upstreams = Upstreams::start(COMPLETION).await;
    let env_keys = [
        ("ANTHROPIC_API_KEY", "sk-ant-from-env"),
        ("OPENAI_API_KEY", "sk-from-env"),
        ("GEMINI_API_KEY", "gemini-from-env"),
        ("XAI_API_KEY", "xai-from-env"),
        ("MAVERICK_API_KEY", "mk_live_from_env"),
    ];
    let proxy = stack.proxy(&upstreams, &env_keys).await;
    for (name, _) in env_keys {
        assert!(
            !proxy.looked_up.iter().any(|looked| looked == name),
            "hosted mode looked up {name}"
        );
    }
    let reply = proxy
        .chat(&tenant, Some("sk-proj-caller"), &chat_body("hello"))
        .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert!(
        upstreams.anthropic.seen().is_empty(),
        "Anthropic was called"
    );
    let openai = upstreams.openai.seen();
    assert_eq!(openai.len(), 1);
    assert_eq!(openai[0].json()["model"], "gpt-5");
    assert_eq!(
        openai[0].header("authorization"),
        Some("Bearer sk-proj-caller")
    );
    for seen in upstreams.seen() {
        for (_, value) in env_keys {
            assert!(
                !seen
                    .headers
                    .iter()
                    .any(|(_, header)| header.contains(value)),
                "{value} reached {}",
                seen.target
            );
        }
    }
    // No credential at all: nothing is called.
    let before = upstreams.seen().len();
    let reply = proxy.messages(&tenant, None, &messages_body("hello")).await;
    assert_ne!(reply.status, 200, "{}", reply.body);
    assert_eq!(upstreams.seen().len(), before);
    proxy.stop().await;
}

/// Test 8 (D30): neither the engine key nor `X-Maverick-API-Key` reaches an upstream.
/// The tenant's key goes only to the engine.
#[tokio::test]
async fn no_engine_key_reaches_an_upstream() {
    let Some(stack) = Stack::start(Options::default()).await else {
        return;
    };
    let tenant = stack.tenant("plain", &[CLAUDE, GPT], false, false).await;
    let upstreams = Upstreams::start(COMPLETION).await;
    let proxy = stack.proxy(&upstreams, &[]).await;
    let reply = proxy
        .chat(&tenant, Some("sk-proj-caller"), &chat_body("hello"))
        .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let reply = proxy
        .messages(&tenant, Some("sk-ant-caller"), &messages_body("hello"))
        .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    proxy.stop().await;

    let seen = upstreams.seen();
    assert_eq!(seen.len(), 2);
    for request in &seen {
        assert!(
            request.header("x-maverick-api-key").is_none(),
            "{request:?}"
        );
        assert!(
            !request
                .headers
                .iter()
                .any(|(_, value)| value.contains(&tenant.key)),
            "the engine key reached {}",
            request.target
        );
        assert!(!request.body.contains(&tenant.key));
    }
    assert!(
        stack
            .hits()
            .iter()
            .any(|hit| hit.path == "/v1/recommend" && hit.bearer.as_deref() == Some(&tenant.key))
    );
}

/// Test 9 (G5): after prompt-form traffic through the engine and the proxy, with a capture
/// window open, the prompt is in no row of any table (as text or as hex bytes). Only the
/// sealed capture holds it.
#[tokio::test]
async fn no_prompt_is_stored_outside_the_capture_ciphertext() {
    let Some(stack) = Stack::start(Options::default()).await else {
        return;
    };
    let canary = "PRIVACY-CANARY-STORAGE-5231";
    let tenant = stack.tenant("plain", &[CLAUDE, GPT], false, false).await;
    let reply = stack
        .engine(
            "POST",
            "/v1/debug/capture-windows",
            &tenant.key,
            r#"{"duration_minutes":60}"#,
        )
        .await;
    assert_eq!(reply.status, 201, "{}", reply.body);
    let prompt = format!("Summarize the contract {canary} for the board.");
    assert_eq!(stack.recommend_prompt(&tenant, &prompt).await.status, 200);
    let upstreams = Upstreams::start(COMPLETION).await;
    let proxy = stack.proxy(&upstreams, &[]).await;
    let reply = proxy
        .chat(&tenant, Some("sk-proj-caller"), &chat_body(&prompt))
        .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let reply = proxy
        .messages(&tenant, Some("sk-ant-caller"), &messages_body(&prompt))
        .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    proxy.stop().await;
    assert!(
        stack.count("debug_captures").await >= 1,
        "the prompt was captured"
    );
    assert!(stack.count("outcomes").await >= 1, "telemetry arrived");

    let tables: Vec<(String, String)> = sqlx::query_as(
        "select table_schema::text, table_name::text from information_schema.tables
         where table_type = 'BASE TABLE'
           and table_schema not in ('pg_catalog', 'information_schema')",
    )
    .fetch_all(&stack.pool)
    .await
    .unwrap();
    assert!(tables.len() >= 20, "{tables:?}");
    let hex: String = canary.bytes().map(|byte| format!("{byte:02x}")).collect();
    for (schema, table) in tables {
        let found: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "select count(*) from \"{schema}\".\"{table}\" t
             where strpos(t::text, $1) > 0 or strpos(lower(t::text), $2) > 0"
        )))
        .bind(canary)
        .bind(&hex)
        .fetch_one(&stack.pool)
        .await
        .unwrap_or_else(|error| panic!("{schema}.{table}: {error}"));
        assert_eq!(found, 0, "the prompt is stored in {schema}.{table}");
    }
}

/// Test 10 (D15): a zero-retention tenant through the proxy's OpenRouter mode. Every
/// attempt carries `provider.zdr: true`, an unverified alias is skipped, a chain of
/// unverified aliases is 409, and nothing ever goes to a direct provider.
#[tokio::test]
async fn openrouter_mode_forces_zdr_and_skips_unverified_aliases() {
    let Some(stack) = Stack::start(Options {
        zero_retention_classifier: true,
    })
    .await
    else {
        return;
    };
    // The engine ranks opus (alias `unknown`) before claude (alias `zero`).
    let tenant = stack
        .tenant("zr", &[OPUS, CLAUDE, GPT, GEMINI], true, false)
        .await;
    let upstreams = Upstreams::start(COMPLETION).await;
    let proxy = stack.proxy(&upstreams, &[]).await;
    let reply = proxy
        .chat(&tenant, Some("sk-or-v1-caller"), &chat_body("hello"))
        .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let openrouter = upstreams.openrouter.seen();
    assert_eq!(openrouter.len(), 1, "{openrouter:?}");
    let sent = openrouter[0].json();
    assert_eq!(sent["model"], "anthropic/claude-sonnet-4.6");
    assert_eq!(sent["provider"]["zdr"], true);
    assert_eq!(
        openrouter[0].header("authorization"),
        Some("Bearer sk-or-v1-caller")
    );

    let unverified = stack.tenant("zr_opus", &[OPUS], true, false).await;
    let reply = proxy
        .chat(&unverified, Some("sk-or-v1-caller"), &chat_body("hello"))
        .await;
    assert_eq!(reply.status, 409, "{}", reply.body);
    assert_eq!(code(&reply.json()), "no_compliant_columns");
    assert_eq!(upstreams.openrouter.seen().len(), 1);
    for (name, upstream) in upstreams.all() {
        if name != "openrouter" {
            assert!(upstream.seen().is_empty(), "{name} was called");
        }
    }
    proxy.stop().await;
}

/// Test 11 (D39): with `telemetry_opt_out`, recommend says `telemetry: false`, the proxy
/// sends no telemetry for the tenant, and a hand-sent batch writes no outcome rows.
/// Another tenant is unaffected.
#[tokio::test]
async fn an_opted_out_tenant_sends_and_stores_no_telemetry() {
    let Some(stack) = Stack::start(Options::default()).await else {
        return;
    };
    let out = stack.tenant("out", &[GPT], false, true).await;
    let on = stack.tenant("on", &[GPT], false, false).await;
    let declared = |tenant: &Tenant| {
        let key = tenant.key.clone();
        let stack = &stack;
        async move {
            stack
                .engine("POST", "/v1/recommend", &key, DECLARED)
                .await
                .json()
        }
    };
    assert_eq!(declared(&out).await["telemetry"], false);
    let on_recommendation = declared(&on).await;
    assert_eq!(on_recommendation["telemetry"], true);

    let upstreams = Upstreams::start(COMPLETION).await;
    let proxy = stack
        .proxy(&upstreams, &[("MAVERICK_PROXY_TELEMETRY", "on")])
        .await;
    for tenant in [&out, &on] {
        let reply = proxy
            .chat(tenant, Some("sk-proj-caller"), &chat_body("hello"))
            .await;
        assert_eq!(reply.status, 200, "{}", reply.body);
    }
    proxy.stop().await;
    let telemetry = |tenant: &Tenant| {
        stack
            .hits()
            .iter()
            .filter(|hit| hit.path == "/v1/telemetry" && hit.bearer.as_deref() == Some(&tenant.key))
            .count()
    };
    assert_eq!(telemetry(&out), 0, "the opted-out tenant sent telemetry");
    assert!(eventually(|| telemetry(&on) >= 1).await);
    assert_eq!(outcomes(&stack, &out).await, 0);
    assert!(outcomes(&stack, &on).await >= 1);

    // A batch sent by hand for the opted-out tenant is dropped, and says nothing more.
    let out_recommendation = declared(&out).await;
    let item = |recommendation: &Value| {
        json!({"items": [{
            "recommendation_id": recommendation["recommendation_id"],
            "model_used": "gpt-5",
            "status": 200
        }]})
        .to_string()
    };
    let reply = stack
        .engine(
            "POST",
            "/v1/telemetry",
            &out.key,
            &item(&out_recommendation),
        )
        .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(reply.json(), json!({"accepted": 0, "dropped": 1}));
    assert_eq!(outcomes(&stack, &out).await, 0);
    let before = outcomes(&stack, &on).await;
    let reply = stack
        .engine("POST", "/v1/telemetry", &on.key, &item(&on_recommendation))
        .await;
    assert_eq!(reply.json(), json!({"accepted": 1, "dropped": 0}));
    assert_eq!(outcomes(&stack, &on).await, before + 1);
}

/// Test 12 (G13): with `MAVERICK_PROXY_TELEMETRY=off` the proxy sends zero bytes to
/// `/v1/telemetry`, and the request is still served.
#[tokio::test]
async fn global_telemetry_off_sends_nothing() {
    let Some(stack) = Stack::start(Options::default()).await else {
        return;
    };
    let tenant = stack.tenant("on", &[GPT], false, false).await;
    let upstreams = Upstreams::start(COMPLETION).await;
    let proxy = stack
        .proxy(&upstreams, &[("MAVERICK_PROXY_TELEMETRY", "off")])
        .await;
    let reply = proxy
        .chat(&tenant, Some("sk-proj-caller"), &chat_body("hello"))
        .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    proxy.stop().await;
    assert!(
        !stack.hits().iter().any(|hit| hit.path == "/v1/telemetry"),
        "{:?}",
        stack.hits()
    );
    assert_eq!(stack.count("outcomes").await, 0);
}
