//! Grader spec validation (`ValidateSpec`, reference Part 2 §6.2 and §7 to §11).
//!
//! The corpus validator checks every envelope's `spec` through a
//! [`ValidationRegistry`]. Each kind validates its spec against the embedded JSON
//! schema for (kind, version 1), then applies its own extra rules. Error texts start
//! with `<kind> spec: `; [`super::Envelope::validate_spec`] adds the
//! `graders: envelope: <kind> spec: ` prefix, so the prefix doubles as in Go
//! (QUIRK Q-G4).
//!
//! Schema-failure text after `<kind> spec: ` comes from the `jsonschema` crate, not
//! from Go's santhosh-tekuri library. Parity is the prefix and the pass/fail decision
//! (D24, D36).
//!
//! Each kind's validator runs [`check_schema`] first, then its extra rules:
//! golden_answer ([`super::golden`]), json_schema ([`super::json_schema`]), key_facts
//! ([`super::keyfacts`]), rule_scan ([`super::rulescan`]), and judge_rubric (duplicate
//! dimension, blank criterion).

use std::collections::{BTreeMap, HashSet};
use std::sync::LazyLock;

use serde::Deserialize;

use super::envelope::{Envelope, EnvelopeError, EnvelopeErrorKind, GraderKind};
use super::golden::GoldenAnswer;
use super::json_schema::JsonSchema;
use super::keyfacts::KeyFacts;
use super::rulescan::RuleScan;
use crate::gofmt;
use crate::gojson::{self, JsonValue};

/// Validates one kind's `spec` payload.
pub trait SpecValidator: Send + Sync {
    /// The envelope `type` this validator checks (`"golden_answer"`, ...).
    fn kind(&self) -> &str;

    /// `Ok` when the spec is valid. The error text starts with `<kind> spec: `.
    fn validate_spec(&self, spec: &JsonValue) -> Result<(), String>;

    /// `ValidateSpec(raw)` on raw JSON text: a syntax error is `<kind> spec: <err>`
    /// (`unexpected EOF`, ...), then [`SpecValidator::validate_spec`].
    fn validate_spec_json(&self, raw: &str) -> Result<(), String> {
        let (spec, rest) =
            gojson::decode_one(raw).map_err(|e| format!("{} spec: {e}", self.kind()))?;
        if let Some(c) = rest
            .trim_start_matches([' ', '\t', '\n', '\r'])
            .chars()
            .next()
        {
            return Err(format!(
                "{} spec: invalid character {} after top-level value",
                self.kind(),
                gojson::quote_char(c)
            ));
        }
        self.validate_spec(&spec)
    }
}

/// The validators the corpus loader uses, one per kind.
#[derive(Default)]
pub struct ValidationRegistry {
    validators: BTreeMap<String, Box<dyn SpecValidator>>,
}

impl ValidationRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// `ValidationRegistry()`: json_schema, rule_scan, golden_answer, key_facts, and the
    /// judge validator.
    pub fn v1() -> Self {
        let mut registry = Self::new();
        registry.register(Box::new(JsonSchema));
        registry.register(Box::new(GoldenAnswer));
        registry.register(Box::new(KeyFacts));
        registry.register(Box::new(RuleScan));
        registry.register(Box::new(JudgeRubricValidator));
        registry
    }

    /// Adds or replaces the validator for its kind.
    pub fn register(&mut self, validator: Box<dyn SpecValidator>) {
        self.validators
            .insert(validator.kind().to_owned(), validator);
    }

    pub fn get(&self, kind: &str) -> Option<&dyn SpecValidator> {
        self.validators.get(kind).map(|v| v.as_ref())
    }

    /// The registered kinds in ascending order.
    pub fn kinds(&self) -> Vec<&str> {
        self.validators.keys().map(String::as_str).collect()
    }
}

impl Envelope {
    /// `Envelope.ValidateSpec(reg)`.
    pub fn validate_spec(&self, registry: &ValidationRegistry) -> Result<(), EnvelopeError> {
        let Some(validator) = registry.get(self.kind.as_str()) else {
            return Err(spec_error(
                EnvelopeErrorKind::NotRegistered,
                format!(
                    "no grader registered for type {}",
                    gofmt::quote(self.kind.as_str())
                ),
            ));
        };
        validator.validate_spec(&self.spec).map_err(|e| {
            spec_error(
                EnvelopeErrorKind::SpecInvalid,
                format!("{} spec: {e}", self.kind),
            )
        })
    }
}

fn spec_error(kind: EnvelopeErrorKind, detail: String) -> EnvelopeError {
    EnvelopeError::new(kind, detail)
}

/// The embedded spec schemas, byte for byte from the Go engine.
fn schema_source(kind: GraderKind) -> &'static str {
    match kind {
        GraderKind::JsonSchema => include_str!("../../schemas/jsonschema.spec.schema.json"),
        GraderKind::RuleScan => include_str!("../../schemas/rulescan.spec.schema.json"),
        GraderKind::GoldenAnswer => include_str!("../../schemas/golden.spec.schema.json"),
        GraderKind::KeyFacts => include_str!("../../schemas/keyfacts.spec.schema.json"),
        GraderKind::JudgeRubric => include_str!("../../schemas/judgerubric.spec.schema.json"),
    }
}

static SCHEMAS: LazyLock<BTreeMap<GraderKind, jsonschema::Validator>> = LazyLock::new(|| {
    GraderKind::ALL
        .into_iter()
        .map(|kind| {
            let schema: serde_json::Value = serde_json::from_str(schema_source(kind))
                .expect("embedded spec schema is valid JSON");
            let validator =
                jsonschema::draft202012::new(&schema).expect("embedded spec schema compiles");
            (kind, validator)
        })
        .collect()
});

/// Validates `spec` against the kind's embedded schema. Returns the spec as a
/// `serde_json::Value` for the kind's own decoding. Errors start with `<kind> spec: `.
pub fn check_schema(kind: GraderKind, spec: &JsonValue) -> Result<serde_json::Value, String> {
    let prefix = format!("{kind} spec: ");
    let value = spec.to_serde().map_err(|e| format!("{prefix}{e}"))?;
    let failures: Vec<String> = SCHEMAS[&kind]
        .iter_errors(&value)
        .map(|error| format!("- at '{}': {error}", error.instance_path()))
        .collect();
    if failures.is_empty() {
        Ok(value)
    } else {
        Err(format!(
            "{prefix}jsonschema validation failed\n{}",
            failures.join("\n")
        ))
    }
}

/// The judge_rubric spec (reference Part 2 §11).
#[derive(Debug, Clone, Deserialize)]
pub struct JudgeRubricSpec {
    pub rubric: Vec<RubricItem>,
    #[serde(default)]
    pub judge_prompt: String,
    #[serde(default)]
    pub reference: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RubricItem {
    pub dimension: String,
    pub criterion: String,
}

/// `judgerubric.NewValidator()`: schema, then in rubric order a duplicate dimension
/// (reported at the second occurrence) or a whitespace-only criterion.
pub struct JudgeRubricValidator;

impl SpecValidator for JudgeRubricValidator {
    fn kind(&self) -> &str {
        GraderKind::JudgeRubric.as_str()
    }

    fn validate_spec(&self, spec: &JsonValue) -> Result<(), String> {
        let value = check_schema(GraderKind::JudgeRubric, spec)?;
        let spec: JudgeRubricSpec =
            serde_json::from_value(value).map_err(|e| format!("judge_rubric spec: {e}"))?;
        let mut seen = HashSet::new();
        for (i, item) in spec.rubric.iter().enumerate() {
            if !seen.insert(item.dimension.as_str()) {
                return Err(format!(
                    "judge_rubric spec: rubric[{i}]: duplicate dimension {}",
                    gofmt::quote(&item.dimension)
                ));
            }
            // Rust's `trim` uses the same whitespace set as Go's `strings.TrimSpace`.
            if item.criterion.trim().is_empty() {
                return Err(format!(
                    "judge_rubric spec: rubric[{i}] ({}): criterion is blank",
                    item.dimension
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gojson;
    use crate::graders::parse_envelope;

    fn spec(json: &str) -> JsonValue {
        gojson::decode_one(json).unwrap().0
    }

    #[test]
    fn registry_holds_the_five_kinds() {
        // tasks.json#validation_registry_kinds
        assert_eq!(
            ValidationRegistry::v1().kinds(),
            [
                "golden_answer",
                "json_schema",
                "judge_rubric",
                "key_facts",
                "rule_scan"
            ]
        );
    }

    #[test]
    fn json_schema_validator_checks_the_spec_schema() {
        let registry = ValidationRegistry::v1();
        let validator = registry.get("json_schema").expect("registered");
        let error = validator
            .validate_spec(&spec(r#"{"schema":1}"#))
            .unwrap_err();
        assert!(
            error.starts_with("json_schema spec: jsonschema validation failed"),
            "{error}"
        );
        assert!(
            validator
                .validate_spec(&spec(r#"{"schema":{"type":"object"}}"#))
                .is_ok()
        );
    }

    #[test]
    fn every_embedded_schema_compiles() {
        for kind in GraderKind::ALL {
            assert!(SCHEMAS.contains_key(&kind), "{kind}");
        }
    }

    #[test]
    fn schema_failure_has_the_contract_prefix() {
        let error = check_schema(GraderKind::GoldenAnswer, &spec(r#"{"answers":[]}"#)).unwrap_err();
        assert!(
            error
                .starts_with("golden_answer spec: jsonschema validation failed\n- at '/answers': "),
            "{error}"
        );
        assert!(
            check_schema(
                GraderKind::GoldenAnswer,
                &spec(r#"{"answers":[{"value":"x"}]}"#)
            )
            .is_ok()
        );
        // Exactly one answer mode, and tolerance needs numeric.
        assert!(
            check_schema(
                GraderKind::GoldenAnswer,
                &spec(r#"{"answers":[{"value":"x","regex":"y"}]}"#)
            )
            .is_err()
        );
        assert!(
            check_schema(
                GraderKind::GoldenAnswer,
                &spec(r#"{"answers":[{"value":"x","tolerance":1}]}"#)
            )
            .is_err()
        );
        // Formats are annotations only.
        assert!(
            check_schema(
                GraderKind::JsonSchema,
                &spec(r#"{"schema":{"format":"email"}}"#)
            )
            .is_ok()
        );
        assert!(
            check_schema(
                GraderKind::JsonSchema,
                &spec(r#"{"schema":true,"checks":[{"path":"a","equals":1}]}"#)
            )
            .is_err()
        );
        assert!(check_schema(GraderKind::KeyFacts, &spec(r#"{}"#)).is_err());
        assert!(
            check_schema(
                GraderKind::RuleScan,
                &spec(r#"{"rules":[{"line_count":{}}]}"#)
            )
            .is_err()
        );
        assert!(
            check_schema(
                GraderKind::RuleScan,
                &spec(r#"{"rules":[{"language":"en","regex":true}]}"#)
            )
            .is_err()
        );
    }

    #[test]
    fn judge_rubric_extra_rules() {
        let validator = JudgeRubricValidator;
        assert_eq!(
            validator
                .validate_spec(&spec(
                    r#"{"rubric":[{"dimension":"a","criterion":"x"},{"dimension":"a","criterion":"y"}]}"#
                ))
                .unwrap_err(),
            "judge_rubric spec: rubric[1]: duplicate dimension \"a\""
        );
        assert_eq!(
            validator
                .validate_spec(&spec(
                    r#"{"rubric":[{"dimension":"a","criterion":"x"},{"dimension":"b","criterion":" \t　"}]}"#
                ))
                .unwrap_err(),
            "judge_rubric spec: rubric[1] (b): criterion is blank"
        );
        assert!(validator
            .validate_spec(&spec(
                r#"{"rubric":[{"dimension":"a","criterion":"x"}],"reference":"r","judge_prompt":"p"}"#
            ))
            .is_ok());
        assert!(
            validator
                .validate_spec(&spec(r#"{"rubric":[{"dimension":"A","criterion":"x"}]}"#))
                .unwrap_err()
                .starts_with("judge_rubric spec: jsonschema validation failed")
        );
    }

    #[test]
    fn envelope_prefix_doubles() {
        let envelope = parse_envelope(
            r#"{"type":"judge_rubric","version":1,"weight_dimensions":{"t":1},"spec":{"rubric":[{"dimension":"t","criterion":"a"},{"dimension":"t","criterion":"b"}]}}"#,
        )
        .unwrap();
        let error = envelope
            .validate_spec(&ValidationRegistry::v1())
            .unwrap_err();
        assert_eq!(error.kind, EnvelopeErrorKind::SpecInvalid);
        assert_eq!(
            error.to_string(),
            "graders: envelope: judge_rubric spec: judge_rubric spec: rubric[1]: duplicate dimension \"t\""
        );
        let error = envelope
            .validate_spec(&ValidationRegistry::new())
            .unwrap_err();
        assert_eq!(error.kind, EnvelopeErrorKind::NotRegistered);
        assert_eq!(
            error.to_string(),
            "graders: envelope: no grader registered for type \"judge_rubric\""
        );
    }
}
