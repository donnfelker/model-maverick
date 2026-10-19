//! The full-stack end-to-end harness (bead 9.10).
//!
//! [`FakeProviderServer`] serves the deterministic fake provider over HTTP, in each wire
//! format the engine's provider clients and the proxy speak, so the real binaries
//! can run against it with only a base URL changed:
//!
//! | wire | path | engine config | proxy config |
//! |---|---|---|---|
//! | Anthropic Messages | `…/v1/messages` | `MAVERICK_ANTHROPIC_BASE_URL` = [`FakeProviderServer::url`] | `MAVERICK_UPSTREAM_ANTHROPIC` |
//! | OpenAI chat completions (OpenAI, xAI) | `…/chat/completions` | `MAVERICK_OPENAI_BASE_URL`, `MAVERICK_XAI_BASE_URL` | `MAVERICK_UPSTREAM_OPENAI`, `…_XAI`, `…_OPENROUTER` |
//! | Google `generateContent` | `…/v1beta/models/{model}:generateContent` | `MAVERICK_GOOGLE_BASE_URL` | none |
//!
//! Each request is decoded to `(model, prompt)`: the prompt is the last user message's
//! text. It is answered by [`FakeProvider`], which looks the pair up by the SHA-256 of the
//! prompt. A hit is encoded back in the request's wire format; for an Anthropic request with
//! a forced tool, the output is the `tool_use` block's input. A scripted error becomes that
//! provider's error body with status 429 (`rate_limited`), 400 (`invalid_request`), 500
//! (`provider_error`), or 504 (`timeout`). A request with no fixture is answered 400 with the
//! fake's exact message, so the caller fails fast without retrying, and it is recorded as a
//! miss for the test report.
//!
//! Every request is recorded ([`Served`]) with its prompt hash, never its prompt text.
//!
//! [`process`] runs the real binaries as OS processes, and [`harness`] holds the step
//! logging and HTTP helpers, for `tests/full_stack.rs` and `tests/latency.rs`.

pub mod harness;
pub mod process;

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use maverick_providers::fake::{FakeProvider, hash_prompt};
use maverick_providers::{CompletionRequest, ProviderClient, ProviderErrorKind};
use serde_json::{Map, Value, json};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// A provider wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    /// Anthropic Messages.
    Anthropic,
    /// OpenAI chat completions (also xAI and OpenRouter).
    OpenAi,
    /// Google `generateContent`.
    Google,
}

/// One request the fake answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Served {
    /// The wire format, or `None` for a path no provider serves (answered 404).
    pub wire: Option<Wire>,
    /// The request path.
    pub path: String,
    /// The requested model.
    pub model: String,
    /// The lowercase-hex SHA-256 of the prompt (the fixture key), never the prompt.
    pub prompt_sha256: String,
    /// The response status.
    pub status: u16,
    /// Whether no fixture matched.
    pub miss: bool,
}

/// The fake provider on a local port.
pub struct FakeProviderServer {
    url: String,
    served: Arc<Mutex<Vec<Served>>>,
    task: JoinHandle<()>,
}

impl FakeProviderServer {
    /// Serves `fake` on `127.0.0.1` at a free port.
    ///
    /// # Errors
    /// When the port cannot be bound.
    pub async fn start(fake: FakeProvider) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}", listener.local_addr()?);
        let served: Arc<Mutex<Vec<Served>>> = Arc::default();
        let state = Arc::new((fake, Arc::clone(&served)));
        let app = Router::new().fallback(move |request: Request| {
            let state = Arc::clone(&state);
            async move { answer(&state.0, &state.1, request).await }
        });
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok(Self { url, served, task })
    }

    /// The base URL, `http://127.0.0.1:<port>`, with no trailing slash.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Every request so far, in arrival order.
    #[must_use]
    pub fn served(&self) -> Vec<Served> {
        self.served.lock().expect("served log").clone()
    }

    /// The requests that matched no fixture.
    #[must_use]
    pub fn misses(&self) -> Vec<Served> {
        self.served().into_iter().filter(|s| s.miss).collect()
    }
}

impl Drop for FakeProviderServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The wire format a path belongs to.
fn wire_of(path: &str) -> Option<Wire> {
    if path.ends_with("/v1/messages") {
        Some(Wire::Anthropic)
    } else if path.ends_with("/chat/completions") {
        Some(Wire::OpenAi)
    } else if path.contains("/models/") && path.ends_with(":generateContent") {
        Some(Wire::Google)
    } else {
        None
    }
}

async fn answer(fake: &FakeProvider, served: &Mutex<Vec<Served>>, request: Request) -> Response {
    let path = request.uri().path().to_owned();
    let wire = wire_of(&path);
    let body: Bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
        .await
        .unwrap_or_default();
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let mut record = Served {
        wire,
        path: path.clone(),
        model: String::new(),
        prompt_sha256: String::new(),
        status: 404,
        miss: false,
    };
    let Some(wire) = wire else {
        served.lock().expect("served log").push(record);
        return StatusCode::NOT_FOUND.into_response();
    };
    let (model, prompt) = decode(wire, &path, &body);
    record.model.clone_from(&model);
    record.prompt_sha256 = hash_prompt(&prompt);
    let result = fake
        .complete(CompletionRequest {
            model: model.clone(),
            system: String::new(),
            prompt,
            temperature: 0.0,
            max_tokens: 0,
            settings: Map::new(),
        })
        .await;
    let (status, reply) = match result {
        Ok(completion) => (
            StatusCode::OK,
            encode(
                wire,
                &body,
                &model,
                &completion.text,
                completion.tokens_in,
                completion.tokens_out,
            ),
        ),
        Err(error) => {
            let (status, message) = match error.kind() {
                ProviderErrorKind::RateLimited => {
                    (StatusCode::TOO_MANY_REQUESTS, error.to_string())
                }
                ProviderErrorKind::InvalidRequest => (StatusCode::BAD_REQUEST, error.to_string()),
                ProviderErrorKind::ProviderError => {
                    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
                }
                ProviderErrorKind::Timeout => (StatusCode::GATEWAY_TIMEOUT, error.to_string()),
                // Only a missing fixture: answered 400 so the caller does not retry.
                ProviderErrorKind::Unknown => {
                    record.miss = true;
                    (StatusCode::BAD_REQUEST, error.to_string())
                }
            };
            (status, error_body(wire, status, &message))
        }
    };
    record.status = status.as_u16();
    served.lock().expect("served log").push(record);
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        reply.to_string(),
    )
        .into_response()
}

/// `(model, prompt)`: the prompt is the last user message's text.
fn decode(wire: Wire, path: &str, body: &Value) -> (String, String) {
    match wire {
        Wire::Anthropic | Wire::OpenAi => {
            let model = body["model"].as_str().unwrap_or_default().to_owned();
            let prompt = body["messages"]
                .as_array()
                .and_then(|messages| {
                    messages
                        .iter()
                        .rev()
                        .find(|message| message["role"] == "user")
                })
                .map(|message| text_of(&message["content"]))
                .unwrap_or_default();
            (model, prompt)
        }
        Wire::Google => {
            let model = path
                .rsplit_once("/models/")
                .map(|(_, rest)| rest.trim_end_matches(":generateContent").to_owned())
                .unwrap_or_default();
            let prompt = body["contents"]
                .as_array()
                .and_then(|contents| {
                    contents
                        .iter()
                        .rev()
                        .find(|content| content["role"].as_str().is_none_or(|r| r == "user"))
                })
                .map(|content| text_of(&content["parts"]))
                .unwrap_or_default();
            (model, prompt)
        }
    }
}

/// A content value's text: a string, or the concatenated `text` of its parts.
fn text_of(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect(),
        _ => String::new(),
    }
}

/// A successful reply in the request's wire format.
fn encode(
    wire: Wire,
    request: &Value,
    model: &str,
    text: &str,
    tokens_in: usize,
    tokens_out: usize,
) -> Value {
    match wire {
        Wire::Anthropic => {
            let block = match request["tool_choice"]["name"].as_str() {
                // A forced tool: the output is the tool call's input.
                Some(tool) => json!({
                    "type": "tool_use",
                    "id": "toolu_fake",
                    "name": tool,
                    "input": serde_json::from_str::<Value>(text)
                        .unwrap_or_else(|_| Value::String(text.to_owned())),
                }),
                None => json!({"type": "text", "text": text}),
            };
            json!({
                "id": "msg_fake",
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": [block],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": tokens_in, "output_tokens": tokens_out},
            })
        }
        Wire::OpenAi => json!({
            "id": "chatcmpl-fake",
            "object": "chat.completion",
            "created": 0,
            "model": model,
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": text},
                "finish_reason": "stop",
            }],
            "usage": {
                "prompt_tokens": tokens_in,
                "completion_tokens": tokens_out,
                "total_tokens": tokens_in + tokens_out,
            },
        }),
        Wire::Google => json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": text}]},
                "finishReason": "STOP",
            }],
            "usageMetadata": {
                "promptTokenCount": tokens_in,
                "candidatesTokenCount": tokens_out,
                "totalTokenCount": tokens_in + tokens_out,
            },
        }),
    }
}

/// An error body in the provider's shape.
fn error_body(wire: Wire, status: StatusCode, message: &str) -> Value {
    match wire {
        Wire::Anthropic => {
            let kind = match status {
                StatusCode::TOO_MANY_REQUESTS => "rate_limit_error",
                StatusCode::BAD_REQUEST => "invalid_request_error",
                _ => "api_error",
            };
            json!({"type": "error", "error": {"type": kind, "message": message}})
        }
        Wire::OpenAi => json!({"error": {"message": message, "type": "fake_error", "code": null}}),
        Wire::Google => json!({"error": {
            "code": status.as_u16(),
            "message": message,
            "status": match status {
                StatusCode::TOO_MANY_REQUESTS => "RESOURCE_EXHAUSTED",
                StatusCode::BAD_REQUEST => "INVALID_ARGUMENT",
                StatusCode::GATEWAY_TIMEOUT => "DEADLINE_EXCEEDED",
                _ => "INTERNAL",
            },
        }}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_pick_the_wire() {
        assert_eq!(wire_of("/v1/messages"), Some(Wire::Anthropic));
        assert_eq!(wire_of("/openai/chat/completions"), Some(Wire::OpenAi));
        assert_eq!(wire_of("/v1/chat/completions"), Some(Wire::OpenAi));
        assert_eq!(
            wire_of("/v1beta/models/gemini-3.1-pro:generateContent"),
            Some(Wire::Google)
        );
        assert_eq!(wire_of("/v1/embeddings"), None);
    }

    #[test]
    fn the_prompt_is_the_last_user_text() {
        let chat = json!({"model": "m", "messages": [
            {"role": "system", "content": "sys"},
            {"role": "user", "content": "first"},
            {"role": "assistant", "content": "a"},
            {"role": "user", "content": [{"type": "text", "text": "sec"}, {"type": "text", "text": "ond"}]},
        ]});
        assert_eq!(
            decode(Wire::OpenAi, "/chat/completions", &chat),
            ("m".to_owned(), "second".to_owned())
        );
        let google = json!({"contents": [{"parts": [{"text": "hello"}]}]});
        assert_eq!(
            decode(Wire::Google, "/v1beta/models/g-1:generateContent", &google),
            ("g-1".to_owned(), "hello".to_owned())
        );
    }
}
