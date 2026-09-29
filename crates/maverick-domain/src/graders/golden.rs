//! The `golden_answer` grader (spec §5.3, reference Part 2 §7).
//!
//! Compares the model output with one or more accepted answers. Each answer has one
//! mode: `value` (normalized equality), `numeric` (within `tolerance`), `set`
//! (normalized set equality over a list in the output), or `regex` (anchored Go RE2
//! match). Emits `answer_correctness`: 1 if any answer matches, else 0.

use std::collections::{BTreeMap, HashSet};

use serde::Deserialize;

use super::envelope::GraderKind;
use super::goregex;
use super::registry::{BoxError, BoxFuture, GradeInput, GradeResult, Grader};
use super::spec::{SpecValidator, check_schema};
use crate::gojson::{self, JsonValue};
use crate::{gostrconv, gostrings};

/// The emitted dimension.
pub const DIMENSION: &str = "answer_correctness";

/// The golden_answer grader.
#[derive(Debug, Clone, Copy, Default)]
pub struct GoldenAnswer;

#[derive(Debug, Deserialize)]
struct Spec {
    #[serde(default)]
    answers: Vec<Answer>,
}

#[derive(Debug, Deserialize)]
struct Answer {
    value: Option<String>,
    numeric: Option<f64>,
    tolerance: Option<f64>,
    set: Option<Vec<String>>,
    regex: Option<String>,
}

/// An answer's mode, by Go's decoding precedence: value, numeric, set, else regex.
enum Mode<'a> {
    Value(&'a str),
    Numeric { target: f64, tolerance: f64 },
    Set(&'a [String]),
    Regex(&'a str),
}

impl Answer {
    fn mode(&self) -> Mode<'_> {
        if let Some(value) = &self.value {
            Mode::Value(value)
        } else if let Some(target) = self.numeric {
            Mode::Numeric {
                target,
                tolerance: self.tolerance.unwrap_or(0.0),
            }
        } else if let Some(set) = &self.set {
            Mode::Set(set)
        } else {
            Mode::Regex(self.regex.as_deref().unwrap_or(""))
        }
    }
}

impl Mode<'_> {
    fn name(&self) -> &'static str {
        match self {
            Mode::Value(_) => "value",
            Mode::Numeric { .. } => "numeric",
            Mode::Set(_) => "set",
            Mode::Regex(_) => "regex",
        }
    }
}

fn decode(spec: &JsonValue) -> Result<Spec, String> {
    serde_json::from_value(spec.to_serde()?).map_err(|e| e.to_string())
}

impl SpecValidator for GoldenAnswer {
    fn kind(&self) -> &str {
        GraderKind::GoldenAnswer.as_str()
    }

    /// Schema, then per answer: a value or set item that normalizes to empty, or a
    /// regex that does not compile when anchored.
    fn validate_spec(&self, spec: &JsonValue) -> Result<(), String> {
        let value = check_schema(GraderKind::GoldenAnswer, spec)?;
        let spec: Spec =
            serde_json::from_value(value).map_err(|e| format!("golden_answer spec: {e}"))?;
        for (i, answer) in spec.answers.iter().enumerate() {
            match answer.mode() {
                Mode::Value(value) if normalize(value).is_empty() => {
                    return Err(format!(
                        "golden_answer spec: answers[{i}]: value normalizes to empty"
                    ));
                }
                Mode::Set(items) => {
                    if let Some(j) = items.iter().position(|item| normalize(item).is_empty()) {
                        return Err(format!(
                            "golden_answer spec: answers[{i}]: set[{j}] normalizes to empty"
                        ));
                    }
                }
                Mode::Regex(expr) => {
                    goregex::compile_anchored(expr)
                        .map_err(|e| format!("golden_answer spec: answers[{i}]: {e}"))?;
                }
                _ => {}
            }
        }
        Ok(())
    }
}

impl Grader for GoldenAnswer {
    fn grade<'a>(
        &'a self,
        spec: &'a JsonValue,
        input: GradeInput<'a>,
    ) -> BoxFuture<'a, Result<GradeResult, BoxError>> {
        Box::pin(async move { grade(spec, input.output).map_err(BoxError::from) })
    }
}

fn grade(spec: &JsonValue, output: &str) -> Result<GradeResult, String> {
    let spec = decode(spec).map_err(|e| format!("golden_answer: decode spec: {e}"))?;
    if spec.answers.is_empty() {
        return Err("golden_answer: spec has no answers".to_owned());
    }
    let mut first_match: Option<(usize, &'static str)> = None;
    let mut records = Vec::with_capacity(spec.answers.len());
    for (i, answer) in spec.answers.iter().enumerate() {
        let mode = answer.mode();
        let matched =
            matches(&mode, output).map_err(|e| format!("golden_answer: answers[{i}]: {e}"))?;
        if matched && first_match.is_none() {
            first_match = Some((i, mode.name()));
        }
        records.push(JsonValue::Object(vec![
            ("index".to_owned(), number(i)),
            ("mode".to_owned(), JsonValue::String(mode.name().to_owned())),
            ("matched".to_owned(), JsonValue::Bool(matched)),
        ]));
    }

    // Detail in Go's field order; `mode` is omitted when nothing matched.
    let mut detail = vec![("matched".to_owned(), JsonValue::Bool(first_match.is_some()))];
    if let Some((_, mode)) = first_match {
        detail.push(("mode".to_owned(), JsonValue::String(mode.to_owned())));
    }
    detail.push((
        "answer_index".to_owned(),
        first_match.map_or(JsonValue::Number("-1".to_owned()), |(i, _)| number(i)),
    ));
    detail.push(("answers".to_owned(), JsonValue::Array(records)));

    let score = if first_match.is_some() { 1.0 } else { 0.0 };
    Ok(GradeResult {
        dimensions: BTreeMap::from([(DIMENSION.to_owned(), score)]),
        detail: gojson::marshal(&JsonValue::Object(detail)),
    })
}

fn number(n: usize) -> JsonValue {
    JsonValue::Number(n.to_string())
}

fn matches(mode: &Mode<'_>, output: &str) -> Result<bool, String> {
    Ok(match mode {
        Mode::Value(value) => normalize(output) == normalize(value),
        Mode::Numeric { target, tolerance } => {
            // An unparseable output is a miss, not an error. NaN never matches.
            parse_number(output).is_some_and(|v| (v - target).abs() <= *tolerance)
        }
        Mode::Set(items) => {
            let expected: HashSet<String> = items.iter().map(|item| normalize(item)).collect();
            let got: HashSet<String> = split_items(output).into_iter().collect();
            expected == got
        }
        Mode::Regex(expr) => goregex::compile_anchored(expr)?.is_match(output.trim()),
    })
}

/// `normalize(s)`: collapse Unicode whitespace to single spaces, strip one trailing run
/// of `.!?`, trim, and lowercase per rune. (Rust's whitespace set equals Go's
/// `unicode.IsSpace`.)
pub fn normalize(s: &str) -> String {
    let collapsed = gostrings::collapse_fields(s);
    let stripped = collapsed.trim_end_matches(['.', '!', '?']);
    gostrings::to_lower(stripped.trim())
}

/// `parseNumber(output)`: trim whitespace, strip trailing `.!?`, trim again, drop each
/// thousands comma (a digit before it and exactly three digits after it, then a
/// non-digit or the end), then Go `strconv.ParseFloat`. `None` when it does not parse.
pub fn parse_number(output: &str) -> Option<f64> {
    let s = output.trim().trim_end_matches(['.', '!', '?']).trim();
    let bytes = s.as_bytes();
    let mut cleaned = String::with_capacity(s.len());
    for (i, c) in s.char_indices() {
        if c == ',' && is_thousands_comma(bytes, i) {
            continue;
        }
        cleaned.push(c);
    }
    gostrconv::parse_float(&cleaned).ok()
}

fn is_thousands_comma(bytes: &[u8], i: usize) -> bool {
    let digit_before = i > 0 && bytes[i - 1].is_ascii_digit();
    let three_after = bytes.len() >= i + 4 && bytes[i + 1..i + 4].iter().all(u8::is_ascii_digit);
    let then_non_digit = bytes.get(i + 4).is_none_or(|b| !b.is_ascii_digit());
    digit_before && three_after && then_non_digit
}

/// `splitItems(s)`: split on `\n`, `,`, `;`; per part trim, drop a leading bullet
/// (`-`, `*`, `•` runs, or `12.` / `12)`) that is followed by ASCII whitespace,
/// normalize, and keep the non-empty ones.
pub fn split_items(s: &str) -> Vec<String> {
    s.split(['\n', ',', ';'])
        .filter(|part| !part.is_empty())
        .map(|part| normalize(strip_bullet(part.trim())))
        .filter(|item| !item.is_empty())
        .collect()
}

/// Go regex `^(?:[-*•]+|\d+[.)])\s+` removed from the start, where `\d` and `\s` are
/// ASCII.
fn strip_bullet(part: &str) -> &str {
    let marker_end = if part.starts_with(['-', '*', '•']) {
        part.find(|c| !matches!(c, '-' | '*' | '•'))
            .unwrap_or(part.len())
    } else {
        let digits = part.bytes().take_while(u8::is_ascii_digit).count();
        match part.as_bytes().get(digits) {
            Some(b'.' | b')') if digits > 0 => digits + 1,
            _ => return part,
        }
    };
    let rest = &part[marker_end..];
    let body = rest.trim_start_matches(['\t', '\n', '\u{c}', '\r', ' ']);
    if body.len() == rest.len() { part } else { body }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands_comma_rule() {
        let check = |s: &str, i: usize| is_thousands_comma(s.as_bytes(), i);
        assert!(check("1,234", 1));
        assert!(check("1,234.5", 1));
        assert!(!check(",123", 0));
        assert!(!check("1,23", 1));
        assert!(!check("1,2345", 1));
        assert!(!check("a,123", 1));
    }

    #[test]
    fn bullets() {
        assert_eq!(strip_bullet("- a"), "a");
        assert_eq!(strip_bullet("-\ta"), "a");
        assert_eq!(strip_bullet("•  c"), "c");
        assert_eq!(strip_bullet("12) x"), "x");
        assert_eq!(strip_bullet("-a"), "-a");
        assert_eq!(strip_bullet("3.z"), "3.z");
        assert_eq!(strip_bullet(". a"), ". a");
        assert_eq!(strip_bullet("-\u{a0}a"), "-\u{a0}a");
        assert_eq!(strip_bullet("-"), "-");
        assert_eq!(strip_bullet("7"), "7");
    }

    #[test]
    fn modes_follow_go_precedence() {
        let answer = Answer {
            value: Some("v".to_owned()),
            numeric: Some(1.0),
            tolerance: None,
            set: Some(vec!["s".to_owned()]),
            regex: Some("r".to_owned()),
        };
        assert_eq!(answer.mode().name(), "value");
        let answer = Answer {
            value: None,
            ..answer
        };
        assert_eq!(answer.mode().name(), "numeric");
        let answer = Answer {
            numeric: None,
            ..answer
        };
        assert_eq!(answer.mode().name(), "set");
        let answer = Answer {
            set: None,
            ..answer
        };
        assert_eq!(answer.mode().name(), "regex");
    }

    #[test]
    fn numeric_edges() {
        let spec = |numeric: &str, tolerance: &str| -> JsonValue {
            gojson::decode_one(&format!(
                r#"{{"answers":[{{"numeric":{numeric},"tolerance":{tolerance}}}]}}"#
            ))
            .unwrap()
            .0
        };
        let score =
            |spec: &JsonValue, output: &str| grade(spec, output).unwrap().dimensions[DIMENSION];
        assert_eq!(score(&spec("100", "0.5"), "99.5"), 1.0);
        assert_eq!(score(&spec("100", "0.5"), "99.49"), 0.0);
        assert_eq!(score(&spec("100", "0"), "NaN"), 0.0);
        assert_eq!(score(&spec("100", "0"), "not a number"), 0.0);
        assert_eq!(score(&spec("-5", "0"), "-5!"), 1.0);
    }

    #[test]
    fn grade_errors() {
        let empty = gojson::decode_one(r#"{"answers":[]}"#).unwrap().0;
        assert_eq!(
            grade(&empty, "x").unwrap_err(),
            "golden_answer: spec has no answers"
        );
        let missing = gojson::decode_one(r#"{}"#).unwrap().0;
        assert_eq!(
            grade(&missing, "x").unwrap_err(),
            "golden_answer: spec has no answers"
        );
        let bad = gojson::decode_one(r#"{"answers":"x"}"#).unwrap().0;
        assert!(
            grade(&bad, "x")
                .unwrap_err()
                .starts_with("golden_answer: decode spec: ")
        );
        let regex = gojson::decode_one(r#"{"answers":[{"value":"no"},{"regex":"("}]}"#)
            .unwrap()
            .0;
        assert_eq!(
            grade(&regex, "x").unwrap_err(),
            "golden_answer: answers[1]: error parsing regexp: unclosed group: `\\A(?:()\\z`"
        );
    }

    #[test]
    fn every_answer_is_recorded_and_the_first_match_wins() {
        let spec =
            gojson::decode_one(r#"{"answers":[{"value":"no"},{"regex":"4\\d"},{"numeric":42}]}"#)
                .unwrap()
                .0;
        let result = grade(&spec, " 42 ").unwrap();
        assert_eq!(
            result.detail,
            r#"{"matched":true,"mode":"regex","answer_index":1,"answers":[{"index":0,"mode":"value","matched":false},{"index":1,"mode":"regex","matched":true},{"index":2,"mode":"numeric","matched":true}]}"#
        );
    }
}
