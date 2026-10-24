//! The `POST /v1/recommend` body: Go-strict decoding (reference Part 5 §4) and the
//! handler and pipeline rules (§5.2, Part 4 §2.6 step 1), in the Go order.
//!
//! Decoding reproduces Go `encoding/json` with unknown fields disallowed: the whole
//! value is scanned first (syntax errors win), keys match case-insensitively, the last
//! duplicate wins, `null` leaves a field absent, and the first unknown field or type
//! mismatch in document order fails the request. After `malformed request body: ` the
//! error text is Go-like but not a parity target (D25).

use axum::http::StatusCode;
use maverick_domain::gojson::{self, JsonValue, match_field};
use maverick_domain::tasks::CAPABILITIES;

use crate::{ApiError, ErrorCode};

/// The prompt length limit, in Unicode code points, inclusive (spec §8.1).
pub(crate) const MAX_PROMPT_CHARS: usize = 500_000;

/// The affinity key length limit, in Unicode code points, inclusive (spec §9.2, D44).
pub(crate) const MAX_AFFINITY_KEY_CHARS: usize = 256;

/// The largest `affinity.context_tokens` (spec §9.2, D44).
pub(crate) const MAX_CONTEXT_TOKENS: u64 = 10_000_000;

/// The decoded request. Absent strings are empty, as in Go.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct RecommendRequest {
    pub workload: Option<Workload>,
    pub pool: String,
    pub allowed_models: Option<Vec<String>>,
    pub mode: String,
    pub constraints: Preferences,
    /// D44: the conversation affinity object, its members as sent. [`affinity`]
    /// validates them.
    pub affinity: Option<AffinityInput>,
}

/// The `affinity` members as decoded: `null` or absent is `None`. The values stay raw so
/// that a bad one is a field error, not a decode error.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct AffinityInput {
    pub key: Option<JsonValue>,
    pub context_tokens: Option<JsonValue>,
    pub cost_aware: Option<JsonValue>,
}

/// A validated `affinity` object (spec §9.2, D44).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Affinity {
    pub key: String,
    pub context_tokens: Option<u64>,
    pub cost_aware: Option<bool>,
}

/// The `workload` object. `prompt` keeps its presence: `""` is a present, empty prompt.
/// `projection_version` is the version of the chat projection a component built the prompt
/// with (D5); it joins the classification cache key.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Workload {
    pub capability: String,
    pub difficulty: Option<i64>,
    pub domain_tags: Option<Vec<String>>,
    pub prompt: Option<String>,
    pub projection_version: String,
}

/// The request `constraints` (preferences).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Preferences {
    pub max_cost_per_run: Option<f64>,
    pub max_latency_ms: Option<i32>,
}

/// A validated request's workload form.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Form {
    /// The capability, the difficulty (`None` when defaulted), and the domain tags.
    Declared {
        capability: String,
        difficulty: Option<i64>,
        domain_tags: Vec<String>,
    },
    /// A non-empty prompt within the length limit, and its projection version when the
    /// request names one (D5).
    Prompt {
        prompt: String,
        projection_version: Option<String>,
    },
}

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        ErrorCode::InvalidRequest,
        message,
    )
}

fn malformed(error: impl std::fmt::Display) -> ApiError {
    invalid(format!("malformed request body: {error}"))
}

/// Reads the body as Go's strict decoder does: required, one value, no trailing value.
pub(crate) fn decode(body: &[u8]) -> Result<RecommendRequest, ApiError> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Err(invalid("request body is required"));
    }
    // Go replaces invalid UTF-8 inside strings with U+FFFD.
    let text = String::from_utf8_lossy(body);
    let (value, rest) = gojson::decode_one(&text).map_err(malformed)?;
    // QUIRK Q2: a trailing `}` or `]` is not detected.
    let trailing = rest
        .trim_start_matches([' ', '\t', '\n', '\r'])
        .bytes()
        .next();
    if trailing.is_some_and(|b| b != b'}' && b != b']') {
        return Err(invalid("request body has trailing content"));
    }
    request(&value).map_err(malformed)
}

/// Go's `UnmarshalTypeError` text for a field.
fn type_error(value: &JsonValue, strukt: &str, path: &str, go_type: &str) -> String {
    let kind = match value {
        JsonValue::Number(literal) => format!("number {literal}"),
        other => other.kind_name().to_owned(),
    };
    format!("json: cannot unmarshal {kind} into Go struct field {strukt}.{path} of type {go_type}")
}

/// The members of an object in document order; `None` for `null`.
fn members<'a>(
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

fn unknown(key: &str) -> String {
    format!("json: unknown field {}", maverick_domain::gofmt::quote(key))
}

fn request(value: &JsonValue) -> Result<RecommendRequest, String> {
    const STRUCT: &str = "RecommendRequest";
    let mut out = RecommendRequest::default();
    let Some(members) = members(value, STRUCT, "")? else {
        return Ok(out);
    };
    for (key, value) in members {
        match match_field(
            key,
            &[
                "workload",
                "pool",
                "allowed_models",
                "mode",
                "constraints",
                "affinity",
            ],
        ) {
            Some("workload") => out.workload = workload(value, out.workload.take())?,
            Some("pool") => string(value, STRUCT, "pool", &mut out.pool)?,
            Some("allowed_models") => {
                out.allowed_models = strings(value, STRUCT, "allowed_models")?;
            }
            Some("mode") => string(value, STRUCT, "mode", &mut out.mode)?,
            Some("constraints") => preferences(value, &mut out.constraints)?,
            Some("affinity") => out.affinity = affinity_input(value, out.affinity.take())?,
            _ => return Err(unknown(key)),
        }
    }
    Ok(out)
}

/// `affinity` decodes like `workload`: `null` clears it, and a duplicate key updates only
/// the members it names. Unknown members fail strict decoding.
fn affinity_input(
    value: &JsonValue,
    existing: Option<AffinityInput>,
) -> Result<Option<AffinityInput>, String> {
    const STRUCT: &str = "AffinityRequest";
    let Some(members) = members(value, STRUCT, "affinity")? else {
        return Ok(None);
    };
    let mut out = existing.unwrap_or_default();
    let present = |value: &JsonValue| (*value != JsonValue::Null).then(|| value.clone());
    for (key, value) in members {
        match match_field(key, &["key", "context_tokens", "cost_aware"]) {
            Some("key") => out.key = present(value),
            Some("context_tokens") => out.context_tokens = present(value),
            Some("cost_aware") => out.cost_aware = present(value),
            _ => return Err(unknown(key)),
        }
    }
    Ok(Some(out))
}

/// The `affinity` rules (spec §9.2, D44): `key` is a string of 1 to 256 characters,
/// `context_tokens` an integer from 0 to 10,000,000, and `cost_aware` a boolean. Each
/// failure names its field.
pub(crate) fn affinity(request: &RecommendRequest) -> Result<Option<Affinity>, ApiError> {
    let Some(input) = &request.affinity else {
        return Ok(None);
    };
    let field = |name: &str, message: &str| invalid(message).detail("field", name);
    let key = match &input.key {
        None => return Err(field("affinity.key", "affinity.key is required")),
        Some(JsonValue::String(key))
            if (1..=MAX_AFFINITY_KEY_CHARS).contains(&key.chars().count()) =>
        {
            key.clone()
        }
        Some(_) => {
            return Err(field(
                "affinity.key",
                "affinity.key must be a string of 1 to 256 characters",
            ));
        }
    };
    let context_tokens = match &input.context_tokens {
        None => None,
        Some(JsonValue::Number(literal)) => Some(
            literal
                .parse::<u64>()
                .ok()
                .filter(|tokens| *tokens <= MAX_CONTEXT_TOKENS)
                .ok_or_else(|| {
                    field(
                        "affinity.context_tokens",
                        "affinity.context_tokens must be an integer from 0 to 10000000",
                    )
                })?,
        ),
        Some(_) => {
            return Err(field(
                "affinity.context_tokens",
                "affinity.context_tokens must be an integer from 0 to 10000000",
            ));
        }
    };
    let cost_aware = match &input.cost_aware {
        None => None,
        Some(JsonValue::Bool(cost_aware)) => Some(*cost_aware),
        Some(_) => {
            return Err(field(
                "affinity.cost_aware",
                "affinity.cost_aware must be true or false",
            ));
        }
    };
    Ok(Some(Affinity {
        key,
        context_tokens,
        cost_aware,
    }))
}

/// `workload` is a Go pointer: `null` clears it, and an object decodes into the existing
/// target, so a duplicate key updates only the fields it names.
fn workload(value: &JsonValue, existing: Option<Workload>) -> Result<Option<Workload>, String> {
    const STRUCT: &str = "WorkloadRequest";
    let Some(members) = members(value, STRUCT, "workload")? else {
        return Ok(None);
    };
    let mut out = existing.unwrap_or_default();
    for (key, value) in members {
        match match_field(
            key,
            &[
                "capability",
                "difficulty",
                "domain_tags",
                "prompt",
                "projection_version",
            ],
        ) {
            Some("capability") => {
                string(value, STRUCT, "workload.capability", &mut out.capability)?;
            }
            Some("difficulty") => {
                out.difficulty = integer(
                    value,
                    STRUCT,
                    "workload.difficulty",
                    "int",
                    i64::MIN,
                    i64::MAX,
                )?;
            }
            Some("domain_tags") => {
                out.domain_tags = strings(value, STRUCT, "workload.domain_tags")?;
            }
            Some("prompt") => match value {
                JsonValue::Null => out.prompt = None,
                JsonValue::String(s) => out.prompt = Some(s.clone()),
                other => return Err(type_error(other, STRUCT, "workload.prompt", "string")),
            },
            Some("projection_version") => {
                string(
                    value,
                    STRUCT,
                    "workload.projection_version",
                    &mut out.projection_version,
                )?;
            }
            _ => return Err(unknown(key)),
        }
    }
    Ok(Some(out))
}

fn preferences(value: &JsonValue, out: &mut Preferences) -> Result<(), String> {
    const STRUCT: &str = "Preferences";
    let Some(members) = members(value, STRUCT, "constraints")? else {
        return Ok(());
    };
    for (key, value) in members {
        match match_field(key, &["max_cost_per_run", "max_latency_ms"]) {
            Some("max_cost_per_run") => match value {
                JsonValue::Null => out.max_cost_per_run = None,
                JsonValue::Number(literal) => match literal.parse::<f64>() {
                    Ok(x) if x.is_finite() => out.max_cost_per_run = Some(x),
                    _ => {
                        return Err(type_error(
                            value,
                            STRUCT,
                            "constraints.max_cost_per_run",
                            "float64",
                        ));
                    }
                },
                other => {
                    return Err(type_error(
                        other,
                        STRUCT,
                        "constraints.max_cost_per_run",
                        "float64",
                    ));
                }
            },
            Some("max_latency_ms") => {
                out.max_latency_ms = integer(
                    value,
                    STRUCT,
                    "constraints.max_latency_ms",
                    "int32",
                    i64::from(i32::MIN),
                    i64::from(i32::MAX),
                )?
                .map(|ms| i32::try_from(ms).expect("range checked"));
            }
            _ => return Err(unknown(key)),
        }
    }
    Ok(())
}

fn string(value: &JsonValue, strukt: &str, path: &str, out: &mut String) -> Result<(), String> {
    match value {
        JsonValue::Null => Ok(()),
        JsonValue::String(s) => {
            s.clone_into(out);
            Ok(())
        }
        other => Err(type_error(other, strukt, path, "string")),
    }
}

/// A `[]string`: `null` is absent, and a `null` element is `""` (Go leaves the zero value).
fn strings(value: &JsonValue, strukt: &str, path: &str) -> Result<Option<Vec<String>>, String> {
    match value {
        JsonValue::Null => Ok(None),
        JsonValue::Array(items) => items
            .iter()
            .map(|item| match item {
                JsonValue::Null => Ok(String::new()),
                JsonValue::String(s) => Ok(s.clone()),
                other => Err(type_error(other, strukt, path, "string")),
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        other => Err(type_error(other, strukt, path, "[]string")),
    }
}

/// A Go integer field: a literal with a fraction or exponent, or one out of range, is a
/// type error (QUIRK Q3).
fn integer(
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

/// The handler rules, then the pipeline validation (reference Part 5 §5.2), in order.
pub(crate) fn validate(request: &RecommendRequest) -> Result<Form, ApiError> {
    let Some(workload) = &request.workload else {
        return Err(invalid("workload is required").detail("field", "workload"));
    };
    if workload.capability.is_empty() == workload.prompt.is_none() {
        return Err(invalid(
            "workload must carry exactly one of capability or prompt (spec §8.1)",
        ));
    }
    let form = match &workload.prompt {
        Some(prompt) => {
            let field = |message: &str| invalid(message).detail("field", "workload.prompt");
            if workload.difficulty.is_some() || workload.domain_tags.is_some() {
                return Err(field(
                    "prompt-form workloads take no difficulty or domain_tags; classification \
                     supplies them (spec §8.1)",
                ));
            }
            if prompt.is_empty() {
                return Err(field("workload.prompt must not be empty"));
            }
            if prompt.chars().count() > MAX_PROMPT_CHARS {
                return Err(field(&format!(
                    "workload.prompt exceeds {MAX_PROMPT_CHARS} characters (spec §8.1)"
                )));
            }
            Form::Prompt {
                prompt: prompt.clone(),
                projection_version: Some(workload.projection_version.clone())
                    .filter(|version| !version.is_empty()),
            }
        }
        None => {
            if !workload.projection_version.is_empty() {
                return Err(invalid(
                    "workload.projection_version applies only to the prompt form (spec §8.1)",
                )
                .detail("field", "workload.projection_version"));
            }
            let capability = &workload.capability;
            if !CAPABILITIES.contains(&capability.as_str()) {
                return Err(invalid(format!(
                    "workload.capability {} is not a v1 capability",
                    maverick_domain::gofmt::quote(capability)
                ))
                .detail("capability", capability.clone()));
            }
            if let Some(d) = workload.difficulty.filter(|d| !(1..=5).contains(d)) {
                return Err(
                    invalid(format!("workload.difficulty {d} is out of range 1-5"))
                        .detail("difficulty", d),
                );
            }
            Form::Declared {
                capability: capability.clone(),
                difficulty: workload.difficulty,
                domain_tags: workload.domain_tags.clone().unwrap_or_default(),
            }
        }
    };
    if !request.mode.is_empty() && !matches!(request.mode.as_str(), "quality" | "balanced" | "cost")
    {
        return Err(invalid(format!(
            "mode {} is not one of quality, balanced, cost",
            maverick_domain::gofmt::quote(&request.mode)
        ))
        .detail("mode", request.mode.clone()));
    }
    if request
        .constraints
        .max_cost_per_run
        .is_some_and(|c| c <= 0.0)
    {
        return Err(
            invalid("constraints.max_cost_per_run must be greater than 0")
                .detail("field", "max_cost_per_run"),
        );
    }
    if request.constraints.max_latency_ms.is_some_and(|l| l <= 0) {
        return Err(invalid("constraints.max_latency_ms must be greater than 0")
            .detail("field", "max_latency_ms"));
    }
    Ok(form)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(body: &str) -> String {
        match decode(body.as_bytes()).and_then(|r| validate(&r).map(|_| r)) {
            Ok(_) => "ok".to_owned(),
            Err(error) => error.message,
        }
    }

    #[test]
    fn go_decoding_rules() {
        assert_eq!(message(""), "request body is required");
        assert_eq!(message(" \n"), "request body is required");
        assert_eq!(message("{"), "malformed request body: unexpected EOF");
        assert_eq!(
            message("abc"),
            "malformed request body: invalid character 'a' looking for beginning of value"
        );
        assert_eq!(
            message(r#"{"workload":{"capability":"factual_qa"}}{"x":1}"#),
            "request body has trailing content"
        );
        // QUIRK Q2: a trailing `}` is ignored.
        assert_eq!(
            message(r#"{"workload":{"capability":"factual_qa"}}}"#),
            "ok"
        );
        assert_eq!(
            message(r#"{"workload":{"capability":"factual_qa"},"model":"x"}"#),
            "malformed request body: json: unknown field \"model\""
        );
        assert_eq!(
            message(r#"{"workload":{"capability":"factual_qa","tier":2}}"#),
            "malformed request body: json: unknown field \"tier\""
        );
        assert_eq!(
            message(r#"{"workload":{"capability":"factual_qa","difficulty":2.5}}"#),
            "malformed request body: json: cannot unmarshal number 2.5 into Go struct field \
             WorkloadRequest.workload.difficulty of type int"
        );
        // Keys match case-insensitively, the last duplicate wins, and null is absent.
        let request = decode(
            br#"{"WORKLOAD":{"Capability":"factual_qa","difficulty":1},"mode":"cost","Mode":"quality","pool":null}"#,
        )
        .unwrap();
        let workload = request.workload.unwrap();
        assert_eq!(workload.capability, "factual_qa");
        assert_eq!(workload.difficulty, Some(1));
        assert_eq!(request.mode, "quality");
        assert_eq!(request.pool, "");
        // A pointer field cleared by a later null; a duplicate object updates in place.
        let request = decode(
            br#"{"workload":{"capability":"factual_qa","difficulty":2},"workload":{"difficulty":null},"allowed_models":["a"],"allowed_models":null}"#,
        )
        .unwrap();
        let workload = request.workload.unwrap();
        assert_eq!(workload.capability, "factual_qa");
        assert_eq!(workload.difficulty, None);
        assert_eq!(request.allowed_models, None);
        // The first error in document order wins; syntax errors win over both.
        assert_eq!(
            message(r#"{"x":1,"workload":{"difficulty":"a"}}"#),
            "malformed request body: json: unknown field \"x\""
        );
        assert_eq!(
            message(r#"{"x":1,"workload":{"difficulty":"a"}"#),
            "malformed request body: unexpected EOF"
        );
        assert_eq!(
            message(r#"{"constraints":{"max_latency_ms":3000000000}}"#),
            "malformed request body: json: cannot unmarshal number 3000000000 into Go struct \
             field Preferences.constraints.max_latency_ms of type int32"
        );
        assert_eq!(
            message(r#"{"constraints":{"max_cost_per_run":1e400}}"#),
            "malformed request body: json: cannot unmarshal number 1e400 into Go struct field \
             Preferences.constraints.max_cost_per_run of type float64"
        );
        assert_eq!(
            message("[]"),
            "malformed request body: json: cannot unmarshal array into Go value of type \
             api.RecommendRequest"
        );
    }

    #[test]
    fn handler_and_pipeline_rules_in_order() {
        let cases = [
            ("{}", "workload is required"),
            ("null", "workload is required"),
            (r#"{"workload":null}"#, "workload is required"),
            (
                r#"{"workload":{}}"#,
                "workload must carry exactly one of capability or prompt (spec §8.1)",
            ),
            (
                r#"{"workload":{"capability":"factual_qa","prompt":"hi"}}"#,
                "workload must carry exactly one of capability or prompt (spec §8.1)",
            ),
            (
                r#"{"workload":{"prompt":"x","difficulty":2}}"#,
                "prompt-form workloads take no difficulty or domain_tags; classification \
                 supplies them (spec §8.1)",
            ),
            (
                r#"{"workload":{"prompt":""}}"#,
                "workload.prompt must not be empty",
            ),
            (
                r#"{"workload":{"capability":"poetry"}}"#,
                "workload.capability \"poetry\" is not a v1 capability",
            ),
            (
                r#"{"workload":{"capability":"factual_qa","difficulty":0}}"#,
                "workload.difficulty 0 is out of range 1-5",
            ),
            (
                r#"{"workload":{"capability":"factual_qa","difficulty":6}}"#,
                "workload.difficulty 6 is out of range 1-5",
            ),
            (
                r#"{"workload":{"capability":"factual_qa"},"mode":"turbo"}"#,
                "mode \"turbo\" is not one of quality, balanced, cost",
            ),
            (
                r#"{"workload":{"capability":"factual_qa"},"constraints":{"max_cost_per_run":0}}"#,
                "constraints.max_cost_per_run must be greater than 0",
            ),
            (
                r#"{"workload":{"capability":"factual_qa"},"constraints":{"max_latency_ms":-5}}"#,
                "constraints.max_latency_ms must be greater than 0",
            ),
            (
                r#"{"workload":{"capability":"factual_qa","difficulty":5}}"#,
                "ok",
            ),
            (r#"{"workload":{"prompt":"hi"},"mode":"cost"}"#, "ok"),
        ];
        for (body, want) in cases {
            assert_eq!(message(body), want, "{body}");
        }
    }

    #[test]
    fn prompt_length_counts_code_points_inclusive() {
        let at = format!(
            r#"{{"workload":{{"prompt":"{}"}}}}"#,
            "é".repeat(MAX_PROMPT_CHARS)
        );
        assert_eq!(message(&at), "ok");
        let over = format!(
            r#"{{"workload":{{"prompt":"{}"}}}}"#,
            "é".repeat(MAX_PROMPT_CHARS + 1)
        );
        assert_eq!(
            message(&over),
            "workload.prompt exceeds 500000 characters (spec §8.1)"
        );
    }

    #[test]
    fn details_carry_the_offending_values() {
        let error = validate(
            &decode(br#"{"workload":{"capability":"factual_qa","difficulty":9}}"#).unwrap(),
        )
        .unwrap_err();
        assert_eq!(error.details["difficulty"], 9);
        let error = validate(&decode(br#"{"workload":{"prompt":""}}"#).unwrap()).unwrap_err();
        assert_eq!(error.details["field"], "workload.prompt");
        let form =
            validate(&decode(br#"{"workload":{"capability":"factual_qa"}}"#).unwrap()).unwrap();
        assert_eq!(
            form,
            Form::Declared {
                capability: "factual_qa".to_owned(),
                difficulty: None,
                domain_tags: Vec::new(),
            }
        );
    }
}
