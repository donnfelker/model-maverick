//! The hand-written Google Gemini client (reference Part 2 §2.3).
//!
//! `POST {base}/v1beta/models/{model}:generateContent` with reqwest on rustls, wrapped
//! whole (call plus response parsing) in the shared retry policy. It sends only the
//! headers the API needs (`x-goog-api-key`, `content-type`), not the Go SDK's identity
//! headers (D32). It has no default timeout, like the Go SDK. The base URL comes only
//! from engine config; no ambient environment variable changes it (D23).

use maverick_domain::gofmt::{fmt_v_f32, fmt_v_f64};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::value::RawValue;

use crate::anthropic::go_compact;
use crate::{
    CompletionRequest, CompletionResponse, ProviderClient, ProviderError, ProviderErrorKind,
    ProviderFuture, RetryPolicy, retry_provider, setting_int, wrap_error,
};

/// The provider name errors carry.
pub const PROVIDER: &str = "google";
/// The default API base (Gemini API backend).
pub const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/";

/// A Gemini API client.
#[derive(Clone)]
pub struct GoogleClient {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
    retry: RetryPolicy,
}

impl std::fmt::Debug for GoogleClient {
    /// Never prints the API key (G5).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleClient")
            .field("base_url", &self.base_url)
            .field("retry", &self.retry)
            .finish_non_exhaustive()
    }
}

impl GoogleClient {
    /// A client for the default base URL with the default retry policy.
    pub fn new(api_key: impl Into<String>) -> Result<Self, reqwest::Error> {
        Ok(GoogleClient {
            http: reqwest::Client::builder().build()?,
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_owned(),
            retry: RetryPolicy::default(),
        })
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// `{base}/v1beta/models/{model}:generateContent`, however the base ends.
    pub fn generate_url(&self, model: &str) -> String {
        format!(
            "{}/v1beta/models/{model}:generateContent",
            self.base_url.trim_end_matches('/')
        )
    }

    async fn attempt(&self, url: &str, body: &[u8]) -> Result<CompletionResponse, ProviderError> {
        let response = self
            .http
            .post(url)
            .header("x-goog-api-key", &self.api_key)
            .header("content-type", "application/json")
            .body(body.to_vec())
            .send()
            .await
            .map_err(|e| wrap_error(PROVIDER, 0, e))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| wrap_error(PROVIDER, 0, e))?;
        if !status.is_success() {
            let error = ApiError::from_response(status, &text);
            return Err(wrap_error(PROVIDER, error.code, error.to_string()));
        }
        parse_response(&text)
    }
}

impl ProviderClient for GoogleClient {
    fn name(&self) -> &str {
        PROVIDER
    }

    fn complete<'a>(&'a self, request: CompletionRequest) -> ProviderFuture<'a> {
        Box::pin(async move {
            let bytes = encode_body(&request).map_err(|e| {
                ProviderError::new(PROVIDER, ProviderErrorKind::Unknown).with_source(e)
            })?;
            let url = self.generate_url(&request.model);
            retry_provider(self.retry.clone(), || self.attempt(&url, &bytes)).await
        })
    }
}

#[derive(Debug, Serialize)]
struct Part<'a> {
    text: &'a str,
}

#[derive(Debug, Serialize)]
struct Content<'a> {
    parts: [Part<'a>; 1],
    role: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ThinkingConfig {
    thinking_budget: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GenerationConfig {
    max_output_tokens: usize,
    /// float32 in Go's JSON form, as the SDK sends it (`0.7`, `0`; QUIRK Q-P1).
    temperature: Box<RawValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking_config: Option<ThinkingConfig>,
}

/// The request body, in the Go SDK's field order.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Body<'a> {
    contents: [Content<'a>; 1],
    generation_config: GenerationConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<Content<'a>>,
}

/// The exact request bytes the Go SDK sends: compact JSON with Go's HTML escaping,
/// ending with a newline (its `json.Encoder`).
pub fn encode_body(request: &CompletionRequest) -> Result<Vec<u8>, String> {
    let body = serde_json::to_string(&build_body(request)?)
        .map_err(|e| format!("google: encoding request: {e}"))?;
    let mut bytes = go_compact(&body).into_bytes();
    bytes.push(b'\n');
    Ok(bytes)
}

/// `response_schema` is ignored, as in Go. `thinking_budget` becomes
/// `thinkingConfig.thinkingBudget`. The system instruction's role is `user` (exact).
fn build_body(request: &CompletionRequest) -> Result<Body<'_>, String> {
    let temperature = RawValue::from_string(encode_f32(request.temperature)?)
        .map_err(|e| format!("google: encoding request: {e}"))?;
    Ok(Body {
        contents: [Content {
            parts: [Part {
                text: &request.prompt,
            }],
            role: "user",
        }],
        generation_config: GenerationConfig {
            max_output_tokens: request.max_tokens,
            temperature,
            thinking_config: setting_int(&request.settings, "thinking_budget")
                .map(|thinking_budget| ThinkingConfig { thinking_budget }),
        },
        system_instruction: (!request.system.is_empty()).then(|| Content {
            parts: [Part {
                text: &request.system,
            }],
            role: "user",
        }),
    })
}

/// Go's JSON form of a `float32`: the shortest float32 digits, plain notation unless
/// the magnitude is `< 1e-6` or `>= 1e21`. NaN and infinities cannot be encoded.
fn encode_f32(x: f32) -> Result<String, String> {
    if !x.is_finite() {
        return Err(format!("json: unsupported value: {}", fmt_v_f32(x)));
    }
    let abs = x.abs();
    if abs != 0.0 && !(1e-6..1e21).contains(&abs) {
        let s = format!("{x:e}");
        Ok(match s.split_once('e') {
            Some((mantissa, exp)) if !exp.starts_with('-') => format!("{mantissa}e+{exp}"),
            _ => s,
        })
    } else {
        Ok(format!("{x}"))
    }
}

#[derive(Debug, Default, Deserialize)]
struct ResponsePart {
    #[serde(default)]
    text: String,
    #[serde(default)]
    thought: bool,
}

#[derive(Debug, Default, Deserialize)]
struct CandidateContent {
    #[serde(default)]
    parts: Vec<ResponsePart>,
}

#[derive(Debug, Default, Deserialize)]
struct Candidate {
    #[serde(default)]
    content: Option<CandidateContent>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageMetadata {
    #[serde(default)]
    prompt_token_count: usize,
    #[serde(default)]
    candidates_token_count: usize,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResponseBody {
    #[serde(default)]
    candidates: Vec<Candidate>,
    #[serde(default)]
    usage_metadata: UsageMetadata,
}

/// Parses a 2xx body. The text is the SDK's `Text()`: the parts of the first candidate
/// with non-empty text that are not thoughts, concatenated. No candidates (a blocked
/// prompt) is a success with empty text. Missing usage is 0/0.
fn parse_response(body: &str) -> Result<CompletionResponse, ProviderError> {
    let parsed: ResponseBody = serde_json::from_str(body)
        .map_err(|e| wrap_error(PROVIDER, 0, format!("error parsing response json: {e}")))?;
    let text = parsed
        .candidates
        .first()
        .and_then(|c| c.content.as_ref())
        .map(|content| {
            content
                .parts
                .iter()
                .filter(|p| !p.text.is_empty() && !p.thought)
                .map(|p| p.text.as_str())
                .collect()
        })
        .unwrap_or_default();
    Ok(CompletionResponse {
        text,
        tokens_in: parsed.usage_metadata.prompt_token_count,
        tokens_out: parsed.usage_metadata.candidates_token_count,
    })
}

/// The SDK's `APIError` for a non-2xx response (QUIRK Q-P3). With a JSON body that has
/// an `error` member, the code is `error.code` (0 when absent), not the HTTP status.
/// Otherwise the code is the HTTP status and the message is the raw body.
#[derive(Debug)]
struct ApiError {
    code: i32,
    message: String,
    status: String,
    details: Vec<Value>,
}

impl ApiError {
    fn from_response(http: reqwest::StatusCode, body: &str) -> Self {
        if let Ok(Value::Object(object)) = serde_json::from_str::<Value>(body)
            && let Some(Value::Object(error)) = object.get("error")
        {
            let code = error
                .get("code")
                .and_then(Value::as_i64)
                .and_then(|c| i32::try_from(c).ok())
                .unwrap_or(0);
            let text = |key: &str| {
                error
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            let details = match error.get("details") {
                Some(Value::Array(details)) => details.clone(),
                _ => Vec::new(),
            };
            return ApiError {
                code,
                message: text("message"),
                status: text("status"),
                details,
            };
        }
        ApiError {
            code: i32::from(http.as_u16()),
            message: body.to_owned(),
            status: format!(
                "{} {}",
                http.as_u16(),
                http.canonical_reason().unwrap_or_default()
            ),
            details: Vec::new(),
        }
    }
}

impl std::fmt::Display for ApiError {
    /// `Error <Code>, Message: <Message>, Status: <Status>, Details: <Go %v>`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let details: Vec<String> = self.details.iter().map(go_v).collect();
        write!(
            f,
            "Error {}, Message: {}, Status: {}, Details: [{}]",
            self.code,
            self.message,
            self.status,
            details.join(" ")
        )
    }
}

/// Go's `%v` of a JSON-decoded value: maps print as `map[k:v ...]` with sorted keys.
fn go_v(value: &Value) -> String {
    match value {
        Value::Null => "<nil>".to_owned(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.as_f64().map_or_else(|| n.to_string(), fmt_v_f64),
        Value::String(s) => s.clone(),
        Value::Array(items) => {
            let items: Vec<String> = items.iter().map(go_v).collect();
            format!("[{}]", items.join(" "))
        }
        Value::Object(members) => {
            let mut keys: Vec<&String> = members.keys().collect();
            keys.sort();
            let items: Vec<String> = keys
                .into_iter()
                .map(|k| format!("{k}:{}", go_v(&members[k])))
                .collect();
            format!("map[{}]", items.join(" "))
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn float32_encodes_like_go_json() {
        assert_eq!(encode_f32(0.0).unwrap(), "0");
        assert_eq!(encode_f32(0.7).unwrap(), "0.7");
        assert_eq!(encode_f32(1.0).unwrap(), "1");
        assert_eq!(encode_f32(-0.25).unwrap(), "-0.25");
        assert_eq!(encode_f32(1e-7).unwrap(), "1e-7");
        assert_eq!(encode_f32(0.000_001).unwrap(), "0.000001");
        assert_eq!(encode_f32(1e21).unwrap(), "1e+21");
        assert_eq!(
            encode_f32(f32::NAN).unwrap_err(),
            "json: unsupported value: NaN"
        );
    }

    #[test]
    fn details_print_like_go() {
        let error = ApiError {
            code: 400,
            message: "bad".to_owned(),
            status: "INVALID_ARGUMENT".to_owned(),
            details: vec![json!({"@type": "type.googleapis.com/x", "reason": "R", "n": 2})],
        };
        assert_eq!(
            error.to_string(),
            "Error 400, Message: bad, Status: INVALID_ARGUMENT, Details: [map[@type:type.googleapis.com/x n:2 reason:R]]"
        );
    }

    #[test]
    fn a_json_error_body_without_error_uses_the_http_status() {
        let error = ApiError::from_response(reqwest::StatusCode::BAD_REQUEST, r#"{"detail":"x"}"#);
        assert_eq!(
            error.to_string(),
            r#"Error 400, Message: {"detail":"x"}, Status: 400 Bad Request, Details: []"#
        );
    }
}
