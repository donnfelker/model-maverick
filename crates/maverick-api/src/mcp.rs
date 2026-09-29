//! The MCP server (reference Part 5 §9, spec §12): three tools that wrap the same
//! services as the REST endpoints, over streamable HTTP at `/mcp` on the public listener
//! and over stdio (`maverick mcp-stdio`).
//!
//! The wire bytes are the Go server's (`parity/fixtures/api/mcp.json` and the recorded
//! exchanges): each JSON-RPC response is one `event: message` SSE frame, and the JSON is
//! written in the Go SDK's key order. So the transport and the JSON-RPC framing are
//! written here, on one dispatcher shared by both transports, rather than taken from a
//! library whose framing and field order differ. Argument validation against the input
//! schemas uses the `jsonschema` crate; its text follows the `validating "arguments": `
//! prefix (D34).

use std::collections::HashSet;
use std::sync::{Arc, LazyLock, Mutex};

use axum::body::Body;
use axum::extract::{Extension, Request, State};
use axum::http::{HeaderValue, Method, Response, StatusCode, header};
use chrono::Utc;
use maverick_domain::gojson::{self, JsonValue};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::observe::RequestLog;
use crate::{ApiState, Identity, RecommendError, read_body};

/// The server identity.
pub const SERVER_NAME: &str = "model-maverick";
pub const SERVER_VERSION: &str = "v1";

/// The protocol versions this server speaks, newest first. A client asking for another
/// gets the newest.
pub const PROTOCOL_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// The `tools/list` result, byte for byte as the Go server sent it (tools sorted by name;
/// the descriptions and input schemas are the contract).
pub const TOOLS_LIST: &str = r#"{"tools":[{"description":"Read the banded policy table (which model column serves each capability and difficulty band, with ceilings and pass rates), mirroring GET /v1/policies.","inputSchema":{"type":"object","properties":{"capability":{"type":"string","description":"one of the ten v1 capabilities; absent returns all ten"},"pool":{"type":"string","description":"pool to report on, default the tenant default pool"}},"additionalProperties":false},"name":"get_policy"},{"description":"List the model column catalog visible to the tenant (provider, model, endpoint, settings, status, pricing, deprecation), mirroring GET /v1/models.","inputSchema":{"type":"object","additionalProperties":false},"name":"list_models"},{"description":"Recommend the best model column for a workload, mirroring POST /v1/recommend. Prefer the declared workload form ({capability, difficulty?, domain_tags?}) whenever you already know the capability: it is faster and nothing leaves the session. Use the prompt form ({prompt}) only when the capability is unknown and classification is needed.","inputSchema":{"type":"object","properties":{"workload":{"type":["null","object"],"properties":{"capability":{"type":["null","string"],"description":"declared form: one of the ten v1 capabilities"},"difficulty":{"type":["null","integer"],"description":"declared form: difficulty tier 1-5, default 3"},"domain_tags":{"type":["null","array"],"items":{"type":"string"},"description":"declared form: domain tags such as legal or medical"},"prompt":{"type":["null","string"],"description":"prompt form: the raw prompt to classify"}},"additionalProperties":false},"pool":{"type":"string","description":"pool to recommend from, default the tenant default pool"},"allowed_models":{"type":["null","array"],"items":{"type":"string"},"description":"restrict candidates to these model ids"},"mode":{"type":"string","description":"quality, balanced, or cost; default balanced"},"constraints":{"type":["null","object"],"properties":{"max_cost_per_run":{"type":["null","number"],"description":"preference: maximum cost per run in USD"},"max_latency_ms":{"type":["null","integer"],"description":"preference: maximum p95 latency in milliseconds","minimum":-2147483648,"maximum":2147483647}},"additionalProperties":false}},"required":["workload"],"additionalProperties":false},"name":"recommend_model"}]}"#;

/// The header that carries the session id.
pub const SESSION_HEADER: &str = "mcp-session-id";

/// The live streamable-HTTP sessions of this process.
#[derive(Clone, Default)]
pub struct McpSessions(Arc<Mutex<HashSet<String>>>);

impl std::fmt::Debug for McpSessions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("McpSessions")
    }
}

impl McpSessions {
    fn open(&self) -> String {
        let id = session_id();
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id.clone());
        id
    }

    fn contains(&self, id: &str) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains(id)
    }

    fn close(&self, id: &str) -> bool {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).remove(id)
    }
}

/// 16 random bytes as 26 characters of RFC 4648 base32 without padding.
fn session_id() -> String {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let bytes: [u8; 16] = rand::random();
    let mut out = String::with_capacity(26);
    let mut buffer: u32 = 0;
    let mut bits = 0;
    for byte in bytes {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(char::from(ALPHABET[((buffer >> bits) & 31) as usize]));
        }
    }
    if bits > 0 {
        out.push(char::from(ALPHABET[((buffer << (5 - bits)) & 31) as usize]));
    }
    out
}

// ---- JSON-RPC ----

/// One incoming JSON-RPC message.
pub struct Message {
    method: Option<String>,
    id: Option<JsonValue>,
    params: Option<JsonValue>,
}

impl Message {
    /// Parses one message; `None` when it is not a JSON object.
    pub fn parse(text: &str) -> Option<Message> {
        let JsonValue::Object(members) = gojson::unmarshal(text).ok()? else {
            return None;
        };
        let find = |name: &str| {
            members
                .iter()
                .rev()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };
        Some(Message {
            method: match find("method") {
                Some(JsonValue::String(method)) => Some(method),
                _ => None,
            },
            id: find("id").filter(|id| !matches!(id, JsonValue::Null)),
            params: find("params"),
        })
    }

    /// A request expects a response; a notification (or a client response) does not.
    pub fn is_request(&self) -> bool {
        self.method.is_some() && self.id.is_some()
    }

    fn param(&self, name: &str) -> Option<&JsonValue> {
        match &self.params {
            Some(JsonValue::Object(members)) => members
                .iter()
                .rev()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value),
            _ => None,
        }
    }
}

fn response(id: &JsonValue, result: &str) -> String {
    format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{result}}}",
        gojson::marshal(id)
    )
}

fn error_response(id: &JsonValue, code: i64, message: &str) -> String {
    let mut text = String::new();
    gojson::write_string(&mut text, message);
    format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{},\"error\":{{\"code\":{code},\"message\":{text}}}}}",
        gojson::marshal(id)
    )
}

/// JSON-RPC: the method does not exist.
const METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC: invalid method parameters.
const INVALID_PARAMS: i64 = -32602;

/// Answers one request (the caller has checked [`Message::is_request`]). `identity` is
/// the tenant the message acts as: its own HTTP request's key, or the stdio session's.
/// The recommendation id of a `recommend_model` call is reported through `log`.
pub async fn dispatch(
    api: &ApiState,
    identity: Option<&Identity>,
    message: &Message,
    log: Option<&RequestLog>,
) -> String {
    let id = message.id.clone().unwrap_or(JsonValue::Null);
    let method = message.method.as_deref().unwrap_or_default();
    match method {
        "initialize" => {
            let asked = match message.param("protocolVersion") {
                Some(JsonValue::String(version)) => version.as_str(),
                _ => "",
            };
            let version = PROTOCOL_VERSIONS
                .iter()
                .find(|v| **v == asked)
                .unwrap_or(&PROTOCOL_VERSIONS[0]);
            response(
                &id,
                &format!(
                    "{{\"capabilities\":{{\"logging\":{{}},\"tools\":{{\"listChanged\":true}}}},\"protocolVersion\":\"{version}\",\"serverInfo\":{{\"name\":\"{SERVER_NAME}\",\"version\":\"{SERVER_VERSION}\"}}}}"
                ),
            )
        }
        "ping" | "logging/setLevel" => response(&id, "{}"),
        "tools/list" => response(&id, TOOLS_LIST),
        "tools/call" => {
            let name = match message.param("name") {
                Some(JsonValue::String(name)) => name.clone(),
                _ => return error_response(&id, INVALID_PARAMS, "missing tool name"),
            };
            let arguments = message
                .param("arguments")
                .cloned()
                .unwrap_or_else(|| JsonValue::Object(Vec::new()));
            match call_tool(api, identity, &name, &arguments, log).await {
                Some(result) => response(&id, &result),
                None => error_response(&id, INVALID_PARAMS, &format!("unknown tool {name:?}")),
            }
        }
        other => error_response(&id, METHOD_NOT_FOUND, &format!("method not found: {other}")),
    }
}

// ---- tools ----

/// A tool's input schema, compiled from [`TOOLS_LIST`].
static VALIDATORS: LazyLock<Vec<(String, jsonschema::Validator)>> = LazyLock::new(|| {
    let list: serde_json::Value = serde_json::from_str(TOOLS_LIST).expect("TOOLS_LIST is JSON");
    list["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|tool| {
            let name = tool["name"].as_str().expect("tool name").to_owned();
            let validator = jsonschema::options()
                .should_validate_formats(false)
                .build(&tool["inputSchema"])
                .expect("input schema compiles");
            (name, validator)
        })
        .collect()
});

/// `validating "arguments": ` then each violation's instance path (`root` for the whole
/// object) and the validator's text, which names the property (D34).
fn validate(name: &str, arguments: &JsonValue) -> Option<Result<(), String>> {
    let (_, validator) = VALIDATORS.iter().find(|(tool, _)| tool == name)?;
    let instance = match arguments.to_serde() {
        Ok(value) => value,
        Err(error) => return Some(Err(format!("validating \"arguments\": {error}"))),
    };
    let errors: Vec<String> = validator
        .iter_errors(&instance)
        .map(|error| {
            let path = error.instance_path().to_string();
            let path = if path.is_empty() {
                "root".to_owned()
            } else {
                path
            };
            format!("{path}: {error}")
        })
        .collect();
    Some(if errors.is_empty() {
        Ok(())
    } else {
        Err(format!("validating \"arguments\": {}", errors.join("; ")))
    })
}

fn tool_error(text: &str) -> String {
    let mut escaped = String::new();
    gojson::write_string(&mut escaped, text);
    format!("{{\"content\":[{{\"type\":\"text\",\"text\":{escaped}}}],\"isError\":true}}")
}

/// A REST body (compact JSON and `\n`) as the tool result: its text, and the same JSON as
/// the structured content.
fn tool_success(body: &str) -> String {
    let json = body.strip_suffix('\n').unwrap_or(body);
    let mut text = String::new();
    gojson::write_string(&mut text, json);
    format!("{{\"content\":[{{\"type\":\"text\",\"text\":{text}}}],\"structuredContent\":{json}}}")
}

/// Runs a tool. `None` for an unknown tool.
async fn call_tool(
    api: &ApiState,
    identity: Option<&Identity>,
    name: &str,
    arguments: &JsonValue,
    log: Option<&RequestLog>,
) -> Option<String> {
    if let Err(error) = validate(name, arguments)? {
        return Some(tool_error(&error));
    }
    let Some(identity) = identity else {
        return Some(tool_error(
            "unauthorized: no authenticated tenant on this session",
        ));
    };
    let Some(pool) = api.pool() else {
        return Some(internal(name, "no database"));
    };
    Some(match name {
        "recommend_model" => {
            let body = gojson::marshal(arguments);
            match crate::recommend(pool, api.recommend(), identity, body.as_bytes()).await {
                Ok(recommended) => {
                    if let Some(log) = log {
                        log.set_recommendation_id(&recommended.id);
                    }
                    tool_success(&recommended.body)
                }
                Err(RecommendError::Api(error)) => {
                    tool_error(&format!("{}: {}", error.code.as_str(), error.message))
                }
                Err(RecommendError::Internal(error)) => internal(name, &error),
            }
        }
        "get_policy" => {
            let mut query = Vec::new();
            for key in ["capability", "pool"] {
                if let Some(JsonValue::String(value)) = member(arguments, key) {
                    query.push(format!("{key}={}", query_escape(value)));
                }
            }
            match crate::policies::policies(
                pool,
                api.recommend().ceiling_buffer,
                &identity.tenant_id,
                &query.join("&"),
            )
            .await
            {
                Ok(body) => tool_success(&body),
                Err(crate::policies::Failure::Api(error)) => {
                    tool_error(&format!("{}: {}", error.code.as_str(), error.message))
                }
                Err(crate::policies::Failure::Internal(error)) => internal(name, &error),
            }
        }
        _ => match crate::models::models(pool, Utc::now()).await {
            Ok(body) => tool_success(&body),
            Err(error) => internal(name, &error),
        },
    })
}

fn member<'a>(value: &'a JsonValue, name: &str) -> Option<&'a JsonValue> {
    match value {
        JsonValue::Object(members) => members
            .iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value),
        _ => None,
    }
}

/// Percent-encodes a query value (every byte outside the unreserved set).
fn query_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn internal(tool: &str, error: &str) -> String {
    tracing::error!(tool, error, "mcp tool failure");
    tool_error("internal error")
}

// ---- streamable HTTP ----

fn sse(frame: &str, session: Option<&str>) -> Response<Body> {
    let mut response = Response::new(Body::from(format!("event: message\ndata: {frame}\n\n")));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-transform"),
    );
    headers.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
    if let Some(session) = session.and_then(|s| HeaderValue::from_str(s).ok()) {
        headers.insert(SESSION_HEADER, session);
    }
    response
}

fn plain(status: StatusCode, text: &str) -> Response<Body> {
    let mut response = Response::new(Body::from(format!("{text}\n")));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}

/// `/mcp` (reference §9.3), behind Auth, RateLimit, and BodyLimit. Every message acts as
/// the key on its own HTTP request.
pub(crate) async fn mcp_handler(
    State(api): State<ApiState>,
    identity: Option<Extension<Identity>>,
    log: Option<Extension<RequestLog>>,
    request: Request,
) -> Response<Body> {
    let session = request
        .headers()
        .get(SESSION_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let sessions = api.mcp_sessions().clone();
    match *request.method() {
        Method::DELETE => match session {
            Some(id) if sessions.close(&id) => {
                let mut response = Response::new(Body::empty());
                *response.status_mut() = StatusCode::NO_CONTENT;
                response
            }
            Some(_) => plain(StatusCode::NOT_FOUND, "session not found"),
            None => plain(StatusCode::BAD_REQUEST, "missing Mcp-Session-Id header"),
        },
        Method::POST => {
            let body = match read_body(request.into_body()).await {
                Ok(body) => body,
                Err(response) => return response,
            };
            let Some(message) = Message::parse(&String::from_utf8_lossy(&body)) else {
                return plain(StatusCode::BAD_REQUEST, "invalid JSON-RPC message");
            };
            let identity = identity.as_ref().map(|Extension(identity)| identity);
            let log = log.as_ref().map(|Extension(log)| log);
            if message.method.as_deref() == Some("initialize") {
                let id = sessions.open();
                let frame = dispatch(&api, identity, &message, log).await;
                return sse(&frame, Some(&id));
            }
            match session {
                Some(id) if sessions.contains(&id) => {}
                Some(_) => return plain(StatusCode::NOT_FOUND, "session not found"),
                None => {
                    return plain(StatusCode::BAD_REQUEST, "missing Mcp-Session-Id header");
                }
            }
            if !message.is_request() {
                let mut response = Response::new(Body::empty());
                *response.status_mut() = StatusCode::ACCEPTED;
                return response;
            }
            let frame = dispatch(&api, identity, &message, log).await;
            sse(&frame, None)
        }
        _ => {
            let mut response = plain(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
            response
                .headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static("POST, DELETE"));
            response
        }
    }
}

// ---- stdio ----

/// JSON-RPC: the message did not parse.
const PARSE_ERROR: i64 = -32700;

/// Serves newline-delimited JSON-RPC from `reader` to `writer` until end of input (the
/// stdio transport, reference §9.4). Every message acts as `identity`, bound at startup;
/// `writer` carries nothing but responses.
pub async fn serve_lines<R, W>(
    api: &ApiState,
    identity: &Identity,
    reader: R,
    mut writer: W,
) -> std::io::Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut lines = reader.lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let frame = match Message::parse(&line) {
            Some(message) if message.is_request() => {
                dispatch(api, Some(identity), &message, None).await
            }
            Some(_) => continue,
            None => error_response(&JsonValue::Null, PARSE_ERROR, "parse error"),
        };
        writer.write_all(frame.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids_are_26_base32_characters() {
        let id = session_id();
        assert_eq!(id.len(), 26);
        assert!(
            id.bytes()
                .all(|b| b.is_ascii_uppercase() || (b'2'..=b'7').contains(&b))
        );
        assert_ne!(id, session_id());
    }

    #[test]
    fn every_tool_schema_compiles() {
        let names: Vec<&str> = VALIDATORS.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["get_policy", "list_models", "recommend_model"]);
    }

    #[test]
    fn query_values_are_escaped() {
        assert_eq!(query_escape("a b&c=d"), "a%20b%26c%3Dd");
        assert_eq!(query_escape("factual_qa"), "factual_qa");
    }
}
