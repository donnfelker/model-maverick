//! Parity: `parity/fixtures/measurement-core/run_quality.json` and `overlay.json`
//! (`internal/scoring`).

use std::collections::BTreeMap;

use maverick_domain::scoring::{
    RunScore, apply_autofail, autofail, high_stakes, overlay_weights, run_quality,
};
use serde_json::Value;

fn fixture(file: &str) -> Value {
    parity_support::fixture("measurement-core", file).expect("fixture loads")
}

fn weights(value: &Value) -> BTreeMap<String, f64> {
    serde_json::from_value(value.clone()).expect("weights")
}

fn grader_dims(value: &Value) -> Vec<Option<BTreeMap<String, f64>>> {
    serde_json::from_value(value.clone()).expect("grader dims")
}

/// Compares a score with a Go `RunScore` row. Go stored `q: 0` for provider errors and
/// timeouts; Rust has no `q_run` for them (D6).
fn assert_score(got: &RunScore, want: &Value, status: &str, tolerance: f64, context: &str) {
    assert_eq!(got.gradable, want["gradable"], "{context}: gradable");
    assert_eq!(got.pass, want["pass"], "{context}: pass");
    let want_q = want["q"].as_f64().expect("q");
    if status == "provider_error" || status == "timeout" {
        let _ = parity_support::divergence!("D6");
        assert_eq!(want_q, 0.0, "{context}");
        assert_eq!(got.q, None, "{context}: error runs have no q_run");
    } else {
        let q = got.q.unwrap_or_else(|| panic!("{context}: q_run"));
        parity_support::assert_f64_within(q, want_q, tolerance);
    }
    parity_support::assert_f64_within(
        got.weight_covered,
        want["weight_covered"].as_f64().expect("covered"),
        tolerance,
    );
    let want_dims = weights(&want["dimensions"]);
    assert_eq!(
        got.dimensions.keys().collect::<Vec<_>>(),
        want_dims.keys().collect::<Vec<_>>(),
        "{context}: dimensions"
    );
    for (dim, w) in &want_dims {
        parity_support::assert_f64_within(got.dimensions[dim], *w, tolerance);
    }
}

fn run_quality_case(id: &str) -> Value {
    let doc = fixture("run_quality.json");
    doc["cases"]
        .as_array()
        .expect("cases")
        .iter()
        .find(|c| c["id"] == id)
        .unwrap_or_else(|| panic!("no case {id}"))
        .clone()
}

fn check_run_quality(id: &str) {
    let tolerance = fixture("run_quality.json")["tolerance"]
        .as_f64()
        .expect("tolerance");
    let case = run_quality_case(id);
    let input = &case["input"];
    let status = input["status"].as_str().expect("status");
    let score = run_quality(
        &weights(&input["weights"]),
        &grader_dims(&input["grader_dims"]),
        status,
        input["pass_threshold"].as_f64().expect("threshold"),
    );
    assert_score(&score, &case["expected"], status, tolerance, id);
}

/// parity: scoring.TestRunQualityWeightedSum
#[test]
fn weighted_sum() {
    check_run_quality("weighted_sum");
}

/// parity: scoring.TestRunQualityRenormalizesAbsentDimensions
#[test]
fn renormalizes_absent() {
    check_run_quality("renormalizes_absent");
}

/// parity: scoring.TestRunQualitySharedDimensionAveraged
#[test]
fn shared_dimension_averaged() {
    check_run_quality("shared_dimension_averaged");
}

/// parity: scoring.TestRunQualityStatusHandling
#[test]
fn status_handling() {
    check_run_quality("status_provider_error");
    check_run_quality("status_timeout");
    check_run_quality("status_parse_failure_graded");
}

/// parity: scoring.TestRunQualityAllGradersAbsent
#[test]
fn all_graders_absent() {
    check_run_quality("all_graders_absent");
}

/// parity: scoring.TestRunQualityIgnoresUnweightedDimensions
#[test]
fn ignores_unweighted() {
    check_run_quality("ignores_unweighted");
}

/// parity: scoring.TestRunQualityPassAtExactThreshold
#[test]
fn pass_at_exact_threshold() {
    check_run_quality("pass_at_exact_threshold");
}

/// parity: scoring.TestOrderSwapMeanOnlyForRunQuality
#[test]
fn order_swap_mean_only() {
    check_run_quality("order_swap_mean_only");
}

/// Every run_quality row, including the extra rows that are not Go tests.
#[test]
fn every_run_quality_row() {
    for case in fixture("run_quality.json")["cases"]
        .as_array()
        .expect("cases")
    {
        check_run_quality(case["id"].as_str().expect("id"));
    }
}

fn details(value: &Value) -> Vec<Option<String>> {
    match value {
        Value::Null => Vec::new(),
        Value::Array(items) => items
            .iter()
            .map(|d| d.as_str().map(str::to_owned))
            .collect(),
        other => panic!("details_raw: {other}"),
    }
}

fn borrowed(details: &[Option<String>]) -> Vec<Option<&str>> {
    details.iter().map(|d| d.as_deref()).collect()
}

/// parity: scoring.TestAutofail
/// parity: scoring.TestAutofail/no_details
/// parity: scoring.TestAutofail/nil_detail
/// parity: scoring.TestAutofail/empty_object
/// parity: scoring.TestAutofail/malformed_detail
/// parity: scoring.TestAutofail/critical_rule_failed
/// parity: scoring.TestAutofail/critical_rule_passed
/// parity: scoring.TestAutofail/forbidden_fact_with_autofail_found
/// parity: scoring.TestAutofail/forbidden_fact_without_autofail
/// parity: scoring.TestAutofail/autofail_fact_absent_from_output
/// parity: scoring.TestAutofail/marker_in_later_grader
#[test]
fn autofail_rows() {
    for row in fixture("overlay.json")["autofail"]
        .as_array()
        .expect("rows")
    {
        let details = details(&row["details_raw"]);
        assert_eq!(
            autofail(&borrowed(&details)),
            row["expected"],
            "{}",
            row["id"]
        );
    }
}

fn run_score(value: &Value) -> RunScore {
    RunScore {
        q: Some(value["q"].as_f64().expect("q")),
        pass: value["pass"].as_bool().expect("pass"),
        gradable: value["gradable"].as_bool().expect("gradable"),
        dimensions: weights(&value["dimensions"]),
        weight_covered: value["weight_covered"].as_f64().expect("covered"),
    }
}

/// parity: scoring.TestApplyAutofailZeroesRun
#[test]
fn apply_autofail_rows() {
    let doc = fixture("overlay.json");
    let tolerance = doc["tolerance"].as_f64().expect("tolerance");
    for row in doc["apply_autofail"].as_array().expect("rows") {
        let mut score = run_score(&row["run_score"]);
        apply_autofail(&mut score, &borrowed(&details(&row["details_raw"])));
        let id = row["id"].as_str().expect("id");
        assert_score(&score, &row["expected"], "ok", tolerance, id);
    }
    // The overlay weights this Go test used.
    let answer = doc["overlay_weights"]
        .as_array()
        .expect("rows")
        .iter()
        .find(|r| r["id"] == "answer_plus_constraint")
        .expect("row")
        .clone();
    assert_eq!(
        overlay_weights(&weights(&answer["input"])),
        weights(&answer["expected"])
    );
}

/// parity: scoring.TestHighStakes
#[test]
fn high_stakes_rows() {
    for case in fixture("overlay.json")["high_stakes"]["cases"]
        .as_array()
        .expect("cases")
    {
        let tags: Vec<String> = match &case["domain_tags"] {
            Value::Null => Vec::new(),
            tags => serde_json::from_value(tags.clone()).expect("tags"),
        };
        assert_eq!(high_stakes(&tags), case["expected"], "{tags:?}");
    }
}

/// parity: scoring.TestOverlayWeightsStructuredExtraction
/// parity: scoring.TestOverlayWeightsNativeCalibrationClass
#[test]
fn overlay_weight_rows() {
    let doc = fixture("overlay.json");
    let tolerance = doc["tolerance"].as_f64().expect("tolerance");
    for row in doc["overlay_weights"].as_array().expect("rows") {
        let got = overlay_weights(&weights(&row["input"]));
        let want = weights(&row["expected"]);
        assert_eq!(
            got.keys().collect::<Vec<_>>(),
            want.keys().collect::<Vec<_>>(),
            "{}",
            row["id"]
        );
        for (dim, w) in &want {
            parity_support::assert_f64_within(got[dim], *w, tolerance);
        }
    }
}

/// The same output scored natively and under the overlay.
///
/// parity: scoring.TestOverlayGoldenSameOutputWithAndWithoutOverlay
#[test]
fn overlay_golden_pair() {
    let doc = fixture("overlay.json");
    let tolerance = doc["tolerance"].as_f64().expect("tolerance");
    let pair = &doc["overlay_golden_pair"];
    let status = pair["status"].as_str().expect("status");
    let threshold = pair["pass_threshold"].as_f64().expect("threshold");
    let cases = pair["cases"].as_array().expect("cases");
    for case in cases {
        let id = case["id"].as_str().expect("id");
        let score = run_quality(
            &weights(&case["weights"]),
            &grader_dims(&case["grader_dims"]),
            status,
            threshold,
        );
        assert_score(&score, &case["expected"], status, tolerance, id);
    }
    // The overlay rows' weights are overlay_weights of the native row's weights.
    let native = weights(&cases[0]["weights"]);
    let overlaid = overlay_weights(&native);
    let recorded = weights(&cases[1]["weights"]);
    for (dim, w) in &recorded {
        parity_support::assert_f64_within(overlaid[dim], *w, tolerance);
    }
}
