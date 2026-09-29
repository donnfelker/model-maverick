//! Parity: `parity/fixtures/measurement-io/jsonschema.json`
//! (`internal/graders/jsonschema`).

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use maverick_domain::gojson::{self, JsonValue};
use maverick_domain::graders::json_schema::{
    Extraction, Segment, extract_json, parse_path, value_equal,
};
use maverick_domain::graders::{GradeInput, Grader, JsonSchema, SpecValidator};
use serde_json::Value;

fn fixture() -> Value {
    parity_support::fixture("measurement-io", "jsonschema.json").expect("jsonschema.json loads")
}

fn testdata_dir() -> PathBuf {
    parity_support::fixture_path(
        "measurement-io",
        "testdata/internal/graders/jsonschema/testdata",
    )
    .expect("testdata dir")
}

/// Library-worded failures (D24): compare Go's text up to the library part, plus the
/// failing instance location when Go names one.
fn assert_library_error(got: &str, go_error: &str, context: &str) {
    let _ = parity_support::divergence!("D24");
    // The engine-authored prefix ends at the first library marker.
    let (at, marker) = ["embedded schema: ", "jsonschema validation failed"]
        .iter()
        .filter_map(|marker| go_error.find(marker).map(|at| (at, *marker)))
        .min()
        .unwrap_or_else(|| panic!("{context}: not a library error: {go_error}"));
    let prefix = &go_error[..at + marker.len()];
    assert!(
        got.starts_with(prefix),
        "{context}: {got:?} lacks {prefix:?}"
    );
    if marker.starts_with("jsonschema") {
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
}

/// A fixture value as a `JsonValue`: `{"json_number": "12500"}` is a number literal,
/// `{"float64": 12500}` a Go float, `{"string": ..}` and `{"object": ..}` wrap their
/// kinds; plain JSON otherwise.
fn typed(value: &Value) -> JsonValue {
    if let Value::Object(map) = value {
        if map.len() == 1 {
            let (tag, inner) = map.iter().next().expect("one entry");
            match (tag.as_str(), inner) {
                ("json_number", Value::String(literal)) => {
                    return JsonValue::Number(literal.clone());
                }
                ("float64", Value::Number(n)) => {
                    let f = n.as_f64().expect("float");
                    return JsonValue::Number(gojson::encode_f64(f).expect("finite"));
                }
                ("string", Value::String(s)) => return JsonValue::String(s.clone()),
                ("object", Value::Object(members)) => {
                    return JsonValue::Object(
                        members.iter().map(|(k, v)| (k.clone(), typed(v))).collect(),
                    );
                }
                _ => {}
            }
        }
    }
    match value {
        Value::Array(items) => JsonValue::Array(items.iter().map(typed).collect()),
        Value::Object(members) => {
            JsonValue::Object(members.iter().map(|(k, v)| (k.clone(), typed(v))).collect())
        }
        other => gojson::decode_one(&other.to_string()).expect("json").0,
    }
}

/// parity: jsonschema.TestKind
#[test]
fn kind() {
    let want = &fixture()["go_tests"]["jsonschema.TestKind"]["expect_kind"];
    assert_eq!(JsonSchema.kind(), want.as_str().expect("kind"));
}

/// parity: jsonschema.TestValidateSpecAccepts
#[test]
fn validate_spec_accepts() {
    let case = &fixture()["go_tests"]["jsonschema.TestValidateSpecAccepts"];
    JsonSchema
        .validate_spec_json(case["spec_raw"].as_str().expect("spec_raw"))
        .expect("valid spec");
}

/// parity: jsonschema.TestValidateSpecRejects
#[test]
fn validate_spec_rejects() {
    for row in fixture()["validate_spec_rejects"].as_array().expect("rows") {
        let id = row["id"].as_str().expect("id");
        // The probe rows record no spec_raw; their ids describe the spec.
        let raw = match (row["spec_raw"].as_str(), id) {
            (Some(raw), _) => raw.to_owned(),
            (None, "unterminated index (probe)") => {
                r#"{"schema": {}, "checks": [{"path": "$.a[1", "equals": 1}]}"#.to_owned()
            }
            (None, "negative index (probe)") => {
                r#"{"schema": {}, "checks": [{"path": "$.a[-1]", "equals": 1}]}"#.to_owned()
            }
            (None, "unexpected char (probe)") => {
                r#"{"schema": {}, "checks": [{"path": "$a", "equals": 1}]}"#.to_owned()
            }
            (None, "boolean schema true (probe)") => r#"{"schema": true}"#.to_owned(),
            (None, "remote ref (probe)") => {
                r#"{"schema": {"$ref": "https://example.com/x.json"}}"#.to_owned()
            }
            (None, other) => panic!("no spec for {other}"),
        };
        let result = JsonSchema.validate_spec_json(&raw);
        let Some(go_error) = row["go_error"].as_str() else {
            assert_eq!(row["valid"], true, "{id}");
            result.unwrap_or_else(|e| panic!("{id}: {e}"));
            continue;
        };
        let error = result.expect_err(id);
        if let Some(needle) = row["expect_error_contains"].as_str() {
            assert!(error.contains(needle), "{id}: {error:?} lacks {needle:?}");
        }
        if go_error.contains("jsonschema validation failed")
            || go_error.contains("embedded schema: ")
        {
            assert_library_error(&error, go_error, id);
        } else if let Some(at) = go_error.find("error parsing regexp: ") {
            let _ = parity_support::divergence!("D36");
            let prefix = &go_error[..at + "error parsing regexp: ".len()];
            assert!(error.starts_with(prefix), "{id}: {error:?}");
            assert!(error.ends_with("`\\A(?:()\\z`"), "{id}: {error:?}");
        } else {
            assert_eq!(error, go_error, "{id}");
        }
    }
}

/// A decoded document as the fixture records it: numbers as values (a literal too large
/// for a float as `{"json_number": ..}`), duplicate keys resolved to the last.
fn as_fixture_doc(value: &JsonValue) -> Value {
    match value {
        JsonValue::Null => Value::Null,
        JsonValue::Bool(b) => Value::Bool(*b),
        JsonValue::Number(literal) => match serde_json::from_str::<Value>(literal) {
            Ok(number) => number,
            Err(_) => serde_json::json!({ "json_number": literal }),
        },
        JsonValue::String(s) => Value::String(s.clone()),
        JsonValue::Array(items) => Value::Array(items.iter().map(as_fixture_doc).collect()),
        JsonValue::Object(members) => {
            let mut map = serde_json::Map::new();
            for (k, v) in members {
                map.insert(k.clone(), as_fixture_doc(v));
            }
            Value::Object(map)
        }
    }
}

/// JSON equality with numbers compared by value.
fn same_json(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(x, y)| same_json(x, y))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| same_json(v, w)))
        }
        _ => a == b,
    }
}

/// parity: jsonschema.TestExtractJSON
#[test]
fn extract_json_rows() {
    for row in fixture()["extract_json"].as_array().expect("rows") {
        let output = row["output"].as_str().expect("output");
        let got = extract_json(output);
        assert_eq!(got.is_some(), row["extracted"] == true, "{output:?}");
        let Some((doc, how)) = got else { continue };
        let method = match how {
            Extraction::Fenced => "fenced",
            Extraction::Bare => "bare",
        };
        assert_eq!(
            method,
            row["method"].as_str().expect("method"),
            "{output:?}"
        );
        let doc = as_fixture_doc(&doc);
        assert!(
            same_json(&doc, &row["doc"]),
            "{output:?}: {doc} != {}",
            row["doc"]
        );
    }
}

/// parity: jsonschema.TestParsePath
#[test]
fn parse_path_rows() {
    let case = &fixture()["go_tests"]["jsonschema.TestParsePath"];
    let segments = parse_path(case["path"].as_str().expect("path")).expect("parses");
    let want: Vec<Segment> = case["segments"]
        .as_array()
        .expect("segments")
        .iter()
        .map(|s| match s["index"].as_i64().expect("index") {
            -1 => Segment::Key(s["key"].as_str().expect("key").to_owned()),
            n => Segment::Index(usize::try_from(n).expect("index")),
        })
        .collect();
    assert_eq!(segments, want);
    let reject = case["reject"].as_str().expect("reject");
    assert_eq!(
        parse_path(reject).unwrap_err(),
        format!("path {reject:?} has a bad array index \"x\"")
    );
}

/// parity: jsonschema.TestValueEqual
#[test]
fn value_equal_rows() {
    for row in fixture()["go_tests"]["jsonschema.TestValueEqual"]
        .as_array()
        .expect("rows")
    {
        let (a, b) = (typed(&row["a"]), typed(&row["b"]));
        assert_eq!(value_equal(&a, &b), row["equal"] == true, "{row}");
    }
}

/// The grade probes (not Go tests): formats off, integral floats, schema error
/// rendering, check types, `got` encoding, and boolean schemas.
#[test]
fn grade_probes() {
    for (n, row) in fixture()["grade_probes"]
        .as_array()
        .expect("rows")
        .iter()
        .enumerate()
    {
        let spec = gojson::decode_one(row["spec"].as_str().expect("spec"))
            .expect("spec")
            .0;
        JsonSchema
            .validate_spec(&spec)
            .expect("probe spec is valid");
        let input = GradeInput {
            task: None,
            output: row["output"].as_str().expect("output"),
            run: None,
        };
        let result = common::block_on(JsonSchema.grade(&spec, input)).expect("grades");
        let want: BTreeMap<String, f64> =
            serde_json::from_value(row["dimensions"].clone()).expect("dimensions");
        assert_eq!(result.dimensions, want, "probe {n}");
        assert_eq!(result.detail, row["detail_exact_bytes"], "probe {n}");
    }
}

/// Every json_schema fixture through the shared harness, then each file's exact result.
///
/// parity: jsonschema.TestGolden
#[test]
fn golden_fixtures() {
    let grader: Arc<dyn Grader> = Arc::new(JsonSchema);
    let names = common::golden::run_dir(grader.clone(), &testdata_dir());
    let rows = fixture()["fixture_results"]
        .as_array()
        .expect("rows")
        .clone();
    assert_eq!(names.len(), rows.len());
    for row in rows {
        let file = row["file"].as_str().expect("file");
        let (fixture, spec) =
            common::golden::load_fixture(grader.kind(), &testdata_dir().join(file)).expect("loads");
        assert_eq!(
            fixture.output,
            row["output"].as_str().expect("output"),
            "{file}"
        );
        if let Some(go_error) = row["validate_error"].as_str() {
            let error = grader.validate_spec(&spec).expect_err(file);
            assert_library_error(&error, go_error, file);
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
