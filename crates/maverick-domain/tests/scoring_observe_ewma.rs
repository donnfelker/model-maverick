//! Parity: `parity/fixtures/measurement-core/observe.json` and `ewma.json`
//! (`internal/scoring`).

use std::collections::BTreeMap;

use maverick_domain::scoring::{
    ALPHA, RunStats, Stream, TaskOutcome, dimension, fold_observation, observe, percentile,
};
use parity_support::assert_f64_within;
use proptest::prelude::*;
use serde_json::Value;

fn fixture(file: &str) -> Value {
    parity_support::fixture("measurement-core", file).expect("fixture loads")
}

fn tasks(value: &Value) -> Vec<TaskOutcome> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|t| TaskOutcome {
                    score: t["score"].as_f64().expect("score"),
                    ambiguity: t["ambiguity"].as_f64().expect("ambiguity"),
                    gradable: t["gradable"].as_bool().expect("gradable"),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Go's runs carry no status; every fixture run counts as having returned output.
fn runs(value: &Value) -> Vec<RunStats> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|r| RunStats {
                    pass: r["pass"].as_bool().expect("pass"),
                    latency_ms: r["latency_ms"].as_i64().map(|l| l as i32),
                    cost_usd: r["cost_usd"].as_f64(),
                    returned_output: true,
                })
                .collect()
        })
        .unwrap_or_default()
}

fn check_observe(id: &str) {
    let doc = fixture("observe.json");
    let tolerance = doc["tolerance"].as_f64().expect("tolerance");
    let row = doc["observe"]
        .as_array()
        .expect("rows")
        .iter()
        .find(|r| r["id"] == id)
        .unwrap_or_else(|| panic!("no row {id}"))
        .clone();
    let runs = runs(&row["runs"]);
    let mut got = observe(&tasks(&row["tasks"]), &runs);
    // D6: availability is new. Every fixture run returned output, so it is 1 whenever
    // there are runs; Go has no such dimension.
    let _ = parity_support::divergence!("D6");
    match got.remove(dimension::AVAILABILITY) {
        Some(availability) => {
            assert!(!runs.is_empty(), "{id}");
            assert_eq!(availability, 1.0, "{id}");
        }
        None => assert!(runs.is_empty(), "{id}"),
    }
    let want: BTreeMap<String, f64> =
        serde_json::from_value(row["expected"].clone()).expect("expected");
    assert_eq!(
        got.keys().map(|k| (*k).to_owned()).collect::<Vec<_>>(),
        want.keys().cloned().collect::<Vec<_>>(),
        "{id}"
    );
    for (dim, w) in &want {
        assert_f64_within(got[dim.as_str()], *w, tolerance);
    }
}

/// parity: scoring.TestObserve
#[test]
fn observe_full() {
    check_observe("full");
}

/// parity: scoring.TestObserveSingleRunPercentiles
#[test]
fn observe_single_run_percentiles() {
    check_observe("single_run_percentiles");
}

/// parity: scoring.TestObserveSparseData
#[test]
fn observe_sparse() {
    check_observe("sparse");
}

/// parity: scoring.TestObserveEmpty
#[test]
fn observe_empty() {
    check_observe("empty");
}

/// Every observe row (including the extras) and the percentile table.
#[test]
fn every_observe_row_and_percentile() {
    let doc = fixture("observe.json");
    for row in doc["observe"].as_array().expect("rows") {
        check_observe(row["id"].as_str().expect("id"));
    }
    for case in doc["percentile"]["cases"].as_array().expect("cases") {
        let sorted: Vec<f64> = serde_json::from_value(case["sorted"].clone()).expect("sorted");
        let p = case["p"].as_f64().expect("p");
        assert_eq!(
            percentile(&sorted, p),
            case["expected"].as_f64().expect("expected"),
            "{case}"
        );
    }
}

/// parity: scoring.TestStreamEmpty
#[test]
fn stream_empty() {
    let want = &fixture("ewma.json")["empty_stream"];
    let stream = Stream::new(ALPHA);
    assert_eq!(stream.ewma(), want["ewma"].as_f64().expect("ewma"));
    assert_eq!(
        stream.variance(),
        want["variance"].as_f64().expect("variance")
    );
    assert_eq!(stream.n(), want["n"].as_u64().expect("n"));
    assert_eq!(stream.measured(3), want["measured_min3"]);
}

fn check_steps(alpha: f64, steps: &Value, tolerance: f64) {
    let mut stream = Stream::new(alpha);
    for step in steps.as_array().expect("steps") {
        stream.fold(step["x"].as_f64().expect("x"));
        assert_f64_within(
            stream.ewma(),
            step["ewma"].as_f64().expect("ewma"),
            tolerance,
        );
        assert_f64_within(
            stream.variance(),
            step["variance"].as_f64().expect("variance"),
            tolerance,
        );
        if let Some(n) = step["n"].as_u64() {
            assert_eq!(stream.n(), n);
        }
        if let Some(measured) = step["measured_min3"].as_bool() {
            assert_eq!(stream.measured(3), measured);
        }
    }
}

/// parity: scoring.TestStreamSeedAndFolds
#[test]
fn stream_seed_and_folds() {
    let doc = fixture("ewma.json");
    let tolerance = doc["tolerance"].as_f64().expect("tolerance");
    let case = &doc["stream_seed_and_folds"];
    check_steps(
        case["alpha"].as_f64().expect("alpha"),
        &case["steps"],
        tolerance,
    );
    // alpha = 1: the EWMA is the latest observation.
    let one = &doc["alpha_one"];
    check_steps(
        one["alpha"].as_f64().expect("alpha"),
        &one["steps"],
        tolerance,
    );
}

/// parity: scoring.TestStreamMeasuredFlipsAtMinNights
#[test]
fn stream_measured_flips_at_min_nights() {
    let case = &fixture("ewma.json")["measured_flip"];
    let min = case["min_nights"].as_u64().expect("min");
    let x = case["x_each_night"].as_f64().expect("x");
    let mut stream = Stream::new(case["alpha"].as_f64().expect("alpha"));
    for want in case["measured_by_night"].as_array().expect("nights") {
        stream.fold(x);
        assert_eq!(stream.measured(min), *want);
    }
}

/// parity: scoring.TestResumeReconstructsObservations
#[test]
fn resume_reconstructs_observations() {
    let case = &fixture("ewma.json")["resume"];
    let tolerance = case["tolerance"].as_f64().expect("tolerance");
    let series: Vec<f64> = serde_json::from_value(case["series"].clone()).expect("series");
    let mut stream = Stream::resume(case["alpha"].as_f64().expect("alpha"), &series);
    let want = &case["expected"];
    assert_f64_within(
        stream.ewma(),
        want["ewma"].as_f64().expect("ewma"),
        tolerance,
    );
    assert_f64_within(
        stream.variance(),
        want["variance"].as_f64().expect("variance"),
        tolerance,
    );
    assert_eq!(stream.n(), want["n"].as_u64().expect("n"));
    stream.fold(0.7);
    let after = &want["after_fold_0_7"];
    assert_f64_within(
        stream.ewma(),
        after["ewma"].as_f64().expect("ewma"),
        tolerance,
    );
    assert_f64_within(
        stream.variance(),
        after["variance"].as_f64().expect("variance"),
        tolerance,
    );
    assert_eq!(stream.n(), after["n"].as_u64().expect("n"));
}

/// parity: scoring.TestResumeEmptySeries
#[test]
fn resume_empty_series() {
    let case = &fixture("ewma.json")["resume_empty"];
    let mut stream = Stream::resume(case["alpha"].as_f64().expect("alpha"), &[]);
    assert_eq!(stream.n(), case["expected"]["n"].as_u64().expect("n"));
    stream.fold(0.5);
    assert_eq!(
        stream.ewma(),
        case["expected"]["after_fold_0_5_ewma"]
            .as_f64()
            .expect("ewma")
    );
}

/// The pure part of FoldObservations, night by night through float32 storage: every
/// stored value must match bit for bit. (The DB-backed Go tests that drive this are the
/// aggregate_rebuild bead's.)
#[test]
fn fold_through_float32_storage_is_bit_exact() {
    for case in fixture("ewma.json")["fold_observations_via_float32_storage"]
        .as_array()
        .expect("cases")
    {
        let alpha = case["alpha"].as_f64().expect("alpha");
        let min_nights = case["min_nights"].as_u64().expect("min");
        let mut stored: Vec<f64> = Vec::new();
        for night in case["nights"].as_array().expect("nights") {
            let got = fold_observation(alpha, &stored, night["x"].as_f64().expect("x"), min_nights);
            let context = format!("{} {}", case["id"], night["as_of"]);
            assert_eq!(
                got.ewma.to_bits(),
                night["stored_ewma_f32"].as_f64().expect("e").to_bits(),
                "{context}: ewma"
            );
            assert_eq!(
                got.variance.to_bits(),
                night["stored_variance_f32"].as_f64().expect("v").to_bits(),
                "{context}: variance"
            );
            assert_eq!(got.n, night["n"].as_u64().expect("n"), "{context}");
            assert_eq!(got.measured, night["measured"], "{context}");
            stored.push(got.ewma);
        }
        // Re-folding the last night replaces its row: the same values come back.
        if let Some(refold) = case.get("refold_last_night") {
            let last = case["nights"]
                .as_array()
                .expect("nights")
                .last()
                .expect("night");
            let got = fold_observation(
                alpha,
                &stored[..stored.len() - 1],
                last["x"].as_f64().expect("x"),
                min_nights,
            );
            assert_eq!(
                got.ewma.to_bits(),
                refold["stored_ewma_f32"].as_f64().expect("e").to_bits()
            );
            assert_eq!(
                got.variance.to_bits(),
                refold["stored_variance_f32"].as_f64().expect("v").to_bits()
            );
            assert_eq!(got.n, refold["n"].as_u64().expect("n"));
        }
    }
}

proptest! {
    /// EWMA convergence (spec §15): the smoothed value stays within the observations'
    /// range, and a constant tail pulls it toward that constant geometrically.
    #[test]
    fn ewma_is_bounded_and_converges(
        values in prop::collection::vec(0.0f64..=1.0, 1..40),
        tail in 0.0f64..=1.0,
        tail_len in 1usize..60,
    ) {
        let mut stream = Stream::new(ALPHA);
        for &x in &values {
            stream.fold(x);
        }
        let (lo, hi) = values.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), &x| (lo.min(x), hi.max(x)));
        prop_assert!(stream.ewma() >= lo - 1e-12 && stream.ewma() <= hi + 1e-12);
        let before = (stream.ewma() - tail).abs();
        for _ in 0..tail_len {
            stream.fold(tail);
        }
        let bound = before * (1.0 - ALPHA).powi(tail_len as i32) + 1e-12;
        prop_assert!((stream.ewma() - tail).abs() <= bound);
    }
}
