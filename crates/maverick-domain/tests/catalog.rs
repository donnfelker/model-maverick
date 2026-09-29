//! Parity tests for the catalog runnable check and the retirement-guard blocked message and
//! alert (bead 3hk.2.20).
//!
//! Fixture: `parity/fixtures/measurement-core/catalog.json`. Go source: `internal/catalog`. The
//! `parse_pricing` and `cost` rows are covered by `crate::pricing` (tests/pricing.rs). The
//! lifecycle transitions and retention window are exercised by unit tests in the module (no Go
//! fixture; they run in the `catalog_sync` job with a database).

use maverick_domain::catalog::{BlockedPool, RetirementBlocked, runnable};
use serde_json::Value;

fn catalog() -> Value {
    parity_support::fixture("measurement-core", "catalog.json").expect("load catalog.json")
}

// parity: measurement-core/catalog.TestRunnable
#[test]
fn runnable_matches_fixture() {
    let fixture = catalog();
    for case in fixture["runnable"]["cases"]
        .as_array()
        .expect("runnable cases")
    {
        let status = case["status"].as_str().expect("status");
        let expected = case["expected"].as_bool().expect("expected");
        assert_eq!(runnable(status), expected, "runnable({status:?})");
    }
}

#[test]
fn retirement_blocked_message_and_alert_match_fixture() {
    let fixture = catalog();
    let row = &fixture["retirement_blocked"];
    let column_id = row["column_id"].as_str().expect("column_id").to_string();
    let pools: Vec<BlockedPool> =
        serde_json::from_value(row["pools"].clone()).expect("blocked pools");
    let blocked = RetirementBlocked { column_id, pools };

    assert_eq!(
        blocked.message(),
        row["expected_error_message"]
            .as_str()
            .expect("error message")
    );
    assert_eq!(
        blocked.alert_subject_json(),
        row["expected_alert_subject_json_as_marshaled"]
            .as_str()
            .expect("alert subject json")
    );
}
