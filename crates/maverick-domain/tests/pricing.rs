//! Parity: `parity/fixtures/measurement-core/catalog.json#parse_pricing,cost`
//! (`internal/catalog`) and `parity/fixtures/measurement-io/runner.json#cost`.

use maverick_domain::pricing::{Pricing, cost, cost_numeric, parse_pricing};
use serde_json::Value;

fn number(value: &Value, key: &str) -> f64 {
    value[key]
        .as_f64()
        .unwrap_or_else(|| panic!("{key}: {value}"))
}

fn tokens(value: &Value, key: &str) -> i64 {
    value[key]
        .as_i64()
        .unwrap_or_else(|| panic!("{key}: {value}"))
}

fn prices(input: f64, output: f64) -> Pricing {
    Pricing {
        input_per_mtok: input,
        output_per_mtok: output,
        ..Pricing::default()
    }
}

/// parity: catalog.TestParsePricing
#[test]
fn parse_pricing_rows() {
    let doc = parity_support::fixture("measurement-core", "catalog.json").expect("fixture");
    for case in doc["parse_pricing"]["cases"].as_array().expect("cases") {
        // A nil snapshot (SQL NULL) is empty bytes.
        let raw = case["raw"].as_str().unwrap_or("");
        let expected = &case["expected"];
        match parse_pricing(raw) {
            Ok(pricing) => {
                let want = &expected["pricing"];
                assert!(want.is_object(), "{raw}: unexpected success");
                assert_eq!(
                    pricing.input_per_mtok,
                    number(want, "input_per_mtok"),
                    "{raw}"
                );
                assert_eq!(
                    pricing.output_per_mtok,
                    number(want, "output_per_mtok"),
                    "{raw}"
                );
                assert_eq!(pricing.currency, want["currency"], "{raw}");
                assert_eq!(pricing.as_of, want["as_of"], "{raw}");
                assert_eq!(pricing.source, want["source"], "{raw}");
            }
            Err(error) => {
                let want = expected["error"].as_str().expect("expected an error");
                // D36: the engine-authored prefix is exact; this Go tail is also
                // reproduced here.
                assert_eq!(error, want, "{raw}");
            }
        }
    }
}

/// parity: catalog.TestCostGolden
/// parity: catalog.TestCostGolden/claude_list_price_small_run
/// parity: catalog.TestCostGolden/structured_extraction_run
/// parity: catalog.TestCostGolden/one_mtok_each_way
/// parity: catalog.TestCostGolden/cheap_column
/// parity: catalog.TestCostGolden/fractional_prices
/// parity: catalog.TestCostGolden/rounds_to_numeric(12,6)_precision
/// parity: catalog.TestCostGolden/zero_tokens
/// parity: catalog.TestCostGolden/empty_pricing_snapshot
#[test]
fn cost_rows() {
    let doc = parity_support::fixture("measurement-core", "catalog.json").expect("fixture");
    for case in doc["cost"]["cases"].as_array().expect("cases") {
        let got = cost(
            tokens(case, "tokens_in"),
            tokens(case, "tokens_out"),
            &prices(
                number(case, "input_per_mtok"),
                number(case, "output_per_mtok"),
            ),
        );
        assert_eq!(got, number(case, "expected"), "{}", case["id"]);
    }
}

/// parity: runner.TestCostNumericRoundTrip
#[test]
fn runner_cost_probes() {
    let doc = parity_support::fixture("measurement-io", "runner.json").expect("fixture");
    let section = &doc["cost"];
    for probe in section["probes"].as_array().expect("probes") {
        let got = cost(
            tokens(probe, "In"),
            tokens(probe, "Out"),
            &prices(number(probe, "InP"), number(probe, "OutP")),
        );
        assert_eq!(got, number(probe, "Cost"), "{probe}");
        assert_eq!(cost_numeric(got), probe["Numeric"], "{probe}");
    }
    let round_trip = &section["go_test_TestCostNumericRoundTrip"];
    assert_eq!(
        cost_numeric(number(round_trip, "in")),
        round_trip["numeric"]
    );
    assert_eq!(cost_numeric(0.0), round_trip["zero_numeric"]);
}
