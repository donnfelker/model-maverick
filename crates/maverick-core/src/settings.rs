//! D14: applying a chain entry's `settings` to the request (spec v1 §7.4 "Chain entries
//! carry settings"; `parity/DIVERGENCES.md` D14).
//!
//! A column is (provider, model, endpoint, settings). MM components apply the entry's
//! settings to each attempt unless the caller selects `X-Maverick-Settings: caller`, and record
//! `settings_applied` in telemetry. Settings are applied before normalization, so the
//! final value is always one the model accepts.
//!
//! This is the one settings-to-wire-field table:
//!
//! | setting | chat completions | messages |
//! |---|---|---|
//! | `reasoning` (string effort tier) | `reasoning_effort` | `output_config.effort` |
//! | `thinking` (bool or object) | not defined: untouched | `thinking`: `true` is `{"type":"adaptive"}`, `false` is `{"type":"disabled"}`, an object is sent as stored |
//!
//! A setting the table does not define for a format, or a value of the wrong type, is
//! left out; the caller of [`wire_settings`] logs it at debug.

use serde_json::{Map, Value, json};

use crate::normalize::{REASONING_EFFORT, WireFormat, WirePath, wire_path};

/// Whether to apply the recommended entry's settings (D14, D45).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsMode {
    /// Apply the entry's settings when no option is supplied.
    Apply,
    /// Preserve the caller's settings.
    Caller,
}

impl SettingsMode {
    /// Read X-Maverick-Settings, or the equivalent SDK per-call option.
    pub fn from_header(value: Option<&str>) -> Result<Self, InvalidSettingsMode> {
        match value {
            None => Ok(Self::Apply),
            Some("caller") => Ok(Self::Caller),
            Some(_) => Err(InvalidSettingsMode),
        }
    }
}

/// An unsupported settings option.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("X-Maverick-Settings must be caller")]
pub struct InvalidSettingsMode;

/// One wire field an entry's settings set.
#[derive(Debug, Clone, PartialEq)]
pub struct WireSetting {
    /// The settings key it came from.
    pub key: String,
    /// Where it goes.
    pub path: WirePath,
    /// The value to write.
    pub value: Value,
}

/// The wire fields of `settings` for a format, in settings-key order, and the keys that
/// have no wire field there (or an unusable value).
#[must_use]
pub fn wire_settings(
    settings: &Map<String, Value>,
    format: WireFormat,
) -> (Vec<WireSetting>, Vec<String>) {
    let mut fields = Vec::new();
    let mut skipped = Vec::new();
    for (key, value) in settings {
        let mapped = match (key.as_str(), value) {
            ("reasoning", Value::String(_)) => {
                wire_path(REASONING_EFFORT, format).map(|path| (path, value.clone()))
            }
            ("thinking", _) if format == WireFormat::Messages => {
                thinking(value).map(|thinking| (WirePath::Top("thinking"), thinking))
            }
            _ => None,
        };
        match mapped {
            Some((path, value)) => fields.push(WireSetting {
                key: key.clone(),
                path,
                value,
            }),
            None => skipped.push(key.clone()),
        }
    }
    (fields, skipped)
}

/// The Anthropic `thinking` object for a `thinking` setting.
fn thinking(value: &Value) -> Option<Value> {
    match value {
        Value::Bool(true) => Some(json!({"type": "adaptive"})),
        Value::Bool(false) => Some(json!({"type": "disabled"})),
        Value::Object(_) => Some(value.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(value: Value) -> Map<String, Value> {
        value.as_object().expect("object").clone()
    }

    fn paths(fields: &[WireSetting]) -> Vec<(WirePath, Value)> {
        fields.iter().map(|f| (f.path, f.value.clone())).collect()
    }

    /// OpenAI (and Google and xAI through the OpenAI-compatible chat format).
    #[test]
    fn chat_completions_table() {
        let (fields, skipped) = wire_settings(
            &settings(json!({"reasoning": "high", "thinking": true, "verbosity": "low"})),
            WireFormat::ChatCompletions,
        );
        assert_eq!(
            paths(&fields),
            [(WirePath::Top("reasoning_effort"), json!("high"))]
        );
        assert_eq!(skipped, ["thinking", "verbosity"]);
        assert_eq!(fields[0].key, "reasoning");
    }

    /// Anthropic.
    #[test]
    fn messages_table() {
        let (fields, skipped) = wire_settings(
            &settings(json!({"reasoning": "max", "thinking": true})),
            WireFormat::Messages,
        );
        assert_eq!(
            paths(&fields),
            [
                (
                    WirePath::Nested {
                        object: "output_config",
                        field: "effort"
                    },
                    json!("max")
                ),
                (WirePath::Top("thinking"), json!({"type": "adaptive"})),
            ]
        );
        assert!(skipped.is_empty());
        let (fields, _) =
            wire_settings(&settings(json!({"thinking": false})), WireFormat::Messages);
        assert_eq!(fields[0].value, json!({"type": "disabled"}));
        let explicit = json!({"type": "enabled", "budget_tokens": 4096});
        let (fields, _) = wire_settings(
            &settings(json!({"thinking": explicit.clone()})),
            WireFormat::Messages,
        );
        assert_eq!(fields[0].value, explicit);
    }

    #[test]
    fn unusable_values_are_skipped() {
        let (fields, skipped) = wire_settings(
            &settings(json!({"reasoning": 3, "thinking": "yes"})),
            WireFormat::Messages,
        );
        assert!(fields.is_empty());
        assert_eq!(skipped, ["reasoning", "thinking"]);
        let (fields, skipped) = wire_settings(&Map::new(), WireFormat::ChatCompletions);
        assert!(fields.is_empty() && skipped.is_empty());
    }
}

#[cfg(test)]
mod mode_tests {
    use super::*;

    /// D14, D45: the opt-out is separate from the model and exact.
    #[test]
    fn settings_header_is_exact() {
        assert_eq!(SettingsMode::from_header(None), Ok(SettingsMode::Apply));
        assert_eq!(
            SettingsMode::from_header(Some("caller")),
            Ok(SettingsMode::Caller)
        );
        for value in ["", "apply", "CALLER", " caller", "caller ", "caller,caller"] {
            assert_eq!(
                SettingsMode::from_header(Some(value)),
                Err(InvalidSettingsMode)
            );
        }
    }
}
