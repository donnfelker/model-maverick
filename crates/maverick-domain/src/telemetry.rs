//! Telemetry items and outcome derivation (spec §9.10, reference Part 4 §6.3).
//!
//! `POST /v1/telemetry` is best-effort by contract: each item decodes alone, and any
//! malformed item is dropped and counted, never a 4xx for the batch. The handler checks
//! ownership (a foreign recommendation id drops like a missing one) and writes one
//! `outcomes` row per accepted item.

use crate::gojson::{self, JsonValue};

/// The most items one batch may carry.
pub const MAX_ITEMS: usize = 500;
/// `error_class` values (exact).
pub const ERROR_CLASSES: [&str; 4] = ["timeout", "connect_error", "provider_error", "other"];
/// `endpoint_used` values (exact).
pub const ENDPOINTS: [&str; 5] = ["direct", "vertex", "bedrock", "azure", "openrouter"];
/// `endpoint_used` when the item has none.
pub const DEFAULT_ENDPOINT: &str = "direct";

/// Outcome values (exact).
pub mod outcome {
    pub const SUCCESS: &str = "success";
    pub const PARSE_FAILURE: &str = "parse_failure";
    pub const QUALITY_INSUFFICIENT: &str = "quality_insufficient";
    pub const LATENCY_EXCEEDED: &str = "latency_exceeded";
    pub const PROVIDER_ERROR: &str = "provider_error";
    /// A 2xx with no check (D16): availability evidence only, never quality evidence.
    pub const COMPLETED_UNCHECKED: &str = "completed_unchecked";
    pub const OTHER: &str = "other";
}

/// One accepted telemetry item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelemetryItem {
    pub recommendation_id: String,
    pub model_used: String,
    pub status: i64,
    pub latency_ms: Option<i64>,
    pub tokens_in: Option<i64>,
    pub tokens_out: Option<i64>,
    pub finish_reason: Option<String>,
    pub error_class: Option<String>,
    /// Three-valued (D16): `None` means no check ran.
    pub check_pass: Option<bool>,
    /// A column of the outcome row, not part of `details`.
    pub endpoint_used: String,
    /// Whether the component applied the entry's settings (D14).
    pub settings_applied: Option<bool>,
}

const FIELDS: [&str; 11] = [
    "recommendation_id",
    "model_used",
    "status",
    "latency_ms",
    "tokens_in",
    "tokens_out",
    "finish_reason",
    "error_class",
    "check_pass",
    "endpoint_used",
    "settings_applied",
];

#[derive(Default)]
struct Raw {
    recommendation_id: String,
    model_used: String,
    status: Option<i64>,
    latency_ms: Option<i64>,
    tokens_in: Option<i64>,
    tokens_out: Option<i64>,
    finish_reason: Option<String>,
    error_class: Option<String>,
    check_pass: Option<bool>,
    endpoint_used: Option<String>,
    settings_applied: Option<bool>,
}

/// Why an item was dropped. Dropped items are only counted; the reason is for tests and
/// debug logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dropped(pub String);

fn int(value: &JsonValue, field: &str) -> Result<Option<i64>, Dropped> {
    match value {
        JsonValue::Null => Ok(None),
        JsonValue::Number(literal) => literal
            .parse::<i64>()
            .map(Some)
            .map_err(|_| Dropped(format!("{field}: {literal} is not an int"))),
        other => Err(Dropped(format!(
            "{field}: {} is not an int",
            other.kind_name()
        ))),
    }
}

fn string(value: &JsonValue, field: &str) -> Result<Option<String>, Dropped> {
    match value {
        JsonValue::Null => Ok(None),
        JsonValue::String(s) => Ok(Some(s.clone())),
        other => Err(Dropped(format!(
            "{field}: {} is not a string",
            other.kind_name()
        ))),
    }
}

fn boolean(value: &JsonValue, field: &str) -> Result<Option<bool>, Dropped> {
    match value {
        JsonValue::Null => Ok(None),
        JsonValue::Bool(b) => Ok(Some(*b)),
        other => Err(Dropped(format!(
            "{field}: {} is not a bool",
            other.kind_name()
        ))),
    }
}

/// Go's lenient `json.Unmarshal` into the item struct: unknown fields ignored, keys
/// matched case-insensitively, the last duplicate wins, `null` clears an optional
/// field and leaves a string field unchanged, and trailing data fails.
fn decode(raw: &str) -> Result<Raw, Dropped> {
    let value = gojson::unmarshal(raw).map_err(|e| Dropped(e.0))?;
    let mut item = Raw::default();
    let members = match &value {
        JsonValue::Null => return Ok(item),
        JsonValue::Object(members) => members,
        other => {
            return Err(Dropped(format!(
                "item is {}, not an object",
                other.kind_name()
            )));
        }
    };
    for (key, value) in members {
        let Some(field) = gojson::match_field(key, &FIELDS) else {
            continue;
        };
        match field {
            "recommendation_id" => {
                if let Some(s) = string(value, field)? {
                    item.recommendation_id = s;
                }
            }
            "model_used" => {
                if let Some(s) = string(value, field)? {
                    item.model_used = s;
                }
            }
            "status" => item.status = int(value, field)?,
            "latency_ms" => item.latency_ms = int(value, field)?,
            "tokens_in" => item.tokens_in = int(value, field)?,
            "tokens_out" => item.tokens_out = int(value, field)?,
            "finish_reason" => item.finish_reason = string(value, field)?,
            "error_class" => item.error_class = string(value, field)?,
            "check_pass" => item.check_pass = boolean(value, field)?,
            "endpoint_used" => item.endpoint_used = string(value, field)?,
            _ => item.settings_applied = boolean(value, field)?,
        }
    }
    Ok(item)
}

/// Decodes and validates one raw item. Required: non-empty `recommendation_id` and
/// `model_used`, and `status >= 0`. `latency_ms`, `tokens_in`, and `tokens_out` must be
/// `>= 0` when present; `error_class` and `endpoint_used` must be in their enums.
pub fn decode_item(raw: &str) -> Result<TelemetryItem, Dropped> {
    let item = decode(raw)?;
    if item.recommendation_id.is_empty() {
        return Err(Dropped("recommendation_id is required".to_owned()));
    }
    if item.model_used.is_empty() {
        return Err(Dropped("model_used is required".to_owned()));
    }
    let status = item
        .status
        .ok_or_else(|| Dropped("status is required".to_owned()))?;
    for (field, value) in [
        ("status", Some(status)),
        ("latency_ms", item.latency_ms),
        ("tokens_in", item.tokens_in),
        ("tokens_out", item.tokens_out),
    ] {
        if value.is_some_and(|v| v < 0) {
            return Err(Dropped(format!("{field} must be >= 0")));
        }
    }
    if let Some(class) = &item.error_class
        && !ERROR_CLASSES.contains(&class.as_str())
    {
        return Err(Dropped(format!("error_class {class:?} is not allowed")));
    }
    let endpoint_used = item
        .endpoint_used
        .unwrap_or_else(|| DEFAULT_ENDPOINT.to_owned());
    if !ENDPOINTS.contains(&endpoint_used.as_str()) {
        return Err(Dropped(format!(
            "endpoint_used {endpoint_used:?} is not allowed"
        )));
    }
    Ok(TelemetryItem {
        recommendation_id: item.recommendation_id,
        model_used: item.model_used,
        status,
        latency_ms: item.latency_ms,
        tokens_in: item.tokens_in,
        tokens_out: item.tokens_out,
        finish_reason: item.finish_reason,
        error_class: item.error_class,
        check_pass: item.check_pass,
        endpoint_used,
        settings_applied: item.settings_applied,
    })
}

impl TelemetryItem {
    /// The outcome, first matching rule wins: a timeout is `latency_exceeded`; a connect
    /// or provider error, status 0, 429, or 5xx is `provider_error`; a 2xx is
    /// `parse_failure`, `success`, or (no check ran, D16) `completed_unchecked`;
    /// anything else is `other`.
    pub fn outcome(&self) -> &'static str {
        let class = self.error_class.as_deref();
        if class == Some("timeout") {
            return outcome::LATENCY_EXCEEDED;
        }
        if matches!(class, Some("connect_error" | "provider_error"))
            || self.status == 0
            || self.status == 429
            || self.status >= 500
        {
            return outcome::PROVIDER_ERROR;
        }
        if (200..=299).contains(&self.status) {
            return match self.check_pass {
                Some(false) => outcome::PARSE_FAILURE,
                Some(true) => outcome::SUCCESS,
                None => outcome::COMPLETED_UNCHECKED,
            };
        }
        outcome::OTHER
    }

    /// The outcome row's `details`: `status` plus every optional field the item carried
    /// (`endpoint_used` is a column), keys sorted as Go's `json.Marshal` of a map writes
    /// them.
    pub fn details_json(&self) -> String {
        let number = |n: i64| JsonValue::Number(n.to_string());
        let mut members: Vec<(&str, JsonValue)> = vec![("status", number(self.status))];
        let optional = [
            ("check_pass", self.check_pass.map(JsonValue::Bool)),
            (
                "error_class",
                self.error_class.clone().map(JsonValue::String),
            ),
            (
                "finish_reason",
                self.finish_reason.clone().map(JsonValue::String),
            ),
            ("latency_ms", self.latency_ms.map(number)),
            (
                "settings_applied",
                self.settings_applied.map(JsonValue::Bool),
            ),
            ("tokens_in", self.tokens_in.map(number)),
            ("tokens_out", self.tokens_out.map(number)),
        ];
        for (key, value) in optional {
            if let Some(value) = value {
                members.push((key, value));
            }
        }
        members.sort_by(|a, b| a.0.cmp(b.0));
        gojson::marshal(&JsonValue::Object(
            members
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v))
                .collect(),
        ))
    }
}
