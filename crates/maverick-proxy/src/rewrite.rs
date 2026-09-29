//! Go `rewriteModel`: the per-attempt upstream body (reference part 6 §10).
//!
//! 1. Decode the body into top-level key to RAW JSON value. The body must be a JSON object.
//!    A duplicate key keeps its last value.
//! 2. Set `model` to the entry's model, encoded as a Go JSON string (HTML-escaped). Only the
//!    exact lowercase key `model` is replaced; a `MODEL` key stays as well (QUIRK 4).
//! 3. Apply the entry's `settings` (D14; skipped for `?settings=caller`), then normalize:
//!    the entry's catalog constraints (D29) where it has one for a field, else the two
//!    fixed Go mappings. Normalization never adds a missing field.
//! 4. Re-encode: top-level keys sorted by byte value, every value compacted (no whitespace
//!    between tokens, nested whitespace included), `<` `>` `&` and U+2028 / U+2029 inside
//!    strings escaped as Go does. Every other byte of a value is kept: number spellings
//!    (`0.50`, `1e2`), existing `\uXXXX` escapes, non-ASCII UTF-8, and nested key order.
//!
//! Invalid UTF-8 in the body becomes U+FFFD before decoding, as the proxy's model decode
//! does. The body is never logged.

use std::collections::BTreeMap;

use maverick_core::gojson::{quote, write_string};
use maverick_core::normalize::{
    self, FIELDS, ParamConstraints, WireFormat, WirePath, go_fallback, wire_path,
};
use maverick_core::openrouter;
use maverick_core::settings::wire_settings;
use serde_json::value::RawValue;
use serde_json::{Map, Value};

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

    /// The wire format the normalizer works on; `None` normalizes nothing.
    fn format(self) -> Option<WireFormat> {
        match self {
            Self::None => None,
            Self::Chat => Some(WireFormat::ChatCompletions),
            Self::Messages => Some(WireFormat::Messages),
        }
    }
}

/// The surface's wire format.
#[must_use]
pub fn wire_format(surface: Surface) -> WireFormat {
    match surface {
        Surface::Chat => WireFormat::ChatCompletions,
        Surface::Messages => WireFormat::Messages,
    }
}

/// What one attempt's body takes from its chain entry besides the model.
#[derive(Debug, Clone, Copy)]
pub struct AttemptParams<'a> {
    /// The surface's wire format.
    pub format: WireFormat,
    /// The entry's settings (D14), or `None` when the alias ends in `?settings=caller`.
    pub settings: Option<&'a Map<String, Value>>,
    /// The entry's parameter constraints (D29).
    pub constraints: &'a ParamConstraints,
    /// OpenRouter execution for a zero-retention tenant (D15): force the Zero Data
    /// Retention routing option on this attempt.
    pub zdr: bool,
}

/// An attempt's upstream body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rewritten {
    pub body: Vec<u8>,
    /// At least one entry setting was written (D14 `settings_applied`).
    pub settings_applied: bool,
}

/// The body is not a JSON object. The attempt is skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("request body is not a JSON object")]
pub struct NotAnObject;

type Members = BTreeMap<String, Box<RawValue>>;

/// Go `rewriteModel`: `body` with `model` replaced and the surface's fixed normalizer
/// applied, re-encoded as Go did. No settings and no catalog constraints.
pub fn rewrite_model(
    body: &[u8],
    model: &str,
    normalizer: Normalizer,
) -> Result<Vec<u8>, NotAnObject> {
    rewrite(
        body,
        model,
        normalizer.format(),
        None,
        &ParamConstraints::new(),
        false,
    )
    .map(|rewritten| rewritten.body)
}

/// The upstream body for one attempt: the model replaced, the entry's settings applied
/// (D14), each parameter normalized (D29, else the Go mapping), re-encoded as Go did.
pub fn rewrite_attempt(
    body: &[u8],
    model: &str,
    params: &AttemptParams<'_>,
) -> Result<Rewritten, NotAnObject> {
    rewrite(
        body,
        model,
        Some(params.format),
        params.settings,
        params.constraints,
        params.zdr,
    )
}

fn rewrite(
    body: &[u8],
    model: &str,
    format: Option<WireFormat>,
    settings: Option<&Map<String, Value>>,
    constraints: &ParamConstraints,
    zdr: bool,
) -> Result<Rewritten, NotAnObject> {
    let text = String::from_utf8_lossy(body);
    let mut members: Members = serde_json::from_str(&text).map_err(|_| NotAnObject)?;
    members.insert("model".to_owned(), raw(quote(model)));
    let mut settings_applied = false;
    if let (Some(format), Some(settings)) = (format, settings) {
        let (fields, skipped) = wire_settings(settings, format);
        for key in &skipped {
            tracing::debug!(
                setting = %key,
                surface = format.as_str(),
                "maverick-proxy: entry setting has no wire field on this surface; left untouched"
            );
        }
        for field in &fields {
            if write_path(&mut members, field.path, &field.value) {
                settings_applied = true;
            } else {
                tracing::debug!(
                    setting = %field.key,
                    surface = format.as_str(),
                    "maverick-proxy: the request field for this setting is not an object; left untouched"
                );
            }
        }
    }
    if let Some(format) = format {
        for field in FIELDS {
            let Some(path) = wire_path(field, format) else {
                continue;
            };
            // Never add a missing field.
            let Some(current) = read_path(&members, path) else {
                continue;
            };
            let target = match normalize::normalize(field, &current, constraints) {
                Some(target) => target,
                None => match go_fallback(field, format, &current) {
                    Some(target) => target,
                    None => continue,
                },
            };
            if target != current {
                write_path(&mut members, path, &target);
            }
        }
    }
    if zdr {
        force_zdr(&mut members);
    }
    Ok(Rewritten {
        body: encode_object(&members).into_bytes(),
        settings_applied,
    })
}

/// D15: `provider.zdr = true`, whatever the caller sent. It runs last, so no setting or
/// normalization can undo it. A `provider` object keeps its other options (re-encoded
/// with sorted keys); a `provider` that is not an object is replaced, because the option
/// must be there (fail closed).
fn force_zdr(members: &mut Members) {
    let path = WirePath::Nested {
        object: openrouter::ZDR_OBJECT,
        field: openrouter::ZDR_FIELD,
    };
    if !write_path(members, path, &Value::Bool(true)) {
        members.insert(
            openrouter::ZDR_OBJECT.to_owned(),
            raw(format!("{{\"{}\":true}}", openrouter::ZDR_FIELD)),
        );
    }
}

/// The value at `path`, when present. A nested field needs its parent to decode as an
/// object; a `null` or non-object parent has no field.
fn read_path(members: &Members, path: WirePath) -> Option<Value> {
    match path {
        WirePath::Top(name) => serde_json::from_str(members.get(name)?.get()).ok(),
        WirePath::Nested { object, field } => {
            let parent: Members = serde_json::from_str(members.get(object)?.get()).ok()?;
            serde_json::from_str(parent.get(field)?.get()).ok()
        }
    }
}

/// Writes `value` at `path`. A nested field's parent must be absent, `null`, or an
/// object: it is then re-encoded with sorted keys (siblings keep their raw bytes), as
/// Go's messages normalizer did. Returns false, writing nothing, for any other parent.
fn write_path(members: &mut Members, path: WirePath, value: &Value) -> bool {
    let encoded = raw(value.to_string());
    match path {
        WirePath::Top(name) => {
            members.insert(name.to_owned(), encoded);
            true
        }
        WirePath::Nested { object, field } => {
            let mut parent: Members = match members.get(object) {
                None => Members::new(),
                Some(existing) => match serde_json::from_str::<Option<Members>>(existing.get()) {
                    Ok(Some(parent)) => parent,
                    Ok(None) => Members::new(),
                    Err(_) => return false,
                },
            };
            parent.insert(field.to_owned(), encoded);
            members.insert(object.to_owned(), raw(encode_object(&parent)));
            true
        }
    }
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
