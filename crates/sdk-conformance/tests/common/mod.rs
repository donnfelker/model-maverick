//! Shared harness for the conformance runners (beads 8.3 and model-maverick-5fs).
//!
//! Both runners replay the same frozen scenarios against the same `Harness` and assert the
//! same `recorded` and `expect` blocks. This module holds the parts that do not depend on
//! how a request reaches the mocks: loading the scenarios and the assertion engine (count
//! ranges, header, body, JSON-pointer, and not_containing checks). The proxy runner
//! (`conformance.rs`) still has its own copies; a later cleanup can move it onto this module.

#![allow(dead_code)]

use std::path::PathBuf;

use sdk_conformance::{Recorded, RecordedRequest};
use serde_json::Value;

/// One loaded scenario: the raw bytes for the harness, plus the parsed document.
pub struct Loaded {
    pub id: String,
    pub group: u64,
    pub doc: Value,
    pub bytes: Vec<u8>,
}

/// Load every scenario in the crate's `scenarios/` directory, sorted by id.
pub fn load_scenarios() -> Vec<Loaded> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scenarios");
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("scenarios dir") {
        let path = entry.expect("entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let bytes = std::fs::read(&path).expect("read scenario");
        let doc: Value = serde_json::from_slice(&bytes).expect("scenario json");
        let id = doc["id"].as_str().expect("id").to_owned();
        let group = doc["group"].as_u64().expect("group");
        out.push(Loaded {
            id,
            group,
            doc,
            bytes,
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// Read `config.<key>` as a positive integer of milliseconds.
pub fn config_ms(config: &Value, key: &str) -> Option<u64> {
    config.get(key).and_then(Value::as_u64)
}

/// An address that refuses TCP connections: bind an ephemeral port, then drop the listener.
pub fn refused_url() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind refused");
    let addr = listener.local_addr().expect("refused addr");
    drop(listener);
    format!("http://{addr}")
}

/// The upstream base URL for one provider: the mock provider, or a refused address when the
/// scenario marks it `{refused}`.
pub fn upstream_for(config: &Value, provider: &str, provider_url: &str, refused: &str) -> String {
    let is_refused = config
        .get("provider_base_urls")
        .and_then(|urls| urls.get(provider))
        .and_then(Value::as_str)
        == Some("{refused}");
    if is_refused {
        refused.to_owned()
    } else {
        provider_url.to_owned()
    }
}

/// Assert the recorded traffic against a `recorded` (or `check_recorded`) block.
pub fn assert_recorded(recorded: &Recorded, expect: &Value) -> Result<(), String> {
    let expect = expect.as_object().ok_or("recorded is not an object")?;
    for (stream, want) in expect {
        let actual = match stream.as_str() {
            "recommend" => &recorded.recommend,
            "telemetry" => &recorded.telemetry,
            "provider" => &recorded.provider,
            other => return Err(format!("unknown stream {other}")),
        };
        assert_stream(actual, want, stream)?;
    }
    Ok(())
}

fn assert_stream(actual: &[RecordedRequest], want: &Value, stream: &str) -> Result<(), String> {
    let count = &want["count"];
    let len = actual.len() as u64;
    if let Some(exact) = count.as_u64() {
        if len != exact {
            return Err(format!("{stream}: {len} requests, want {exact}"));
        }
    } else if let Some(range) = count.as_object() {
        let min = range["min"].as_u64().unwrap_or(0);
        let max = range["max"].as_u64().unwrap_or(u64::MAX);
        if len < min || len > max {
            return Err(format!("{stream}: {len} requests, want {min}..={max}"));
        }
    }
    if let Some(requests) = want.get("requests").and_then(Value::as_array) {
        for (i, req) in requests.iter().enumerate() {
            let recorded = actual
                .get(i)
                .ok_or_else(|| format!("{stream}: no request {i}"))?;
            assert_request(recorded, req, &format!("{stream}[{i}]"))?;
        }
    }
    Ok(())
}

fn assert_request(actual: &RecordedRequest, want: &Value, where_: &str) -> Result<(), String> {
    if let Some(headers) = want.get("headers").and_then(Value::as_object) {
        for (name, value) in headers {
            let got = actual.headers.get(name).map(String::as_str);
            if got != value.as_str() {
                return Err(format!("{where_}: header {name} is {got:?}, want {value}"));
            }
        }
    }
    let body: Value = serde_json::from_str(&actual.body).unwrap_or(Value::Null);
    if let Some(expected_body) = want.get("body") {
        if &body != expected_body {
            return Err(format!("{where_}: body {body} != {expected_body}"));
        }
    }
    if let Some(checks) = want.get("checks").and_then(Value::as_array) {
        for (i, check) in checks.iter().enumerate() {
            apply_check(&body, check).map_err(|e| format!("{where_}.checks[{i}]: {e}"))?;
        }
    }
    if let Some(needles) = want.get("not_containing").and_then(Value::as_array) {
        let mut haystack = actual.body.clone();
        haystack.push('\n');
        haystack.push_str(&actual.query);
        for value in actual.headers.values() {
            haystack.push('\n');
            haystack.push_str(value);
        }
        for needle in needles.iter().filter_map(Value::as_str) {
            if haystack.contains(needle) {
                return Err(format!("{where_}: request contains {needle:?}"));
            }
        }
    }
    Ok(())
}

fn apply_check(body: &Value, check: &Value) -> Result<(), String> {
    let pointer = check["pointer"].as_str().ok_or("missing pointer")?;
    let found = body.pointer(pointer);
    if let Some(expected) = check.get("equals") {
        return match found {
            Some(value) if value == expected => Ok(()),
            other => Err(format!("{pointer}: {other:?} != {expected}")),
        };
    }
    if let Some(expected) = check.get("absent_or_equals") {
        return match found {
            None => Ok(()),
            Some(value) if value == expected => Ok(()),
            Some(value) => Err(format!("{pointer}: {value} != {expected} (or absent)")),
        };
    }
    if check.get("absent").and_then(Value::as_bool) == Some(true) {
        return if found.is_none() {
            Ok(())
        } else {
            Err(format!("{pointer}: present, want absent"))
        };
    }
    if let Some(substring) = check.get("contains").and_then(Value::as_str) {
        return match found.and_then(Value::as_str) {
            Some(text) if text.contains(substring) => Ok(()),
            other => Err(format!(
                "{pointer}: {other:?} does not contain {substring:?}"
            )),
        };
    }
    if let Some(length) = check.get("length").and_then(Value::as_u64) {
        let got = found.map(value_len);
        return if got == Some(length as usize) {
            Ok(())
        } else {
            Err(format!("{pointer}: length {got:?}, want {length}"))
        };
    }
    if let Some(kind) = check.get("type").and_then(Value::as_str) {
        return check_type(
            found,
            kind,
            check.get("min").and_then(Value::as_u64),
            pointer,
        );
    }
    Err(format!("{pointer}: unknown assertion {check}"))
}

fn value_len(value: &Value) -> usize {
    match value {
        Value::Array(items) => items.len(),
        Value::Object(map) => map.len(),
        Value::String(text) => text.chars().count(),
        _ => 0,
    }
}

fn check_type(
    found: Option<&Value>,
    kind: &str,
    min: Option<u64>,
    pointer: &str,
) -> Result<(), String> {
    let value = found.ok_or_else(|| format!("{pointer}: absent, want {kind}"))?;
    let ok = match kind {
        "integer" => value.is_i64() || value.is_u64(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        _ => false,
    };
    if !ok {
        return Err(format!("{pointer}: {value} is not {kind}"));
    }
    if let Some(min) = min {
        let measure = if kind == "integer" {
            value.as_i64().unwrap_or(0).max(0) as u64
        } else {
            value_len(value) as u64
        };
        if measure < min {
            return Err(format!("{pointer}: {measure} < min {min}"));
        }
    }
    Ok(())
}
