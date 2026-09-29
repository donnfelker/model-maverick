//! The hand-written Anthropic Messages client (reference Part 2 §2.1).
//!
//! `POST {base}/v1/messages` with reqwest on rustls, wrapped whole (call plus response
//! parsing) in the shared retry policy. It sends only the headers the API needs
//! (`x-api-key`, `anthropic-version`, `content-type`), not the Go SDK's identity headers
//! (D32), and it does not refuse large `max_tokens` before sending (D33). The base URL
//! comes only from engine config; no ambient environment variable changes it (D23).

use std::time::Duration;

use maverick_domain::gofmt::quote;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use serde_json::{Map, Value};

use crate::{
    CompletionRequest, CompletionResponse, ProviderClient, ProviderError, ProviderErrorKind,
    ProviderFuture, RetryPolicy, SETTING_RESPONSE_SCHEMA, retry_provider, setting_bool,
    setting_int, wrap_error,
};

/// The provider name errors carry.
pub const PROVIDER: &str = "anthropic";
/// The default API base.
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com/";
/// The `anthropic-version` header.
pub const API_VERSION: &str = "2023-06-01";
/// The forced tool's name.
pub const FORCED_TOOL_NAME: &str = "record_result";
/// The forced tool's description.
pub const FORCED_TOOL_DESCRIPTION: &str = "Record the structured result.";
/// `budget_tokens` when thinking is on and the column sets no budget.
pub const DEFAULT_THINKING_BUDGET: i64 = 1024;
/// The per-request timeout the Go SDK set (`X-Stainless-Timeout: 600`).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);

/// An Anthropic Messages API client.
#[derive(Debug, Clone)]
pub struct AnthropicClient {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
    retry: RetryPolicy,
}

impl AnthropicClient {
    /// A client for the default base URL with the default retry policy.
    pub fn new(api_key: impl Into<String>) -> Result<Self, reqwest::Error> {
        Ok(AnthropicClient {
            http: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()?,
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

    /// `{base}/v1/messages`, however the base ends.
    pub fn messages_url(&self) -> String {
        format!("{}/v1/messages", self.base_url.trim_end_matches('/'))
    }

    async fn attempt(
        &self,
        body: &[u8],
        forced: bool,
    ) -> Result<CompletionResponse, ProviderError> {
        let url = self.messages_url();
        let response = self
            .http
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
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
            // The SDK's error text: the raw body when it is JSON, else nothing.
            let raw = if serde_json::from_str::<&RawValue>(&text).is_ok() {
                text.as_str()
            } else {
                ""
            };
            return Err(wrap_error(
                PROVIDER,
                i32::from(status.as_u16()),
                format!(
                    "POST {}: {} {} {raw}",
                    quote(&url),
                    status.as_u16(),
                    status.canonical_reason().unwrap_or("")
                ),
            ));
        }
        parse_response(&text, forced)
    }
}

impl ProviderClient for AnthropicClient {
    fn name(&self) -> &str {
        PROVIDER
    }

    fn complete<'a>(&'a self, request: CompletionRequest) -> ProviderFuture<'a> {
        Box::pin(async move {
            let body = build_body(&request)?;
            let forced = body.tools.is_some();
            let bytes = serde_json::to_vec(&body).map_err(|e| {
                ProviderError::new(PROVIDER, ProviderErrorKind::Unknown)
                    .with_source(format!("anthropic: encoding request: {e}"))
            })?;
            retry_provider(self.retry.clone(), || self.attempt(&bytes, forced)).await
        })
    }
}

#[derive(Debug, Serialize)]
struct TextBlock<'a> {
    text: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Debug, Serialize)]
struct Message<'a> {
    content: [TextBlock<'a>; 1],
    role: &'static str,
}

#[derive(Debug, Serialize)]
struct Thinking {
    budget_tokens: i64,
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Debug, Serialize)]
struct ToolChoice {
    name: &'static str,
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Debug, Serialize)]
struct InputSchema {
    #[serde(skip_serializing_if = "Option::is_none")]
    properties: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    required: Option<Vec<String>>,
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

#[derive(Debug, Serialize)]
struct Tool {
    input_schema: InputSchema,
    name: &'static str,
    description: &'static str,
}

/// The request body, in the Go SDK's field order.
#[derive(Debug, Serialize)]
struct Body<'a> {
    max_tokens: usize,
    messages: [Message<'a>; 1],
    model: &'a str,
    temperature: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<[TextBlock<'a>; 1]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<Thinking>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<ToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<[Tool; 1]>,
}

/// Go's `%T` of a JSON-decoded settings value.
fn go_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "<nil>",
        Value::Bool(_) => "bool",
        Value::Number(_) => "float64",
        Value::String(_) => "string",
        Value::Array(_) => "[]interface {}",
        Value::Object(_) => "map[string]interface {}",
    }
}

/// The forced tool's `input_schema`: `type` is always `object`, `properties` passes
/// through, `required` keeps the string elements, and every other key is copied.
fn input_schema(schema: &Map<String, Value>) -> InputSchema {
    let required = match schema.get("required") {
        Some(Value::Array(items)) => Some(
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect(),
        ),
        _ => None,
    };
    let extra = schema
        .iter()
        .filter(|(key, _)| !matches!(key.as_str(), "type" | "properties" | "required"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    InputSchema {
        properties: schema.get("properties").cloned(),
        required,
        kind: "object",
        extra,
    }
}

fn build_body(request: &CompletionRequest) -> Result<Body<'_>, ProviderError> {
    let thinking = (setting_bool(&request.settings, "thinking") == Some(true)).then(|| Thinking {
        budget_tokens: setting_int(&request.settings, "thinking_budget")
            .unwrap_or(DEFAULT_THINKING_BUDGET),
        kind: "enabled",
    });
    let tools = match request.settings.get(SETTING_RESPONSE_SCHEMA) {
        None => None,
        Some(Value::Object(schema)) => Some([Tool {
            input_schema: input_schema(schema),
            name: FORCED_TOOL_NAME,
            description: FORCED_TOOL_DESCRIPTION,
        }]),
        Some(other) => {
            return Err(
                ProviderError::new(PROVIDER, ProviderErrorKind::Unknown).with_source(format!(
                    "anthropic: building forced-tool schema: response schema is {}, want map[string]any",
                    go_type_name(other)
                )),
            );
        }
    };
    Ok(Body {
        max_tokens: request.max_tokens,
        messages: [Message {
            content: [TextBlock {
                text: &request.prompt,
                kind: "text",
            }],
            role: "user",
        }],
        model: &request.model,
        temperature: f64::from(request.temperature),
        system: (!request.system.is_empty()).then_some([TextBlock {
            text: &request.system,
            kind: "text",
        }]),
        thinking,
        tool_choice: tools.as_ref().map(|_| ToolChoice {
            name: FORCED_TOOL_NAME,
            kind: "tool",
        }),
        tools,
    })
}

#[derive(Debug, Deserialize)]
struct ResponseBlock<'a> {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    text: String,
    #[serde(borrow, default)]
    input: Option<&'a RawValue>,
}

#[derive(Debug, Default, Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: usize,
    #[serde(default)]
    output_tokens: usize,
}

#[derive(Debug, Deserialize)]
struct ResponseBody<'a> {
    #[serde(borrow, default)]
    content: Vec<ResponseBlock<'a>>,
    #[serde(default)]
    usage: Usage,
}

/// Parses a 2xx body. Non-JSON is `error parsing response json: <json error>` (status
/// 0, `provider_error`, retried). A forced call returns the first `tool_use` block's
/// `input`, compacted with its key order kept; otherwise the text blocks concatenated.
fn parse_response(body: &str, forced: bool) -> Result<CompletionResponse, ProviderError> {
    maverick_domain::gojson::unmarshal(body)
        .map_err(|e| wrap_error(PROVIDER, 0, format!("error parsing response json: {}", e.0)))?;
    let parsed: ResponseBody<'_> = serde_json::from_str(body)
        .map_err(|e| wrap_error(PROVIDER, 0, format!("error parsing response json: {e}")))?;
    let text = if forced {
        let input = parsed
            .content
            .iter()
            .find(|block| block.kind == "tool_use")
            .map(|block| block.input.map_or("null", RawValue::get))
            .ok_or_else(|| {
                ProviderError::new(PROVIDER, ProviderErrorKind::Unknown).with_source(format!(
                    "anthropic: response has no tool_use block for forced tool {}",
                    quote(FORCED_TOOL_NAME)
                ))
            })?;
        go_compact(input)
    } else {
        parsed
            .content
            .iter()
            .filter(|block| block.kind == "text")
            .map(|block| block.text.as_str())
            .collect()
    };
    Ok(CompletionResponse {
        text,
        tokens_in: parsed.usage.input_tokens,
        tokens_out: parsed.usage.output_tokens,
    })
}

/// Go's `json.Marshal(json.RawMessage)`: the raw JSON with insignificant whitespace
/// removed and, inside strings, `<`, `>`, `&`, U+2028, and U+2029 escaped. Everything
/// else, including key order, number literals, and existing escapes, is kept.
pub fn go_compact(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut in_string = false;
    let mut escaped = false;
    for c in raw.chars() {
        if in_string {
            match c {
                _ if escaped => {
                    escaped = false;
                    out.push(c);
                }
                '\\' => {
                    escaped = true;
                    out.push(c);
                }
                '"' => {
                    in_string = false;
                    out.push(c);
                }
                '<' => out.push_str("\\u003c"),
                '>' => out.push_str("\\u003e"),
                '&' => out.push_str("\\u0026"),
                '\u{2028}' => out.push_str("\\u2028"),
                '\u{2029}' => out.push_str("\\u2029"),
                _ => out.push(c),
            }
        } else {
            match c {
                ' ' | '\t' | '\n' | '\r' => {}
                '"' => {
                    in_string = true;
                    out.push(c);
                }
                _ => out.push(c),
            }
        }
    }
    out
}
