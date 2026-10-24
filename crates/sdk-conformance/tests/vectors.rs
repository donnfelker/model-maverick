//! The frozen vectors, replayed against the Rust implementations the way an SDK core's
//! conformance runner replays them against its own (bead 8.2). The expectations in the
//! files are hand-written; these tests check the Rust code against them.
//!
//! `projection.p1.json` is replayed by `crates/maverick-core/tests/projection.rs`, and
//! `normalize.d29.json` by `crates/maverick-core/tests/normalize_vectors.rs` and
//! `crates/maverick-proxy/tests/normalize_vectors.rs`.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use maverick_core::alias::{AliasError, Mode, ParsedModel, parse_alias};
use maverick_core::chain::{AttemptRecord, ErrorClass};
use maverick_core::recommend::RetentionMode as WireRetention;
use maverick_core::telemetry::{UsageShape, parse_usage};
use maverick_domain::policy::RetentionMode;
use maverick_domain::telemetry::decode_item;
use serde_json::{Value, json};

fn vectors(name: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("vectors")
        .join(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn cases<'a>(doc: &'a Value, key: &str) -> &'a [Value] {
    let cases = doc[key].as_array().unwrap_or_else(|| panic!("no {key}"));
    let mut ids: Vec<&str> = cases
        .iter()
        .map(|c| c["id"].as_str().expect("id"))
        .collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), cases.len(), "{key}: duplicate ids");
    cases
}

#[test]
fn alias_vectors() {
    let doc = vectors("alias.v2.json");
    assert_eq!(doc["version"], "v2");
    let cases = cases(&doc, "cases");
    assert_eq!(cases.len(), 49);
    for case in cases {
        let id = case["id"].as_str().unwrap();
        let model = case["model"].as_str().unwrap();
        let got = match parse_alias(model) {
            Ok(ParsedModel::Route(alias)) => json!({
                "kind": "route",
                "pool": alias.pool,
                "mode": match alias.mode {
                    Mode::Balanced => "balanced",
                    Mode::Quality => "quality",
                    Mode::Cost => "cost",
                },
            }),
            Ok(ParsedModel::Direct { author, slug }) => json!({
                "kind": "direct", "author": author, "slug": slug,
            }),
            Err(AliasError::MalformedAlias) => json!({"kind": "malformed_alias"}),
        };
        assert_eq!(got, case["expect"], "{id}: {model:?}");
    }
}

fn counts(pair: (Option<i64>, Option<i64>)) -> Value {
    json!({"tokens_in": pair.0, "tokens_out": pair.1})
}

#[test]
fn telemetry_usage_vectors() {
    let doc = vectors("telemetry.v1.json");
    assert_eq!(doc["version"], "v1");
    let cases = cases(&doc, "usage_cases");
    assert_eq!(cases.len(), 14);
    for case in cases {
        let id = case["id"].as_str().unwrap();
        let body = case["body"].as_str().unwrap().as_bytes();
        assert_eq!(
            counts(parse_usage(UsageShape::Chat, body)),
            case["chat_completions"],
            "{id}: chat"
        );
        assert_eq!(
            counts(parse_usage(UsageShape::Messages, body)),
            case["messages"],
            "{id}: messages"
        );
    }
}

/// An item as JSON, with `endpoint_used` filled in when it is absent (the engine reads
/// absent as `direct`).
fn normalized(mut item: Value) -> Value {
    let object = item.as_object_mut().expect("item object");
    object
        .entry("endpoint_used")
        .or_insert_with(|| Value::from("direct"));
    item
}

#[test]
fn telemetry_attempt_vectors() {
    let doc = vectors("telemetry.v1.json");
    let cases = cases(&doc, "attempt_cases");
    assert_eq!(cases.len(), 13);
    for case in cases {
        let id = case["id"].as_str().unwrap();
        let attempt = &case["attempt"];
        let status = u16::try_from(attempt["status"].as_u64().unwrap()).unwrap();
        let record = AttemptRecord {
            index: 0,
            provider: "openai".to_owned(),
            model: attempt["model"].as_str().unwrap().to_owned(),
            endpoint_used: attempt["endpoint_used"].as_str().unwrap().to_owned(),
            status,
            error_class: match attempt["error_class"].as_str() {
                Some("timeout") => Some(ErrorClass::Timeout),
                Some("connect_error") => Some(ErrorClass::ConnectError),
                None => None,
                Some(other) => panic!("{id}: unknown error_class {other}"),
            },
            latency_ms: attempt["latency_ms"].as_u64().unwrap(),
            settings_applied: attempt["settings_applied"].as_bool().unwrap(),
            retry_after_wait: None,
            relayed: false,
        };
        let mut item = record.telemetry_item(attempt["recommendation_id"].as_str().unwrap());
        // Token counts come only from a buffered 2xx body.
        if (200..300).contains(&status)
            && let Some(body) = attempt["buffered_body"].as_str()
        {
            let shape = match attempt["surface"].as_str().unwrap() {
                "chat_completions" => UsageShape::Chat,
                "messages" => UsageShape::Messages,
                other => panic!("{id}: unknown surface {other}"),
            };
            (item.tokens_in, item.tokens_out) = parse_usage(shape, body.as_bytes());
        }
        item.check_pass = attempt["check_pass"].as_bool();
        let mut text = String::new();
        item.write_json(&mut text);
        let got: Value = serde_json::from_str(&text).expect("item json");
        assert_eq!(normalized(got), normalized(case["item"].clone()), "{id}");
    }
}

#[test]
fn telemetry_outcome_vectors() {
    let doc = vectors("telemetry.v1.json");
    let cases = cases(&doc, "outcome_cases");
    assert_eq!(cases.len(), 24);
    for case in cases {
        let id = case["id"].as_str().unwrap();
        let raw = serde_json::to_string(&case["item"]).unwrap();
        match (decode_item(&raw), case["expect"].as_str().unwrap()) {
            (Err(_), "dropped") => {}
            (Err(dropped), want) => panic!("{id}: dropped ({dropped:?}), want {want}"),
            (Ok(item), "dropped") => panic!("{id}: accepted as {}", item.outcome()),
            (Ok(item), want) => {
                assert_eq!(item.outcome(), want, "{id}");
                assert_eq!(item.endpoint_used, case["endpoint_used"], "{id}");
            }
        }
    }
}

fn instant(value: &Value) -> Option<DateTime<Utc>> {
    value.as_str().map(|text| {
        DateTime::parse_from_rfc3339(text)
            .unwrap_or_else(|e| panic!("{text}: {e}"))
            .with_timezone(&Utc)
    })
}

#[test]
fn retention_expiry_vectors() {
    let doc = vectors("retention.v1.json");
    assert_eq!(doc["version"], "v1");
    let cases = cases(&doc, "expiry_cases");
    assert_eq!(cases.len(), 12);
    for case in cases {
        let id = case["id"].as_str().unwrap();
        let mode: RetentionMode = serde_json::from_value(case["mode"].clone()).expect("mode");
        let now = instant(&case["now"]).expect("now");
        let got = mode.effective(instant(&case["valid_until"]), now);
        assert_eq!(got.as_str(), case["expect"], "{id}");
    }
}

#[test]
fn retention_parse_vectors() {
    let doc = vectors("retention.v1.json");
    let cases = cases(&doc, "parse_cases");
    assert_eq!(cases.len(), 9);
    for case in cases {
        let id = case["id"].as_str().unwrap();
        // A null or absent member decodes as the empty string.
        let got = WireRetention::parse(case["value"].as_str().unwrap_or_default());
        assert_eq!(got.as_str(), case["expect"], "{id}");
    }
}
