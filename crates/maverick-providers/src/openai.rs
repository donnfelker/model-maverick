//! The hand-written OpenAI chat-completions client, and xAI as the same client with
//! xAI's base URL and label (reference Part 2 §2.2).
//!
//! `POST {base}/chat/completions` with reqwest on rustls, the whole call plus parsing
//! inside the shared retry policy, and no timeout of its own (the caller's deadline
//! bounds it). It sends only `Authorization` and `Content-Type`, not the Go SDK's
//! identity headers (D32). The base URL comes only from engine config; no ambient
//! `OPENAI_*` variable changes anything (D23).

use maverick_domain::gofmt::quote;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::anthropic::go_compact;
use crate::{
    CompletionRequest, CompletionResponse, ProviderClient, ProviderError, ProviderErrorKind,
    ProviderFuture, RetryPolicy, retry_provider_observed, setting_string, wrap_error,
};

/// The OpenAI default API base.
pub const OPENAI_BASE_URL: &str = "https://api.openai.com/v1/";
/// The xAI default API base.
pub const XAI_BASE_URL: &str = "https://api.x.ai/v1";

/// A chat-completions client for OpenAI or xAI.
#[derive(Debug, Clone)]
pub struct OpenAiClient {
    http: reqwest::Client,
    provider: &'static str,
    api_key: String,
    base_url: String,
    retry: RetryPolicy,
}

impl OpenAiClient {
    fn with_provider(
        provider: &'static str,
        base_url: &str,
        api_key: impl Into<String>,
    ) -> Result<Self, reqwest::Error> {
        Ok(OpenAiClient {
            http: reqwest::Client::builder().build()?,
            provider,
            api_key: api_key.into(),
            base_url: base_url.to_owned(),
            retry: RetryPolicy::default(),
        })
    }

    /// The OpenAI client (label `openai`).
    pub fn openai(api_key: impl Into<String>) -> Result<Self, reqwest::Error> {
        Self::with_provider("openai", OPENAI_BASE_URL, api_key)
    }

    /// The xAI client (label `xai`), the same client with xAI's base URL.
    pub fn xai(api_key: impl Into<String>) -> Result<Self, reqwest::Error> {
        Self::with_provider("xai", XAI_BASE_URL, api_key)
    }

    /// An explicit base URL is kept as given.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// `{base}/chat/completions`, however the base ends.
    pub fn completions_url(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }

    async fn attempt(&self, body: &[u8]) -> Result<CompletionResponse, ProviderError> {
        let url = self.completions_url();
        let response = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            .header("content-type", "application/json")
            .body(body.to_vec())
            .send()
            .await
            .map_err(|e| wrap_error(self.provider, 0, e))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| wrap_error(self.provider, 0, e))?;
        if !status.is_success() {
            return Err(wrap_error(
                self.provider,
                i32::from(status.as_u16()),
                format!(
                    "POST {}: {} {} {}",
                    quote(&url),
                    status.as_u16(),
                    status.canonical_reason().unwrap_or(""),
                    error_object(&text)
                ),
            ));
        }
        self.parse_response(&text)
    }

    fn parse_response(&self, body: &str) -> Result<CompletionResponse, ProviderError> {
        maverick_domain::gojson::unmarshal(body).map_err(|e| {
            wrap_error(
                self.provider,
                0,
                format!("error parsing response json: {}", e.0),
            )
        })?;
        let parsed: ResponseBody = serde_json::from_str(body).map_err(|e| {
            wrap_error(
                self.provider,
                0,
                format!("error parsing response json: {e}"),
            )
        })?;
        let Some(choice) = parsed.choices.into_iter().next() else {
            return Err(wrap_error(
                self.provider,
                0,
                "response contained no choices",
            ));
        };
        Ok(CompletionResponse {
            text: choice.message.content.unwrap_or_default(),
            tokens_in: parsed.usage.prompt_tokens,
            tokens_out: parsed.usage.completion_tokens,
        })
    }
}

/// The SDK's error text: the body's `error` object re-emitted compactly with its key
/// order kept, or nothing when the body is not JSON or has no `error`.
fn error_object(body: &str) -> String {
    #[derive(Deserialize)]
    struct ErrorBody<'a> {
        #[serde(borrow, default)]
        error: Option<&'a RawValue>,
    }
    serde_json::from_str::<ErrorBody<'_>>(body)
        .ok()
        .and_then(|b| b.error)
        .map(|raw| go_compact(raw.get()))
        .unwrap_or_default()
}

impl ProviderClient for OpenAiClient {
    fn name(&self) -> &str {
        self.provider
    }

    fn complete<'a>(&'a self, request: CompletionRequest) -> ProviderFuture<'a> {
        Box::pin(async move {
            let body = serde_json::to_vec(&Body::new(&request)).map_err(|e| {
                ProviderError::new(self.provider, ProviderErrorKind::Unknown)
                    .with_source(format!("{}: encoding request: {e}", self.provider))
            })?;
            retry_provider_observed(self.provider, self.retry.clone(), || self.attempt(&body)).await
        })
    }
}

#[derive(Debug, Serialize)]
struct Message<'a> {
    content: &'a str,
    role: &'static str,
}

/// The request body, in the Go SDK's field order. `response_schema` and every setting
/// other than `reasoning` are ignored.
#[derive(Debug, Serialize)]
struct Body<'a> {
    messages: Vec<Message<'a>>,
    model: &'a str,
    max_completion_tokens: usize,
    temperature: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'a str>,
}

impl<'a> Body<'a> {
    fn new(request: &'a CompletionRequest) -> Self {
        let mut messages = Vec::with_capacity(2);
        if !request.system.is_empty() {
            messages.push(Message {
                content: &request.system,
                role: "system",
            });
        }
        messages.push(Message {
            content: &request.prompt,
            role: "user",
        });
        Body {
            messages,
            model: &request.model,
            max_completion_tokens: request.max_tokens,
            temperature: f64::from(request.temperature),
            reasoning_effort: setting_string(&request.settings, "reasoning"),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct ResponseMessage {
    #[serde(default)]
    content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    #[serde(default)]
    message: ResponseMessage,
}

#[derive(Debug, Default, Deserialize)]
struct Usage {
    #[serde(default)]
    prompt_tokens: usize,
    #[serde(default)]
    completion_tokens: usize,
}

#[derive(Debug, Deserialize)]
struct ResponseBody {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Usage,
}
