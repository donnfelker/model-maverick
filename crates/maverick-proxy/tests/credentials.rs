//! Credential resolution and upstream headers (bead 6.6; D12, D13, D30).
//!
//! - `unit.json#bearer_token-*` and `#build_upstream_headers-*` rows.
//! - The four credential scenarios, checked at the attempt level: for each chain entry, the
//!   resolved key and the exact headers the proxy will send. The expected headers are the
//!   scenario's recorded upstream request minus Go's transport headers (`Accept-Encoding`
//!   and `Content-Length`; D32). Sending those requests is bead 6.7, and the full scenario
//!   replay is bead 6.11.
//! - D12: a hosted instance given only an OpenAI key never calls Anthropic; an OpenRouter
//!   key never reaches a direct provider. Neither the engine key nor the tenant key ever
//!   appears in an upstream header.

use std::collections::BTreeMap;

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use maverick_proxy::chain::Surface;
use maverick_proxy::config::{Config, DeploymentMode, load_config};
use maverick_proxy::credentials::{
    CallerCredential, EnvKeys, attempt_headers, bearer_token, resolve_key, upstream_base_headers,
};
use serde_json::Value;

fn header_map(pairs: &[(String, String)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        map.append(
            HeaderName::from_bytes(name.as_bytes()).expect("header name"),
            HeaderValue::from_str(value).expect("header value"),
        );
    }
    map
}

fn pairs(value: &Value) -> Vec<(String, String)> {
    value
        .as_array()
        .expect("header pairs")
        .iter()
        .map(|pair| {
            (
                pair[0].as_str().expect("name").to_owned(),
                pair[1].as_str().expect("value").to_owned(),
            )
        })
        .collect()
}

/// A header map as lowercase name to values in order.
fn as_table(headers: &HeaderMap) -> BTreeMap<String, Vec<String>> {
    let mut table: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, value) in headers {
        table
            .entry(name.as_str().to_owned())
            .or_default()
            .push(value.to_str().expect("text header").to_owned());
    }
    table
}

/// A recorded `{Name: [values]}` object as the same table, minus `ignore`.
fn expected_table(value: &Value, ignore: &[&str]) -> BTreeMap<String, Vec<String>> {
    value
        .as_object()
        .expect("header object")
        .iter()
        .filter(|(name, _)| !ignore.iter().any(|skip| skip.eq_ignore_ascii_case(name)))
        .map(|(name, values)| {
            (
                name.to_ascii_lowercase(),
                values
                    .as_array()
                    .expect("values")
                    .iter()
                    .map(|value| value.as_str().expect("value").to_owned())
                    .collect(),
            )
        })
        .collect()
}

#[test]
fn bearer_token_rows() {
    let rows = parity_support::cases("fixtures/proxy/unit.json#bearer_token").expect("rows");
    assert_eq!(rows.len(), 10);
    for row in rows {
        let header = row["header"].as_str().expect("header");
        assert_eq!(
            bearer_token(header),
            row["token"].as_str().expect("token"),
            "{}: {header:?}",
            row["id"]
        );
    }
}

/// Rows 0 and 1 match Go. Row 2 is D30: Go forwarded `X-Maverick-Api-Key` upstream; Rust
/// strips it, so the tenant's Maverick key never reaches a provider.
#[test]
fn build_upstream_headers_rows() {
    let rows =
        parity_support::cases("fixtures/proxy/unit.json#build_upstream_headers").expect("rows");
    assert_eq!(rows.len(), 3);
    for row in rows {
        let id = row["id"].as_str().expect("id");
        let built = as_table(&upstream_base_headers(&header_map(&pairs(
            &row["client_headers"],
        ))));
        let want = if id == "build_upstream_headers-2" {
            let d30 = parity_support::divergence!("D30");
            assert_eq!(d30.id(), "D30");
            expected_table(&row["upstream_base_headers"], &["X-Maverick-Api-Key"])
        } else {
            expected_table(&row["upstream_base_headers"], &[])
        };
        assert_eq!(built, want, "{id}: {}", row["name"]);
    }
}

// ---------------------------------------------------------------------------------------
// Scenario attempt plans

fn scenario_config(env: &Value, mode: DeploymentMode) -> Config {
    let mut vars: BTreeMap<String, String> = env
        .as_object()
        .expect("proxy_env")
        .iter()
        .map(|(name, value)| (name.clone(), value.as_str().expect("env value").to_owned()))
        .collect();
    if mode == DeploymentMode::Hosted {
        vars.insert("MAVERICK_PROXY_MODE".to_owned(), "hosted".to_owned());
    }
    load_config(|name| vars.get(name).cloned()).expect("scenario config")
}

/// What the proxy will send for each chain entry: `None` for a skipped entry, else the key
/// and the attempt headers. The messages surface skips non-anthropic entries (6.7's rule,
/// applied here so the plan matches the scenario).
fn plan(
    surface: Surface,
    inbound: &HeaderMap,
    config: &Config,
    chain: &[(String, String)],
) -> Vec<Option<(String, HeaderMap)>> {
    let credential = CallerCredential::from_headers(surface, inbound, config.mode);
    let env = EnvKeys::from_config(config);
    let base = upstream_base_headers(inbound);
    chain
        .iter()
        .map(|(provider, _)| {
            if surface == Surface::Messages && provider != "anthropic" {
                return None;
            }
            let key = resolve_key(provider, surface, &credential, &env)?;
            Some((key.to_owned(), attempt_headers(surface, &base, key)?))
        })
        .collect()
}

fn chain_of(engine_body: &str) -> Vec<(String, String)> {
    let body: Value = serde_json::from_str(engine_body).expect("engine body");
    body["recommendations"]
        .as_array()
        .expect("chain")
        .iter()
        .map(|entry| {
            (
                entry["provider"].as_str().expect("provider").to_owned(),
                entry["model"].as_str().expect("model").to_owned(),
            )
        })
        .collect()
}

#[test]
fn credential_scenarios_plan_the_recorded_upstream_headers() {
    let doc = parity_support::fixture("proxy", "scenarios.json").expect("scenarios.json");
    let scenarios = doc["scenarios"].as_array().expect("scenarios");
    for id in [
        "chat_hop_by_hop_stripped",
        "messages_client_authorization_stripped",
        "chat_client_x_api_key_stripped",
        "x_chat_no_credential_502",
    ] {
        let scenario = scenarios
            .iter()
            .find(|scenario| scenario["id"] == id)
            .unwrap_or_else(|| panic!("scenario {id}"));
        let inbound = &scenario["inbound_request"];
        let surface = match inbound["path_and_query"].as_str().expect("path") {
            "/v1/messages" => Surface::Messages,
            _ => Surface::Chat,
        };
        let config = scenario_config(&scenario["proxy_env"], DeploymentMode::SelfHosted);
        let chain = chain_of(
            scenario["engine_recommend_response"]["body"]
                .as_str()
                .expect("engine body"),
        );
        let planned = plan(
            surface,
            &header_map(&pairs(&inbound["headers"])),
            &config,
            &chain,
        );

        let recorded: Vec<&Value> = scenario["expected"]["upstream_requests"]
            .as_object()
            .expect("upstream requests")
            .values()
            .flat_map(|requests| requests.as_array().expect("requests"))
            .collect();
        let attempted: Vec<&(String, HeaderMap)> = planned.iter().flatten().collect();
        assert_eq!(attempted.len(), recorded.len(), "{id}: attempts");
        for ((_, headers), request) in attempted.iter().zip(&recorded) {
            let want = expected_table(&request["headers"], &["Accept-Encoding", "Content-Length"]);
            assert_eq!(as_table(headers), want, "{id}: upstream headers");
            for value in headers.values() {
                let value = value.to_str().expect("text");
                assert!(
                    !value.contains("mk_live"),
                    "{id}: a Maverick key went upstream: {value}"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// D12, D13, D30

fn hosted_config_with_every_env_key() -> Config {
    let vars: BTreeMap<&str, &str> = [
        ("MAVERICK_PROXY_MODE", "hosted"),
        ("ANTHROPIC_API_KEY", "operator-anthropic"),
        ("OPENAI_API_KEY", "operator-openai"),
        ("GEMINI_API_KEY", "operator-google"),
        ("XAI_API_KEY", "operator-xai"),
    ]
    .into_iter()
    .collect();
    load_config(|name| vars.get(name).map(|value| (*value).to_owned())).expect("hosted config")
}

fn full_chain() -> Vec<(String, String)> {
    ["anthropic", "openai", "google", "xai"]
        .into_iter()
        .map(|provider| (provider.to_owned(), format!("{provider}-model")))
        .collect()
}

/// Required by bead 6.6: a hosted instance given only an OpenAI key never calls Anthropic,
/// even when the operator's environment holds provider keys.
#[test]
fn hosted_openai_key_never_calls_anthropic() {
    let config = hosted_config_with_every_env_key();
    let inbound = header_map(&[(
        "Authorization".to_owned(),
        "Bearer sk-caller-openai".to_owned(),
    )]);
    let planned = plan(Surface::Chat, &inbound, &config, &full_chain());
    let keys: Vec<Option<&str>> = planned
        .iter()
        .map(|attempt| attempt.as_ref().map(|(key, _)| key.as_str()))
        .collect();
    assert_eq!(keys, [None, Some("sk-caller-openai"), None, None]);

    // The same request self-hosted falls back to the env keys (Go behavior).
    let mut self_hosted = config.clone();
    self_hosted.mode = DeploymentMode::SelfHosted;
    self_hosted.anthropic_api_key = "env-anthropic".to_owned();
    let planned = plan(Surface::Chat, &inbound, &self_hosted, &full_chain());
    assert_eq!(
        planned[0].as_ref().map(|(key, _)| key.as_str()),
        Some("env-anthropic")
    );
}

#[test]
fn hosted_messages_key_restricts_the_chain_to_anthropic() {
    let config = hosted_config_with_every_env_key();
    let inbound = header_map(&[("x-api-key".to_owned(), "sk-ant-caller".to_owned())]);
    let planned = plan(Surface::Messages, &inbound, &config, &full_chain());
    let keys: Vec<Option<&str>> = planned
        .iter()
        .map(|attempt| attempt.as_ref().map(|(key, _)| key.as_str()))
        .collect();
    assert_eq!(keys, [Some("sk-ant-caller"), None, None, None]);
}

#[test]
fn hosted_openrouter_key_never_reaches_a_direct_provider() {
    let config = hosted_config_with_every_env_key();
    let inbound = header_map(&[(
        "Authorization".to_owned(),
        "Bearer sk-or-v1-caller".to_owned(),
    )]);
    assert_eq!(
        CallerCredential::from_headers(Surface::Chat, &inbound, config.mode),
        CallerCredential::OpenRouter("sk-or-v1-caller".to_owned())
    );
    let planned = plan(Surface::Chat, &inbound, &config, &full_chain());
    assert!(
        planned.iter().all(Option::is_none),
        "no direct attempt with an OpenRouter key"
    );
}

#[test]
fn maverick_keys_and_client_credentials_never_go_upstream() {
    let inbound = header_map(
        &[
            ("X-Maverick-API-Key", "mk_live_tenant_secret"),
            ("Authorization", "Bearer sk-caller"),
            ("x-api-key", "stray"),
            ("Cookie", "session=1"),
            ("X-Maverick-Workload", "code_generation:3"),
            ("X-Maverick-Settings", "caller"),
            ("Anthropic-Beta", "b1"),
            ("Anthropic-Beta", "b2"),
        ]
        .map(|(name, value)| (name.to_owned(), value.to_owned())),
    );
    let base = upstream_base_headers(&inbound);
    assert_eq!(
        as_table(&base)
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        [
            "anthropic-beta",
            "content-type",
            "cookie",
            "x-maverick-workload"
        ],
        "Go forwards every other client header (QUIRK 14); D30 strips the tenant key and D45 strips settings"
    );
    assert_eq!(as_table(&base)["anthropic-beta"], ["b1", "b2"]);
    for surface in [Surface::Chat, Surface::Messages] {
        let headers = attempt_headers(surface, &base, "provider-key").expect("headers");
        for (name, value) in &headers {
            let value = value.to_str().expect("text");
            assert!(
                !value.contains("mk_live"),
                "{surface:?}: {name} carries a Maverick key"
            );
            assert!(
                !value.contains("sk-caller") && !value.contains("stray"),
                "{surface:?}: {name}"
            );
        }
    }
}
