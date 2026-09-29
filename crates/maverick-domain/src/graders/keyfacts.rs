//! The `key_facts` grader (spec §5.3, reference Part 2 §9).
//!
//! Each fact lists accepted surface forms. A literal form is found when the output,
//! whitespace-collapsed and lowercased, contains the form treated the same way (so
//! `Acme` matches `Acmeville`). A regex form is an unanchored Go RE2 match on the raw
//! output. `fact_coverage` is the weighted share of required facts found;
//! `hallucination_free` is `1 - sum(weight of forbidden facts present)`, clamped to
//! `[0, 1]`. A forbidden fact may carry `autofail`, which the high-stakes overlay reads
//! from the detail.

use std::collections::{BTreeMap, HashSet};

use serde::Deserialize;

use super::envelope::GraderKind;
use super::goregex;
use super::registry::{BoxError, BoxFuture, GradeInput, GradeResult, Grader};
use super::spec::{SpecValidator, check_schema};
use crate::gofmt;
use crate::gojson::{self, JsonValue};
use crate::gostrings;

/// The emitted dimensions.
pub const FACT_COVERAGE: &str = "fact_coverage";
pub const HALLUCINATION_FREE: &str = "hallucination_free";

/// The key_facts grader.
#[derive(Debug, Clone, Copy, Default)]
pub struct KeyFacts;

#[derive(Debug, Deserialize)]
struct Spec {
    #[serde(default)]
    facts: Vec<Fact>,
    #[serde(default)]
    forbidden_facts: Vec<Fact>,
}

#[derive(Debug, Deserialize)]
struct Fact {
    #[serde(default)]
    id: String,
    #[serde(default)]
    any_of: Vec<String>,
    #[serde(default)]
    regex: bool,
    weight: Option<f64>,
    #[serde(default)]
    autofail: bool,
}

impl Fact {
    /// The weight, 1 when unset.
    fn weight(&self) -> f64 {
        self.weight.unwrap_or(1.0)
    }
}

/// `normalizeSearch(s)`: whitespace collapsed to single spaces, lowercased per rune. No
/// punctuation handling.
pub fn normalize_search(s: &str) -> String {
    gostrings::to_lower(&gostrings::collapse_fields(s))
}

impl SpecValidator for KeyFacts {
    fn kind(&self) -> &str {
        GraderKind::KeyFacts.as_str()
    }

    /// Schema, then facts and forbidden facts in order with one shared id set: a
    /// duplicate id, `autofail` on a required fact, a regex form that does not compile,
    /// or a literal form that normalizes to empty.
    fn validate_spec(&self, spec: &JsonValue) -> Result<(), String> {
        let value = check_schema(GraderKind::KeyFacts, spec)?;
        let spec: Spec =
            serde_json::from_value(value).map_err(|e| format!("key_facts spec: {e}"))?;
        let mut ids = HashSet::new();
        for (group, facts) in [
            ("facts", &spec.facts),
            ("forbidden_facts", &spec.forbidden_facts),
        ] {
            for (i, fact) in facts.iter().enumerate() {
                if !ids.insert(fact.id.as_str()) {
                    return Err(format!(
                        "key_facts spec: duplicate fact id {}",
                        gofmt::quote(&fact.id)
                    ));
                }
                if group == "facts" && fact.autofail {
                    return Err(format!(
                        "key_facts spec: facts[{i}]: autofail applies only to forbidden_facts (cmd §4)"
                    ));
                }
                for (j, form) in fact.any_of.iter().enumerate() {
                    let at = format!("key_facts spec: {group}[{i}].any_of[{j}]");
                    if fact.regex {
                        goregex::compile(form).map_err(|e| format!("{at}: {e}"))?;
                    } else if normalize_search(form).is_empty() {
                        return Err(format!("{at}: normalizes to empty"));
                    }
                }
            }
        }
        Ok(())
    }
}

impl Grader for KeyFacts {
    fn grade<'a>(
        &'a self,
        spec: &'a JsonValue,
        input: GradeInput<'a>,
    ) -> BoxFuture<'a, Result<GradeResult, BoxError>> {
        Box::pin(async move { grade(spec, input.output).map_err(BoxError::from) })
    }
}

fn grade(spec: &JsonValue, output: &str) -> Result<GradeResult, String> {
    let spec: Spec = serde_json::from_value(spec.to_serde()?)
        .map_err(|e| format!("key_facts: decode spec: {e}"))?;
    if spec.facts.is_empty() && spec.forbidden_facts.is_empty() {
        return Err("key_facts: spec has neither facts nor forbidden_facts".to_owned());
    }
    let normalized_output = normalize_search(output);
    let mut dimensions = BTreeMap::new();
    let mut detail = Vec::new();

    if !spec.facts.is_empty() {
        let (records, found_weight, total_weight) =
            check_group("facts", &spec.facts, output, &normalized_output)?;
        dimensions.insert(FACT_COVERAGE.to_owned(), found_weight / total_weight);
        detail.push(("facts".to_owned(), JsonValue::Array(records)));
    }
    if !spec.forbidden_facts.is_empty() {
        let (records, present_weight, _) = check_group(
            "forbidden_facts",
            &spec.forbidden_facts,
            output,
            &normalized_output,
        )?;
        dimensions.insert(HALLUCINATION_FREE.to_owned(), clamp01(1.0 - present_weight));
        detail.push(("forbidden_facts".to_owned(), JsonValue::Array(records)));
    }

    Ok(GradeResult {
        dimensions,
        detail: gojson::marshal(&JsonValue::Object(detail)),
    })
}

/// Checks each fact of a group. Returns the detail records, the weight of the facts
/// found, and the total weight.
fn check_group(
    group: &str,
    facts: &[Fact],
    output: &str,
    normalized_output: &str,
) -> Result<(Vec<JsonValue>, f64, f64), String> {
    let mut records = Vec::with_capacity(facts.len());
    let mut found_weight = 0.0;
    let mut total_weight = 0.0;
    for (i, fact) in facts.iter().enumerate() {
        let matched = find_fact(fact, output, normalized_output)
            .map_err(|(j, e)| format!("key_facts: {group}[{i}].any_of[{j}]: {e}"))?;
        let weight = fact.weight();
        total_weight += weight;
        if matched.is_some() {
            found_weight += weight;
        }
        // Go's field order; matched_form only when found, autofail only when true.
        let mut record = vec![
            ("id".to_owned(), JsonValue::String(fact.id.clone())),
            ("found".to_owned(), JsonValue::Bool(matched.is_some())),
            (
                "weight".to_owned(),
                JsonValue::Number(gojson::encode_f64(weight)?),
            ),
        ];
        if let Some(form) = matched {
            record.push((
                "matched_form".to_owned(),
                JsonValue::String(form.to_owned()),
            ));
        }
        if fact.autofail {
            record.push(("autofail".to_owned(), JsonValue::Bool(true)));
        }
        records.push(JsonValue::Object(record));
    }
    Ok((records, found_weight, total_weight))
}

/// The first form of `fact` found in the output, as authored. A regex that does not
/// compile is an error naming the form's index.
fn find_fact<'f>(
    fact: &'f Fact,
    output: &str,
    normalized_output: &str,
) -> Result<Option<&'f str>, (usize, String)> {
    for (j, form) in fact.any_of.iter().enumerate() {
        let found = if fact.regex {
            goregex::compile(form).map_err(|e| (j, e))?.is_match(output)
        } else {
            normalized_output.contains(&normalize_search(form))
        };
        if found {
            return Ok(Some(form));
        }
    }
    Ok(None)
}

/// Go `math.Min(1, math.Max(0, x))`.
fn clamp01(x: f64) -> f64 {
    if x.is_nan() {
        x
    } else if x > 0.0 {
        x.min(1.0)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(json: &str) -> JsonValue {
        gojson::decode_one(json).expect("json").0
    }

    #[test]
    fn grade_errors() {
        assert_eq!(
            grade(&spec("{}"), "x").unwrap_err(),
            "key_facts: spec has neither facts nor forbidden_facts"
        );
        assert!(
            grade(&spec(r#"{"facts":"x"}"#), "x")
                .unwrap_err()
                .starts_with("key_facts: decode spec: ")
        );
        assert_eq!(
            grade(
                &spec(r#"{"facts":[{"id":"a","any_of":["zzz"]}],"forbidden_facts":[{"id":"b","any_of":["x","("],"regex":true}]}"#),
                "y"
            )
            .unwrap_err(),
            "key_facts: forbidden_facts[0].any_of[1]: error parsing regexp: unclosed group: `(`"
        );
    }

    #[test]
    fn first_form_wins_and_regex_sees_raw_output() {
        let result = grade(
            &spec(r#"{"facts":[{"id":"a","any_of":["\\bNet  30\\b","net 30"],"regex":true}]}"#),
            "pay Net  30",
        )
        .unwrap();
        assert_eq!(
            result.detail,
            r#"{"facts":[{"id":"a","found":true,"weight":1,"matched_form":"\\bNet  30\\b"}]}"#
        );
        // Literal forms compare normalized text.
        let result = grade(
            &spec(r#"{"facts":[{"id":"a","any_of":["NET   30"]}]}"#),
            "pay net 30",
        )
        .unwrap();
        assert_eq!(result.dimensions[FACT_COVERAGE], 1.0);
    }

    #[test]
    fn autofail_is_recorded_whether_found_or_not() {
        let result = grade(
            &spec(r#"{"forbidden_facts":[{"id":"t","any_of":["x"],"autofail":true}]}"#),
            "nothing",
        )
        .unwrap();
        assert_eq!(
            result.detail,
            r#"{"forbidden_facts":[{"id":"t","found":false,"weight":1,"autofail":true}]}"#
        );
        assert_eq!(result.dimensions[HALLUCINATION_FREE], 1.0);
    }

    #[test]
    fn clamp01_follows_go() {
        assert!(clamp01(f64::NAN).is_nan());
        assert_eq!(clamp01(-0.0).to_bits(), 0.0f64.to_bits());
        assert_eq!(clamp01(-1.0), 0.0);
        assert_eq!(clamp01(0.5), 0.5);
        assert_eq!(clamp01(2.0), 1.0);
    }
}
