//! Parity: `parity/fixtures/measurement-core/predictions.json` (`internal/predictions`).

use std::collections::BTreeMap;

use maverick_domain::predictions::{Cell, Pair, brier, derive, overconfidence};
use maverick_domain::scoring::f32_round;
use serde_json::Value;

fn fixture() -> Value {
    parity_support::fixture("measurement-core", "predictions.json").expect("fixture loads")
}

fn number(value: &Value, key: &str) -> f64 {
    value[key]
        .as_f64()
        .unwrap_or_else(|| panic!("{key}: {value}"))
}

fn pairs(value: &Value) -> Vec<Pair> {
    value
        .as_array()
        .expect("pairs")
        .iter()
        .map(|pair| Pair {
            p: number(pair, "p"),
            o: pair["o"].as_bool().expect("o"),
        })
        .collect()
}

fn case(id: &str) -> Value {
    fixture()["brier_and_overconfidence"]["cases"]
        .as_array()
        .expect("cases")
        .iter()
        .find(|c| c["id"] == id)
        .unwrap_or_else(|| panic!("no case {id}"))
        .clone()
}

/// Checks both values and their persisted float32 forms.
fn check(id: &str) {
    let tolerance = number(&fixture(), "tolerance");
    let case = case(id);
    let pairs = pairs(&case["pairs"]);
    let expected = &case["expected"];
    let got_brier = brier(&pairs);
    let got_over = overconfidence(&pairs);
    parity_support::assert_f64_within(got_brier, number(expected, "brier"), tolerance);
    parity_support::assert_f64_within(got_over, number(expected, "overconfidence"), tolerance);
    assert_eq!(f32_round(got_brier), number(expected, "brier_f32"), "{id}");
    assert_eq!(
        f32_round(got_over),
        number(expected, "overconfidence_f32"),
        "{id}"
    );
}

/// parity: predictions.TestBrierGolden
/// parity: predictions.TestBrierGolden/mixed_hand-computed
/// parity: predictions.TestBrierGolden/perfect
/// parity: predictions.TestBrierGolden/perfectly_wrong
/// parity: predictions.TestBrierGolden/uninformative
/// parity: predictions.TestBrierGolden/single_pair
/// parity: predictions.TestBrierGolden/no_pairs
#[test]
fn brier_golden() {
    for id in [
        "mixed hand-computed",
        "perfect",
        "perfectly wrong",
        "uninformative",
        "single pair",
        "no pairs",
    ] {
        check(id);
    }
}

/// The Go "calibrated" row (one pass and one fail at 0.5) is the fixture's
/// "uninformative" case.
///
/// parity: predictions.TestOverconfidenceGolden
/// parity: predictions.TestOverconfidenceGolden/mixed_hand-computed
/// parity: predictions.TestOverconfidenceGolden/underconfident
/// parity: predictions.TestOverconfidenceGolden/overconfident
/// parity: predictions.TestOverconfidenceGolden/calibrated
/// parity: predictions.TestOverconfidenceGolden/no_pairs
#[test]
fn overconfidence_golden() {
    for id in [
        "mixed hand-computed",
        "underconfident",
        "overconfident",
        "uninformative",
        "no pairs",
    ] {
        check(id);
    }
}

/// Every row, including the per-capability rows taken from the jobs tests.
#[test]
fn every_brier_and_overconfidence_row() {
    for case in fixture()["brier_and_overconfidence"]["cases"]
        .as_array()
        .expect("cases")
    {
        check(case["id"].as_str().expect("id"));
    }
}

fn cell(value: &Value) -> Cell {
    Cell {
        model_column_id: value["model_column_id"]
            .as_str()
            .expect("column")
            .to_owned(),
        capability: value["capability"].as_str().expect("capability").to_owned(),
        difficulty_tier: value["difficulty_tier"].as_i64().expect("tier"),
    }
}

fn cells(value: &Value) -> Vec<Cell> {
    value.as_array().expect("cells").iter().map(cell).collect()
}

fn ewma(value: &Value) -> BTreeMap<Cell, f64> {
    value
        .as_array()
        .expect("ewma")
        .iter()
        .map(|row| (cell(row), number(row, "ewma")))
        .collect()
}

fn check_derive(case: &Value) {
    let tolerance = number(&fixture(), "tolerance");
    let got = derive(&cells(&case["scheduled"]), &ewma(&case["pass_rate_ewma"]));
    let want = case["expected"].as_array().expect("expected");
    assert_eq!(got.len(), want.len(), "{got:?}");
    for (got, want) in got.iter().zip(want) {
        assert_eq!(got.cell, cell(want));
        parity_support::assert_f64_within(got.pass_prob, number(want, "pass_prob"), tolerance);
    }
}

/// parity: predictions.TestDeriveGolden
#[test]
fn derive_golden() {
    check_derive(&fixture()["derive"]);
}

/// parity: predictions.TestDeriveEmpty
#[test]
fn derive_empty() {
    for case in fixture()["derive_empty"].as_array().expect("cases") {
        check_derive(case);
    }
}
