//! The `json_schema` grader (spec §5.3, reference Part 2 §8).
//!
//! Extracts the first JSON value from the output (a fenced block, else the first bare
//! object or array), validates it against the task's embedded schema (draft 2020-12
//! unless the schema names another draft; format and content assertions off; no remote
//! references), and runs optional per-field checks. `schema_validity` is 1 when a value
//! was extracted and is valid; `field_accuracy` is the share of checks that pass.
//!
//! Schema-failure text comes from the `jsonschema` crate, not Go's library, so the
//! `schema_errors` detail and embedded-schema compile errors are library wording
//! (D24, QUIRK Q-JS2).

use std::collections::{BTreeMap, BTreeSet};

use regex::Regex;

use super::envelope::GraderKind;
use super::goregex;
use super::registry::{BoxError, BoxFuture, GradeInput, GradeResult, Grader};
use super::spec::{SpecValidator, check_schema};
use crate::gofmt;
use crate::gojson::{self, JsonValue};
use crate::gostrconv;

/// The emitted dimensions.
pub const SCHEMA_VALIDITY: &str = "schema_validity";
pub const FIELD_ACCURACY: &str = "field_accuracy";

/// The json_schema grader.
#[derive(Debug, Clone, Copy, Default)]
pub struct JsonSchema;

/// A decoded spec.
struct Spec {
    schema: Option<JsonValue>,
    checks: Vec<CheckSpec>,
}

struct CheckSpec {
    path: String,
    kind: CheckKind,
}

enum CheckKind {
    Equals(JsonValue),
    Regex(String),
    Numeric { target: f64, tolerance: f64 },
}

impl CheckKind {
    fn name(&self) -> &'static str {
        match self {
            CheckKind::Equals(_) => "equals",
            CheckKind::Regex(_) => "regex",
            CheckKind::Numeric { .. } => "numeric",
        }
    }
}

/// The last member named `key`, as Go's decoder keeps the last duplicate.
fn member<'a>(members: &'a [(String, JsonValue)], key: &str) -> Option<&'a JsonValue> {
    members.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v)
}

fn number(value: &JsonValue, field: &str) -> Result<f64, String> {
    match value {
        JsonValue::Number(literal) => gostrconv::parse_float(literal).map_err(|_| {
            format!("json: cannot unmarshal number {literal} into Go struct field .{field} of type float64")
        }),
        other => Err(format!(
            "json: cannot unmarshal {} into Go struct field .{field} of type float64",
            other.kind_name()
        )),
    }
}

fn string(value: &JsonValue, field: &str) -> Result<String, String> {
    match value {
        JsonValue::String(s) => Ok(s.clone()),
        other => Err(format!(
            "json: cannot unmarshal {} into Go struct field .{field} of type string",
            other.kind_name()
        )),
    }
}

/// Decodes the spec like Go's struct decode: `schema` kept raw, each check's kind by
/// presence in the order equals, regex, numeric (`"equals": null` is an equals check).
fn decode(spec: &JsonValue) -> Result<Spec, String> {
    let JsonValue::Object(members) = spec else {
        return Err(format!(
            "json: cannot unmarshal {} into Go value of type jsonschema.spec",
            spec.kind_name()
        ));
    };
    let schema = member(members, "schema").cloned();
    let checks = match member(members, "checks") {
        None | Some(JsonValue::Null) => Vec::new(),
        Some(JsonValue::Array(items)) => items
            .iter()
            .enumerate()
            .map(|(i, item)| decode_check(item).map_err(|e| format!("checks[{i}]: {e}")))
            .collect::<Result<_, _>>()?,
        Some(other) => {
            return Err(format!(
                "json: cannot unmarshal {} into Go struct field spec.checks of type []jsonschema.check",
                other.kind_name()
            ));
        }
    };
    Ok(Spec { schema, checks })
}

fn decode_check(item: &JsonValue) -> Result<CheckSpec, String> {
    let JsonValue::Object(members) = item else {
        return Err(format!(
            "json: cannot unmarshal {} into Go value of type jsonschema.check",
            item.kind_name()
        ));
    };
    let path = member(members, "path")
        .map(|v| string(v, "path"))
        .transpose()?
        .unwrap_or_default();
    let kind = if let Some(expected) = member(members, "equals") {
        CheckKind::Equals(expected.clone())
    } else if let Some(expr) = member(members, "regex") {
        CheckKind::Regex(string(expr, "regex")?)
    } else if let Some(target) = member(members, "numeric") {
        CheckKind::Numeric {
            target: number(target, "numeric")?,
            tolerance: member(members, "tolerance")
                .map(|t| number(t, "tolerance"))
                .transpose()?
                .unwrap_or(0.0),
        }
    } else {
        return Err("check has no kind".to_owned());
    };
    Ok(CheckSpec { path, kind })
}

/// Compiles the embedded schema: draft 2020-12 by default, formats off, and every
/// external reference refused (Go loads none).
fn compile_schema(schema: Option<&JsonValue>) -> Result<jsonschema::Validator, String> {
    let Some(schema) = schema else {
        return Err("missing schema".to_owned());
    };
    let schema = to_serde_lossy(schema);
    jsonschema::options()
        .offline()
        .should_validate_formats(false)
        .build(&schema)
        .map_err(|e| e.to_string())
}

/// A path segment: an object key or an array index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Key(String),
    Index(usize),
}

/// `parsePath(p)`: `$` then any of `.name` and `[n]`. Errors are Go's exact texts.
pub fn parse_path(path: &str) -> Result<Vec<Segment>, String> {
    let quoted = gofmt::quote(path);
    let Some(rest) = path.strip_prefix('$') else {
        return Err(format!("path {quoted} must start with $"));
    };
    let bytes = rest.as_bytes();
    let mut segments = Vec::new();
    let mut i = 0;
    // Each pass consumes at least one byte, so the path length bounds the loop. A
    // `while i < len` loop would re-parse forever under a mutated index, and a
    // cargo-mutants timeout fails CI.
    for _ in 0..bytes.len() {
        let Some(&b) = bytes.get(i) else {
            break;
        };
        match b {
            b'.' => {
                let start = i + 1;
                let end = bytes[start..]
                    .iter()
                    .position(|&b| b == b'.' || b == b'[')
                    .map_or(bytes.len(), |n| start + n);
                if end == start {
                    return Err(format!("path {quoted} has an empty segment"));
                }
                segments.push(Segment::Key(rest[start..end].to_owned()));
                i = end;
            }
            b'[' => {
                let start = i + 1;
                let Some(close) = bytes[start..].iter().position(|&b| b == b']') else {
                    return Err(format!("path {quoted} has an unterminated index"));
                };
                let text = &rest[start..start + close];
                let index = go_atoi(text)
                    .and_then(|n| usize::try_from(n).ok())
                    .ok_or_else(|| {
                        format!("path {quoted} has a bad array index {}", gofmt::quote(text))
                    })?;
                segments.push(Segment::Index(index));
                i = start + close + 1;
            }
            other => {
                return Err(format!(
                    "path {quoted}: unexpected {}",
                    gojson::quote_char(char::from(other))
                ));
            }
        }
    }
    Ok(segments)
}

/// Go `strconv.Atoi`: an optional sign and ASCII digits (leading zeros allowed). Rust's
/// `i64` parse accepts exactly that.
fn go_atoi(text: &str) -> Option<i64> {
    text.parse::<i64>().ok()
}

/// Follows `segments` into `doc`. An object key takes the last duplicate.
fn resolve<'d>(doc: &'d JsonValue, segments: &[Segment]) -> Option<&'d JsonValue> {
    segments
        .iter()
        .try_fold(doc, |value, segment| match (segment, value) {
            (Segment::Key(key), JsonValue::Object(members)) => member(members, key),
            (Segment::Index(i), JsonValue::Array(items)) => items.get(*i),
            _ => None,
        })
}

impl SpecValidator for JsonSchema {
    fn kind(&self) -> &str {
        GraderKind::JsonSchema.as_str()
    }

    /// Schema, decode, compile the embedded schema, then per check the path and an
    /// anchored regex.
    fn validate_spec(&self, spec: &JsonValue) -> Result<(), String> {
        check_schema(GraderKind::JsonSchema, spec)?;
        let spec = decode(spec).map_err(|e| format!("json_schema spec: {e}"))?;
        compile_schema(spec.schema.as_ref())
            .map_err(|e| format!("json_schema spec: embedded schema: {e}"))?;
        for (i, check) in spec.checks.iter().enumerate() {
            let at = |e: String| format!("json_schema spec: checks[{i}]: {e}");
            parse_path(&check.path).map_err(at)?;
            if let CheckKind::Regex(expr) = &check.kind {
                goregex::compile_anchored(expr).map_err(at)?;
            }
        }
        Ok(())
    }
}

impl Grader for JsonSchema {
    fn grade<'a>(
        &'a self,
        spec: &'a JsonValue,
        input: GradeInput<'a>,
    ) -> BoxFuture<'a, Result<GradeResult, BoxError>> {
        Box::pin(async move { grade(spec, input.output).map_err(BoxError::from) })
    }
}

/// How the value was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Extraction {
    Fenced,
    Bare,
}

/// `extractJSON(output)`: the first fenced block that is exactly one JSON value (any
/// type), else the first `{` or `[` offset where one JSON value decodes (trailing text
/// allowed). Bare scalars are never extracted.
pub fn extract_json(output: &str) -> Option<(JsonValue, Extraction)> {
    for block in fenced_blocks(output) {
        if let Ok((value, rest)) = gojson::decode_one(&block) {
            if gojson::is_blank(rest) {
                return Some((value, Extraction::Fenced));
            }
        }
    }
    output
        .bytes()
        .enumerate()
        .filter(|&(_, b)| b == b'{' || b == b'[')
        .find_map(|(i, _)| gojson::decode_one(&output[i..]).ok())
        .map(|(value, _)| (value, Extraction::Bare))
}

/// The contents of ``` fenced blocks: a line whose trimmed form starts with three
/// backticks toggles in and out (the info string is ignored); an unterminated block is
/// dropped.
fn fenced_blocks(output: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current: Option<Vec<&str>> = None;
    for line in output.split('\n') {
        if line.trim().starts_with("```") {
            match current.take() {
                Some(lines) => blocks.push(lines.join("\n")),
                None => current = Some(Vec::new()),
            }
        } else if let Some(lines) = current.as_mut() {
            lines.push(line);
        }
    }
    blocks
}

/// Go's view of a decoded value: object keys deduplicated (last wins) and sorted, as a
/// `map[string]any` has them.
fn canonical(value: &JsonValue) -> JsonValue {
    match value {
        JsonValue::Array(items) => JsonValue::Array(items.iter().map(canonical).collect()),
        JsonValue::Object(members) => {
            let map: BTreeMap<&str, &JsonValue> =
                members.iter().map(|(k, v)| (k.as_str(), v)).collect();
            JsonValue::Object(
                map.into_iter()
                    .map(|(k, v)| (k.to_owned(), canonical(v)))
                    .collect(),
            )
        }
        other => other.clone(),
    }
}

/// A `serde_json::Value` for schema validation. A number literal too large for `f64`
/// becomes ±`f64::MAX` (Go validates it as a huge exact number).
fn to_serde_lossy(value: &JsonValue) -> serde_json::Value {
    match value {
        JsonValue::Null => serde_json::Value::Null,
        JsonValue::Bool(b) => serde_json::Value::Bool(*b),
        JsonValue::Number(literal) => serde_json::from_str(literal).unwrap_or_else(|_| {
            let big = if literal.starts_with('-') {
                -f64::MAX
            } else {
                f64::MAX
            };
            serde_json::Value::from(big)
        }),
        JsonValue::String(s) => serde_json::Value::String(s.clone()),
        JsonValue::Array(items) => {
            serde_json::Value::Array(items.iter().map(to_serde_lossy).collect())
        }
        JsonValue::Object(members) => serde_json::Value::Object(
            members
                .iter()
                .map(|(k, v)| (k.clone(), to_serde_lossy(v)))
                .collect(),
        ),
    }
}

/// The schema errors as `/<instance path>: <keyword>`, sorted and deduplicated. A
/// `false` schema has an empty keyword.
fn schema_errors(validator: &jsonschema::Validator, doc: &serde_json::Value) -> Vec<String> {
    let errors: BTreeSet<String> = validator
        .iter_errors(doc)
        .map(|error| {
            let instance = error.instance_path().to_string();
            let keyword = match error.kind().keyword() {
                "falseSchema" => "",
                keyword => keyword,
            };
            let instance = if instance.is_empty() {
                "/"
            } else {
                instance.as_str()
            };
            format!("{instance}: {keyword}")
        })
        .collect();
    errors.into_iter().collect()
}

/// `valueEqual(a, b)`: numbers compare as `float64` (`5` equals `5.0`; a literal that
/// does not fit never equals); strings, bools, and null by value; arrays element-wise;
/// objects key-wise. Different types are never equal.
pub fn value_equal(a: &JsonValue, b: &JsonValue) -> bool {
    match (canonical(a), canonical(b)) {
        (JsonValue::Number(x), JsonValue::Number(y)) => {
            match (gostrconv::parse_float(&x), gostrconv::parse_float(&y)) {
                (Ok(x), Ok(y)) => x == y,
                _ => false,
            }
        }
        (JsonValue::Array(x), JsonValue::Array(y)) => {
            x.len() == y.len() && x.iter().zip(&y).all(|(x, y)| value_equal(x, y))
        }
        (JsonValue::Object(x), JsonValue::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| member(&y, k).is_some_and(|w| value_equal(v, w)))
        }
        (x, y) => !matches!(x, JsonValue::Number(_)) && x == y,
    }
}

/// A numeric check's value: a number, or a string that parses after trimming.
fn numeric_value(value: &JsonValue) -> Option<f64> {
    match value {
        JsonValue::Number(literal) => gostrconv::parse_float(literal).ok(),
        JsonValue::String(s) => gostrconv::parse_float(s.trim()).ok(),
        _ => None,
    }
}

fn check_passes(kind: &CheckKind, compiled: Option<&Regex>, value: &JsonValue) -> bool {
    match kind {
        CheckKind::Equals(expected) => value_equal(value, expected),
        CheckKind::Regex(_) => match (compiled, value) {
            (Some(re), JsonValue::String(s)) => re.is_match(s),
            _ => false,
        },
        CheckKind::Numeric { target, tolerance } => {
            numeric_value(value).is_some_and(|v| (v - target).abs() <= *tolerance)
        }
    }
}

fn grade(spec: &JsonValue, output: &str) -> Result<GradeResult, String> {
    let spec = decode(spec).map_err(|e| format!("json_schema: decode spec: {e}"))?;
    let validator = compile_schema(spec.schema.as_ref())
        .map_err(|e| format!("json_schema: compile embedded schema: {e}"))?;
    let extracted = extract_json(output);

    let (valid, errors) = match &extracted {
        Some((doc, _)) => {
            let errors = schema_errors(&validator, &to_serde_lossy(&canonical(doc)));
            (errors.is_empty(), errors)
        }
        None => (false, Vec::new()),
    };
    let mut dimensions =
        BTreeMap::from([(SCHEMA_VALIDITY.to_owned(), if valid { 1.0 } else { 0.0 })]);
    let extraction = match extracted.as_ref().map(|(_, how)| how) {
        Some(Extraction::Fenced) => "fenced",
        Some(Extraction::Bare) => "bare",
        None => "none",
    };
    let mut detail = vec![
        (
            "extraction".to_owned(),
            JsonValue::String(extraction.to_owned()),
        ),
        ("schema_valid".to_owned(), JsonValue::Bool(valid)),
    ];
    if !errors.is_empty() {
        detail.push((
            "schema_errors".to_owned(),
            JsonValue::Array(errors.into_iter().map(JsonValue::String).collect()),
        ));
    }

    if !spec.checks.is_empty() {
        let mut passed = 0_usize;
        let mut records = Vec::with_capacity(spec.checks.len());
        for (i, check) in spec.checks.iter().enumerate() {
            let at = |e: String| format!("json_schema: checks[{i}]: {e}");
            let segments = parse_path(&check.path).map_err(at)?;
            let compiled = match &check.kind {
                CheckKind::Regex(expr) => Some(goregex::compile_anchored(expr).map_err(at)?),
                _ => None,
            };
            let value = extracted
                .as_ref()
                .and_then(|(doc, _)| resolve(doc, &segments));
            let pass = value.is_some_and(|v| check_passes(&check.kind, compiled.as_ref(), v));
            if pass {
                passed += 1;
            }
            let mut record = vec![
                ("path".to_owned(), JsonValue::String(check.path.clone())),
                (
                    "kind".to_owned(),
                    JsonValue::String(check.kind.name().to_owned()),
                ),
                ("pass".to_owned(), JsonValue::Bool(pass)),
            ];
            if let Some(value) = value {
                let got = gojson::marshal(&canonical(value));
                record.push(("got".to_owned(), JsonValue::String(got)));
            }
            records.push(JsonValue::Object(record));
        }
        dimensions.insert(
            FIELD_ACCURACY.to_owned(),
            passed as f64 / spec.checks.len() as f64,
        );
        detail.push(("checks".to_owned(), JsonValue::Array(records)));
    }

    Ok(GradeResult {
        dimensions,
        detail: gojson::marshal(&JsonValue::Object(detail)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(text: &str) -> JsonValue {
        gojson::decode_one(text).expect("json").0
    }

    #[test]
    fn path_errors() {
        assert_eq!(parse_path("$").unwrap(), []);
        assert_eq!(
            parse_path("$.a[+01].b").unwrap(),
            [
                Segment::Key("a".to_owned()),
                Segment::Index(1),
                Segment::Key("b".to_owned())
            ]
        );
        assert_eq!(parse_path("a").unwrap_err(), "path \"a\" must start with $");
        assert_eq!(
            parse_path("$.").unwrap_err(),
            "path \"$.\" has an empty segment"
        );
        assert_eq!(
            parse_path("$.a.[0]").unwrap_err(),
            "path \"$.a.[0]\" has an empty segment"
        );
        assert_eq!(
            parse_path("$[]").unwrap_err(),
            "path \"$[]\" has a bad array index \"\""
        );
        assert_eq!(parse_path("$[-0]").unwrap(), [Segment::Index(0)]);
        assert_eq!(
            parse_path("$[+]").unwrap_err(),
            "path \"$[+]\" has a bad array index \"+\""
        );
        assert_eq!(
            parse_path("$[99999999999999999999]").unwrap_err(),
            "path \"$[99999999999999999999]\" has a bad array index \"99999999999999999999\""
        );
        assert_eq!(parse_path("$.a]").unwrap(), [Segment::Key("a]".to_owned())]);
        assert_eq!(
            parse_path("$'").unwrap_err(),
            "path \"$'\": unexpected '\\''"
        );
    }

    #[test]
    fn resolve_follows_keys_and_indexes() {
        let doc = json(r#"{"a":[{"b":1},{"b":2,"b":3}],"s":"x"}"#);
        let at = |p: &str| resolve(&doc, &parse_path(p).unwrap()).cloned();
        assert_eq!(at("$.a[1].b"), Some(JsonValue::Number("3".to_owned())));
        assert_eq!(at("$.a[2]"), None);
        assert_eq!(at("$.s.x"), None);
        assert_eq!(at("$.s[0]"), None);
        assert_eq!(at("$.a.b"), None);
        assert_eq!(at("$"), Some(doc.clone()));
    }

    #[test]
    fn equality_edges() {
        assert!(value_equal(&json("5"), &json("5.0")));
        assert!(!value_equal(&json("5"), &json("\"5\"")));
        assert!(!value_equal(&json("\"5\""), &json("5")));
        assert!(!value_equal(&json("1e400"), &json("1e400")));
        assert!(value_equal(&json("null"), &json("null")));
        assert!(!value_equal(&json("null"), &json("false")));
        assert!(value_equal(&json("[1,[2]]"), &json("[1.0,[2e0]]")));
        assert!(!value_equal(&json("[1]"), &json("[1,2]")));
        assert!(!value_equal(&json("[1,2]"), &json("[1,3]")));
        assert!(value_equal(
            &json(r#"{"a":1,"b":2}"#),
            &json(r#"{"b":2,"a":1.0}"#)
        ));
        assert!(!value_equal(&json(r#"{"a":1}"#), &json(r#"{"a":1,"b":2}"#)));
        assert!(!value_equal(&json(r#"{"a":1}"#), &json(r#"{"b":1}"#)));
        assert!(value_equal(&json(r#"{"a":1,"a":2}"#), &json(r#"{"a":2}"#)));
        assert!(!value_equal(&json("true"), &json("\"true\"")));
    }

    #[test]
    fn oversized_literals_validate_as_huge_numbers() {
        assert_eq!(
            to_serde_lossy(&json("1e400")),
            serde_json::Value::from(f64::MAX)
        );
        assert_eq!(
            to_serde_lossy(&json("-1e400")),
            serde_json::Value::from(-f64::MAX)
        );
        assert_eq!(to_serde_lossy(&json("12")), serde_json::json!(12));
    }

    #[test]
    fn go_atoi_accepts_sign_and_zeros() {
        assert_eq!(go_atoi("+05"), Some(5));
        assert_eq!(go_atoi("-3"), Some(-3));
        for bad in ["", "+", "x", "1_0", "0x1", " 1", "１"] {
            assert_eq!(go_atoi(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn numeric_values() {
        assert_eq!(numeric_value(&json("\" 12.5 \"")), Some(12.5));
        assert_eq!(numeric_value(&json("\"x\"")), None);
        assert_eq!(numeric_value(&json("true")), None);
        assert_eq!(numeric_value(&json("1e400")), None);
    }

    #[test]
    fn compile_refuses_remote_refs_and_ignores_formats() {
        let remote = json(r#"{"$ref":"https://example.com/x.json"}"#);
        assert!(compile_schema(Some(&remote)).is_err());
        let file = json(r#"{"$ref":"file:///etc/passwd"}"#);
        assert!(compile_schema(Some(&file)).is_err());
        assert_eq!(compile_schema(None).unwrap_err(), "missing schema");
        let formats = compile_schema(Some(&json(
            r#"{"properties":{"d":{"format":"date"},"b":{"contentEncoding":"base64"}}}"#,
        )))
        .unwrap();
        assert!(schema_errors(&formats, &serde_json::json!({"d": "nope", "b": "!!!"})).is_empty());
        let draft7 = compile_schema(Some(&json(
            r#"{"$schema":"http://json-schema.org/draft-07/schema#","items":[{"type":"string"}]}"#,
        )))
        .unwrap();
        assert_eq!(
            schema_errors(&draft7, &serde_json::json!([1])),
            ["/0: type"]
        );
    }

    #[test]
    fn decode_edges() {
        assert!(decode(&json("[]")).is_err());
        assert!(decode(&json(r#"{"schema":{},"checks":{}}"#)).is_err());
        assert!(decode(&json(r#"{"schema":{},"checks":[1]}"#)).is_err());
        assert_eq!(
            decode(&json(r#"{"schema":{},"checks":[{"path":"$"}]}"#)).err(),
            Some("checks[0]: check has no kind".to_owned())
        );
        assert!(decode(&json(r#"{"schema":{},"checks":[{"path":1,"equals":1}]}"#)).is_err());
        assert!(decode(&json(r#"{"schema":{},"checks":[{"path":"$","regex":1}]}"#)).is_err());
        assert!(
            decode(&json(
                r#"{"schema":{},"checks":[{"path":"$","numeric":"1"}]}"#
            ))
            .is_err()
        );
        assert!(
            decode(&json(
                r#"{"schema":{},"checks":[{"path":"$","numeric":1,"tolerance":"x"}]}"#
            ))
            .is_err()
        );
        let spec = decode(&json(r#"{"schema":{},"checks":null}"#)).unwrap();
        assert!(spec.checks.is_empty());
        // Presence decides the kind, in the order equals, regex, numeric.
        let spec = decode(&json(
            r#"{"schema":{},"checks":[{"path":"$","numeric":1,"regex":"x","equals":null},{"path":"$","numeric":1,"regex":"x"}]}"#,
        ))
        .unwrap();
        assert_eq!(spec.checks[0].kind.name(), "equals");
        assert_eq!(spec.checks[1].kind.name(), "regex");
    }

    #[test]
    fn grade_errors() {
        assert!(
            grade(&json("1"), "x")
                .unwrap_err()
                .starts_with("json_schema: decode spec: ")
        );
        assert!(
            grade(&json(r#"{"schema":{"type":"nope"}}"#), "x")
                .unwrap_err()
                .starts_with("json_schema: compile embedded schema: ")
        );
        assert_eq!(
            grade(
                &json(r#"{"schema":{},"checks":[{"path":"x","equals":1}]}"#),
                "{}"
            )
            .unwrap_err(),
            "json_schema: checks[0]: path \"x\" must start with $"
        );
        assert_eq!(
            grade(
                &json(r#"{"schema":{},"checks":[{"path":"$","regex":"("}]}"#),
                "{}"
            )
            .unwrap_err(),
            "json_schema: checks[0]: error parsing regexp: unclosed group: `\\A(?:()\\z`"
        );
    }
}
