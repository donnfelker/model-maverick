//! Parity: `parity/fixtures/measurement-io/keyfacts.json` (`internal/graders/keyfacts`).

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use maverick_domain::gojson;
use maverick_domain::graders::keyfacts::normalize_search;
use maverick_domain::graders::{GradeInput, Grader, KeyFacts, SpecValidator};
use serde_json::Value;

fn keyfacts() -> Value {
    parity_support::fixture("measurement-io", "keyfacts.json").expect("keyfacts.json loads")
}

fn testdata_dir() -> PathBuf {
    parity_support::fixture_path(
        "measurement-io",
        "testdata/internal/graders/keyfacts/testdata",
    )
    .expect("testdata dir")
}

/// JSON-schema failure detail is library wording (D24): compare Go's text up to and
/// including `jsonschema validation failed`, and the failing instance location.
fn assert_schema_error(got: &str, go_error: &str, context: &str) {
    const MARKER: &str = "jsonschema validation failed";
    let _ = parity_support::divergence!("D24");
    let at = go_error.find(MARKER).expect("schema error");
    let prefix = &go_error[..at + MARKER.len()];
    assert!(
        got.starts_with(prefix),
        "{context}: {got:?} lacks {prefix:?}"
    );
    let location = go_error
        .split("- at '")
        .nth(1)
        .and_then(|rest| rest.split('\'').next())
        .expect("location");
    assert!(
        got.contains(&format!("'{location}'")),
        "{context}: {got:?} lacks location '{location}'"
    );
}

/// parity: keyfacts.TestKind
#[test]
fn kind() {
    let want = &keyfacts()["go_tests"]["keyfacts.TestKind"]["expect_kind"];
    assert_eq!(KeyFacts.kind(), want.as_str().expect("kind"));
}

/// parity: keyfacts.TestValidateSpecAccepts
#[test]
fn validate_spec_accepts() {
    let case = &keyfacts()["go_tests"]["keyfacts.TestValidateSpecAccepts"];
    KeyFacts
        .validate_spec_json(&case["spec"].to_string())
        .expect("valid spec");
}

/// parity: keyfacts.TestValidateSpecRejects
#[test]
fn validate_spec_rejects() {
    for row in keyfacts()["validate_spec_rejects"]
        .as_array()
        .expect("rows")
    {
        let id = row["id"].as_str().expect("id");
        let raw = row["spec_raw"].as_str().expect("spec_raw");
        let go_error = row["go_error"].as_str().expect("go_error");
        let error = KeyFacts.validate_spec_json(raw).expect_err(id);
        if let Some(needle) = row["expect_error_contains"].as_str() {
            assert!(error.contains(needle), "{id}: {error:?} lacks {needle:?}");
        }
        if go_error.contains("jsonschema validation failed") {
            assert_schema_error(&error, go_error, id);
        } else if let Some(at) = go_error.find("error parsing regexp: ") {
            // The regex reason is the regex crate's wording (D36).
            let _ = parity_support::divergence!("D36");
            let prefix = &go_error[..at + "error parsing regexp: ".len()];
            assert!(error.starts_with(prefix), "{id}: {error:?}");
            assert!(error.ends_with(": `(`"), "{id}: {error:?}");
        } else {
            assert_eq!(error, go_error, "{id}");
        }
    }
}

/// parity: keyfacts.TestNormalizeSearch
#[test]
fn normalize_search_rows() {
    for row in keyfacts()["normalize_search"].as_array().expect("rows") {
        let input = row["in"].as_str().expect("in");
        assert_eq!(
            normalize_search(input),
            row["out"].as_str().expect("out"),
            "{input:?}"
        );
    }
}

/// The grade probes, the first of which is the Go test.
///
/// parity: keyfacts.TestHallucinationFreeClamped
#[test]
fn grade_probes() {
    for (n, row) in keyfacts()["grade_probes"]
        .as_array()
        .expect("rows")
        .iter()
        .enumerate()
    {
        let raw = row["spec"].as_str().expect("spec");
        let spec = gojson::decode_one(raw).expect("spec json").0;
        KeyFacts.validate_spec(&spec).expect("probe spec is valid");
        let input = GradeInput {
            task: None,
            output: row["output"].as_str().expect("output"),
            run: None,
        };
        let result = common::block_on(KeyFacts.grade(&spec, input)).expect("grades");
        let want: BTreeMap<String, f64> =
            serde_json::from_value(row["dimensions"].clone()).expect("dimensions");
        assert_eq!(result.dimensions, want, "probe {n}");
        assert_eq!(result.detail, row["detail_exact_bytes"], "probe {n}");
    }
}

/// Every key_facts fixture through the shared harness, then each file's exact result.
///
/// parity: keyfacts.TestGolden
#[test]
fn golden_fixtures() {
    let grader: Arc<dyn Grader> = Arc::new(KeyFacts);
    let names = common::golden::run_dir(grader.clone(), &testdata_dir());
    let rows = keyfacts()["fixture_results"]
        .as_array()
        .expect("rows")
        .clone();
    assert_eq!(names.len(), rows.len());
    for row in rows {
        let file = row["file"].as_str().expect("file");
        let (fixture, spec) =
            common::golden::load_fixture(grader.kind(), &testdata_dir().join(file)).expect("loads");
        assert_eq!(
            gojson::marshal(&spec),
            gojson::marshal(
                &gojson::decode_one(&row["spec_json"].to_string())
                    .expect("json")
                    .0
            ),
            "{file}"
        );
        assert_eq!(
            fixture.output,
            row["output"].as_str().expect("output"),
            "{file}"
        );
        if let Some(go_error) = row["validate_error"].as_str() {
            assert_eq!(
                grader.validate_spec(&spec).expect_err(file),
                go_error,
                "{file}"
            );
            continue;
        }
        let (result, failures) = common::golden::grade_fixture(grader.clone(), &fixture, &spec);
        let result = result.unwrap_or_else(|e| panic!("{file}: {e}"));
        let want: BTreeMap<String, f64> =
            serde_json::from_value(row["dimensions"].clone()).expect("dimensions");
        assert_eq!(result.dimensions, want, "{file}");
        assert_eq!(result.detail, row["detail_exact_bytes"], "{file}");
        assert_eq!(failures as u64, row["failure_counter"], "{file}");
    }
}
