//! Parity: `parity/fixtures/decision/classify_*.json` and `classify_text/` (the pure
//! parts of `internal/classify`).

use chrono::{DateTime, Utc};
use maverick_domain::classify::{
    self, ALERT_KIND, ALERT_SEVERITY, Classification, DEGRADED_FAILURE_RATE, DEGRADED_MIN_SAMPLES,
    DEGRADED_WINDOW_SECONDS, DegradedTracker, classifier_version, degraded_alert_message,
    degraded_alert_subject, fence, model_from_column, parse, parse_column_ref, response_schema,
    strip_code_fence, truncate,
};
use maverick_domain::tasks::CAPABILITIES;
use serde_json::Value;

fn fixture(file: &str) -> Value {
    parity_support::fixture("decision", file).expect("fixture loads")
}

fn text_file(name: &str) -> String {
    let root = parity_support::repo_root().expect("repo root");
    std::fs::read_to_string(
        root.join("parity/fixtures/decision/classify_text")
            .join(name),
    )
    .expect("text file")
}

fn constants() -> Value {
    fixture("classify_constants.json")
}

#[test]
fn texts_match_byte_for_byte() {
    let c = constants();
    assert_eq!(classify::SYSTEM_PROMPT, text_file("system_prompt.txt"));
    assert_eq!(classify::SYSTEM_PROMPT, c["system_prompt"]);
    assert_eq!(
        classify::SYSTEM_PROMPT.len(),
        usize::try_from(c["system_prompt_sha256_bytes"].as_u64().expect("bytes")).expect("fits")
    );
    assert_eq!(
        classify::TRUNCATION_MARKER,
        text_file("truncation_marker.txt")
    );
    assert_eq!(classify::TRUNCATION_MARKER, c["truncation_marker"]);
    assert_eq!(
        classify::TRUNCATION_MARKER.chars().count(),
        usize::try_from(c["truncation_marker_runes"].as_u64().expect("runes")).expect("fits")
    );
    assert_eq!(fence(""), text_file("fenced_empty.txt"));
    assert_eq!(fence("hi"), text_file("fenced_example_hi.txt"));
    assert_eq!(fence("hi"), c["fence_workload_hi"]);
    assert_eq!(
        fence("").chars().count(),
        usize::try_from(c["fence_overhead_runes"].as_u64().expect("runes")).expect("fits")
    );
    assert_eq!(classify::WORKLOAD_OPEN, c["workload_open_marker"]);
    assert_eq!(classify::WORKLOAD_CLOSE, c["workload_close_marker"]);
}

/// parity: classify.TestClassifyVersion
/// parity: classify.TestClassifyPromptVersionBumped
#[test]
fn versions_and_constants() {
    let c = constants();
    assert_eq!(classify::PROMPT_VERSION, c["prompt_version"]);
    assert_eq!(
        classifier_version(classify::DEFAULT_COLUMN, classify::PROMPT_VERSION),
        c["version_default"]
    );
    assert_eq!(
        classifier_version(classify::DEFAULT_COLUMN, "v7"),
        c["version_v7"]
    );
    assert_eq!(
        model_from_column(classify::DEFAULT_COLUMN),
        c["model_from_column"]
    );
    assert_eq!(
        model_from_column("org/model/direct"),
        c["model_from_column_multi"]
    );
    assert_eq!(model_from_column("noslash"), c["model_from_column_noslash"]);
    assert_eq!(classify::MAX_OUTPUT_TOKENS, 512);
    assert_eq!(c["max_output_tokens"], 512);
    assert_eq!(classify::TEMPERATURE, 0.0);
    assert_eq!(classify::RETRIES, 1);
    assert_eq!(c["retries"], 1);
    assert_eq!(
        c["default_max_input_chars"],
        classify::DEFAULT_MAX_INPUT_CHARS
    );
    assert_eq!(
        c["truncated_confidence_cap"],
        classify::TRUNCATED_CONFIDENCE_CAP
    );
    assert_eq!(c["head_numerator"], 4);
    assert_eq!(c["head_denominator"], 5);
    assert_eq!(c["alert_kind"], ALERT_KIND);
    assert_eq!(ALERT_SEVERITY, "critical");
    assert_eq!(c["degraded_failure_rate"], DEGRADED_FAILURE_RATE);
    assert_eq!(c["degraded_min_samples"], DEGRADED_MIN_SAMPLES);
    assert_eq!(c["degraded_window_seconds"], DEGRADED_WINDOW_SECONDS);
    // The per-attempt 5 s timeout is replaced by one 1.5 s total deadline (D3), owned
    // by the classifier client.
    let _ = parity_support::divergence!("D3");
    assert_eq!(c["default_timeout_ms"], 5000);
}

/// parity: classify.TestResponseSchemaEnumeratesCapabilities
#[test]
fn response_schema_enumerates_capabilities() {
    let c = constants();
    assert_eq!(classify::RESPONSE_SCHEMA_JSON, c["response_schema_json"]);
    assert_eq!(
        classify::RESPONSE_SCHEMA_JSON,
        text_file("response_schema.json").trim_end()
    );
    let schema = response_schema();
    let enumerated: Vec<&str> = schema["properties"]["capability"]["enum"]
        .as_array()
        .expect("enum")
        .iter()
        .map(|v| v.as_str().expect("capability"))
        .collect();
    assert_eq!(enumerated, CAPABILITIES);
}

/// parity: classify.TestFallbackClassification
#[test]
fn fallback_classification() {
    let want = &constants()["fallback"];
    let fallback = Classification::fallback();
    assert_eq!(fallback.capability, want["capability"]);
    assert_eq!(fallback.difficulty, want["difficulty"]);
    assert_eq!(fallback.domain_tags, Vec::<String>::new());
    assert_eq!(fallback.confidence, want["confidence"]);
    assert!(!fallback.truncated);
    assert_eq!(
        fallback.cache_json(),
        r#"{"capability":"multi_step_reasoning","difficulty":3,"domain_tags":[],"confidence":0}"#
    );
}

/// Every classify_parse row: the fence strip and the accept or reject decision with its
/// exact error.
///
/// parity: classify.TestClassifyInvalidOutputFails
/// parity: classify.TestClassifyBoundaryOutputs
#[test]
fn parse_rows() {
    for case in fixture("classify_parse.json")["cases"]
        .as_array()
        .expect("cases")
    {
        let id = case["id"].as_str().expect("id");
        let input = case["input"].as_str().expect("input");
        assert_eq!(strip_code_fence(input), case["strip_code_fence"], "{id}");
        let expected = &case["expected"];
        match parse(input) {
            Ok(got) => {
                assert_eq!(expected["ok"], true, "{id}: accepted");
                let want = &expected["classification"];
                assert_eq!(got.capability, want["capability"], "{id}");
                assert_eq!(got.difficulty, want["difficulty"], "{id}");
                assert_eq!(
                    serde_json::json!(got.domain_tags),
                    want["domain_tags"],
                    "{id}"
                );
                assert_eq!(got.confidence, want["confidence"], "{id}");
                assert!(!got.truncated);
            }
            Err(error) => {
                assert_eq!(expected["ok"], false, "{id}: rejected with {error}");
                assert_eq!(error, expected["go_error"], "{id}");
            }
        }
    }
}

/// parity: classify.TestClassifyTruncation
#[test]
fn truncation_rows() {
    for case in fixture("classify_truncate.json")["cases"]
        .as_array()
        .expect("cases")
    {
        let id = case["id"].as_str().expect("id");
        let input = &case["input"];
        let max = usize::try_from(input["max_chars"].as_u64().expect("max")).expect("fits");
        let raw = input["prompt"].as_str().expect("prompt");
        // `c*n` stands for the letter repeated n times.
        let prompt = match raw.split_once('*') {
            Some((letter, n)) if id == "default_budget_100001" => {
                letter.repeat(n.parse().expect("count"))
            }
            _ => raw.to_owned(),
        };
        let (text, truncated) = truncate(&prompt, max);
        let expected = &case["expected"];
        assert_eq!(truncated, expected["truncated"], "{id}");
        if let Some(want) = expected.get("text") {
            assert_eq!(text, *want, "{id}");
            assert_eq!(fence(&text), expected["fenced"], "{id}");
        } else {
            assert_eq!(text.chars().count(), expected["rune_len"], "{id}");
            let head =
                usize::try_from(expected["head_runes"].as_u64().expect("head")).expect("fits");
            let tail =
                usize::try_from(expected["tail_runes"].as_u64().expect("tail")).expect("fits");
            assert_eq!(
                text,
                format!(
                    "{}{}{}",
                    "a".repeat(head),
                    classify::TRUNCATION_MARKER,
                    "a".repeat(tail)
                )
            );
        }
    }
}

#[test]
fn truncated_classifications_cap_confidence() {
    let mut sure = Classification {
        confidence: 0.95,
        ..Classification::fallback()
    };
    sure.mark_truncated();
    assert!(sure.truncated);
    assert_eq!(sure.confidence, classify::TRUNCATED_CONFIDENCE_CAP);
    assert_eq!(
        sure.cache_json(),
        r#"{"capability":"multi_step_reasoning","difficulty":3,"domain_tags":[],"confidence":0.8,"truncated":true}"#
    );
    let mut unsure = Classification {
        confidence: 0.5,
        domain_tags: vec!["legal".to_owned()],
        ..Classification::fallback()
    };
    unsure.mark_truncated();
    assert_eq!(unsure.confidence, 0.5);
    assert!(unsure.high_stakes());
    assert!(!Classification::fallback().high_stakes());
}

#[test]
fn column_refs_split_at_the_last_slash() {
    assert_eq!(
        parse_column_ref("claude-haiku-4-5/direct"),
        Ok(("claude-haiku-4-5", "direct"))
    );
    assert_eq!(
        parse_column_ref("org/model/vertex"),
        Ok(("org/model", "vertex"))
    );
    for bad in ["noslash", "/direct", "model/", ""] {
        assert_eq!(
            parse_column_ref(bad),
            Err(format!(
                "classify: classifier column ref {} is not \"model/endpoint\"",
                maverick_domain::gofmt::quote(bad)
            )),
            "{bad}"
        );
    }
}

fn at(value: &Value) -> DateTime<Utc> {
    value["at"].as_str().expect("at").parse().expect("RFC 3339")
}

/// The degraded tracker's exact event sequences: 1/20 does not fire, 2/21 fires once,
/// later failures in the same episode do not, and it re-fires after recovery.
///
/// parity: classify.TestResolveDegradedAlert
#[test]
fn degraded_tracker_rows() {
    let doc = fixture("classify_degraded_tracker.json");
    for case in doc["cases"].as_array().expect("cases") {
        let mut tracker = DegradedTracker::new();
        for (i, event) in case["events"]
            .as_array()
            .expect("events")
            .iter()
            .enumerate()
        {
            let got = tracker.record(at(event), event["failed"].as_bool().expect("failed"));
            let context = format!("{} event {i}", case["id"]);
            assert_eq!(got.samples, event["samples"], "{context}");
            parity_support::assert_f64_within(
                got.rate,
                event["rate"].as_f64().expect("rate"),
                1e-12,
            );
            assert_eq!(got.fire, event["fire"], "{context}");
        }
    }
    let rate = 2.0 / 21.0;
    assert_eq!(
        degraded_alert_message("claude-haiku-4-5/direct/v2", rate),
        doc["alert_message_example"]
    );
    assert_eq!(
        degraded_alert_subject("claude-haiku-4-5/direct/v2", rate, 21),
        Ok(format!(
            r#"{{"classifier_version":"claude-haiku-4-5/direct/v2","failure_rate":{},"samples":21,"window_seconds":900}}"#,
            rate
        ))
    );
    assert!(degraded_alert_subject("v", f64::NAN, 1).is_err());
}
