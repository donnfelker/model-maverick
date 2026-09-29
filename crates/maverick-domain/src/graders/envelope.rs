//! The grader spec envelope (spec §5.4, reference Part 2 §6.2).
//!
//! Every grader spec in a task arrives as
//! `{"type": ..., "version": 1, "weight_dimensions": {...}, "spec": ...}`.
//! [`parse_envelope`] decodes it with Go `encoding/json` rules (strict fields,
//! case-insensitive names, no trailing data) and applies the envelope checks in Go's
//! order. The error texts are exact.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::gofmt;
use crate::gojson::{self, JsonValue};

/// The five grader kinds that v1 registers. `test_suite` is reserved and rejected
/// ([`TEST_SUITE`]), so it is not a variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraderKind {
    JsonSchema,
    RuleScan,
    GoldenAnswer,
    KeyFacts,
    JudgeRubric,
}

impl GraderKind {
    /// Every kind, in the order the reference lists them.
    pub const ALL: [GraderKind; 5] = [
        GraderKind::JsonSchema,
        GraderKind::RuleScan,
        GraderKind::GoldenAnswer,
        GraderKind::KeyFacts,
        GraderKind::JudgeRubric,
    ];

    /// The envelope `type` string.
    pub fn as_str(self) -> &'static str {
        match self {
            GraderKind::JsonSchema => "json_schema",
            GraderKind::RuleScan => "rule_scan",
            GraderKind::GoldenAnswer => "golden_answer",
            GraderKind::KeyFacts => "key_facts",
            GraderKind::JudgeRubric => "judge_rubric",
        }
    }

    /// The kind for an envelope `type` string. `test_suite` is not a kind.
    pub fn from_type(type_name: &str) -> Option<GraderKind> {
        GraderKind::ALL
            .into_iter()
            .find(|k| k.as_str() == type_name)
    }
}

impl std::fmt::Display for GraderKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The reserved kind. Schema-valid, but rejected in v1: no model-generated code runs.
pub const TEST_SUITE: &str = "test_suite";

/// The only envelope version v1 registers.
pub const VERSION: i64 = 1;

/// True for the six type names the envelope schema knows (the five kinds and
/// `test_suite`).
pub fn is_known_type(type_name: &str) -> bool {
    type_name == TEST_SUITE || GraderKind::from_type(type_name).is_some()
}

/// A parsed, checked envelope.
#[derive(Debug, Clone, PartialEq)]
pub struct Envelope {
    pub kind: GraderKind,
    pub version: i64,
    /// Task dimension name to multiplier (normally 1.0). Non-empty; every multiplier is
    /// a positive finite number.
    pub weight_dimensions: BTreeMap<String, f64>,
    /// The kind-specific payload, not yet validated. Never `null`.
    pub spec: JsonValue,
}

/// Why an envelope was rejected. The corpus validator maps these to its rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvelopeErrorKind {
    /// JSON syntax, an unknown field, a type mismatch, or trailing data.
    Decode,
    MissingType,
    /// `test_suite` (Go `ErrGraderUnsupported`).
    Unsupported,
    UnknownType,
    MissingVersion,
    UnsupportedVersion,
    WeightDimensions,
    MissingSpec,
    /// No validator is registered for the kind.
    NotRegistered,
    /// The kind's validator rejected the spec.
    SpecInvalid,
    /// [`Envelope::map_dimensions`] could not pair emitted and task dimensions.
    AmbiguousMapping,
}

/// An envelope error. `to_string()` is the exact Go text, starting with
/// `graders: envelope: `.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvelopeError {
    pub kind: EnvelopeErrorKind,
    message: String,
}

impl EnvelopeError {
    pub(crate) fn new(kind: EnvelopeErrorKind, detail: impl std::fmt::Display) -> Self {
        EnvelopeError {
            kind,
            message: format!("graders: envelope: {detail}"),
        }
    }
}

impl std::fmt::Display for EnvelopeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for EnvelopeError {}

/// The fields of Go's `Envelope` struct, for case-insensitive matching.
const FIELDS: [&str; 4] = ["type", "version", "weight_dimensions", "spec"];

/// The raw decode result before the envelope checks.
#[derive(Default)]
struct Decoded {
    type_name: String,
    version: i64,
    weight_dimensions: Option<BTreeMap<String, f64>>,
    spec: Option<JsonValue>,
}

/// `ParseEnvelope(raw)`: decode, reject trailing data, then check in this order (first
/// failure wins): type present, not `test_suite`, known, version present, version 1,
/// weight_dimensions non-empty with valid entries, spec present and not `null`.
pub fn parse_envelope(raw: &str) -> Result<Envelope, EnvelopeError> {
    let decoded = decode(raw)?;
    check(decoded)
}

/// `ParseEnvelopeValue(v)`: a decoded YAML envelope (from a task file) re-encoded as
/// JSON with [`gojson::from_yaml`], then [`parse_envelope`].
pub fn parse_envelope_value(value: &serde_norway::Value) -> Result<Envelope, EnvelopeError> {
    let json = gojson::from_yaml(value).map_err(|message| EnvelopeError {
        kind: EnvelopeErrorKind::Decode,
        message,
    })?;
    parse_envelope(&gojson::marshal(&json))
}

fn decode(raw: &str) -> Result<Decoded, EnvelopeError> {
    let decode_err = |detail: String| EnvelopeError::new(EnvelopeErrorKind::Decode, detail);
    let (value, rest) = gojson::decode_one(raw).map_err(|e| decode_err(e.0))?;

    let mut out = Decoded::default();
    // Go keeps decoding after a type error and reports the first saved error.
    let mut first_error: Option<String> = None;
    let mut save = |error: String| {
        first_error.get_or_insert(error);
    };

    match value {
        JsonValue::Null => {}
        JsonValue::Object(members) => {
            for (key, value) in members {
                match match_field(&key) {
                    None => save(format!("json: unknown field {}", gofmt::quote(&key))),
                    Some("type") => match value {
                        JsonValue::String(s) => out.type_name = s,
                        JsonValue::Null => {}
                        other => save(type_error(other.kind_name(), "type", "string")),
                    },
                    Some("version") => match value {
                        JsonValue::Number(lit) => match lit.parse::<i64>() {
                            Ok(v) => out.version = v,
                            Err(_) => save(type_error(&format!("number {lit}"), "version", "int")),
                        },
                        JsonValue::Null => {}
                        other => save(type_error(other.kind_name(), "version", "int")),
                    },
                    Some("weight_dimensions") => match value {
                        JsonValue::Object(entries) => {
                            // Go decodes into the existing map, so a repeated field merges.
                            let map = out.weight_dimensions.get_or_insert_with(BTreeMap::new);
                            for (dim, weight) in entries {
                                let w = match weight {
                                    JsonValue::Number(lit) => match parse_go_f64(&lit) {
                                        Some(w) => w,
                                        None => {
                                            save(type_error(
                                                &format!("number {lit}"),
                                                "weight_dimensions",
                                                "float64",
                                            ));
                                            0.0
                                        }
                                    },
                                    // `null` into a map element stores the zero value.
                                    JsonValue::Null => 0.0,
                                    other => {
                                        save(type_error(
                                            other.kind_name(),
                                            "weight_dimensions",
                                            "float64",
                                        ));
                                        0.0
                                    }
                                };
                                map.insert(dim, w);
                            }
                        }
                        // `null` into a map sets it to nil.
                        JsonValue::Null => out.weight_dimensions = None,
                        other => save(type_error(
                            other.kind_name(),
                            "weight_dimensions",
                            "map[string]float64",
                        )),
                    },
                    Some(_) => out.spec = Some(value),
                }
            }
        }
        other => save(format!(
            "json: cannot unmarshal {} into Go value of type graders.Envelope",
            other.kind_name()
        )),
    }

    if let Some(error) = first_error {
        return Err(decode_err(error));
    }
    if !gojson::is_blank(rest) {
        return Err(decode_err("trailing data after envelope".to_owned()));
    }
    Ok(out)
}

fn check(decoded: Decoded) -> Result<Envelope, EnvelopeError> {
    use EnvelopeErrorKind as K;
    let type_name = decoded.type_name;
    if type_name.is_empty() {
        return Err(EnvelopeError::new(K::MissingType, "missing type"));
    }
    if type_name == TEST_SUITE {
        return Err(EnvelopeError::new(
            K::Unsupported,
            format!(
                "grader_unsupported: type {} is reserved until v1.1",
                gofmt::quote(TEST_SUITE)
            ),
        ));
    }
    let Some(kind) = GraderKind::from_type(&type_name) else {
        return Err(EnvelopeError::new(
            K::UnknownType,
            format!("unknown type {}", gofmt::quote(&type_name)),
        ));
    };
    if decoded.version == 0 {
        return Err(EnvelopeError::new(K::MissingVersion, "missing version"));
    }
    if decoded.version != VERSION {
        return Err(EnvelopeError::new(
            K::UnsupportedVersion,
            format!(
                "unsupported version {} for type {} (v1 registers version 1)",
                decoded.version,
                gofmt::quote(&type_name)
            ),
        ));
    }
    let weight_dimensions = decoded.weight_dimensions.unwrap_or_default();
    if weight_dimensions.is_empty() {
        return Err(EnvelopeError::new(
            K::WeightDimensions,
            "weight_dimensions must be a non-empty map",
        ));
    }
    // Go iterates the map in random order (QUIRK Q-G3); sorted order is deterministic
    // and reports the same text whenever only one entry is bad.
    for (dim, &w) in &weight_dimensions {
        if dim.is_empty() {
            return Err(EnvelopeError::new(
                K::WeightDimensions,
                "weight_dimensions has an empty dimension name",
            ));
        }
        if w <= 0.0 || !w.is_finite() {
            return Err(EnvelopeError::new(
                K::WeightDimensions,
                format!(
                    "weight_dimensions[{}] = {}: multiplier must be a positive finite number",
                    gofmt::quote(dim),
                    gofmt::fmt_v_f64(w)
                ),
            ));
        }
    }
    let spec = match decoded.spec {
        None | Some(JsonValue::Null) => {
            return Err(EnvelopeError::new(K::MissingSpec, "missing spec"));
        }
        Some(spec) => spec,
    };
    Ok(Envelope {
        kind,
        version: decoded.version,
        weight_dimensions,
        spec,
    })
}

impl Envelope {
    /// `MapDimensions(emitted)`: maps a grader's emitted dimensions onto the task's
    /// dimension names (reference Part 2 §6.2).
    ///
    /// A task dimension that the grader emitted under the same name gets
    /// `clamp01(emitted * multiplier)`. When exactly one task dimension is left open and
    /// exactly one emitted dimension is unclaimed, the emitted one is renamed onto it.
    /// Other unclaimed emitted dimensions are dropped and unemitted task dimensions
    /// stay absent, unless both sides have leftovers that cannot be paired: that is an
    /// error.
    pub fn map_dimensions(
        &self,
        emitted: Option<&BTreeMap<String, f64>>,
    ) -> Result<BTreeMap<String, f64>, EnvelopeError> {
        let empty = BTreeMap::new();
        let emitted = emitted.unwrap_or(&empty);
        let mut mapped = BTreeMap::new();
        let mut open = Vec::new();
        for (task_dim, &multiplier) in &self.weight_dimensions {
            match emitted.get(task_dim) {
                Some(&score) => {
                    mapped.insert(task_dim.clone(), clamp01(score * multiplier));
                }
                None => open.push(task_dim.as_str()),
            }
        }
        let leftover: Vec<&str> = emitted
            .keys()
            .filter(|d| !self.weight_dimensions.contains_key(*d))
            .map(String::as_str)
            .collect();
        if open.is_empty() || leftover.is_empty() {
            return Ok(mapped);
        }
        if let ([task_dim], [emitted_dim]) = (open.as_slice(), leftover.as_slice()) {
            let multiplier = self.weight_dimensions[*task_dim];
            mapped.insert(
                (*task_dim).to_owned(),
                clamp01(emitted[*emitted_dim] * multiplier),
            );
            return Ok(mapped);
        }
        // Both lists are already sorted (BTreeMap order). The dash is an em dash.
        Err(EnvelopeError::new(
            EnvelopeErrorKind::AmbiguousMapping,
            format!(
                "ambiguous weight_dimensions mapping for type {}: task dimensions [{}] vs emitted [{}] \u{2014} align the names",
                gofmt::quote(self.kind.as_str()),
                open.join(" "),
                leftover.join(" ")
            ),
        ))
    }
}

/// Go `math.Min(1, math.Max(0, x))`: NaN stays NaN and `-0` becomes `0`.
fn clamp01(x: f64) -> f64 {
    if x.is_nan() {
        x
    } else if x > 0.0 {
        x.min(1.0)
    } else {
        0.0
    }
}

/// The `type` string as the corpus validator reads it when it classifies an envelope
/// error: the last string-valued `type` field (matched like Go struct fields), or ""
/// when there is none.
pub fn raw_type(raw: &str) -> String {
    let Ok((JsonValue::Object(members), _)) = gojson::decode_one(raw) else {
        return String::new();
    };
    let mut type_name = String::new();
    for (key, value) in members {
        if let (Some("type"), JsonValue::String(s)) = (match_field(&key), value) {
            type_name = s;
        }
    }
    type_name
}

fn type_error(value: &str, field: &str, go_type: &str) -> String {
    format!(
        "json: cannot unmarshal {value} into Go struct field Envelope.{field} of type {go_type}"
    )
}

/// Go's struct field matching: an exact name first, then a case-insensitive match that
/// folds each rune to its simple case-fold class (so `TYPE`, `Spec`, and `ſpec` match).
fn match_field(key: &str) -> Option<&'static str> {
    FIELDS
        .iter()
        .find(|f| **f == key)
        .or_else(|| FIELDS.iter().find(|f| fold_eq(f, key)))
        .copied()
}

fn fold_eq(a: &str, b: &str) -> bool {
    let fold = |c: char| -> String { c.to_uppercase().collect::<String>().to_lowercase() };
    let mut a = a.chars();
    let mut b = b.chars();
    loop {
        match (a.next(), b.next()) {
            (None, None) => return true,
            (Some(x), Some(y)) if fold(x) == fold(y) => {}
            _ => return false,
        }
    }
}

/// Go `strconv.ParseFloat(lit, 64)` for a JSON number literal: `None` on overflow
/// (`1e309`).
fn parse_go_f64(lit: &str) -> Option<f64> {
    lit.parse::<f64>().ok().filter(|v| v.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(raw: &str) -> String {
        parse_envelope(raw).unwrap_err().to_string()
    }

    #[test]
    fn check_order_is_type_then_version_then_weights_then_spec() {
        assert_eq!(err("{}"), "graders: envelope: missing type");
        assert_eq!(
            err(r#"{"type":"x","version":0}"#),
            "graders: envelope: unknown type \"x\""
        );
        assert_eq!(
            err(r#"{"type":"test_suite"}"#),
            "graders: envelope: grader_unsupported: type \"test_suite\" is reserved until v1.1"
        );
        assert_eq!(
            err(r#"{"type":"key_facts"}"#),
            "graders: envelope: missing version"
        );
        assert_eq!(
            err(r#"{"type":"key_facts","version":-1}"#),
            "graders: envelope: unsupported version -1 for type \"key_facts\" (v1 registers version 1)"
        );
        assert_eq!(
            err(r#"{"type":"key_facts","version":1,"weight_dimensions":null}"#),
            "graders: envelope: weight_dimensions must be a non-empty map"
        );
        assert_eq!(
            err(r#"{"type":"key_facts","version":1,"weight_dimensions":{"a":1,"b":null}}"#),
            "graders: envelope: weight_dimensions[\"b\"] = 0: multiplier must be a positive finite number"
        );
        assert_eq!(
            err(r#"{"type":"key_facts","version":1,"weight_dimensions":{"a":1}}"#),
            "graders: envelope: missing spec"
        );
    }

    #[test]
    fn decode_errors() {
        assert_eq!(
            err(r#"{"type":5}"#),
            "graders: envelope: json: cannot unmarshal number into Go struct field Envelope.type of type string"
        );
        assert_eq!(
            err(r#"{"weight_dimensions":[1]}"#),
            "graders: envelope: json: cannot unmarshal array into Go struct field Envelope.weight_dimensions of type map[string]float64"
        );
        // The first saved error wins, in document order.
        assert_eq!(
            err(r#"{"zzz":1,"version":1.5}"#),
            "graders: envelope: json: unknown field \"zzz\""
        );
        assert_eq!(
            err("\"x\""),
            "graders: envelope: json: cannot unmarshal string into Go value of type graders.Envelope"
        );
        assert_eq!(err(""), "graders: envelope: EOF");
        assert_eq!(err("null"), "graders: envelope: missing type");
    }

    #[test]
    fn duplicate_fields_follow_go() {
        let envelope = parse_envelope(
            r#"{"type":"rule_scan","TYPE":"key_facts","version":1,"weight_dimensions":{"a":1},"Weight_dimensions":{"b":2},"spec":{}}"#,
        )
        .unwrap();
        assert_eq!(envelope.kind, GraderKind::KeyFacts);
        assert_eq!(
            envelope.weight_dimensions,
            BTreeMap::from([("a".to_owned(), 1.0), ("b".to_owned(), 2.0)])
        );
        // U+017F (long s) folds to "s".
        assert!(
            parse_envelope(
                r#"{"type":"rule_scan","version":1,"weight_dimensions":{"a":1},"ſpec":{}}"#
            )
            .is_ok()
        );
    }

    #[test]
    fn clamp01_follows_go_min_max() {
        assert!(clamp01(f64::NAN).is_nan());
        assert_eq!(clamp01(-0.0).to_bits(), 0.0f64.to_bits());
        assert_eq!(clamp01(-2.0), 0.0);
        assert_eq!(clamp01(0.25), 0.25);
        assert_eq!(clamp01(1.0), 1.0);
        assert_eq!(clamp01(1.5), 1.0);
        assert_eq!(clamp01(f64::INFINITY), 1.0);
    }

    #[test]
    fn raw_type_reads_the_last_string_type() {
        assert_eq!(
            raw_type(r#"{"type":"quantum_score","version":1}"#),
            "quantum_score"
        );
        assert_eq!(raw_type(r#"{"Type":"a","type":5}"#), "a");
        assert_eq!(raw_type(r#"{"version":1}"#), "");
        assert_eq!(raw_type("[1]"), "");
    }

    #[test]
    fn kinds_round_trip() {
        for kind in GraderKind::ALL {
            assert_eq!(GraderKind::from_type(kind.as_str()), Some(kind));
            assert!(is_known_type(kind.as_str()));
        }
        assert!(is_known_type("test_suite"));
        assert!(!is_known_type("quantum_score"));
        assert_eq!(GraderKind::from_type("test_suite"), None);
        assert_eq!(
            serde_json::to_string(&GraderKind::GoldenAnswer).unwrap(),
            "\"golden_answer\""
        );
    }
}
