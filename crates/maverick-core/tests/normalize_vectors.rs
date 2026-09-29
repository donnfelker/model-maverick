//! D29: the normalization conformance vectors in
//! `crates/sdk-conformance/vectors/normalize.d29.json`, run the way an SDK core's
//! conformance runner runs them. The expectations are hand-written; this test checks
//! `maverick_core::normalize` against them.

use std::path::PathBuf;

use maverick_core::normalize::{
    FIELDS, WireFormat, WirePath, constraints_of, go_fallback, normalize, wire_path,
};
use serde_json::Value;

fn vectors() -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../sdk-conformance/vectors/normalize.d29.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("read vectors")).expect("json")
}

fn format(name: &str) -> WireFormat {
    match name {
        "chat_completions" => WireFormat::ChatCompletions,
        "messages" => WireFormat::Messages,
        other => panic!("unknown format {other}"),
    }
}

fn dotted(path: WirePath) -> String {
    match path {
        WirePath::Top(field) => field.to_owned(),
        WirePath::Nested { object, field } => format!("{object}.{field}"),
    }
}

#[test]
fn the_wire_paths_match_the_vectors() {
    let doc = vectors();
    let paths = doc["wire_paths"].as_object().expect("wire_paths");
    assert_eq!(paths.keys().map(String::as_str).collect::<Vec<_>>(), FIELDS);
    for (field, formats) in paths {
        for (name, want) in formats.as_object().expect("formats") {
            let got = wire_path(field, format(name)).map(dotted);
            assert_eq!(got.as_deref(), want.as_str(), "{field} {name}");
        }
    }
}

#[test]
fn every_vector_case() {
    let doc = vectors();
    let cases = doc["cases"].as_array().expect("cases");
    assert_eq!(cases.len(), 34);
    let mut ids = Vec::new();
    for case in cases {
        let id = case["id"].as_str().expect("id");
        assert!(!ids.contains(&id), "duplicate id {id}");
        ids.push(id);
        let entry = case.as_object().expect("case object");
        let field = case["field"].as_str().expect("field");
        let format = format(case["format"].as_str().expect("format"));
        let Some(value) = entry.get("value") else {
            // The request does not carry the field: nothing to normalize.
            assert!(entry.get("expect").is_none(), "{id}");
            continue;
        };
        let constraints = constraints_of(entry);
        let got = normalize(field, value, &constraints)
            .or_else(|| go_fallback(field, format, value))
            .unwrap_or_else(|| value.clone());
        assert_eq!(&got, &case["expect"], "{id}");
    }
}
