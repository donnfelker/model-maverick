//! Parity: `parity/fixtures/measurement-core/penalties.json` and `orderswap.json`
//! (`internal/scoring`).

use std::collections::BTreeMap;

use maverick_domain::scoring::{
    LAMBDA_AMB, LAMBDA_VAR, ambiguity, ambiguity_penalty, order_swap_observations,
    shared_dimension_spread, variance_penalty,
};
use parity_support::assert_f64_within;
use proptest::prelude::*;
use serde_json::Value;

fn penalties() -> Value {
    parity_support::fixture("measurement-core", "penalties.json").expect("penalties.json")
}

fn tolerance() -> f64 {
    penalties()["tolerance"].as_f64().expect("tolerance")
}

fn row(section: &str, id: &str) -> Value {
    penalties()[section]
        .as_array()
        .expect("section")
        .iter()
        .find(|r| r["id"] == id)
        .unwrap_or_else(|| panic!("no row {section}/{id}"))
        .clone()
}

fn floats(value: &Value) -> Vec<f64> {
    serde_json::from_value(value.clone()).expect("floats")
}

fn check_variance(id: &str) {
    let row = row("variance_penalty", id);
    let got = variance_penalty(
        &floats(&row["q_runs"]),
        row["lambda_var"].as_f64().expect("λ"),
    );
    let want = &row["expected"];
    let tol = tolerance();
    assert_eq!(got.ok, want["ok"], "{id}");
    assert_eq!(got.n as u64, want["n"], "{id}");
    assert_f64_within(got.score, want["score"].as_f64().expect("score"), tol);
    assert_f64_within(got.mu, want["mu"].as_f64().expect("mu"), tol);
    assert_f64_within(got.sigma, want["sigma"].as_f64().expect("sigma"), tol);
}

/// parity: scoring.TestVariancePenalty
#[test]
fn variance_three_repeats() {
    check_variance("three_repeats");
}

/// parity: scoring.TestVariancePenaltySingleRepeat
#[test]
fn variance_single_repeat() {
    check_variance("single_repeat");
}

/// parity: scoring.TestVariancePenaltyFloorsAtZero
#[test]
fn variance_floors_at_zero() {
    check_variance("floors_at_zero");
}

/// parity: scoring.TestVariancePenaltyNoGradableRepeats
#[test]
fn variance_no_gradable_repeats() {
    check_variance("no_gradable");
}

/// Every variance row, including the extras.
#[test]
fn every_variance_row() {
    for row in penalties()["variance_penalty"].as_array().expect("rows") {
        check_variance(row["id"].as_str().expect("id"));
    }
}

fn check_spread(id: &str) {
    let row = row("shared_dimension_spread", id);
    let dims: Vec<Option<BTreeMap<String, f64>>> =
        serde_json::from_value(row["grader_dims"].clone()).expect("dims");
    let got = shared_dimension_spread(&dims);
    let want = &row["expected"];
    assert_eq!(got.is_some(), want["ok"] == true, "{id}");
    assert_f64_within(
        got.unwrap_or(0.0),
        want["spread"].as_f64().expect("spread"),
        tolerance(),
    );
}

/// parity: scoring.TestSharedDimensionSpread
#[test]
fn spread_one_shared() {
    check_spread("one_shared");
}

/// parity: scoring.TestSharedDimensionSpreadMultipleSharedDims
#[test]
fn spread_two_shared() {
    check_spread("two_shared");
}

/// parity: scoring.TestSharedDimensionSpreadNoSharedDimension
#[test]
fn spread_none_shared() {
    check_spread("none_shared");
}

/// parity: scoring.TestJudgeOrderSwapDisagreement
#[test]
fn spread_judge_order_swap() {
    check_spread("judge_order_swap");
}

/// A judge's two calls plus a deterministic grader: the judge's disagreement is the
/// spread, then it feeds ambiguity and the penalty.
///
/// parity: scoring.TestOrderSwapDisagreementFeedsAmbiguity
#[test]
fn order_swap_feeds_ambiguity() {
    check_spread("order_swap_plus_deterministic");
    let a = row("ambiguity", "order_swap");
    assert_f64_within(
        ambiguity(&floats(&a["spreads"])),
        a["expected"].as_f64().expect("a"),
        tolerance(),
    );
    check_penalty("order_swap");
    let style_pair = orderswap_case("style pair");
    check_orderswap(&style_pair);
}

/// Every spread row, including the extras.
#[test]
fn every_spread_row() {
    for row in penalties()["shared_dimension_spread"]
        .as_array()
        .expect("rows")
    {
        check_spread(row["id"].as_str().expect("id"));
    }
}

/// parity: scoring.TestAmbiguity
#[test]
fn ambiguity_rows() {
    for id in ["three", "empty"] {
        let row = row("ambiguity", id);
        assert_f64_within(
            ambiguity(&floats(&row["spreads"])),
            row["expected"].as_f64().expect("a"),
            tolerance(),
        );
    }
}

fn check_penalty(id: &str) {
    let row = row("ambiguity_penalty", id);
    assert_f64_within(
        ambiguity_penalty(
            row["task_score"].as_f64().expect("score"),
            row["ambiguity"].as_f64().expect("a"),
            row["lambda_amb"].as_f64().expect("λ"),
        ),
        row["expected"].as_f64().expect("expected"),
        tolerance(),
    );
}

/// parity: scoring.TestAmbiguityPenalty
#[test]
fn ambiguity_penalty_default_lambda() {
    check_penalty("default_lambda");
}

/// parity: scoring.TestAmbiguityPenaltyFloorsAtZero
#[test]
fn ambiguity_penalty_floors_at_zero() {
    check_penalty("floors_at_zero");
}

/// The task-score pipeline the jobs test drives: variance, then ambiguity.
#[test]
fn task_pipeline() {
    for row in penalties()["task_pipeline"].as_array().expect("rows") {
        let variance = variance_penalty(
            &floats(&row["q_runs"]),
            row["lambda_var"].as_f64().expect("λ"),
        );
        let a = ambiguity(&floats(&row["spreads"]));
        let score = ambiguity_penalty(variance.score, a, row["lambda_amb"].as_f64().expect("λ"));
        let want = &row["expected"];
        let tol = tolerance();
        assert_f64_within(
            variance.score,
            want["variance_score"].as_f64().expect("v"),
            tol,
        );
        assert_f64_within(a, want["ambiguity"].as_f64().expect("a"), tol);
        assert_f64_within(score, want["task_score"].as_f64().expect("s"), tol);
    }
}

fn orderswap_case(id: &str) -> Value {
    parity_support::fixture("measurement-core", "orderswap.json").expect("orderswap.json")["cases"]
        .as_array()
        .expect("cases")
        .iter()
        .find(|c| c["id"] == id)
        .unwrap_or_else(|| panic!("no case {id}"))
        .clone()
}

fn check_orderswap(case: &Value) {
    let got = order_swap_observations(case["detail"].as_str());
    let want = &case["expected"];
    assert_eq!(got.is_some(), want["ok"] == true, "{}", case["id"]);
    let want_calls: Option<Vec<BTreeMap<String, f64>>> =
        serde_json::from_value(want["calls"].clone()).expect("calls");
    assert_eq!(got, want_calls, "{}", case["id"]);
}

/// parity: scoring.TestOrderSwapObservations
/// parity: scoring.TestOrderSwapObservations/nil_detail
/// parity: scoring.TestOrderSwapObservations/empty_detail
/// parity: scoring.TestOrderSwapObservations/no_order_swap
/// parity: scoring.TestOrderSwapObservations/single_call
/// parity: scoring.TestOrderSwapObservations/call_w/o_scores
/// parity: scoring.TestOrderSwapObservations/not_an_array
/// parity: scoring.TestOrderSwapObservations/malformed_json
/// parity: scoring.TestOrderSwapObservations/non-object_array
#[test]
fn order_swap_observation_rows() {
    let doc =
        parity_support::fixture("measurement-core", "orderswap.json").expect("orderswap.json");
    for case in doc["cases"].as_array().expect("cases") {
        check_orderswap(case);
    }
}

proptest! {
    /// scoring.TestVariancePenaltySigmaMatchesDirectFormula (reference Part 3 §1.5): the
    /// reported sigma equals the two-pass population standard deviation within 1e-12.
    ///
    /// parity: scoring.TestVariancePenaltySigmaMatchesDirectFormula
    #[test]
    fn variance_penalty_sigma_matches_direct_formula(
        q_runs in prop::collection::vec(0.0f64..=1.0, 1..=20),
    ) {
        let got = variance_penalty(&q_runs, LAMBDA_VAR);
        let n = q_runs.len() as f64;
        let mean = q_runs.iter().sum::<f64>() / n;
        let direct = (q_runs.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n).sqrt();
        prop_assert!((got.sigma - direct).abs() <= 1e-12);
    }

    /// scoring.TestSharedDimensionSpreadBoundsProperty (reference Part 3 §1.6): 2 to 4
    /// graders scoring one shared dimension in [0, 1] give a spread in [0, 0.5].
    ///
    /// parity: scoring.TestSharedDimensionSpreadBoundsProperty
    #[test]
    fn shared_dimension_spread_is_bounded(
        scores in prop::collection::vec(0.0f64..=1.0, 2..=4),
    ) {
        let grader_dims: Vec<Option<BTreeMap<String, f64>>> = scores
            .iter()
            .map(|&s| Some(BTreeMap::from([("dim".to_owned(), s)])))
            .collect();
        let spread = shared_dimension_spread(&grader_dims).expect("a shared dimension");
        prop_assert!((0.0..=0.5 + 1e-12).contains(&spread));
    }

    /// scoring.TestPenaltyProperties (reference Part 3 §1.5, §1.6): sigma is non-negative,
    /// the variance-penalized score is in [0, mu] (+1e-15), and the ambiguity-penalized
    /// score is in [0, score].
    ///
    /// parity: scoring.TestPenaltyProperties
    #[test]
    fn penalties_stay_in_range(
        q_runs in prop::collection::vec(0.0f64..=1.0, 1..=20),
        ambiguity_value in 0.0f64..=1.0,
    ) {
        let v = variance_penalty(&q_runs, LAMBDA_VAR);
        prop_assert!(v.sigma >= 0.0);
        prop_assert!(v.score >= 0.0);
        prop_assert!(v.score <= v.mu + 1e-15);
        let penalized = ambiguity_penalty(v.score, ambiguity_value, LAMBDA_AMB);
        prop_assert!(penalized >= 0.0);
        prop_assert!(penalized <= v.score);
    }
}
