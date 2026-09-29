//! Every frozen scenario is well formed (bead 8.2): it loads into the harness as is, and
//! its SDK part uses only the keys and assertion kinds the README defines. Running the
//! scenarios against `maverick-core` and the proxy is bead 8.3.

use std::collections::BTreeSet;
use std::path::PathBuf;

use sdk_conformance::Scenario;
use serde_json::{Map, Value};

const TOP_KEYS: [&str; 10] = [
    "id", "name", "group", "source", "summary", "config", "engine", "provider", "steps", "recorded",
];
const CONFIG_KEYS: [&str; 12] = [
    "api_key",
    "privacy",
    "telemetry",
    "default_model",
    "zero_retention_verified",
    "recommend_timeout_ms",
    "attempt_timeout_ms",
    "telemetry_flush_interval_ms",
    "telemetry_max_batch",
    "execution",
    "provider_keys",
    "provider_base_urls",
];
const PROVIDERS: [&str; 5] = ["openai", "anthropic", "google", "xai", "openrouter"];
const ERROR_CODES: [&str; 3] = [
    "zero_retention_requires_declared",
    "no_compliant_columns",
    "no_executable_candidates",
];
const STREAMS: [&str; 3] = ["recommend", "provider", "telemetry"];

fn scenarios() -> Vec<(String, Value)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scenarios");
    let mut out: Vec<(String, Value)> = std::fs::read_dir(&dir)
        .expect("scenarios dir")
        .map(|entry| {
            let path = entry.expect("entry").path();
            assert_eq!(
                path.extension().and_then(|e| e.to_str()),
                Some("json"),
                "{}",
                path.display()
            );
            let stem = path.file_stem().unwrap().to_str().unwrap().to_owned();
            let bytes = std::fs::read(&path).expect("read scenario");
            Scenario::from_json(&bytes)
                .unwrap_or_else(|e| panic!("{stem}: not a harness scenario: {e}"));
            (stem, serde_json::from_slice(&bytes).expect("json"))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn object<'a>(value: &'a Value, what: &str) -> &'a Map<String, Value> {
    value
        .as_object()
        .unwrap_or_else(|| panic!("{what}: not an object"))
}

fn keys_within(map: &Map<String, Value>, allowed: &[&str], what: &str) {
    for key in map.keys() {
        assert!(allowed.contains(&key.as_str()), "{what}: unknown key {key}");
    }
}

fn millis(value: &Value, what: &str) {
    assert!(
        value.as_u64().is_some_and(|n| n > 0),
        "{what}: not a positive integer"
    );
}

fn check_config(config: &Map<String, Value>, id: &str) {
    keys_within(config, &CONFIG_KEYS, &format!("{id} config"));
    for (key, value) in config {
        let what = format!("{id} config.{key}");
        match key.as_str() {
            "privacy" => assert!(
                ["prompt", "declared", "off"].contains(&value.as_str().unwrap_or_default()),
                "{what}"
            ),
            "telemetry" => assert!(
                ["on", "off"].contains(&value.as_str().unwrap_or_default()),
                "{what}"
            ),
            "execution" => assert!(
                ["direct", "openrouter"].contains(&value.as_str().unwrap_or_default()),
                "{what}"
            ),
            "api_key" | "default_model" => assert!(value.is_string(), "{what}"),
            "zero_retention_verified" => assert!(
                value
                    .as_array()
                    .is_some_and(|models| models.iter().all(Value::is_string)),
                "{what}"
            ),
            "provider_keys" | "provider_base_urls" => {
                for (provider, v) in object(value, &what) {
                    assert!(PROVIDERS.contains(&provider.as_str()), "{what}: {provider}");
                    assert!(v.is_string(), "{what}.{provider}");
                    if key == "provider_base_urls" {
                        assert!(
                            ["{provider}", "{refused}"].contains(&v.as_str().unwrap()),
                            "{what}"
                        );
                    }
                }
            }
            _ => millis(value, &what),
        }
    }
}

fn check_call(call: &Map<String, Value>, what: &str) {
    keys_within(
        call,
        &["surface", "model", "workload", "original_model", "body"],
        what,
    );
    assert_eq!(
        call["surface"], "chat_completions",
        "{what}: v1 scenarios use chat completions"
    );
    assert!(call["model"].is_string(), "{what}.model");
    for optional in ["workload", "original_model"] {
        if let Some(value) = call.get(optional) {
            assert!(value.is_string(), "{what}.{optional}");
        }
    }
    let body = object(&call["body"], &format!("{what}.body"));
    assert!(
        !body.contains_key("model"),
        "{what}: the runner sets body.model from call.model"
    );
}

fn check_expect(expect: &Map<String, Value>, what: &str) {
    match expect["outcome"].as_str() {
        Some("response") => {
            keys_within(
                expect,
                &[
                    "outcome",
                    "status",
                    "body",
                    "min_elapsed_ms",
                    "max_elapsed_ms",
                ],
                what,
            );
            assert!(expect["status"].as_u64().is_some(), "{what}.status");
            assert!(expect.contains_key("body"), "{what}.body");
            for bound in ["min_elapsed_ms", "max_elapsed_ms"] {
                if let Some(value) = expect.get(bound) {
                    millis(value, &format!("{what}.{bound}"));
                }
            }
        }
        Some("error") => {
            keys_within(expect, &["outcome", "code", "status"], what);
            assert!(
                ERROR_CODES.contains(&expect["code"].as_str().unwrap_or_default()),
                "{what}.code"
            );
            if let Some(status) = expect.get("status") {
                assert!(status.as_u64().is_some(), "{what}.status");
            }
        }
        other => panic!("{what}: unknown outcome {other:?}"),
    }
}

fn check_pointer_check(check: &Map<String, Value>, what: &str) {
    let pointer = check["pointer"]
        .as_str()
        .unwrap_or_else(|| panic!("{what}.pointer"));
    assert!(
        pointer.starts_with('/'),
        "{what}: {pointer} is not a JSON Pointer"
    );
    let assertions: Vec<&str> = check
        .keys()
        .map(String::as_str)
        .filter(|key| *key != "pointer")
        .collect();
    match assertions.as_slice() {
        ["equals"] | ["absent_or_equals"] => {}
        ["absent"] => assert_eq!(check["absent"], true, "{what}"),
        ["contains"] => assert!(check["contains"].is_string(), "{what}"),
        ["length"] => assert!(check["length"].as_u64().is_some(), "{what}"),
        ["type"] | ["min", "type"] | ["type", "min"] => {
            assert!(
                ["integer", "string", "boolean", "object", "array"]
                    .contains(&check["type"].as_str().unwrap_or_default()),
                "{what}.type"
            );
        }
        other => panic!("{what}: unknown assertion {other:?}"),
    }
}

fn check_request(request: &Map<String, Value>, what: &str) {
    keys_within(
        request,
        &["headers", "body", "checks", "not_containing"],
        what,
    );
    if let Some(headers) = request.get("headers") {
        for (name, value) in object(headers, what) {
            assert_eq!(
                name,
                &name.to_ascii_lowercase(),
                "{what}: header names are lower case"
            );
            assert!(value.is_string(), "{what}.headers.{name}");
        }
    }
    if let Some(checks) = request.get("checks") {
        for (i, check) in checks.as_array().expect("checks array").iter().enumerate() {
            check_pointer_check(object(check, what), &format!("{what}.checks[{i}]"));
        }
    }
    if let Some(needles) = request.get("not_containing") {
        assert!(
            needles
                .as_array()
                .is_some_and(|n| n.iter().all(|s| s.as_str().is_some_and(|s| !s.is_empty()))),
            "{what}.not_containing"
        );
    }
}

fn check_recorded(recorded: &Map<String, Value>, what: &str) {
    keys_within(recorded, &STREAMS, what);
    for (stream, expected) in recorded {
        let what = format!("{what}.{stream}");
        let expected = object(expected, &what);
        keys_within(expected, &["count", "requests"], &what);
        let min = match &expected["count"] {
            Value::Number(n) => n.as_u64().expect("count"),
            Value::Object(range) => {
                let min = range["min"].as_u64().expect("count.min");
                let max = range["max"].as_u64().expect("count.max");
                assert!(min <= max, "{what}: count range");
                min
            }
            other => panic!("{what}: count {other}"),
        };
        if let Some(requests) = expected.get("requests") {
            let requests = requests.as_array().expect("requests array");
            assert!(
                requests.len() as u64 <= min,
                "{what}: more requests than the count"
            );
            for (i, request) in requests.iter().enumerate() {
                check_request(object(request, &what), &format!("{what}.requests[{i}]"));
            }
        }
    }
}

#[test]
fn every_scenario_is_well_formed() {
    let scenarios = scenarios();
    assert_eq!(scenarios.len(), 39);
    let mut groups = BTreeSet::new();
    for (stem, doc) in &scenarios {
        let top = object(doc, stem);
        keys_within(top, &TOP_KEYS, stem);
        let id = top["id"].as_str().expect("id");
        assert_eq!(id, stem, "the file is named after the id");
        let bytes = id.as_bytes();
        assert!(
            bytes.len() >= 3
                && bytes[0] == b'S'
                && bytes[1..3].iter().all(u8::is_ascii_digit)
                && bytes[3..].iter().all(u8::is_ascii_lowercase)
                && bytes.len() <= 4,
            "{id}: ids are S<two-digit group>[letter]"
        );
        let group = top["group"].as_u64().expect("group");
        assert_eq!(group, id[1..3].parse::<u64>().unwrap(), "{id}: group");
        groups.insert(group);
        let name = top["name"].as_str().expect("name");
        assert!(
            name.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
            "{id}: name is snake_case"
        );
        for text in ["source", "summary"] {
            assert!(
                top[text].as_str().is_some_and(|s| !s.is_empty()),
                "{id}.{text}"
            );
        }
        if let Some(config) = top.get("config") {
            check_config(object(config, id), id);
        }
        let steps = top["steps"].as_array().expect("steps");
        assert!(
            steps.iter().any(|s| s.get("call").is_some()),
            "{id}: no call"
        );
        for (i, step) in steps.iter().enumerate() {
            let what = format!("{id}.steps[{i}]");
            let step = object(step, &what);
            let kinds: Vec<&str> = step.keys().map(String::as_str).collect();
            match kinds.as_slice() {
                ["call", "expect"] => {
                    check_call(object(&step["call"], &what), &format!("{what}.call"));
                    check_expect(object(&step["expect"], &what), &format!("{what}.expect"));
                }
                ["wait_ms"] => millis(&step["wait_ms"], &what),
                ["check_recorded"] => {
                    check_recorded(object(&step["check_recorded"], &what), &what);
                }
                ["shutdown"] => assert_eq!(step["shutdown"], true, "{what}"),
                other => panic!("{what}: unknown step {other:?}"),
            }
        }
        check_recorded(object(&top["recorded"], id), &format!("{id}.recorded"));
    }
    assert_eq!(
        groups,
        (1..=12).collect(),
        "every group of sdk-v0 §2.3 and openrouter §6"
    );
}
