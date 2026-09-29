//! D29: the normalization conformance vectors in
//! `crates/sdk-conformance/vectors/normalize.d29.json`, through the proxy's attempt body
//! rewrite: each case's value is sent at its wire path, and the upstream body must carry
//! the expected value there (or still carry no field).

use std::path::PathBuf;

use maverick_core::normalize::{WireFormat, constraints_of};
use maverick_proxy::rewrite::{AttemptParams, rewrite_attempt};
use serde_json::{Value, json};

fn vectors() -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../sdk-conformance/vectors/normalize.d29.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("read vectors")).expect("json")
}

#[test]
fn every_vector_case_through_the_body_rewrite() {
    let doc = vectors();
    let cases = doc["cases"].as_array().expect("cases");
    assert!(!cases.is_empty());
    for case in cases {
        let id = case["id"].as_str().expect("id");
        assert_eq!(case["field"], "reasoning_effort", "{id}");
        let entry = case.as_object().expect("case object");
        let value = entry.get("value");
        let (format, body) = match case["format"].as_str().expect("format") {
            "chat_completions" => {
                let mut body = json!({"model": "caller", "messages": []});
                if let Some(value) = value {
                    body["reasoning_effort"] = value.clone();
                }
                (WireFormat::ChatCompletions, body)
            }
            "messages" => {
                let mut body = json!({"model": "caller", "max_tokens": 8, "output_config": {}});
                if let Some(value) = value {
                    body["output_config"]["effort"] = value.clone();
                }
                (WireFormat::Messages, body)
            }
            other => panic!("unknown format {other}"),
        };
        let constraints = constraints_of(entry);
        let rewritten = rewrite_attempt(
            body.to_string().as_bytes(),
            "routed-model",
            &AttemptParams {
                format,
                settings: None,
                constraints: &constraints,
                zdr: false,
            },
        )
        .expect(id);
        let sent: Value = serde_json::from_slice(&rewritten.body).expect("upstream json");
        assert_eq!(sent["model"], "routed-model", "{id}");
        let at = match format {
            WireFormat::ChatCompletions => sent.get("reasoning_effort"),
            WireFormat::Messages => sent["output_config"].get("effort"),
        };
        assert_eq!(at, entry.get("expect"), "{id}");
    }
}
