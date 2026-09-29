//! `Assemble(task, column)`: the provider request for one benchmark run (reference
//! Part 2 §5.1).
//!
//! The prompt bytes are identical for every column and every call. The system slot is
//! always empty for benchmark runs; only the model and the column's settings differ.

use maverick_domain::gojson::{self, JsonValue};
use maverick_domain::{gostrconv, tasks};
use maverick_providers::CompletionRequest;
use serde_json::{Map, Value};

/// The task fields assembly reads (a `tasks` row).
#[derive(Debug, Clone, Copy)]
pub struct BenchTask<'a> {
    pub id: &'a str,
    pub prompt: &'a str,
    /// The `attachments` column as JSON text (empty means none).
    pub attachments_json: &'a str,
    pub temperature: f32,
    pub max_tokens: i32,
}

/// The column fields assembly reads (a `model_columns` row).
#[derive(Debug, Clone, Copy)]
pub struct BenchColumn<'a> {
    pub id: &'a str,
    pub model_id: &'a str,
    /// The `settings` column as JSON text (empty means none).
    pub settings_json: &'a str,
}

/// An assembly failure. The text starts with `task <id>: parsing attachments: ` or
/// `column <id>: parsing settings: `; the rest is library wording (D36).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssembleError(pub String);

impl std::fmt::Display for AssembleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AssembleError {}

/// Builds the request: `model` from the column, empty `system`, the task's prompt text,
/// temperature, and max_tokens, and the column's settings.
pub fn assemble(
    task: &BenchTask<'_>,
    column: &BenchColumn<'_>,
) -> Result<CompletionRequest, AssembleError> {
    let attachments = tasks::parse_attachments(task.attachments_json)
        .map_err(|e| AssembleError(format!("task {}: {e}", task.id)))?;
    let prompt = tasks::prompt_text(task.prompt, &attachments);
    let settings = parse_settings(column.settings_json)
        .map_err(|e| AssembleError(format!("column {}: parsing settings: {e}", column.id)))?;
    Ok(CompletionRequest {
        model: column.model_id.to_owned(),
        system: String::new(),
        prompt,
        temperature: task.temperature,
        // The column is `int4 > 0` in the schema; a negative value cannot reach here.
        max_tokens: usize::try_from(task.max_tokens).unwrap_or(0),
        settings,
    })
}

/// The `settings` column decoded like Go's `json.Unmarshal` into `map[string]any`:
/// empty text, `null`, and `{}` are no settings; numbers become `f64`; duplicate keys
/// keep the last; any non-object is an error.
fn parse_settings(raw: &str) -> Result<Map<String, Value>, String> {
    if raw.is_empty() {
        return Ok(Map::new());
    }
    match gojson::unmarshal(raw).map_err(|e| e.0)? {
        JsonValue::Null => Ok(Map::new()),
        JsonValue::Object(members) => {
            let mut map = Map::new();
            for (key, value) in &members {
                map.insert(key.clone(), generic(value)?);
            }
            Ok(map)
        }
        other => Err(format!(
            "json: cannot unmarshal {} into Go value of type map[string]interface {{}}",
            other.kind_name()
        )),
    }
}

/// A JSON value as Go's `any` holds it: every number a `float64`.
fn generic(value: &JsonValue) -> Result<Value, String> {
    Ok(match value {
        JsonValue::Null => Value::Null,
        JsonValue::Bool(b) => Value::Bool(*b),
        JsonValue::Number(literal) => {
            let number = gostrconv::parse_float(literal).map_err(|_| {
                format!("json: cannot unmarshal number {literal} into Go value of type float64")
            })?;
            Value::from(number)
        }
        JsonValue::String(s) => Value::String(s.clone()),
        JsonValue::Array(items) => {
            Value::Array(items.iter().map(generic).collect::<Result<_, _>>()?)
        }
        JsonValue::Object(members) => {
            let mut map = Map::new();
            for (key, value) in members {
                map.insert(key.clone(), generic(value)?);
            }
            Value::Object(map)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_edges() {
        assert_eq!(parse_settings(""), Ok(Map::new()));
        assert_eq!(parse_settings("null"), Ok(Map::new()));
        assert_eq!(parse_settings("{}"), Ok(Map::new()));
        let settings = parse_settings(r#"{"a":1,"b":{"c":[2,"x",null,true]},"a":3}"#).unwrap();
        assert_eq!(
            Value::Object(settings),
            serde_json::json!({"a": 3.0, "b": {"c": [2.0, "x", null, true]}})
        );
        assert_eq!(
            parse_settings("[]").unwrap_err(),
            "json: cannot unmarshal array into Go value of type map[string]interface {}"
        );
        assert_eq!(
            parse_settings("\"x\"").unwrap_err(),
            "json: cannot unmarshal string into Go value of type map[string]interface {}"
        );
        assert_eq!(
            parse_settings(r#"{"a":1e400}"#).unwrap_err(),
            "json: cannot unmarshal number 1e400 into Go value of type float64"
        );
        assert_eq!(
            parse_settings(r#"{"a":[1e400]}"#).unwrap_err(),
            "json: cannot unmarshal number 1e400 into Go value of type float64"
        );
        assert_eq!(
            parse_settings(r#"{"a":{"b":1e400}}"#).unwrap_err(),
            "json: cannot unmarshal number 1e400 into Go value of type float64"
        );
    }

    #[test]
    fn negative_max_tokens_is_zero() {
        let task = BenchTask {
            id: "t",
            prompt: "p",
            attachments_json: "",
            temperature: 0.5,
            max_tokens: -1,
        };
        let column = BenchColumn {
            id: "c",
            model_id: "m",
            settings_json: "",
        };
        let request = assemble(&task, &column).unwrap();
        assert_eq!(request.max_tokens, 0);
        assert_eq!(request.temperature, 0.5);
        assert!(request.settings.is_empty());
    }
}
