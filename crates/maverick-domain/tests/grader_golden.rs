//! Parity: `parity/fixtures/measurement-io/golden.json` (`internal/graders/golden`).

mod common;

use std::path::PathBuf;
use std::sync::Arc;

use maverick_domain::gojson;
use maverick_domain::graders::golden::{normalize, parse_number, split_items};
use maverick_domain::graders::{GoldenAnswer, Grader, SpecValidator};
use serde_json::Value;

fn golden() -> Value {
    parity_support::fixture("measurement-io", "golden.json").expect("golden.json loads")
}

fn testdata_dir() -> PathBuf {
    parity_support::fixture_path(
        "measurement-io",
        "testdata/internal/graders/golden/testdata",
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

/// parity: golden.TestKind
#[test]
fn kind() {
    let want = &golden()["go_tests"]["golden.TestKind"]["expect_kind"];
    assert_eq!(GoldenAnswer.kind(), want.as_str().expect("kind"));
}

/// parity: golden.TestValidateSpecAccepts
#[test]
fn validate_spec_accepts() {
    let case = &golden()["go_tests"]["golden.TestValidateSpecAccepts"];
    let raw = case["spec"].to_string();
    GoldenAnswer.validate_spec_json(&raw).expect("valid spec");
}

/// parity: golden.TestValidateSpecRejects
#[test]
fn validate_spec_rejects() {
    for row in golden()["validate_spec_rejects"].as_array().expect("rows") {
        let id = row["id"].as_str().expect("id");
        let raw = row["spec_raw"].as_str().expect("spec_raw");
        let go_error = row["go_error"].as_str().expect("go_error");
        let error = GoldenAnswer.validate_spec_json(raw).expect_err(id);
        if let Some(needle) = row["expect_error_contains"].as_str() {
            assert!(error.contains(needle), "{id}: {error:?} lacks {needle:?}");
        }
        if go_error.contains("jsonschema validation failed") {
            assert_schema_error(&error, go_error, id);
        } else if let Some(at) = go_error.find("error parsing regexp: ") {
            // The regex reason is the regex crate's wording (D36); the prefix is ours.
            let _ = parity_support::divergence!("D36");
            let prefix = &go_error[..at + "error parsing regexp: ".len()];
            assert!(error.starts_with(prefix), "{id}: {error:?}");
            assert!(error.ends_with("`\\A(?:()\\z`"), "{id}: {error:?}");
        } else {
            assert_eq!(error, go_error, "{id}");
        }
    }
}

/// parity: golden.TestNormalize
#[test]
fn normalize_rows() {
    for row in golden()["normalize"].as_array().expect("rows") {
        let input = row["in"].as_str().expect("in");
        assert_eq!(
            normalize(input),
            row["out"].as_str().expect("out"),
            "{input:?}"
        );
    }
}

/// parity: golden.TestParseNumber
#[test]
fn parse_number_rows() {
    for row in golden()["parse_number"].as_array().expect("rows") {
        let input = row["in"].as_str().expect("in");
        let got = parse_number(input);
        if row["ok"] == false {
            assert_eq!(got, None, "{input:?}");
            continue;
        }
        let got = got.unwrap_or_else(|| panic!("{input:?} should parse"));
        match &row["value"] {
            Value::String(special) => match special.as_str() {
                "+Inf" => assert_eq!(got, f64::INFINITY, "{input:?}"),
                "-Inf" => assert_eq!(got, f64::NEG_INFINITY, "{input:?}"),
                "NaN" => assert!(got.is_nan(), "{input:?}"),
                other => panic!("unknown special {other}"),
            },
            value => assert_eq!(got, value.as_f64().expect("number"), "{input:?}"),
        }
    }
}

/// parity: golden.TestSplitItems
#[test]
fn split_items_rows() {
    for row in golden()["split_items"].as_array().expect("rows") {
        let input = row["in"].as_str().expect("in");
        let want: Vec<&str> = row["out"]
            .as_array()
            .expect("out")
            .iter()
            .map(|v| v.as_str().expect("item"))
            .collect();
        assert_eq!(split_items(input), want, "{input:?}");
    }
}

/// Every golden fixture through the shared harness, then each file's exact result.
///
/// parity: golden.TestGolden
#[test]
fn golden_fixtures() {
    let grader: Arc<dyn Grader> = Arc::new(GoldenAnswer);
    let names = common::golden::run_dir(grader.clone(), &testdata_dir());
    let rows = golden()["fixture_results"]
        .as_array()
        .expect("rows")
        .clone();
    assert_eq!(names.len(), rows.len());

    for row in rows {
        let file = row["file"].as_str().expect("file");
        let path = testdata_dir().join(file);
        let (fixture, spec) = common::golden::load_fixture(grader.kind(), &path).expect("loads");
        // The fixture's spec re-encodes to the recorded JSON.
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
            let error = grader.validate_spec(&spec).expect_err(file);
            assert_schema_error(&error, go_error, file);
            continue;
        }
        let (result, failures) = common::golden::grade_fixture(grader.clone(), &fixture, &spec);
        let result = result.unwrap_or_else(|e| panic!("{file}: {e}"));
        let want: std::collections::BTreeMap<String, f64> =
            serde_json::from_value(row["dimensions"].clone()).expect("dimensions");
        assert_eq!(result.dimensions, want, "{file}");
        assert_eq!(result.detail, row["detail_exact_bytes"], "{file}");
        assert_eq!(failures as u64, row["failure_counter"], "{file}");
    }
}
