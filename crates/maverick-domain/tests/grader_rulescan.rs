//! Parity: `parity/fixtures/measurement-io/rulescan.json` (`internal/graders/rulescan`).

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use maverick_domain::gojson;
use maverick_domain::graders::rulescan::{count_lines, language_matches};
use maverick_domain::graders::{GradeInput, Grader, RuleScan, SpecValidator};
use serde_json::Value;

fn rulescan() -> Value {
    parity_support::fixture("measurement-io", "rulescan.json").expect("keyfacts.json loads")
}

fn testdata_dir() -> PathBuf {
    parity_support::fixture_path(
        "measurement-io",
        "testdata/internal/graders/rulescan/testdata",
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

/// parity: rulescan.TestKind
#[test]
fn kind() {
    let want = &rulescan()["go_tests"]["rulescan.TestKind"]["expect_kind"];
    assert_eq!(RuleScan.kind(), want.as_str().expect("kind"));
}

/// parity: rulescan.TestValidateSpecAccepts
#[test]
fn validate_spec_accepts() {
    let case = &rulescan()["go_tests"]["rulescan.TestValidateSpecAccepts"];
    RuleScan
        .validate_spec_json(&case["spec"].to_string())
        .expect("valid spec");
}

/// parity: rulescan.TestValidateSpecRejects
#[test]
fn validate_spec_rejects() {
    for row in rulescan()["validate_spec_rejects"]
        .as_array()
        .expect("rows")
    {
        let id = row["id"].as_str().expect("id");
        // Three probe rows record no spec_raw; their ids describe the spec.
        let raw = match (row["spec_raw"].as_str(), id) {
            (Some(raw), _) => raw,
            (None, "case_insensitive on length (probe)") => {
                r#"{"rules": [{"max_length_chars": 10, "case_insensitive": true}]}"#
            }
            (None, "line_count float (probe)") => r#"{"rules": [{"line_count": 1.5}]}"#,
            (None, "max_length_chars 1.0 (probe)") => r#"{"rules": [{"max_length_chars": 1.0}]}"#,
            (None, other) => panic!("no spec for {other}"),
        };
        let go_error = row["go_error"].as_str().expect("go_error");
        let error = RuleScan.validate_spec_json(raw).expect_err(id);
        if let Some(needle) = row["expect_error_contains"].as_str() {
            assert!(error.contains(needle), "{id}: {error:?} lacks {needle:?}");
        }
        if go_error.contains("jsonschema validation failed") {
            assert_schema_error(&error, go_error, id);
        } else if go_error.starts_with("rule_scan spec: json: cannot unmarshal") {
            // QUIRK Q-R1: 1.0 passes the schema's integer type but not the typed decode.
            // The decode error is library wording (D36).
            let _ = parity_support::divergence!("D36");
            assert!(error.starts_with("rule_scan spec: "), "{id}: {error:?}");
            assert!(!error.contains("jsonschema"), "{id}: {error:?}");
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

/// parity: rulescan.TestCountLines
#[test]
fn count_lines_rows() {
    for row in rulescan()["count_lines"].as_array().expect("rows") {
        let input = row["in"].as_str().expect("in");
        assert_eq!(
            count_lines(input),
            row["out"].as_i64().expect("out"),
            "{input:?}"
        );
    }
}

/// parity: rulescan.TestLanguageMatches
#[test]
fn language_matches_rows() {
    for row in rulescan()["language_matches"].as_array().expect("rows") {
        let code = row["code"].as_str().expect("code");
        let output = row["output"].as_str().expect("output");
        assert_eq!(
            language_matches(code, output),
            row["match"].as_bool().expect("match"),
            "{code}: {output:?}"
        );
    }
}

/// The grade probes (not Go tests): Unicode lengths, Go case folding, ASCII regex
/// classes, anchors without multiline, and critical weights.
#[test]
fn grade_probes() {
    for (n, row) in rulescan()["grade_probes"]
        .as_array()
        .expect("rows")
        .iter()
        .enumerate()
    {
        let raw = row["spec"].as_str().expect("spec");
        let spec = gojson::decode_one(raw).expect("spec json").0;
        RuleScan.validate_spec(&spec).expect("probe spec is valid");
        let input = GradeInput {
            task: None,
            output: row["output"].as_str().expect("output"),
            run: None,
        };
        let result = common::block_on(RuleScan.grade(&spec, input)).expect("grades");
        let want: BTreeMap<String, f64> =
            serde_json::from_value(row["dimensions"].clone()).expect("dimensions");
        assert_eq!(result.dimensions, want, "probe {n}");
        assert_eq!(result.detail, row["detail_exact_bytes"], "probe {n}");
    }
}

/// Every key_facts fixture through the shared harness, then each file's exact result.
///
/// parity: rulescan.TestGolden
#[test]
fn golden_fixtures() {
    let grader: Arc<dyn Grader> = Arc::new(RuleScan);
    let names = common::golden::run_dir(grader.clone(), &testdata_dir());
    let rows = rulescan()["fixture_results"]
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
            let error = grader.validate_spec(&spec).expect_err(file);
            assert_schema_error(&error, go_error, file);
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
