//! Parity: `parity/fixtures/measurement-io/testutil_harness.json` (the golden-fixture
//! harness self-tests, `graders/testutil/golden_test.go`).

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use maverick_domain::gojson::JsonValue;
use maverick_domain::graders::{
    BoxError, BoxFuture, GradeError, GradeInput, GradeResult, Grader, SpecValidator,
};
use serde::Deserialize;
use serde_json::Value;

/// Kind `echo`. The spec is `{"fail": bool}` (unknown fields rejected). Grade fails with
/// `echo: forced failure` when `fail`; else `chars = min(1, len/100)`,
/// `nonempty = 1 if len > 0`, detail `{"length": len}` (byte lengths).
struct Echo;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EchoSpec {
    #[serde(default)]
    fail: bool,
}

fn echo_spec(spec: &JsonValue) -> Result<EchoSpec, String> {
    serde_json::from_value(spec.to_serde()?).map_err(|e| format!("echo spec: {e}"))
}

impl SpecValidator for Echo {
    fn kind(&self) -> &str {
        "echo"
    }

    fn validate_spec(&self, spec: &JsonValue) -> Result<(), String> {
        echo_spec(spec).map(|_| ())
    }
}

impl Grader for Echo {
    fn grade<'a>(
        &'a self,
        spec: &'a JsonValue,
        input: GradeInput<'a>,
    ) -> BoxFuture<'a, Result<GradeResult, BoxError>> {
        Box::pin(async move {
            if echo_spec(spec)?.fail {
                return Err("echo: forced failure".into());
            }
            let len = input.output.len();
            let dimensions = BTreeMap::from([
                ("chars".to_owned(), (len as f64 / 100.0).min(1.0)),
                ("nonempty".to_owned(), if len > 0 { 1.0 } else { 0.0 }),
            ]);
            Ok(GradeResult {
                dimensions,
                detail: format!("{{\"length\":{len}}}"),
            })
        })
    }
}

/// Kind `rule_scan`. The spec must be a JSON object. `constraint_compliance` is 1 when
/// the output contains `WHERE`; detail `{"matched": bool}`.
struct Where;

impl SpecValidator for Where {
    fn kind(&self) -> &str {
        "rule_scan"
    }

    fn validate_spec(&self, spec: &JsonValue) -> Result<(), String> {
        match spec {
            JsonValue::Object(_) => Ok(()),
            other => Err(format!(
                "rule_scan spec: not an object: {}",
                other.kind_name()
            )),
        }
    }
}

impl Grader for Where {
    fn grade<'a>(
        &'a self,
        _spec: &'a JsonValue,
        input: GradeInput<'a>,
    ) -> BoxFuture<'a, Result<GradeResult, BoxError>> {
        Box::pin(async move {
            let matched = input.output.contains("WHERE");
            Ok(GradeResult {
                dimensions: BTreeMap::from([(
                    "constraint_compliance".to_owned(),
                    if matched { 1.0 } else { 0.0 },
                )]),
                detail: format!("{{\"matched\":{matched}}}"),
            })
        })
    }
}

fn harness() -> Value {
    parity_support::fixture("measurement-io", "testutil_harness.json").expect("fixture loads")
}

fn testdata(relative: &str) -> PathBuf {
    parity_support::fixture_path("measurement-io", relative).expect("fixture path")
}

fn harness_case(go_test: &str) -> Value {
    harness()["cases"]
        .as_array()
        .expect("cases")
        .iter()
        .find(|c| c["go_test"] == go_test)
        .unwrap_or_else(|| panic!("no case {go_test}"))
        .clone()
}

/// Checks the exact result the harness saw for one fixture file.
fn assert_case(grader: Arc<dyn Grader>, case: &Value) {
    let path = testdata(case["fixture"].as_str().expect("fixture"));
    let (fixture, spec) = common::golden::load_fixture(grader.kind(), &path).expect("loads");
    let (result, failures) = common::golden::grade_fixture(grader, &fixture, &spec);
    if let Some(error) = case["grade_error"].as_str() {
        match result {
            Err(GradeError::Failed(e)) => assert_eq!(e.to_string(), error),
            other => panic!("expected a grader failure, got {other:?}"),
        }
        assert!(case["dimensions"].is_null());
        assert_eq!(failures as u64, case["failure_counter"]);
    } else {
        let result = result.expect("grades");
        let want: BTreeMap<String, f64> =
            serde_json::from_value(case["dimensions"].clone()).expect("dimensions");
        assert_eq!(result.dimensions, want);
        assert_eq!(result.detail, case["detail_exact_bytes"]);
        assert_eq!(failures, 0);
    }
}

/// parity: testutil.TestRunGoldenEcho
#[test]
fn run_golden_echo() {
    let grader: Arc<dyn Grader> = Arc::new(Echo);
    let dir = testdata("testdata/internal/graders/testutil/testdata");
    // Not recursive: the rulescan/ subdirectory is not an echo fixture.
    let names = common::golden::run_dir(grader.clone(), &dir);
    assert_eq!(names, ["echo_basic", "echo_error"]);

    let basic = harness_case("testutil.TestRunGoldenEcho/echo_basic");
    let path = testdata(basic["fixture"].as_str().expect("fixture"));
    let (fixture, _) = common::golden::load_fixture("echo", &path).expect("loads");
    assert_eq!(fixture.output.len() as u64, basic["output_len_bytes"]);
    assert_case(grader.clone(), &basic);
    assert_case(
        grader,
        &harness_case("testutil.TestRunGoldenEcho/echo_error"),
    );
}

/// parity: testutil.TestRunGoldenDirEnvelopeFixture
#[test]
fn run_golden_dir_envelope_fixture() {
    let grader: Arc<dyn Grader> = Arc::new(Where);
    let dir = testdata("testdata/internal/graders/testutil/testdata/rulescan");
    let names = common::golden::run_dir(grader.clone(), &dir);
    assert_eq!(names, ["where_pass"]);
    assert_case(
        grader,
        &harness_case("testutil.TestRunGoldenDirEnvelopeFixture/where_pass"),
    );
}

/// The harness rejects fixtures that break its contract.
#[test]
fn harness_rejects_bad_fixtures() {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("grader_harness_bad");
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("clear");
    }
    std::fs::create_dir_all(&dir).expect("mkdir");
    let grader: Arc<dyn Grader> = Arc::new(Echo);
    for (name, text, needle) in [
        (
            "both.yaml",
            "spec: {}\nenvelope: {}\nexpect: {error: true}\n",
            "exactly one",
        ),
        (
            "neither.yaml",
            "output: x\nexpect: {error: true}\n",
            "exactly one",
        ),
        ("no_expect.yaml", "spec: {}\nexpect: {}\n", "must expect"),
        (
            "unknown_key.yaml",
            "spec: {}\nnotes: x\nexpect: {error: true}\n",
            "notes",
        ),
        (
            "bad_spec.yaml",
            "spec: {bogus: 1}\nexpect: {dimensions: {}}\n",
            "validate_spec",
        ),
        (
            "spec_error_ok.yaml",
            "spec: {bogus: 1}\nexpect: {error: true, error_contains: bogus}\n",
            "",
        ),
        (
            "wrong_dims.yaml",
            "spec: {}\noutput: abc\nexpect: {dimensions: {chars: 0.03}}\n",
            "dimensions",
        ),
        (
            "wrong_score.yaml",
            "spec: {}\noutput: abc\nexpect: {dimensions: {chars: 0.5, nonempty: 1}}\n",
            "chars = 0.03",
        ),
        (
            "epsilon.yaml",
            "spec: {}\noutput: abc\nepsilon: 0.5\nexpect: {dimensions: {chars: 0.5, nonempty: 1}}\n",
            "",
        ),
        (
            "detail.yaml",
            "spec: {}\noutput: abc\nexpect: {dimensions: {chars: 0.03, nonempty: 1}, detail_contains: ['\"length\":4']}\n",
            "lacks",
        ),
        (
            "unexpected_ok.yaml",
            "spec: {}\noutput: abc\nexpect: {error: true}\n",
            "expected an error",
        ),
        (
            "wrong_error.yaml",
            "spec: {fail: true}\nexpect: {error: true, error_contains: nope}\n",
            "lacks",
        ),
        (
            "envelope_kind.yaml",
            "envelope: {type: key_facts, version: 1, weight_dimensions: {a: 1}, spec: {}}\nexpect: {error: true}\n",
            "is not the grader kind",
        ),
    ] {
        let path = dir.join(name);
        std::fs::write(&path, text).expect("write");
        let result = common::golden::run_file(grader.clone(), &path);
        if needle.is_empty() {
            assert_eq!(result, Ok(()), "{name}");
        } else {
            let error = result.expect_err(name);
            assert!(error.contains(needle), "{name}: {error}");
        }
    }
}
