//! Wire formats for `POST /v1/recommend`: the request body the Go proxy wrote, and the
//! response decoding rules Go's `encoding/json` applied (reference part 6 §7).

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde_json::{Map, Value};

use super::{
    ChainEntry, EngineFailure, RecommendError, RecommendRequest, Recommendation, RetentionMode,
    Workload,
};
use crate::alias::Mode;
use crate::gojson::{apply_bool, apply_string, members, type_error, write_string};

// ---------------------------------------------------------------------------------------
// Request body

/// The request body: compact JSON, members in lexicographic order, strings escaped as Go's
/// `json.Marshal` escapes them (HTML-safe). `pool` is omitted when empty.
pub(super) fn encode_request(request: &RecommendRequest) -> String {
    let mut out = String::from("{\"mode\":");
    write_string(&mut out, engine_mode(request.mode));
    if let Some(pool) = request.pool.as_deref().filter(|pool| !pool.is_empty()) {
        out.push_str(",\"pool\":");
        write_string(&mut out, pool);
    }
    out.push_str(",\"workload\":{");
    match &request.workload {
        Workload::Declared {
            capability,
            difficulty,
            domain_tags,
        } => {
            out.push_str("\"capability\":");
            write_string(&mut out, capability);
            if let Some(difficulty) = difficulty {
                let _ = write!(out, ",\"difficulty\":{difficulty}");
            }
            if !domain_tags.is_empty() {
                out.push_str(",\"domain_tags\":[");
                for (index, tag) in domain_tags.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    write_string(&mut out, tag);
                }
                out.push(']');
            }
        }
        Workload::Prompt {
            prompt,
            projection_version,
        } => {
            // "projection_version" sorts before "prompt" ('j' < 'm').
            if let Some(version) = projection_version
                .as_deref()
                .filter(|version| !version.is_empty())
            {
                out.push_str("\"projection_version\":");
                write_string(&mut out, version);
                out.push(',');
            }
            out.push_str("\"prompt\":");
            write_string(&mut out, prompt);
        }
    }
    out.push_str("}}");
    out
}

/// The engine's name for a mode.
fn engine_mode(mode: Mode) -> &'static str {
    match mode {
        Mode::Balanced => "balanced",
        Mode::Quality => "quality",
        Mode::Cost => "cost",
    }
}

// ---------------------------------------------------------------------------------------
// Error responses

/// Map a non-2xx response. The two privacy codes are terminal (D17) whatever the status,
/// so a zero-retention refusal can never turn into a fail-open fallback.
pub(super) fn error_response(status: u16, body: &[u8]) -> RecommendError {
    let envelope = serde_json::from_slice::<Value>(body).ok();
    let error = envelope.as_ref().and_then(|value| value.get("error"));
    let code = error
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str);
    let message = error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    match code {
        Some("zero_retention_requires_declared") => {
            RecommendError::ZeroRetentionRequiresDeclared { status, message }
        }
        Some("no_compliant_columns") => RecommendError::NoCompliantColumns { status, message },
        code => EngineFailure::Status {
            status,
            code: code.map(str::to_owned),
        }
        .into(),
    }
}

// ---------------------------------------------------------------------------------------
// Response decoding (Go `encoding/json` rules)

/// Decode a 2xx body the way Go decoded it into its response struct:
///
/// - member names match field names case-insensitively (Go's fold: ASCII case, plus
///   U+212A KELVIN SIGN for `k` and U+017F LONG S for `s`); an exact match wins;
/// - unknown members are ignored and missing members keep their zero value;
/// - `null` clears a slice or map and leaves every other field as it was;
/// - a member of the wrong JSON type fails the whole decode, as does a body that is not
///   JSON or whose top level is not an object or `null`.
///
/// When two members fold to one field, the exact-case member is applied last. Go applies
/// them in document order; the engine never sends such duplicates.
pub(super) fn decode_response(body: &[u8]) -> Result<Recommendation, String> {
    let value: Value =
        serde_json::from_slice(body).map_err(|error| format!("not JSON: {error}"))?;
    let mut out = Recommendation::default();
    let object = match &value {
        Value::Null => return Ok(out),
        Value::Object(object) => object,
        other => return Err(type_error("response", other, "object")),
    };
    apply_string(object, "recommendation_id", &mut out.recommendation_id)?;
    apply_bool(object, "measured", &mut out.measured)?;
    apply_bool(object, "zero_retention", &mut out.zero_retention)?;
    apply_bool(
        object,
        "classification_quota_exceeded",
        &mut out.classification_quota_exceeded,
    )?;
    for value in members(object, "classification") {
        match value {
            Value::Object(classification) => {
                apply_string(classification, "method", &mut out.classification_method)?;
            }
            Value::Null => {}
            other => return Err(type_error("classification", other, "object")),
        }
    }
    for value in members(object, "recommendations") {
        match value {
            Value::Array(items) => {
                out.chain = items.iter().map(chain_entry).collect::<Result<_, _>>()?;
            }
            Value::Null => out.chain.clear(),
            other => return Err(type_error("recommendations", other, "array")),
        }
    }
    Ok(out)
}

fn chain_entry(value: &Value) -> Result<ChainEntry, String> {
    let mut entry = ChainEntry::default();
    let object = match value {
        Value::Object(object) => object,
        Value::Null => return Ok(entry),
        other => return Err(type_error("recommendations[]", other, "object")),
    };
    apply_string(object, "provider", &mut entry.provider)?;
    apply_string(object, "model", &mut entry.model)?;
    apply_string(object, "endpoint", &mut entry.endpoint)?;
    apply_string(object, "column_id", &mut entry.column_id)?;
    let mut retention = String::new();
    apply_string(object, "retention_mode", &mut retention)?;
    entry.retention_mode = RetentionMode::parse(&retention);
    for value in members(object, "settings") {
        match value {
            Value::Object(settings) => entry.settings.extend(
                settings
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone())),
            ),
            Value::Null => entry.settings.clear(),
            other => return Err(type_error("settings", other, "object")),
        }
    }
    let mut aliases = BTreeMap::new();
    apply_string_map(object, "execution_aliases", &mut aliases)?;
    entry.execution_aliases = aliases;
    let mut alias_retention = BTreeMap::new();
    apply_string_map(object, "execution_alias_retention", &mut alias_retention)?;
    entry.execution_alias_retention = alias_retention
        .into_iter()
        .map(|(alias, mode)| (alias, RetentionMode::parse(&mode)))
        .collect();
    Ok(entry)
}

/// A Go `map[string]string`: members merge in, `null` clears, a `null` element is "".
fn apply_string_map(
    object: &Map<String, Value>,
    field: &str,
    target: &mut BTreeMap<String, String>,
) -> Result<(), String> {
    for value in members(object, field) {
        match value {
            Value::Object(map) => {
                for (key, element) in map {
                    let element = match element {
                        Value::String(text) => text.clone(),
                        Value::Null => String::new(),
                        other => return Err(type_error(field, other, "string values")),
                    };
                    target.insert(key.clone(), element);
                }
            }
            Value::Null => target.clear(),
            other => return Err(type_error(field, other, "object")),
        }
    }
    Ok(())
}
