//! Parity: `parity/fixtures/measurement-io/graders_framework.json` registry and
//! dimension-mapping rows (`internal/graders/grader_test.go`, `envelope_test.go`).

mod common;

use std::collections::BTreeMap;
use std::error::Error as _;
use std::sync::Arc;

use common::block_on;
use common::golden::Counting;
use maverick_domain::gojson::{self, JsonValue};
use maverick_domain::graders::{
    BoxError, BoxFuture, GradeError, GradeInput, GradeResult, Grader, GraderError, RegisterError,
    Registry, SpecValidator, parse_envelope,
};
use serde_json::Value;

fn framework() -> Value {
    parity_support::fixture("measurement-io", "graders_framework.json").expect("fixture loads")
}

fn registry_case(go_test: &str) -> Value {
    framework()["registry_cases"]
        .as_array()
        .expect("cases")
        .iter()
        .find(|c| c["go_test"] == go_test)
        .unwrap_or_else(|| panic!("no case {go_test}"))
        .clone()
}

/// What a fake grader does when asked to grade.
enum Behavior {
    Succeed(BTreeMap<String, f64>, String),
    Fail(String),
    FailWithGraderError(String, String),
}

struct Fake {
    kind: String,
    behavior: Behavior,
}

fn fake(kind: &str, behavior: Behavior) -> Arc<dyn Grader> {
    Arc::new(Fake {
        kind: kind.to_owned(),
        behavior,
    })
}

fn noop(kind: &str) -> Arc<dyn Grader> {
    fake(kind, Behavior::Succeed(BTreeMap::new(), "{}".to_owned()))
}

impl SpecValidator for Fake {
    fn kind(&self) -> &str {
        &self.kind
    }

    fn validate_spec(&self, _spec: &JsonValue) -> Result<(), String> {
        Ok(())
    }
}

impl Grader for Fake {
    fn grade<'a>(
        &'a self,
        _spec: &'a JsonValue,
        _input: GradeInput<'a>,
    ) -> BoxFuture<'a, Result<GradeResult, BoxError>> {
        Box::pin(async move {
            match &self.behavior {
                Behavior::Succeed(dimensions, detail) => Ok(GradeResult {
                    dimensions: dimensions.clone(),
                    detail: detail.clone(),
                }),
                Behavior::Fail(message) => Err(message.clone().into()),
                Behavior::FailWithGraderError(kind, message) => {
                    Err(Box::new(GraderError::new(kind.clone(), message.clone())) as BoxError)
                }
            }
        })
    }
}

fn empty_spec() -> JsonValue {
    JsonValue::Object(Vec::new())
}

fn input() -> GradeInput<'static> {
    GradeInput {
        task: None,
        output: "output",
        run: None,
    }
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .expect("array")
        .iter()
        .map(|v| v.as_str().expect("string").to_owned())
        .collect()
}

/// parity: graders.TestRegistryRegisterAndLookup
#[test]
fn registry_register_and_lookup() {
    let case = registry_case("graders.TestRegistryRegisterAndLookup");
    let mut registry = Registry::new();
    for kind in strings(&case["register"]) {
        registry.register(noop(&kind)).expect("registers");
    }
    for (kind, found) in case["lookup"].as_object().expect("lookup") {
        assert_eq!(
            registry.lookup(kind).is_some(),
            found.as_bool().expect("bool"),
            "{kind}"
        );
    }
    let found = registry.lookup("rule_scan").expect("registered");
    assert_eq!(found.kind(), "rule_scan");
}

/// parity: graders.TestRegistryRegisterDuplicate
/// parity: graders.TestRegistryRegisterEmptyKind
#[test]
fn registry_register_errors() {
    for go_test in [
        "graders.TestRegistryRegisterDuplicate",
        "graders.TestRegistryRegisterEmptyKind",
    ] {
        let case = registry_case(go_test);
        let error_on = case["expect_error_on"].as_u64().expect("index") as usize;
        let mut registry = Registry::new();
        for (i, kind) in strings(&case["register"]).into_iter().enumerate() {
            let result = registry.register(noop(&kind));
            if i + 1 == error_on {
                let error = result.expect_err(go_test);
                assert_eq!(error.to_string(), case["error"], "{go_test}");
            } else {
                result.expect("registers");
            }
        }
    }
    assert_eq!(
        Registry::new().register(noop("")),
        Err(RegisterError::EmptyKind)
    );
}

/// parity: graders.TestRegistryMustRegisterDuplicatePanics
#[test]
fn registry_must_register_duplicate_panics() {
    let case = registry_case("graders.TestRegistryMustRegisterDuplicatePanics");
    let kinds = strings(&case["register_must"]);
    let mut registry = Registry::new();
    registry.must_register(noop(&kinds[0]));
    let second = kinds[1].clone();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        registry.must_register(noop(&second));
    }))
    .expect_err("second must_register panics");
    let message = panic.downcast_ref::<String>().cloned().unwrap_or_default();
    assert_eq!(
        message,
        format!("graders: register: duplicate kind \"{}\"", kinds[1])
    );
}

/// parity: graders.TestRegistryKindsSorted
#[test]
fn registry_kinds_sorted() {
    let case = registry_case("graders.TestRegistryKindsSorted");
    let mut registry = Registry::new();
    for kind in strings(&case["register"]) {
        registry.register(noop(&kind)).expect("registers");
    }
    assert_eq!(registry.kinds(), strings(&case["kinds"]));
}

/// parity: graders.TestRegistryGradeUnknownKind
#[test]
fn registry_grade_unknown_kind() {
    let case = registry_case("graders.TestRegistryGradeUnknownKind");
    let counter = Arc::new(Counting::default());
    let registry = Registry::with_failure_counter(counter.clone());
    let kind = case["grade_kind"].as_str().expect("kind");
    let error = block_on(registry.grade(kind, &empty_spec(), input())).expect_err("unknown");
    assert!(matches!(error, GradeError::NotRegistered(_)));
    assert!(error.source().is_none());
    assert_eq!(error.to_string(), case["error"]);
    assert_eq!(counter.count() as u64, case["failure_counter"]);
}

/// parity: graders.TestRegistryGradeSuccess
#[test]
fn registry_grade_success() {
    let case = registry_case("graders.TestRegistryGradeSuccess");
    let returns = &case["grader_returns"];
    let dimensions: BTreeMap<String, f64> =
        serde_json::from_value(returns["dims"].clone()).expect("dims");
    let detail = returns["detail"].as_str().expect("detail").to_owned();
    let counter = Arc::new(Counting::default());
    let mut registry = Registry::with_failure_counter(counter.clone());
    registry.must_register(fake("key_facts", Behavior::Succeed(dimensions, detail)));
    let result = block_on(registry.grade("key_facts", &empty_spec(), input())).expect("grades");
    let want: BTreeMap<String, f64> =
        serde_json::from_value(case["result"]["dims"].clone()).expect("dims");
    assert_eq!(result.dimensions, want);
    assert_eq!(result.detail, case["result"]["detail"]);
    assert_eq!(counter.count() as u64, case["failure_counter"]);
}

/// A grader error leaves no dimensions: in Go the registry drops a partial map; in
/// Rust `Err` carries none, so the absence holds by construction.
///
/// parity: graders.TestRegistryGradeErrorMarksDimensionsAbsent
#[test]
fn registry_grade_error_marks_dimensions_absent() {
    let case = registry_case("graders.TestRegistryGradeErrorMarksDimensionsAbsent");
    let kind = case["grader_kind"].as_str().expect("kind");
    let inner = case["grader_returns"]["error"].as_str().expect("error");
    let counter = Arc::new(Counting::default());
    let mut registry = Registry::with_failure_counter(counter.clone());
    registry.must_register(fake(kind, Behavior::Fail(inner.to_owned())));
    let error = block_on(registry.grade(kind, &empty_spec(), input())).expect_err("fails");
    assert!(case["result"]["dims"].is_null());
    assert_eq!(error.to_string(), case["result"]["error"]);
    let GradeError::Failed(grader_error) = &error else {
        panic!("not a grader failure: {error}");
    };
    assert_eq!(grader_error.kind, kind);
    // Unwraps to the grader's own error.
    assert_eq!(grader_error.source().expect("source").to_string(), inner);
    assert_eq!(
        error.source().expect("source").to_string(),
        grader_error.to_string()
    );
    assert_eq!(counter.count() as u64, case["failure_counter"]);
}

/// parity: graders.TestRegistryGradePreservesGraderError
#[test]
fn registry_grade_preserves_grader_error() {
    let case = registry_case("graders.TestRegistryGradePreservesGraderError");
    let kind = case["grader_kind"].as_str().expect("kind");
    let counter = Arc::new(Counting::default());
    let mut registry = Registry::with_failure_counter(counter.clone());
    // The grader's own GraderError names another kind, so a re-wrap would show.
    registry.must_register(fake(
        kind,
        Behavior::FailWithGraderError(kind.to_owned(), "bad regex".to_owned()),
    ));
    let error = block_on(registry.grade(kind, &empty_spec(), input())).expect_err("fails");
    assert_eq!(error.to_string(), case["result"]["error"]);

    let mut registry = Registry::with_failure_counter(counter.clone());
    registry.must_register(fake(
        kind,
        Behavior::FailWithGraderError("other_kind".to_owned(), "bad regex".to_owned()),
    ));
    let error = block_on(registry.grade(kind, &empty_spec(), input())).expect_err("fails");
    assert_eq!(error.to_string(), "grader other_kind: bad regex");
    assert_eq!(
        counter.count() as u64,
        2 * case["failure_counter"].as_u64().expect("n")
    );
}

/// Runs every `map_dimensions` row, Go tests and probes.
///
/// parity: graders.TestMapDimensions
#[test]
fn map_dimensions_rows() {
    for row in framework()["map_dimensions"].as_array().expect("rows") {
        let name = row["name"].as_str().expect("name");
        let raw = format!(
            r#"{{"type":"key_facts","version":1,"weight_dimensions":{},"spec":{{}}}}"#,
            row["weight_dimensions"]
        );
        let envelope = parse_envelope(&raw).unwrap_or_else(|e| panic!("{name}: {e}"));
        let emitted: Option<BTreeMap<String, f64>> =
            serde_json::from_value(row["emitted"].clone()).expect("emitted");
        let result = envelope.map_dimensions(emitted.as_ref());
        match row.get("error") {
            Some(want) => {
                let error = result.expect_err(name);
                assert_eq!(error.to_string(), *want, "{name}");
            }
            None => {
                let want: BTreeMap<String, f64> =
                    serde_json::from_value(row["mapped"].clone()).expect("mapped");
                assert_eq!(result.expect(name), want, "{name}");
            }
        }
    }
}

/// A NaN score survives the clamp, as in Go's `math.Min(1, math.Max(0, NaN))`.
#[test]
fn map_dimensions_keeps_nan() {
    let envelope =
        parse_envelope(r#"{"type":"key_facts","version":1,"weight_dimensions":{"a":1},"spec":{}}"#)
            .expect("parses");
    let emitted = BTreeMap::from([("a".to_owned(), f64::NAN)]);
    assert!(envelope.map_dimensions(Some(&emitted)).expect("maps")["a"].is_nan());
    let renamed = BTreeMap::from([("b".to_owned(), f64::NAN)]);
    assert!(envelope.map_dimensions(Some(&renamed)).expect("maps")["a"].is_nan());
}

/// The spec a grader receives is the envelope payload as Go would re-encode it.
#[test]
fn spec_payload_reaches_the_grader() {
    let raw = framework()["spec_example_envelope_raw"]
        .as_str()
        .expect("raw")
        .to_owned();
    let envelope = parse_envelope(&raw).expect("parses");
    assert!(gojson::marshal(&envelope.spec).contains("must_include"));
}
