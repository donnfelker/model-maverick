//! D29: per-model parameter constraints and the normalization of caller values (spec v1
//! §8.1 "Proxy behavior"; reference Part 6 §10; `parity/DIVERGENCES.md` D29).
//!
//! The model catalog stores `param_constraints` per column as
//! `{"<field>": {"allowed": [...], "order": [...]}}`, keyed by a surface-neutral field
//! name such as `reasoning_effort`, and every chain entry carries them. A caller value
//! the routed model does not accept becomes the nearest accepted value. When an entry has
//! no constraint for a field, Go's two fixed mappings apply instead. Normalization never
//! adds a field the caller did not send.
//!
//! The proxy and the SDK cores share this module, so a value is normalized the same way
//! whichever MM component executes the call.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

/// The surface-neutral name of the reasoning-effort parameter.
pub const REASONING_EFFORT: &str = "reasoning_effort";

/// The request wire format a parameter is written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireFormat {
    /// OpenAI chat completions (also Google and xAI through their OpenAI-compatible APIs).
    ChatCompletions,
    /// Anthropic Messages.
    Messages,
}

impl WireFormat {
    /// The name used in logs.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat_completions",
            Self::Messages => "messages",
        }
    }
}

/// Where a parameter lives in a request body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WirePath {
    /// A top-level member.
    Top(&'static str),
    /// A member of a top-level object member.
    Nested {
        object: &'static str,
        field: &'static str,
    },
}

/// The wire location of a surface-neutral parameter in a format, if the format has one.
///
/// | field | chat completions | messages |
/// |---|---|---|
/// | `reasoning_effort` | `reasoning_effort` | `output_config.effort` |
#[must_use]
pub fn wire_path(field: &str, format: WireFormat) -> Option<WirePath> {
    match (field, format) {
        (REASONING_EFFORT, WireFormat::ChatCompletions) => Some(WirePath::Top("reasoning_effort")),
        (REASONING_EFFORT, WireFormat::Messages) => Some(WirePath::Nested {
            object: "output_config",
            field: "effort",
        }),
        _ => None,
    }
}

/// The surface-neutral fields that have a wire location in some format.
pub const FIELDS: [&str; 1] = [REASONING_EFFORT];

/// Go's fixed mapping for a field that has no catalog constraint (reference Part 6 §10):
/// chat `reasoning_effort` `"xhigh"` to `"high"`, messages `output_config.effort`
/// `"xhigh"` to `"max"`. Exact case; only JSON strings.
#[must_use]
pub fn go_fallback(field: &str, format: WireFormat, value: &Value) -> Option<Value> {
    if field != REASONING_EFFORT || value.as_str() != Some("xhigh") {
        return None;
    }
    Some(Value::String(
        match format {
            WireFormat::ChatCompletions => "high",
            WireFormat::Messages => "max",
        }
        .to_owned(),
    ))
}

/// One field's constraint: the values the model accepts, and the order that defines
/// "nearest".
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ParamConstraint {
    /// The accepted values.
    pub allowed: Vec<Value>,
    /// Every known value, from lowest to highest.
    pub order: Vec<Value>,
}

/// An entry's constraints by surface-neutral field name.
pub type ParamConstraints = BTreeMap<String, ParamConstraint>;

/// Reads the catalog shape. A field whose value is not
/// `{"allowed": [...], "order": [...]}` with a non-empty `allowed` is skipped, so the Go
/// mapping still applies to it: a malformed constraint fails open to Go behavior, never
/// to a rejected request.
#[must_use]
pub fn parse_constraints(value: &Value) -> ParamConstraints {
    let Value::Object(fields) = value else {
        return ParamConstraints::new();
    };
    fields
        .iter()
        .filter_map(|(field, constraint)| {
            let constraint = constraint.as_object()?;
            let list = |key: &str| constraint.get(key).and_then(Value::as_array).cloned();
            let allowed = list("allowed").filter(|allowed| !allowed.is_empty())?;
            let order = list("order").unwrap_or_default();
            Some((field.clone(), ParamConstraint { allowed, order }))
        })
        .collect()
}

/// The value the routed model accepts for `value`, or `None` when the entry has no
/// constraint for `field` (then [`go_fallback`] applies).
///
/// - An accepted value is kept.
/// - A value that is in `order` but not accepted becomes the accepted value nearest to it
///   in `order`. On a tie the higher value wins, so the call never gets less than the
///   caller asked for when an equally close higher value exists.
/// - A value that is not in `order` cannot be placed, and it is kept as sent.
#[must_use]
pub fn normalize(field: &str, value: &Value, constraints: &ParamConstraints) -> Option<Value> {
    let constraint = constraints.get(field)?;
    if constraint.allowed.contains(value) {
        return Some(value.clone());
    }
    let Some(at) = constraint.order.iter().position(|known| known == value) else {
        return Some(value.clone());
    };
    let nearest = constraint
        .order
        .iter()
        .enumerate()
        .filter(|(_, candidate)| constraint.allowed.contains(candidate))
        .min_by_key(|(index, _)| (index.abs_diff(at), std::cmp::Reverse(*index)))
        .map(|(_, candidate)| candidate.clone());
    Some(nearest.unwrap_or_else(|| value.clone()))
}

/// The constraints of a recommend entry's `param_constraints` member (absent is none).
#[must_use]
pub fn constraints_of(entry: &Map<String, Value>) -> ParamConstraints {
    entry
        .get("param_constraints")
        .map(parse_constraints)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn effort(allowed: &[&str]) -> ParamConstraints {
        parse_constraints(&json!({
            "reasoning_effort": {
                "allowed": allowed,
                "order": ["minimal", "low", "medium", "high", "xhigh", "max"],
            }
        }))
    }

    fn norm(value: &str, constraints: &ParamConstraints) -> Option<Value> {
        normalize(REASONING_EFFORT, &json!(value), constraints)
    }

    #[test]
    fn nearest_accepted_value() {
        let c = effort(&["low", "medium", "high"]);
        assert_eq!(norm("medium", &c), Some(json!("medium")), "exact");
        assert_eq!(norm("xhigh", &c), Some(json!("high")), "nearest down");
        assert_eq!(
            norm("max", &c),
            Some(json!("high")),
            "nearest down, two steps"
        );
        assert_eq!(norm("minimal", &c), Some(json!("low")), "nearest up");
        assert_eq!(
            norm("turbo", &c),
            Some(json!("turbo")),
            "unknown value kept"
        );
        // A tie goes to the higher value.
        let c = effort(&["high", "max"]);
        assert_eq!(norm("xhigh", &c), Some(json!("max")));
        let c = effort(&["low", "high"]);
        assert_eq!(norm("medium", &c), Some(json!("high")));
    }

    #[test]
    fn no_constraint_means_none() {
        assert_eq!(norm("xhigh", &ParamConstraints::new()), None);
        assert_eq!(normalize("temperature", &json!(1), &effort(&["low"])), None);
    }

    #[test]
    fn malformed_constraints_are_skipped() {
        let parsed = parse_constraints(&json!({
            "reasoning_effort": {"allowed": [], "order": ["low"]},
            "a": "not an object",
            "b": {"order": ["x"]},
            "c": {"allowed": ["x"]},
        }));
        assert_eq!(
            parsed.keys().collect::<Vec<_>>(),
            ["c"],
            "empty or missing allowed lists are skipped; a missing order is empty"
        );
        assert_eq!(normalize("c", &json!("y"), &parsed), Some(json!("y")));
        assert!(parse_constraints(&json!([1])).is_empty());
        assert!(parse_constraints(&json!(null)).is_empty());
        let entry = json!({"param_constraints": {"reasoning_effort": {"allowed": ["low"]}}});
        assert_eq!(constraints_of(entry.as_object().unwrap()).len(), 1);
        assert!(constraints_of(&Map::new()).is_empty());
    }

    #[test]
    fn allowed_values_outside_the_order_are_never_picked_as_nearest() {
        let c = parse_constraints(&json!({
            "reasoning_effort": {"allowed": ["secret", "low"], "order": ["low", "medium", "high"]}
        }));
        assert_eq!(norm("high", &c), Some(json!("low")));
        let c = parse_constraints(&json!({
            "reasoning_effort": {"allowed": ["secret"], "order": ["low", "high"]}
        }));
        assert_eq!(
            norm("high", &c),
            Some(json!("high")),
            "nothing placeable: kept"
        );
    }

    #[test]
    fn go_fallback_and_wire_paths() {
        let x = json!("xhigh");
        assert_eq!(
            go_fallback(REASONING_EFFORT, WireFormat::ChatCompletions, &x),
            Some(json!("high"))
        );
        assert_eq!(
            go_fallback(REASONING_EFFORT, WireFormat::Messages, &x),
            Some(json!("max"))
        );
        assert_eq!(
            go_fallback(REASONING_EFFORT, WireFormat::Messages, &json!("XHIGH")),
            None
        );
        assert_eq!(
            go_fallback(REASONING_EFFORT, WireFormat::Messages, &json!("high")),
            None
        );
        assert_eq!(go_fallback("temperature", WireFormat::Messages, &x), None);
        assert_eq!(
            wire_path(REASONING_EFFORT, WireFormat::ChatCompletions),
            Some(WirePath::Top("reasoning_effort"))
        );
        assert_eq!(
            wire_path(REASONING_EFFORT, WireFormat::Messages),
            Some(WirePath::Nested {
                object: "output_config",
                field: "effort"
            })
        );
        assert_eq!(wire_path("temperature", WireFormat::Messages), None);
        assert_eq!(WireFormat::Messages.as_str(), "messages");
        assert_eq!(WireFormat::ChatCompletions.as_str(), "chat_completions");
    }
}
