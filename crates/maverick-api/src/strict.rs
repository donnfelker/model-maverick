//! Go-strict request decoding (reference Part 5 §4) on [`gojson`] values.
//!
//! The whole body is scanned first, so syntax errors win. Keys then match struct fields
//! case-insensitively, the last duplicate wins, JSON `null` leaves a pointer, slice, or
//! map field absent, and the first unknown field or type mismatch in document order
//! fails the decode. After `malformed request body: ` the error text is Go-like but not a
//! parity target (D25).

use axum::http::StatusCode;
use maverick_domain::gofmt::quote;
use maverick_domain::gojson::{self, JsonValue};

use crate::{ApiError, ErrorCode};

/// A 422 `invalid_request` with no details.
pub(crate) fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        ErrorCode::InvalidRequest,
        message,
    )
}

/// `<field> is required`, details `{"field"}`.
pub(crate) fn required_field(field: &str) -> ApiError {
    invalid(format!("{field} is required")).detail("field", field)
}

/// Reads one JSON value from a body the way Go's strict decoder does: the body is
/// required, and a second value after it is trailing content (QUIRK Q2: a trailing `}`
/// or `]` is not detected).
pub(crate) fn body_value(body: &[u8]) -> Result<JsonValue, ApiError> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Err(invalid("request body is required"));
    }
    // Go replaces invalid UTF-8 inside strings with U+FFFD.
    let text = String::from_utf8_lossy(body);
    let (value, rest) = gojson::decode_one(&text).map_err(malformed)?;
    let trailing = rest
        .trim_start_matches([' ', '\t', '\n', '\r'])
        .bytes()
        .next();
    if trailing.is_some_and(|b| b != b'}' && b != b']') {
        return Err(invalid("request body has trailing content"));
    }
    Ok(value)
}

/// `malformed request body: <decode error>`.
pub(crate) fn malformed(error: impl std::fmt::Display) -> ApiError {
    invalid(format!("malformed request body: {error}"))
}

/// Go's `UnmarshalTypeError` text for a struct field.
pub(crate) fn type_error(value: &JsonValue, strukt: &str, path: &str, go_type: &str) -> String {
    let kind = match value {
        JsonValue::Number(literal) => format!("number {literal}"),
        other => other.kind_name().to_owned(),
    };
    format!("json: cannot unmarshal {kind} into Go struct field {strukt}.{path} of type {go_type}")
}

/// `json: unknown field "<key>"`.
pub(crate) fn unknown(key: &str) -> String {
    format!("json: unknown field {}", quote(key))
}

/// The members of an object in document order; `None` for `null`. `path` is empty for
/// the top-level value.
pub(crate) fn members<'a>(
    value: &'a JsonValue,
    strukt: &str,
    path: &str,
) -> Result<Option<&'a [(String, JsonValue)]>, String> {
    match value {
        JsonValue::Null => Ok(None),
        JsonValue::Object(members) => Ok(Some(members)),
        other if path.is_empty() => Err(format!(
            "json: cannot unmarshal {} into Go value of type api.{strukt}",
            other.kind_name()
        )),
        other => Err(type_error(other, strukt, path, &format!("api.{strukt}"))),
    }
}

/// The items of an array; `None` for `null`.
pub(crate) fn items<'a>(
    value: &'a JsonValue,
    strukt: &str,
    path: &str,
    go_type: &str,
) -> Result<Option<&'a [JsonValue]>, String> {
    match value {
        JsonValue::Null => Ok(None),
        JsonValue::Array(items) => Ok(Some(items)),
        other => Err(type_error(other, strukt, path, go_type)),
    }
}

/// A Go `string` field: `null` leaves `out` unchanged.
pub(crate) fn string(
    value: &JsonValue,
    strukt: &str,
    path: &str,
    out: &mut String,
) -> Result<(), String> {
    match value {
        JsonValue::Null => Ok(()),
        JsonValue::String(s) => {
            s.clone_into(out);
            Ok(())
        }
        other => Err(type_error(other, strukt, path, "string")),
    }
}

/// A Go `*string` field: `null` clears it.
pub(crate) fn optional_string(
    value: &JsonValue,
    strukt: &str,
    path: &str,
) -> Result<Option<String>, String> {
    match value {
        JsonValue::Null => Ok(None),
        JsonValue::String(s) => Ok(Some(s.clone())),
        other => Err(type_error(other, strukt, path, "string")),
    }
}

/// A Go `[]string`: `null` is absent, and a `null` element is `""` (Go leaves the zero
/// value).
pub(crate) fn strings(
    value: &JsonValue,
    strukt: &str,
    path: &str,
) -> Result<Option<Vec<String>>, String> {
    let Some(items) = items(value, strukt, path, "[]string")? else {
        return Ok(None);
    };
    items
        .iter()
        .map(|item| match item {
            JsonValue::Null => Ok(String::new()),
            JsonValue::String(s) => Ok(s.clone()),
            other => Err(type_error(other, strukt, path, "string")),
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

/// A Go `bool` field: `null` leaves `out` unchanged.
pub(crate) fn boolean(
    value: &JsonValue,
    strukt: &str,
    path: &str,
    out: &mut bool,
) -> Result<(), String> {
    match value {
        JsonValue::Null => Ok(()),
        JsonValue::Bool(b) => {
            *out = *b;
            Ok(())
        }
        other => Err(type_error(other, strukt, path, "bool")),
    }
}

/// A Go integer pointer field: a literal with a fraction or exponent, or one outside
/// `min..=max`, is a type error (QUIRK Q3). `null` clears it.
pub(crate) fn integer(
    value: &JsonValue,
    strukt: &str,
    path: &str,
    go_type: &str,
    min: i64,
    max: i64,
) -> Result<Option<i64>, String> {
    match value {
        JsonValue::Null => Ok(None),
        JsonValue::Number(literal) => literal
            .parse::<i64>()
            .ok()
            .filter(|n| (min..=max).contains(n))
            .map(Some)
            .ok_or_else(|| type_error(value, strukt, path, go_type)),
        other => Err(type_error(other, strukt, path, go_type)),
    }
}

/// A Go `*float64` field. A literal out of the float64 range is a type error. `null`
/// clears it.
pub(crate) fn float64(value: &JsonValue, strukt: &str, path: &str) -> Result<Option<f64>, String> {
    match value {
        JsonValue::Null => Ok(None),
        JsonValue::Number(literal) => match literal.parse::<f64>() {
            Ok(x) if x.is_finite() => Ok(Some(x)),
            _ => Err(type_error(value, strukt, path, "float64")),
        },
        other => Err(type_error(other, strukt, path, "float64")),
    }
}

/// A Go `*float32` field: the literal rounded to the nearest float32, as Go's
/// `ParseFloat(s, 32)` does; out of the float32 range is a type error. `null` clears it.
pub(crate) fn float32(value: &JsonValue, strukt: &str, path: &str) -> Result<Option<f32>, String> {
    match value {
        JsonValue::Null => Ok(None),
        JsonValue::Number(literal) => match literal.parse::<f32>() {
            Ok(x) if x.is_finite() => Ok(Some(x)),
            _ => Err(type_error(value, strukt, path, "float32")),
        },
        other => Err(type_error(other, strukt, path, "float32")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn number(literal: &str) -> JsonValue {
        JsonValue::Number(literal.to_owned())
    }

    #[test]
    fn body_rules() {
        let message = |body: &str| body_value(body.as_bytes()).unwrap_err().message;
        assert_eq!(message(" "), "request body is required");
        assert_eq!(message("{"), "malformed request body: unexpected EOF");
        assert_eq!(message("{}{}"), "request body has trailing content");
        assert!(body_value(b"{}}").is_ok(), "QUIRK Q2");
        assert!(body_value(b" {} \n").is_ok());
    }

    #[test]
    fn numbers_follow_go_types() {
        assert_eq!(integer(&number("5"), "S", "p", "int32", 0, 9), Ok(Some(5)));
        for bad in ["5.0", "1e1", "10", "-1"] {
            assert_eq!(
                integer(&number(bad), "S", "p", "int32", 0, 9),
                Err(format!(
                    "json: cannot unmarshal number {bad} into Go struct field S.p of type int32"
                ))
            );
        }
        assert_eq!(float64(&number("0.25"), "S", "p"), Ok(Some(0.25)));
        assert!(float64(&number("1e400"), "S", "p").is_err());
        assert_eq!(float32(&number("0.1505"), "S", "p"), Ok(Some(0.1505f32)));
        assert!(float32(&number("1e39"), "S", "p").is_err());
        assert_eq!(float32(&JsonValue::Null, "S", "p"), Ok(None));
        assert_eq!(
            float32(&JsonValue::Bool(true), "S", "p"),
            Err("json: cannot unmarshal bool into Go struct field S.p of type float32".to_owned())
        );
    }

    #[test]
    fn nulls_and_kinds() {
        let mut s = "kept".to_owned();
        string(&JsonValue::Null, "S", "p", &mut s).unwrap();
        assert_eq!(s, "kept");
        let mut b = true;
        boolean(&JsonValue::Null, "S", "p", &mut b).unwrap();
        assert!(b);
        boolean(&JsonValue::Bool(false), "S", "p", &mut b).unwrap();
        assert!(!b);
        assert_eq!(optional_string(&JsonValue::Null, "S", "p"), Ok(None));
        assert_eq!(
            strings(
                &JsonValue::Array(vec![JsonValue::Null, JsonValue::String("a".into())]),
                "S",
                "p"
            ),
            Ok(Some(vec![String::new(), "a".to_owned()]))
        );
        assert_eq!(
            members(&JsonValue::Array(Vec::new()), "Body", ""),
            Err("json: cannot unmarshal array into Go value of type api.Body".to_owned())
        );
        assert_eq!(
            members(&number("1"), "Inner", "x.y"),
            Err(
                "json: cannot unmarshal number 1 into Go struct field Inner.x.y of type api.Inner"
                    .to_owned()
            )
        );
        assert_eq!(unknown("color"), "json: unknown field \"color\"");
        assert_eq!(
            required_field("pools[0].name").message,
            "pools[0].name is required"
        );
    }
}
