//! Parity: `parity/fixtures/measurement-core/regression.json` (`internal/scoring`).

use chrono::NaiveDate;
use maverick_domain::scoring::{
    AlertMark, Finding, RegressionKind, RegressionParams, SeriesPoint, detect_regressions,
    filter_alertable,
};
use serde_json::Value;

fn fixture() -> Value {
    parity_support::fixture("measurement-core", "regression.json").expect("fixture loads")
}

fn tolerance() -> f64 {
    fixture()["tolerance"].as_f64().expect("tolerance")
}

fn number(value: &Value, key: &str) -> f64 {
    value[key]
        .as_f64()
        .unwrap_or_else(|| panic!("{key}: {value}"))
}

fn date(value: &Value) -> NaiveDate {
    value["as_of"]
        .as_str()
        .expect("as_of")
        .parse()
        .expect("date")
}

fn params() -> RegressionParams {
    let doc = fixture();
    let p = &doc["params"];
    let days = |key: &str| u32::try_from(p[key].as_u64().expect(key)).expect(key);
    RegressionParams {
        alpha: number(p, "alpha"),
        trailing_days: days("trailing_days"),
        drop_points: number(p, "drop_points"),
        shock_sigma: number(p, "shock_sigma"),
        shock_floor_points: number(p, "shock_floor_points"),
        dedup_days: days("dedup_days"),
        deepen_points: number(p, "deepen_points"),
    }
}

fn series(value: &Value) -> Vec<SeriesPoint> {
    value
        .as_array()
        .expect("series")
        .iter()
        .map(|row| SeriesPoint {
            as_of: date(row),
            ewma: number(row, "ewma"),
            variance: number(row, "variance"),
        })
        .collect()
}

fn kind(value: &Value) -> RegressionKind {
    match value["kind"].as_str().expect("kind") {
        "drop" => RegressionKind::Drop,
        "shock" => RegressionKind::Shock,
        other => panic!("kind {other}"),
    }
}

fn findings(value: &Value) -> Vec<Finding> {
    value
        .as_array()
        .expect("findings")
        .iter()
        .map(|row| {
            assert_eq!(row["dimension"], "quality");
            Finding {
                as_of: date(row),
                dimension: "quality",
                kind: kind(row),
                baseline: number(row, "baseline"),
                current: number(row, "current"),
                delta: number(row, "delta"),
            }
        })
        .collect()
}

fn assert_findings(got: &[Finding], want: &Value, context: &str) {
    let want = want.as_array().expect("expected");
    assert_eq!(got.len(), want.len(), "{context}: {got:?}");
    for (got, want) in got.iter().zip(want) {
        assert_eq!(got.as_of, date(want), "{context}");
        assert_eq!(got.dimension, want["dimension"], "{context}");
        assert_eq!(got.kind.as_str(), want["kind"], "{context}");
        assert_eq!(got.kind, kind(want), "{context}");
        for (value, key) in [
            (got.baseline, "baseline"),
            (got.current, "current"),
            (got.delta, "delta"),
        ] {
            parity_support::assert_f64_within(value, number(want, key), tolerance());
        }
    }
}

fn detect_case(id: &str) -> Value {
    fixture()["detect"]
        .as_array()
        .expect("detect")
        .iter()
        .find(|c| c["id"] == id)
        .unwrap_or_else(|| panic!("no case {id}"))
        .clone()
}

fn check_detect(id: &str) {
    let case = detect_case(id);
    let got = detect_regressions(&series(&case["series"]), &params());
    assert_findings(&got, &case["expected"], id);
}

#[test]
fn default_params_match_the_fixture() {
    assert_eq!(RegressionParams::default(), params());
}

/// parity: scoring.TestDetectRegressionsGradualDropFlagsAtCrossingNight
#[test]
fn gradual_drop_flags_at_crossing_night() {
    check_detect("gradual_drop_8");
}

/// parity: scoring.TestDetectRegressionsShockRespectsVarianceBand
#[test]
fn shock_respects_variance_band() {
    check_detect("variance_band_noisy");
    check_detect("variance_band_quiet");
}

/// parity: scoring.TestDetectRegressionsStableSeriesStaysQuiet
#[test]
fn stable_series_stays_quiet() {
    check_detect("stable");
}

/// parity: scoring.TestDetectRegressionsSuddenShock
#[test]
fn sudden_shock() {
    check_detect("sudden_shock");
}

/// parity: scoring.TestDetectRegressionsTrailingWindowExpires
#[test]
fn trailing_window_expires() {
    check_detect("trailing_window_expires");
}

/// Every detect row, including the float32-storage variants of the 11-night decline, a
/// single-row series, and a gap in the series (QUIRK Q6).
#[test]
fn every_detect_row() {
    for case in fixture()["detect"].as_array().expect("detect") {
        check_detect(case["id"].as_str().expect("id"));
    }
}

fn check_filter(id: &str) {
    let doc = fixture();
    let case = doc["filter_alertable"]
        .as_array()
        .expect("filter_alertable")
        .iter()
        .find(|c| c["id"] == id)
        .unwrap_or_else(|| panic!("no case {id}"))
        .clone();
    let last = match &case["last"] {
        Value::Null => None,
        mark => Some(AlertMark {
            as_of: date(mark),
            current: number(mark, "current"),
        }),
    };
    let got = filter_alertable(&findings(&case["findings"]), last, &params());
    assert_findings(&got, &case["expected"], id);
}

/// parity: scoring.TestFilterAlertableDedupAndDeepening
#[test]
fn filter_alertable_dedup_and_deepening() {
    check_filter("dedup_and_deepening");
}

/// parity: scoring.TestFilterAlertableWindowExpiry
#[test]
fn filter_alertable_window_expiry() {
    check_filter("inside_window_not_deeper");
    check_filter("exactly_seven_days_rearms");
}

/// Every filter_alertable row, including a finding exactly `deepen_points` deeper.
#[test]
fn every_filter_alertable_row() {
    for case in fixture()["filter_alertable"]
        .as_array()
        .expect("filter_alertable")
    {
        check_filter(case["id"].as_str().expect("id"));
    }
}

/// parity: scoring.TestRegressionCrossesBar
#[test]
fn crosses_bar() {
    let doc = fixture();
    for case in doc["crosses_bar"]["cases"].as_array().expect("cases") {
        let finding = Finding {
            as_of: NaiveDate::from_ymd_opt(2031, 5, 1).expect("date"),
            dimension: "quality",
            kind: RegressionKind::Drop,
            baseline: number(case, "baseline"),
            current: number(case, "current"),
            delta: number(case, "baseline") - number(case, "current"),
        };
        assert_eq!(
            finding.crosses_bar(number(case, "bar")),
            case["expected"],
            "{case}"
        );
    }
}
