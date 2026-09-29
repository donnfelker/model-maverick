//! `parse(text)`: the classifier's structured output (reference Part 4 §3.3).

use super::Classification;
use crate::gofmt::{fmt_v_f64, quote};
use crate::gojson::{self, JsonValue};
use crate::tasks::CAPABILITIES;

/// The Go type named in a top-level type error.
const GO_TYPE: &str = r#"struct { Capability *string "json:\"capability\""; Difficulty *int "json:\"difficulty\""; DomainTags []string "json:\"domain_tags\""; Confidence *float64 "json:\"confidence\"" }"#;

const FIELDS: [&str; 4] = ["capability", "difficulty", "domain_tags", "confidence"];

/// `stripCodeFence`: a trimmed text that starts with three backticks and has a newline
/// loses everything through its first newline and everything from the last three
/// backticks after it, then is trimmed. Any other text is returned unchanged.
pub fn strip_code_fence(text: &str) -> &str {
    let s = text.trim();
    let fenced = s
        .strip_prefix("```")
        .and_then(|_| s.split_once('\n'))
        .map(|(_, rest)| rest);
    match fenced {
        Some(rest) => rest.rfind("```").map_or(rest, |end| &rest[..end]).trim(),
        None => text,
    }
}

fn mismatch(value: &JsonValue, field: &str, go_type: &str) -> String {
    format!(
        "json: cannot unmarshal {} into Go struct field .{field} of type {go_type}",
        value.kind_name()
    )
}

/// The parsed fields; `None` is absent or JSON `null`.
#[derive(Default)]
struct Fields {
    capability: Option<String>,
    difficulty: Option<i64>,
    domain_tags: Option<Vec<String>>,
    confidence: Option<f64>,
}

/// Go's strict decode into the anonymous struct: unknown fields rejected, keys matched
/// case-insensitively, the last duplicate wins, and the first type error is reported.
fn decode(value: &JsonValue) -> Result<Fields, String> {
    let mut fields = Fields::default();
    let members = match value {
        JsonValue::Null => return Ok(fields),
        JsonValue::Object(members) => members,
        other => {
            return Err(format!(
                "json: cannot unmarshal {} into Go value of type {GO_TYPE}",
                other.kind_name()
            ));
        }
    };
    for (key, value) in members {
        let Some(field) = gojson::match_field(key, &FIELDS) else {
            return Err(format!("json: unknown field {}", quote(key)));
        };
        match (field, value) {
            ("capability", JsonValue::Null) => fields.capability = None,
            ("capability", JsonValue::String(s)) => fields.capability = Some(s.clone()),
            ("capability", other) => return Err(mismatch(other, field, "string")),
            ("difficulty", JsonValue::Null) => fields.difficulty = None,
            ("difficulty", JsonValue::Number(literal)) => {
                let n = literal.parse::<i64>().map_err(|_| {
                    format!(
                        "json: cannot unmarshal number {literal} into Go struct field .difficulty of type int"
                    )
                })?;
                fields.difficulty = Some(n);
            }
            ("difficulty", other) => return Err(mismatch(other, field, "int")),
            ("confidence", JsonValue::Null) => fields.confidence = None,
            ("confidence", JsonValue::Number(literal)) => match literal.parse::<f64>() {
                Ok(x) if x.is_finite() => fields.confidence = Some(x),
                _ => {
                    return Err(format!(
                        "json: cannot unmarshal number {literal} into Go struct field .confidence of type float64"
                    ));
                }
            },
            ("confidence", other) => return Err(mismatch(other, field, "float64")),
            (_, JsonValue::Null) => fields.domain_tags = None,
            (_, JsonValue::Array(items)) => {
                let mut tags = Vec::with_capacity(items.len());
                for item in items {
                    match item {
                        JsonValue::String(s) => tags.push(s.clone()),
                        // A null element is the zero value.
                        JsonValue::Null => tags.push(String::new()),
                        other => return Err(mismatch(other, field, "string")),
                    }
                }
                fields.domain_tags = Some(tags);
            }
            (_, other) => return Err(mismatch(other, field, "[]string")),
        }
    }
    Ok(fields)
}

/// `parse(text)`: strip a code fence, decode one strict JSON object (errors prefixed
/// `parsing structured output: `; anything but whitespace after it is `trailing data
/// after JSON object`), then check in order: capability, difficulty, and confidence
/// present, the capability in the v1 enum, difficulty in 1..=5, and confidence in
/// [0, 1]. Absent or null `domain_tags` is empty. `truncated` is left false.
pub fn parse(text: &str) -> Result<Classification, String> {
    let error = |e: String| format!("parsing structured output: {e}");
    let (value, rest) = gojson::decode_one(strip_code_fence(text)).map_err(|e| error(e.0))?;
    let fields = decode(&value).map_err(error)?;
    if !gojson::is_blank(rest) {
        return Err(error("trailing data after JSON object".to_owned()));
    }
    let capability = fields
        .capability
        .ok_or("structured output missing capability")?;
    let difficulty = fields
        .difficulty
        .ok_or("structured output missing difficulty")?;
    let confidence = fields
        .confidence
        .ok_or("structured output missing confidence")?;
    if !CAPABILITIES.contains(&capability.as_str()) {
        return Err(format!(
            "capability {} is not a v1 capability",
            quote(&capability)
        ));
    }
    if !(1..=5).contains(&difficulty) {
        return Err(format!("difficulty {difficulty} is out of range 1-5"));
    }
    if !(0.0..=1.0).contains(&confidence) {
        return Err(format!(
            "confidence {} is out of range 0-1",
            fmt_v_f64(confidence)
        ));
    }
    Ok(Classification {
        capability,
        difficulty,
        domain_tags: fields.domain_tags.unwrap_or_default(),
        confidence,
        truncated: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#""capability":"factual_qa","difficulty":2,"confidence":0.5"#;

    fn with(extra: &str) -> String {
        format!("{{{GOOD},{extra}}}")
    }

    #[test]
    fn field_type_errors_name_the_field() {
        let cases = [
            (
                r#"{"capability":1}"#.to_owned(),
                "json: cannot unmarshal number into Go struct field .capability of type string",
            ),
            (
                r#"{"difficulty":true}"#.to_owned(),
                "json: cannot unmarshal bool into Go struct field .difficulty of type int",
            ),
            (
                r#"{"difficulty":99999999999999999999}"#.to_owned(),
                "json: cannot unmarshal number 99999999999999999999 into Go struct field .difficulty of type int",
            ),
            (
                r#"{"confidence":"x"}"#.to_owned(),
                "json: cannot unmarshal string into Go struct field .confidence of type float64",
            ),
            (
                r#"{"confidence":1e400}"#.to_owned(),
                "json: cannot unmarshal number 1e400 into Go struct field .confidence of type float64",
            ),
            (
                with(r#""domain_tags":"x""#),
                "json: cannot unmarshal string into Go struct field .domain_tags of type []string",
            ),
            (
                with(r#""domain_tags":[1]"#),
                "json: cannot unmarshal number into Go struct field .domain_tags of type string",
            ),
            (
                "\"x\"".to_owned(),
                &format!("json: cannot unmarshal string into Go value of type {GO_TYPE}"),
            ),
        ];
        for (input, tail) in cases {
            assert_eq!(
                parse(&input),
                Err(format!("parsing structured output: {tail}")),
                "{input}"
            );
        }
    }

    #[test]
    fn nulls_and_duplicates() {
        assert_eq!(
            parse("null"),
            Err("structured output missing capability".to_owned())
        );
        assert_eq!(
            parse(&with(r#""capability":null"#)),
            Err("structured output missing capability".to_owned())
        );
        assert_eq!(
            parse(&with(r#""difficulty":null"#)),
            Err("structured output missing difficulty".to_owned())
        );
        assert_eq!(
            parse(&with(r#""confidence":null"#)),
            Err("structured output missing confidence".to_owned())
        );
        let tags = parse(&with(r#""domain_tags":[null,"legal"]"#)).expect("parses");
        assert_eq!(tags.domain_tags, ["", "legal"]);
        let cleared = parse(&with(r#""domain_tags":["a"],"domain_tags":null"#)).expect("parses");
        assert!(cleared.domain_tags.is_empty());
    }

    #[test]
    fn fences_without_a_close_or_a_newline() {
        assert_eq!(strip_code_fence("```\n x \n"), "x");
        assert_eq!(strip_code_fence("  ```x```  "), "  ```x```  ");
        assert_eq!(strip_code_fence(" plain "), " plain ");
        assert_eq!(
            strip_code_fence("```json\n{}\n```\ntrailer ``` end"),
            "{}\n```\ntrailer"
        );
    }
}
