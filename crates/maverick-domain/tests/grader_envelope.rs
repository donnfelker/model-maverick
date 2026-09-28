//! Parity: `parity/fixtures/measurement-io/graders_framework.json` envelope rows
//! (`internal/graders/envelope_test.go`).

use maverick_domain::gojson::{self, JsonValue};
use maverick_domain::graders::{
    EnvelopeErrorKind, GraderKind, SpecValidator, ValidationRegistry, parse_envelope,
    parse_envelope_value,
};
use serde_json::Value;

fn framework() -> Value {
    parity_support::fixture("measurement-io", "graders_framework.json").expect("fixture loads")
}

/// The `parsed` object of a fixture row as JSON, for comparison with an envelope.
fn envelope_as_json(envelope: &maverick_domain::graders::Envelope) -> Value {
    serde_json::json!({
        "type": envelope.kind.as_str(),
        "version": envelope.version,
        "weight_dimensions": envelope.weight_dimensions,
        "spec": envelope.spec.to_serde().expect("spec converts"),
    })
}

/// JSON equality that compares numbers by value (`1` equals `1.0`), as the fixture
/// records Go's decoded values.
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

/// Runs every `parse_envelope` row: parsed rows compare the decoded envelope, error rows
/// compare the exact error text.
///
/// parity: graders.TestParseEnvelopeValid
/// parity: graders.TestParseEnvelopeUnknownType
/// parity: graders.TestParseEnvelopeTestSuiteRejected
/// parity: graders.TestParseEnvelopeMissingFields
/// parity: graders.TestParseEnvelopeBadWeights
/// parity: graders.TestParseEnvelopeBadVersion
/// parity: graders.TestParseEnvelopeUnknownField
#[test]
fn parse_envelope_rows() {
    let rows = framework()["parse_envelope"]
        .as_array()
        .expect("rows")
        .clone();
    assert!(rows.len() >= 20, "fixture has {} rows", rows.len());
    for row in rows {
        let name = row["case"].as_str().expect("case name");
        let input = row["input"].as_str().expect("input");
        let result = parse_envelope(input);
        if let Some(parsed) = row.get("parsed") {
            let envelope = result.unwrap_or_else(|e| panic!("{name}: unexpected error {e}"));
            let got = envelope_as_json(&envelope);
            assert!(same_json(&got, parsed), "{name}: {got} != {parsed}");
        } else {
            let want = row["error"].as_str().expect("error text");
            let got = result
                .err()
                .unwrap_or_else(|| panic!("{name}: expected an error"));
            assert_eq!(got.to_string(), want, "{name}");
        }
    }
}

/// parity: graders.TestParseEnvelopeValueYAML
#[test]
fn parse_envelope_value_yaml() {
    let case = &framework()["parse_envelope_value_yaml"];
    let yaml: serde_norway::Value =
        serde_norway::from_str(case["yaml"].as_str().expect("yaml")).expect("yaml parses");
    let envelope = parse_envelope_value(&yaml).expect("envelope parses");
    let expect = &case["expect"];
    assert_eq!(envelope.kind.as_str(), expect["type"]);
    assert_eq!(envelope.version, expect["version"]);
    assert_eq!(
        envelope.weight_dimensions.len() as u64,
        expect["weight_dimension_count"]
    );
    let spec = gojson::marshal(&envelope.spec);
    assert!(spec.contains(expect["spec_contains"].as_str().expect("needle")));

    // json_from_yaml_value_example: the exact re-encoded bytes.
    let example = &framework()["json_from_yaml_value_example"];
    let yaml: serde_norway::Value =
        serde_norway::from_str(example["yaml"].as_str().expect("yaml")).expect("yaml parses");
    assert_eq!(
        gojson::marshal(&gojson::from_yaml(&yaml).expect("converts")),
        example["json"]
    );
}

/// parity: graders.TestParseEnvelopeValueNonStringKeys
#[test]
fn parse_envelope_value_non_string_keys() {
    let case = &framework()["parse_envelope_value_non_string_keys"];
    let yaml: serde_norway::Value = serde_norway::from_str(
        "type: rule_scan\nversion: 1\nweight_dimensions: {a: 1}\nspec: {1: \"x\"}\n",
    )
    .expect("yaml parses");
    let error = parse_envelope_value(&yaml).expect_err("non-string key");
    assert_eq!(error.kind, EnvelopeErrorKind::Decode);
    assert_eq!(error.to_string(), case["error"]);
}

/// A test validator that records the payload it receives.
struct Stub {
    reject: bool,
    seen: std::sync::Mutex<Option<String>>,
}

impl SpecValidator for Stub {
    fn kind(&self) -> GraderKind {
        GraderKind::RuleScan
    }

    fn validate_spec(&self, spec: &JsonValue) -> Result<(), String> {
        *self.seen.lock().expect("lock") = Some(gojson::marshal(spec));
        if self.reject {
            Err("bad rule grammar".to_owned())
        } else {
            Ok(())
        }
    }
}

/// parity: graders.TestEnvelopeValidateSpecDelegates
#[test]
fn validate_spec_delegates() {
    let case = &framework()["validate_spec_delegation"];
    let raw = framework()["spec_example_envelope_raw"]
        .as_str()
        .expect("raw")
        .to_owned();
    let envelope = parse_envelope(&raw).expect("parses");

    let error = envelope
        .validate_spec(&ValidationRegistry::new())
        .expect_err("unregistered");
    assert_eq!(error.to_string(), case["unregistered kind"]);

    let mut registry = ValidationRegistry::new();
    registry.register(Box::new(Stub {
        reject: false,
        seen: std::sync::Mutex::new(None),
    }));
    envelope.validate_spec(&registry).expect("accepted");

    let stub = std::sync::Arc::new(Stub {
        reject: true,
        seen: std::sync::Mutex::new(None),
    });
    struct Shared(std::sync::Arc<Stub>);
    impl SpecValidator for Shared {
        fn kind(&self) -> GraderKind {
            self.0.kind()
        }
        fn validate_spec(&self, spec: &JsonValue) -> Result<(), String> {
            self.0.validate_spec(spec)
        }
    }
    let mut registry = ValidationRegistry::new();
    registry.register(Box::new(Shared(stub.clone())));
    let error = envelope.validate_spec(&registry).expect_err("rejected");
    assert_eq!(error.to_string(), case["propagates grader rejection"]);
    let seen = stub.seen.lock().expect("lock").clone().expect("delegated");
    assert!(seen.contains("must_include"), "{seen}");
}

/// The spec schemas embedded in the crate are byte-for-byte the Go engine's.
#[test]
fn embedded_spec_schemas_match_parity() {
    for name in [
        "golden.spec.schema.json",
        "jsonschema.spec.schema.json",
        "judgerubric.spec.schema.json",
        "keyfacts.spec.schema.json",
        "rulescan.spec.schema.json",
    ] {
        let embedded = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("schemas")
                .join(name),
        )
        .expect("embedded schema");
        let parity = parity_support::fixture_bytes("measurement-io", format!("schemas/{name}"))
            .expect("parity schema");
        assert!(embedded == parity, "{name} drifted from parity");
    }
}
