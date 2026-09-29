//! Parity: `parity/fixtures/decision/checks.json` (`internal/recommend` check registry).

use maverick_domain::checks::{
    CHECK_DEFINITIONS, JSON_SCHEMA, RULE_SCAN, check_spec_for, lookup_check,
};
use maverick_domain::gojson;
use maverick_domain::graders::{JsonSchema, RuleScan, SpecValidator};
use maverick_domain::tasks::CAPABILITIES;
use serde_json::{Value, json};

fn cases() -> Vec<Value> {
    let fixture = parity_support::fixture("decision", "checks.json").expect("fixture loads");
    fixture["cases"].as_array().expect("cases").clone()
}

// parity: recommend.TestEveryCapabilityHasAResolvableCheckSpec
// parity: recommend.TestLookupCheckUnknownRef
#[test]
fn every_capability_resolves_to_go_definition() {
    let cases = cases();
    assert_eq!(cases.len(), 10);
    for case in &cases {
        let id = case["id"].as_str().expect("id");
        let capability = case["capability"].as_str().expect("capability");

        let spec = check_spec_for(capability);
        assert_eq!(
            serde_json::to_value(spec).expect("spec serializes"),
            case["check_spec"],
            "{id}: check_spec"
        );

        let definition = lookup_check(spec.reference).expect("registered ref");
        assert_eq!(definition.capability, capability, "{id}");
        let spec_exact = case["spec_exact"].as_str().expect("spec_exact");
        assert_eq!(definition.spec_json, spec_exact, "{id}: spec bytes");

        let document = definition.document();
        let parsed: Value = serde_json::from_str(&document).expect("document is JSON");
        assert_eq!(parsed, case["definition"], "{id}: document");
        assert!(
            document.ends_with(&format!("\"spec\":{spec_exact}}}")),
            "{id}: spec embedded byte for byte: {document}"
        );
    }

    // Every v1 capability has a check, in enum order.
    let registered: Vec<&str> = CHECK_DEFINITIONS.iter().map(|d| d.capability).collect();
    assert_eq!(registered, CAPABILITIES);

    // Unknown capability and unknown ref.
    assert_eq!(
        serde_json::to_value(check_spec_for("poetry")).expect("serializes"),
        json!({"type": "", "ref": ""})
    );
    assert!(lookup_check("checks/poetry/v1").is_none());
    assert!(lookup_check("").is_none());
}

// parity: recommend.TestCheckSpecTypesFollowSection84
#[test]
fn check_types_follow_section_8_4() {
    for definition in CHECK_DEFINITIONS {
        let expected = match definition.capability {
            "structured_extraction" | "instruction_schema_following" => JSON_SCHEMA,
            _ => RULE_SCAN,
        };
        assert_eq!(definition.kind, expected, "{}", definition.capability);
        assert!(definition.reference.starts_with("checks/"));
        assert!(definition.reference.ends_with("/v1"));
    }
}

// parity: recommend.TestCheckDefinitionSpecsAreExecutableShapes
// parity: recommend.TestCheckDefinitionSpecsValidateAgainstGraders
#[test]
fn check_specs_validate_against_their_graders() {
    for definition in CHECK_DEFINITIONS {
        let spec = gojson::unmarshal(definition.spec_json).expect("spec is JSON");
        let result = match definition.kind {
            JSON_SCHEMA => JsonSchema.validate_spec(&spec),
            RULE_SCAN => RuleScan.validate_spec(&spec),
            other => panic!("unexpected check type {other}"),
        };
        assert_eq!(result, Ok(()), "{}", definition.reference);
    }
}
