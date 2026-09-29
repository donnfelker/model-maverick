//! Parity tests for difficulty ceilings, `clears`, and the banded policy table (bead 3hk.2.18).
//!
//! Fixtures: `parity/fixtures/decision/{ceiling.json, ceiling_property.json, bands.json}`.
//! Go source: `internal/recommend/ceiling.go`. On a mismatch each case prints the fixture
//! anchor and a field-path diff via the shared test observability (bead 1.7).

use std::collections::BTreeMap;

use maverick_domain::ceilings::{
    BandCandidate, Cell, PassRate, ceiling, ceilings, clears, resolve_bands,
};
use serde::Deserialize;
use serde_json::{Value, json};
use test_support::format_json_diff;

#[derive(Deserialize)]
struct CeilingInput {
    bar: f64,
    buffer: f64,
    pass_rates: Vec<PassRate>,
}

#[derive(Deserialize)]
struct ClearsInput {
    ceiling: u8,
    difficulty: u8,
}

#[derive(Deserialize)]
struct CeilingsInput {
    bar_by_capability: BTreeMap<String, f64>,
    buffer: f64,
    cells: Vec<Cell>,
}

/// A `resolve_bands` candidate as the fixture writes it: `cost_by_tier` and `pass_by_tier`
/// are `null` when a column has no cost or pass data (a Go nil map), which becomes an empty
/// map (no known cost, no pass rate at the top tier).
#[derive(Deserialize)]
struct BandCandidateInput {
    column_id: String,
    ceiling: u8,
    #[serde(default)]
    cost_by_tier: Option<BTreeMap<u8, f64>>,
    #[serde(default)]
    pass_by_tier: Option<BTreeMap<u8, f64>>,
}

impl From<BandCandidateInput> for BandCandidate {
    fn from(input: BandCandidateInput) -> Self {
        BandCandidate {
            column_id: input.column_id,
            ceiling: input.ceiling,
            cost_by_tier: input.cost_by_tier.unwrap_or_default(),
            pass_by_tier: input.pass_by_tier.unwrap_or_default(),
        }
    }
}

fn load_cases(file: &str) -> Vec<Value> {
    let value = parity_support::fixture("decision", file).expect("load decision fixture");
    value
        .get("cases")
        .and_then(Value::as_array)
        .expect("fixture has a cases array")
        .clone()
}

/// Run one fixture case. Returns `Some(diff)` on a mismatch, `None` when it matches.
fn run_case(file: &str, case: &Value) -> Option<String> {
    let function = case.get("function").and_then(Value::as_str).unwrap_or("");
    let id = case.get("id").and_then(Value::as_str).unwrap_or("<no id>");
    let anchor = format!("fixtures/decision/{file}#{id}");
    let input = case.get("input").cloned().unwrap_or(Value::Null);
    let expected = case.get("expected").cloned().unwrap_or(Value::Null);

    let actual: Value = match function {
        "ceiling" => {
            let input: CeilingInput = serde_json::from_value(input).expect("ceiling input");
            json!(ceiling(&input.pass_rates, input.bar, input.buffer))
        }
        "clears" => {
            let input: ClearsInput = serde_json::from_value(input).expect("clears input");
            json!(clears(input.ceiling, input.difficulty))
        }
        "ceilings" => {
            let input: CeilingsInput = serde_json::from_value(input).expect("ceilings input");
            serde_json::to_value(ceilings(
                &input.cells,
                &input.bar_by_capability,
                input.buffer,
            ))
            .expect("serialize ceilings")
        }
        "resolve_bands" => {
            let raw: Vec<BandCandidateInput> =
                serde_json::from_value(input).expect("resolve_bands input");
            let candidates: Vec<BandCandidate> = raw.into_iter().map(Into::into).collect();
            serde_json::to_value(resolve_bands(&candidates)).expect("serialize bands")
        }
        other => panic!("{anchor}: unknown function {other:?}"),
    };

    format_json_diff(&expected, &actual, Some(&anchor))
}

// parity: decision/recommend.TestCeilingGoldens
// parity: decision/recommend.TestCeilingHonorsPerTenantBar
// parity: decision/recommend.TestClears
// parity: decision/recommend.TestCeilingsGrid
#[test]
fn ceiling_clears_and_ceilings_goldens() {
    let failures: Vec<String> = load_cases("ceiling.json")
        .iter()
        .filter_map(|case| run_case("ceiling.json", case))
        .collect();
    assert!(
        failures.is_empty(),
        "{} ceiling.json case(s) failed:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

// parity: decision/recommend.TestCeilingMonotonicity
#[test]
fn ceiling_monotonicity_recorded_cases() {
    let cases = load_cases("ceiling_property.json");
    assert_eq!(cases.len(), 4000, "the monotonicity fixture has 4000 calls");
    let mut failures = 0usize;
    let mut first = String::new();
    for case in &cases {
        if let Some(diff) = run_case("ceiling_property.json", case) {
            failures += 1;
            if first.is_empty() {
                first = diff;
            }
        }
    }
    assert_eq!(
        failures, 0,
        "{failures} of 4000 monotonicity cases failed; first:\n{first}"
    );
}

// parity: decision/recommend.TestBands
// parity: decision/recommend.TestResolveBandsCheapestClearingColumn
// parity: decision/recommend.TestResolveBandsCostAveragedOverBandTiers
// parity: decision/recommend.TestResolveBandsFallbackWhenNothingClears
// parity: decision/recommend.TestResolveBandsFallbackCeilingTieBreaksOnCost
// parity: decision/recommend.TestResolveBandsKnownCostBeatsUnknown
// parity: decision/recommend.TestResolveBandsTiesKeepPoolOrder
// parity: decision/recommend.TestResolveBandsMissingPassRateAtTopTier
// parity: decision/recommend.TestResolveBandsEmptyCandidates
#[test]
fn banded_policy_table() {
    let failures: Vec<String> = load_cases("bands.json")
        .iter()
        .filter_map(|case| run_case("bands.json", case))
        .collect();
    assert!(
        failures.is_empty(),
        "{} bands.json case(s) failed:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

proptest::proptest! {
    /// A higher bar never raises the ceiling (the monotonicity invariant behind
    /// `ceiling_property.json`), for any measured five-tier grid.
    #[test]
    fn ceiling_is_monotone_non_increasing_in_the_bar(
        ewmas in proptest::collection::vec(0.0f64..=1.0, 5),
        bar_low in 0.0f64..=0.5,
        delta in 0.0f64..=0.5,
    ) {
        let rates: Vec<PassRate> = ewmas
            .iter()
            .enumerate()
            .map(|(index, &ewma)| PassRate { tier: (index + 1) as u8, ewma, measured: true })
            .collect();
        let low = ceiling(&rates, bar_low, 0.05);
        let high = ceiling(&rates, bar_low + delta, 0.05);
        proptest::prop_assert!(high <= low, "bar {bar_low}+{delta}: {low} -> {high}");
    }
}
