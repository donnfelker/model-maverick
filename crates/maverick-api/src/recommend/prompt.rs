//! Prompt-path pieces of the recommend response and trace (reference Part 4 §2.6).
//!
//! - [`classification_block`]: `{capability, difficulty, domain_tags, high_stakes,
//!   confidence, method, truncated}`, `truncated` only when true.
//! - [`exclude_prompt`]: the stored request (Go `excludePrompt`). The body and its
//!   `workload` are decoded as objects of raw values, the prompt is deleted, and both are
//!   re-encoded as Go's `json.Marshal` does: keys sorted by byte value, every value
//!   compacted with `<` `>` `&` U+2028 U+2029 escaped inside strings, and every other byte
//!   kept (number spellings, nested key order). Go deleted only the exact key `prompt`, so
//!   a prompt sent as `Prompt` (which its case-insensitive decoder accepted) was stored.
//!   The prompt is never stored here: every key that case-insensitively names the prompt
//!   is deleted (spec §10, G5).
//! - [`exclude_affinity_key`]: the same for the conversation affinity key, on either
//!   workload form (D44).

use std::collections::BTreeMap;

use maverick_domain::gojson::{self, JsonValue, match_field, write_string};
use maverick_store::JsonRawValue;

use crate::classify::Resolution;

/// The response's `classification` block for a prompt-path resolution.
pub(super) fn classification_block(resolution: &Resolution) -> JsonValue {
    let classification = &resolution.classification;
    let mut members = vec![
        (
            "capability".to_owned(),
            JsonValue::String(classification.capability.clone()),
        ),
        (
            "difficulty".to_owned(),
            JsonValue::Number(classification.difficulty.to_string()),
        ),
        (
            "domain_tags".to_owned(),
            JsonValue::Array(
                classification
                    .domain_tags
                    .iter()
                    .map(|tag| JsonValue::String(tag.clone()))
                    .collect(),
            ),
        ),
        (
            "high_stakes".to_owned(),
            JsonValue::Bool(classification.high_stakes()),
        ),
        (
            "confidence".to_owned(),
            // A parsed or cached confidence is finite.
            JsonValue::Number(gojson::encode_f64(classification.confidence).unwrap_or_default()),
        ),
        (
            "method".to_owned(),
            JsonValue::String(resolution.method.as_str().to_owned()),
        ),
    ];
    if classification.truncated {
        members.push(("truncated".to_owned(), JsonValue::Bool(true)));
    }
    JsonValue::Object(members)
}

type Members = BTreeMap<String, Box<JsonRawValue>>;

/// The request body as stored on the prompt path: the prompt removed, re-encoded as Go
/// did. Fails only for a body that is not an object (the decoder already accepted it).
pub(super) fn exclude_prompt(body: &str) -> Result<String, String> {
    exclude(body, &[("workload", "prompt")])
}

/// The request body as stored with conversation affinity (spec §10.2, D44): the affinity
/// key removed, like the prompt, and the prompt too on the prompt path. Re-encoded as
/// [`exclude_prompt`] does.
pub(super) fn exclude_affinity_key(body: &str, prompt_path: bool) -> Result<String, String> {
    if prompt_path {
        exclude(body, &[("workload", "prompt"), ("affinity", "key")])
    } else {
        exclude(body, &[("affinity", "key")])
    }
}

/// Deletes every `inner` key of every `outer` object member, matching both names
/// case-insensitively, and re-encodes the body.
fn exclude(body: &str, fields: &[(&'static str, &'static str)]) -> Result<String, String> {
    let mut top: Members = serde_json::from_str(body).map_err(|error| error.to_string())?;
    for &(outer, field) in fields {
        let objects = top
            .iter()
            .filter(|(key, _)| match_field(key, &[outer]).is_some())
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in objects {
            let Some(raw) = top.get(&key) else { continue };
            // A member that is not an object carries no such field.
            let Ok(mut inner) = serde_json::from_str::<Members>(raw.get()) else {
                continue;
            };
            inner.retain(|key, _| match_field(key, &[field]).is_none());
            let encoded = JsonRawValue::from_string(encode_object(&inner))
                .map_err(|error| error.to_string())?;
            top.insert(key, encoded);
        }
    }
    Ok(encode_object(&top))
}

/// Go `json.Marshal` of `map[string]json.RawMessage`.
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

/// Go `compact` with HTML escaping, for valid JSON text.
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

fn push_escape(out: &mut String, hex: &str) {
    out.push('\\');
    out.push('u');
    out.push_str(hex);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `validate_input.json#exclude_prompt/*` row.
    #[test]
    fn exclude_prompt_rows() {
        let doc = parity_support_rows();
        let rows: Vec<&serde_json::Value> = doc
            .iter()
            .filter(|row| row["function"] == "exclude_prompt")
            .collect();
        assert_eq!(rows.len(), 3);
        for row in rows {
            assert_eq!(
                exclude_prompt(row["input"].as_str().unwrap()).unwrap(),
                row["expected"].as_str().unwrap(),
                "{}",
                row["id"]
            );
        }
    }

    fn parity_support_rows() -> Vec<serde_json::Value> {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../parity/fixtures/decision/validate_input.json"
        );
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        doc["cases"].as_array().unwrap().clone()
    }

    #[test]
    fn every_spelling_of_the_prompt_key_is_removed() {
        let body =
            r#"{"workload":{"Prompt":"a secret","PROMPT":"b secret","prompt":"c"},"mode":"cost"}"#;
        let stored = exclude_prompt(body).unwrap();
        assert_eq!(stored, r#"{"mode":"cost","workload":{}}"#);
        assert!(!stored.contains("secret"));
        // Go's decoder also matches `Workload`.
        assert_eq!(
            exclude_prompt(r#"{"Workload":{"prompt":"x","capability":"y"}}"#).unwrap(),
            r#"{"Workload":{"capability":"y"}}"#
        );
    }
}
