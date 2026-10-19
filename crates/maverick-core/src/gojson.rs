//! Go `encoding/json` compatibility helpers for the SDK core and the proxy.
//!
//! The Go proxy wrote JSON with `json.Marshal` and read it with `json.Unmarshal` into
//! structs. These helpers reproduce the parts the Rust port needs:
//!
//! - [`write_string`] / [`quote`]: `json.Marshal` string escaping (HTML-safe).
//! - [`members`], [`fold_eq`], [`apply_string`], [`apply_bool`]: struct-field decoding.
//!   Member names match field names case-insensitively (Go's fold), missing members keep
//!   their zero value, `null` leaves a string or bool as it was, and a member of the wrong
//!   JSON type is an error.
//!
//! The engine crates have their own, larger `maverick_domain::gojson`; this module is the
//! small subset the proxy and the SDK core share.

use std::fmt::Write as _;

use serde_json::{Map, Value};

/// Write `text` as a JSON string the way Go's `json.Marshal` does: `\b \f \n \r \t` use
/// short forms; other control characters, the HTML-sensitive `< > &`, and U+2028/U+2029
/// are written as lowercase `\u00xx` escapes; everything else is written raw.
pub fn write_string(out: &mut String, text: &str) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '<' | '>' | '&' | '\u{2028}' | '\u{2029}' => {
                let _ = write!(out, "\\u{:04x}", u32::from(ch));
            }
            control if control < ' ' => {
                let _ = write!(out, "\\u{:04x}", u32::from(control));
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

/// `text` as a Go `json.Marshal` JSON string, quotes included.
#[must_use]
pub fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    write_string(&mut out, text);
    out
}

/// Every member of `object` whose name folds to `field`, with an exact-case match last.
///
/// Go applies members in document order, so a later duplicate wins. `serde_json` maps do
/// not keep document order, so the exact-case member is applied last instead. The two
/// differ only for an object that holds two case variants of one name.
#[must_use]
pub fn members<'a>(object: &'a Map<String, Value>, field: &str) -> Vec<&'a Value> {
    let mut folded = Vec::new();
    let mut exact = None;
    for (name, value) in object {
        if name == field {
            exact = Some(value);
        } else if fold_eq(name, field) {
            folded.push(value);
        }
    }
    folded.extend(exact);
    folded
}

/// Go's case-insensitive member match against an ASCII field name: ASCII case folds, and
/// so do U+212A KELVIN SIGN (to `k`) and U+017F LONG S (to `s`).
#[must_use]
pub fn fold_eq(name: &str, field: &str) -> bool {
    let fold = |ch: char| match ch {
        '\u{212a}' => Some('K'),
        '\u{17f}' => Some('S'),
        ch if ch.is_ascii() => Some(ch.to_ascii_uppercase()),
        _ => None,
    };
    name.chars().count() == field.len()
        && name
            .chars()
            .zip(field.chars())
            .all(|(left, right)| fold(left).is_some_and(|left| Some(left) == fold(right)))
}

/// Decode a Go `string` field: each matching member that is a string sets `target`,
/// `null` leaves it, and any other type is an error.
pub fn apply_string(
    object: &Map<String, Value>,
    field: &str,
    target: &mut String,
) -> Result<(), String> {
    for value in members(object, field) {
        match value {
            Value::String(text) => text.clone_into(target),
            Value::Null => {}
            other => return Err(type_error(field, other, "string")),
        }
    }
    Ok(())
}

/// Decode a Go `bool` field: each matching member that is a bool sets `target`, `null`
/// leaves it, and any other type is an error.
pub fn apply_bool(
    object: &Map<String, Value>,
    field: &str,
    target: &mut bool,
) -> Result<(), String> {
    for value in members(object, field) {
        match value {
            Value::Bool(flag) => *target = *flag,
            Value::Null => {}
            other => return Err(type_error(field, other, "bool")),
        }
    }
    Ok(())
}

/// A decode error message naming the field, the JSON type found, and the type wanted.
#[must_use]
pub fn type_error(field: &str, value: &Value, want: &str) -> String {
    let got = match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    };
    format!("`{field}` is {got}, want {want}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_escape_like_go_json_marshal() {
        assert_eq!(
            quote("q\"b\\n\nr\rt\tb\u{8}f\u{c}c\u{1}\u{1f}<>&\u{2028}\u{2029}\u{7f}\u{2014}é😀"),
            "\"q\\\"b\\\\n\\nr\\rt\\tb\\bf\\fc\\u0001\\u001f\\u003c\\u003e\\u0026\\u2028\\u2029\u{7f}\u{2014}é😀\""
        );
    }

    #[test]
    fn fold_matches_go() {
        assert!(fold_eq("MODEL", "model"));
        assert!(fold_eq("Model", "model"));
        assert!(fold_eq("\u{17f}ettings", "settings"));
        assert!(fold_eq("\u{212a}ind", "kind"));
        assert!(!fold_eq("model_", "model"));
        assert!(!fold_eq("mode", "model"));
        assert!(!fold_eq("m\u{f6}del", "model"));
        assert!(!fold_eq("", "model"));
    }

    #[test]
    fn exact_member_is_applied_last() {
        let object: Map<String, Value> =
            serde_json::from_str(r#"{"model":"exact","MODEL":"upper","Model":"title"}"#)
                .expect("json");
        let mut model = String::new();
        apply_string(&object, "model", &mut model).expect("strings");
        assert_eq!(model, "exact");
        assert_eq!(members(&object, "model").len(), 3);
    }

    #[test]
    fn string_and_bool_fields_follow_go() {
        let object: Map<String, Value> =
            serde_json::from_str(r#"{"a":null,"b":true,"c":5,"d":"x"}"#).expect("json");
        let mut text = "kept".to_owned();
        apply_string(&object, "a", &mut text).expect("null leaves the value");
        assert_eq!(text, "kept");
        apply_string(&object, "missing", &mut text).expect("missing leaves the value");
        assert_eq!(text, "kept");
        assert_eq!(
            apply_string(&object, "c", &mut text),
            Err("`c` is number, want string".to_owned())
        );
        let mut flag = false;
        apply_bool(&object, "b", &mut flag).expect("bool");
        assert!(flag);
        assert_eq!(
            apply_bool(&object, "d", &mut flag),
            Err("`d` is string, want bool".to_owned())
        );
    }
}
