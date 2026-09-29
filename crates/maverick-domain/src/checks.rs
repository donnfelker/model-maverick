//! The per-capability check registry (spec §8.4; reference Part 4 §2 "Check specs").
//!
//! Every recommend response carries a `check_spec` reference for its capability, and
//! `GET /v1/checks/{ref}` serves the definition so callers can run the check on model
//! output. The spec documents are part of the parity contract and are kept as the
//! exact bytes Go served.

use serde::Serialize;

use crate::gojson;

/// The `rule_scan` grader type.
pub const RULE_SCAN: &str = "rule_scan";
/// The `json_schema` grader type.
pub const JSON_SCHEMA: &str = "json_schema";

const RULE_SCAN_SPEC: &str =
    r#"{"rules":[{"must_include":"\\S","regex":true,"weight":1,"critical":true}]}"#;
const EXTRACTION_SPEC: &str = r#"{"schema":{"$schema":"https://json-schema.org/draft/2020-12/schema","type":["object","array"]}}"#;
const SCHEMA_FOLLOWING_SPEC: &str =
    r#"{"schema":{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object"}}"#;

/// The `check_spec` reference on a recommend response: `{type, ref}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CheckSpec {
    #[serde(rename = "type")]
    pub kind: &'static str,
    #[serde(rename = "ref")]
    pub reference: &'static str,
}

/// A registered check definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckDefinition {
    pub reference: &'static str,
    pub capability: &'static str,
    pub kind: &'static str,
    /// The spec as the exact JSON bytes served inside the document.
    pub spec_json: &'static str,
}

impl CheckDefinition {
    /// The `{type, ref}` reference to this definition.
    pub fn check_spec(&self) -> CheckSpec {
        CheckSpec {
            kind: self.kind,
            reference: self.reference,
        }
    }

    /// The `GET /v1/checks/{ref}` body: `{"ref","capability","type","spec"}` with the
    /// spec embedded byte for byte.
    pub fn document(&self) -> String {
        let mut out = String::with_capacity(64 + self.spec_json.len());
        out.push_str("{\"ref\":");
        gojson::write_string(&mut out, self.reference);
        out.push_str(",\"capability\":");
        gojson::write_string(&mut out, self.capability);
        out.push_str(",\"type\":");
        gojson::write_string(&mut out, self.kind);
        out.push_str(",\"spec\":");
        out.push_str(self.spec_json);
        out.push('}');
        out
    }
}

const fn definition(
    capability: &'static str,
    kind: &'static str,
    reference: &'static str,
    spec_json: &'static str,
) -> CheckDefinition {
    CheckDefinition {
        reference,
        capability,
        kind,
        spec_json,
    }
}

/// One definition per v1 capability, in the capability enum order.
pub const CHECK_DEFINITIONS: [CheckDefinition; 10] = [
    definition(
        "code_generation",
        RULE_SCAN,
        "checks/code_generation/v1",
        RULE_SCAN_SPEC,
    ),
    definition(
        "structured_extraction",
        JSON_SCHEMA,
        "checks/extraction/v1",
        EXTRACTION_SPEC,
    ),
    definition(
        "summarization_synthesis",
        RULE_SCAN,
        "checks/summarization/v1",
        RULE_SCAN_SPEC,
    ),
    definition(
        "multi_step_reasoning",
        RULE_SCAN,
        "checks/reasoning/v1",
        RULE_SCAN_SPEC,
    ),
    definition(
        "instruction_schema_following",
        JSON_SCHEMA,
        "checks/schema_following/v1",
        SCHEMA_FOLLOWING_SPEC,
    ),
    definition(
        "long_context_qa",
        RULE_SCAN,
        "checks/long_context_qa/v1",
        RULE_SCAN_SPEC,
    ),
    definition(
        "factual_qa",
        RULE_SCAN,
        "checks/factual_qa/v1",
        RULE_SCAN_SPEC,
    ),
    definition(
        "rewriting_editing",
        RULE_SCAN,
        "checks/rewriting/v1",
        RULE_SCAN_SPEC,
    ),
    definition(
        "translation_multilingual",
        RULE_SCAN,
        "checks/translation/v1",
        RULE_SCAN_SPEC,
    ),
    definition(
        "critique_and_judging",
        RULE_SCAN,
        "checks/critique/v1",
        RULE_SCAN_SPEC,
    ),
];

/// `CheckSpecFor(capability)`: the reference for a capability; an unknown capability
/// gets `{type: "", ref: ""}`.
pub fn check_spec_for(capability: &str) -> CheckSpec {
    CHECK_DEFINITIONS
        .iter()
        .find(|definition| definition.capability == capability)
        .map_or(
            CheckSpec {
                kind: "",
                reference: "",
            },
            CheckDefinition::check_spec,
        )
}

/// `LookupCheck(ref)`: the definition behind a reference, or `None` (the handler
/// answers 404).
pub fn lookup_check(reference: &str) -> Option<&'static CheckDefinition> {
    CHECK_DEFINITIONS
        .iter()
        .find(|definition| definition.reference == reference)
}
