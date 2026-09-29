//! Go `rewriteModel`: the per-attempt upstream body (reference part 6 §10).
//!
//! 1. Decode the body into top-level key to RAW JSON value. The body must be a JSON object.
//!    A duplicate key keeps its last value.
//! 2. Set `model` to the entry's model, encoded as a Go JSON string (HTML-escaped). Only the
//!    exact lowercase key `model` is replaced; a `MODEL` key stays as well (QUIRK 4).
//! 3. Apply the surface's normalizer (the two fixed Go mappings; D29 adds catalog-driven
//!    constraints in bead 6.8).
//! 4. Re-encode: top-level keys sorted by byte value, every value compacted (no whitespace
//!    between tokens, nested whitespace included), `<` `>` `&` and U+2028 / U+2029 inside
//!    strings escaped as Go does. Every other byte of a value is kept: number spellings
//!    (`0.50`, `1e2`), existing `\uXXXX` escapes, non-ASCII UTF-8, and nested key order.
//!
//! Invalid UTF-8 in the body becomes U+FFFD before decoding, as the proxy's model decode
//! does. The body is never logged.

use std::collections::BTreeMap;

use maverick_core::gojson::{quote, write_string};
use serde_json::value::RawValue;

use crate::chain::Surface;

/// The fixed Go normalizer a surface applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Normalizer {
    /// No normalization.
    None,
    /// Chat: a top-level `reasoning_effort` of `"xhigh"` becomes `"high"`.
    Chat,
    /// Messages: an `output_config.effort` of `"xhigh"` becomes `"max"`, and
    /// `output_config` is re-encoded with sorted keys.
    Messages,
}

impl Normalizer {
    /// The surface's normalizer.
    #[must_use]
    pub fn for_surface(surface: Surface) -> Self {
        match surface {
            Surface::Chat => Self::Chat,
            Surface::Messages => Self::Messages,
        }
    }
}

/// The body is not a JSON object. The attempt is skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("request body is not a JSON object")]
pub struct NotAnObject;

type Members = BTreeMap<String, Box<RawValue>>;

/// The upstream body for one attempt: `body` with `model` replaced and the surface's
/// normalizer applied, re-encoded as Go did.
pub fn rewrite_model(
    body: &[u8],
    model: &str,
    normalizer: Normalizer,
) -> Result<Vec<u8>, NotAnObject> {
    let text = String::from_utf8_lossy(body);
    let mut members: Members = serde_json::from_str(&text).map_err(|_| NotAnObject)?;
    members.insert("model".to_owned(), raw(quote(model)));
    match normalizer {
        Normalizer::None => {}
        Normalizer::Chat => {
            if is_string(members.get("reasoning_effort").map(Box::as_ref), "xhigh") {
                members.insert("reasoning_effort".to_owned(), raw(quote("high")));
            }
        }
        Normalizer::Messages => normalize_output_config(&mut members),
    }
    Ok(encode_object(&members).into_bytes())
}

/// Messages: `output_config` must decode as an object whose `effort` is the string
/// `"xhigh"`. Then `effort` becomes `"max"` and the object is re-encoded (sorted keys,
/// sibling values kept). Otherwise `output_config` is left to the final compaction.
fn normalize_output_config(members: &mut Members) {
    let Some(config) = members.get("output_config") else {
        return;
    };
    let Ok(Some(mut config)) = serde_json::from_str::<Option<Members>>(config.get()) else {
        return;
    };
    if !is_string(config.get("effort").map(Box::as_ref), "xhigh") {
        return;
    }
    config.insert("effort".to_owned(), raw(quote("max")));
    members.insert("output_config".to_owned(), raw(encode_object(&config)));
}

/// True when `value` decodes as the JSON string `want` (escapes decoded, exact case).
fn is_string(value: Option<&RawValue>, want: &str) -> bool {
    value.is_some_and(|value| {
        serde_json::from_str::<String>(value.get()).is_ok_and(|text| text == want)
    })
}

/// A raw value from JSON text this module produced.
fn raw(json: String) -> Box<RawValue> {
    RawValue::from_string(json).expect("the rewrite only builds valid JSON")
}

/// Go `json.Marshal` of `map[string]json.RawMessage`: keys sorted by byte value and
/// written as Go strings, values compacted with HTML escaping.
fn encode_object(members: &Members) -> String {
    let mut out = String::from("{");
    for (index, (key, value)) in members.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        write_string(&mut out, key);
        out.push(':');
        compact_into(&mut out, value.get());
    }
    out.push('}');
    out
}

/// Go `compact` with HTML escaping, for valid JSON text: whitespace between tokens is
/// dropped, and inside strings `<` `>` `&` U+2028 U+2029 become `\u` escapes (lowercase
/// hex). Every other character is kept.
fn compact_into(out: &mut String, json: &str) {
    let mut in_string = false;
    let mut escaped = false;
    for ch in json.chars() {
        if !in_string {
            match ch {
                ' ' | '\t' | '\n' | '\r' => {}
                '"' => {
                    in_string = true;
                    out.push(ch);
                }
                _ => out.push(ch),
            }
            continue;
        }
        if escaped {
            escaped = false;
            out.push(ch);
            continue;
        }
        match ch {
            '\\' => {
                escaped = true;
                out.push(ch);
            }
            '"' => {
                in_string = false;
                out.push(ch);
            }
            '<' => push_escape(out, "003c"),
            '>' => push_escape(out, "003e"),
            '&' => push_escape(out, "0026"),
            '\u{2028}' => push_escape(out, "2028"),
            '\u{2029}' => push_escape(out, "2029"),
            _ => out.push(ch),
        }
    }
}

/// Write a backslash, `u`, and the four hex digits.
fn push_escape(out: &mut String, hex: &str) {
    out.push('\\');
    out.push('u');
    out.push_str(hex);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compact(json: &str) -> String {
        let mut out = String::new();
        compact_into(&mut out, json);
        out
    }

    #[test]
    fn compaction_keeps_strings_and_escapes() {
        assert_eq!(compact(" { \"a b\" : [ 1 , 2 ] } "), "{\"a b\":[1,2]}");
        assert_eq!(compact("\"\\\" <\""), "\"\\\" \\u003c\"");
        assert_eq!(compact("\"\\\\\" "), "\"\\\\\"");
        assert_eq!(compact("\"\t\""), "\"\t\"", "only JSON text reaches here");
        assert_eq!(compact("\"\u{2028}&\""), "\"\\u2028\\u0026\"");
        assert_eq!(compact("\"\u{2029}>\""), "\"\\u2029\\u003e\"");
        assert_eq!(
            compact("[ 0.50 ,1e2,\"\\u00e9\"]"),
            "[0.50,1e2,\"\\u00e9\"]"
        );
    }

    #[test]
    fn keys_are_go_strings_in_byte_order() {
        let body = br#"{"b":1,"<":2,"a":3,"B":4,"\u00e9":5}"#;
        assert_eq!(
            String::from_utf8(rewrite_model(body, "m", Normalizer::None).expect("rewrite"))
                .expect("utf-8"),
            "{\"\\u003c\":2,\"B\":4,\"a\":3,\"b\":1,\"model\":\"m\",\"\u{e9}\":5}"
        );
    }

    #[test]
    fn non_objects_fail() {
        for body in [&b"[1]"[..], b"\"s\"", b"1", b"null", b"{", b"{} x", b""] {
            assert_eq!(
                rewrite_model(body, "m", Normalizer::Chat),
                Err(NotAnObject),
                "{body:?}"
            );
        }
    }
}
