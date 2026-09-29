//! The recorded script: `parity/blackbox/api-recorded.jsonl` and `api-seed-sql.jsonl`
//! merged by `gseq` (parity/blackbox/README.md §2, §3), with the truncated request bodies
//! rebuilt from their generators (§6).

use std::path::PathBuf;

use serde_json::Value;
use sha2::{Digest, Sha256};

/// Which handler the exchange went to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Listener {
    Public,
    Internal,
}

/// One recorded HTTP exchange.
#[derive(Debug, Clone)]
pub struct Exchange {
    pub test: String,
    pub seq: u64,
    pub gseq: u64,
    pub listener: Listener,
    pub method: String,
    pub path: String,
    /// Go's `r.ContentLength`: `-1` for a chunked body.
    pub content_length: i64,
    pub headers: Vec<(String, String)>,
    pub authorization: String,
    pub auth_kind: String,
    pub body: Vec<u8>,
    pub status: u16,
    pub response_headers: Vec<(String, String)>,
    pub response_body: String,
}

/// One SQL statement the Go test code ran.
#[derive(Debug, Clone)]
pub struct Statement {
    pub gseq: u64,
    pub test: String,
    pub name: String,
    pub sql: String,
    pub args: Vec<Value>,
}

/// The merged script, in `gseq` order.
#[derive(Debug, Clone)]
pub enum Event {
    Seed(Statement),
    Http(Box<Exchange>),
}

impl Event {
    pub fn test(&self) -> &str {
        match self {
            Event::Seed(statement) => &statement.test,
            Event::Http(exchange) => &exchange.test,
        }
    }

    fn gseq(&self) -> u64 {
        match self {
            Event::Seed(statement) => statement.gseq,
            Event::Http(exchange) => exchange.gseq,
        }
    }
}

fn blackbox(file: &str) -> Vec<Value> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../parity/blackbox")
        .join(file);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .lines()
        .map(|line| serde_json::from_str(line).expect("jsonl row"))
        .collect()
}

fn headers(value: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(map) = value.as_object() {
        for (name, values) in map {
            for value in values.as_array().into_iter().flatten() {
                out.push((name.clone(), value.as_str().unwrap_or_default().to_owned()));
            }
        }
    }
    out
}

/// A recorded body: the string itself, or a truncated body rebuilt from its generator.
fn body(test: &str, value: &Value) -> Vec<u8> {
    match value {
        Value::String(text) => text.clone().into_bytes(),
        Value::Object(truncated) => {
            let rebuilt = rebuild(test);
            let want_len = truncated["len"].as_u64().expect("len");
            assert_eq!(
                rebuilt.len() as u64,
                want_len,
                "{test}: rebuilt body length"
            );
            let sha: String = Sha256::digest(&rebuilt)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            assert_eq!(sha, truncated["sha256"], "{test}: rebuilt body sha256");
            rebuilt
        }
        other => panic!("{test}: body {other}"),
    }
}

/// README §6: the six truncated request bodies.
fn rebuild(test: &str) -> Vec<u8> {
    let repeat = |n: usize| "a".repeat(n);
    match test {
        "TestV1OversizeBody413" => repeat(2_097_153).into_bytes(),
        "TestV1BodyAtCapIsNotRejected" => repeat(2_097_152).into_bytes(),
        "TestConfigOversizeBodyDeclaredLength413" | "TestConfigOversizeBodyChunked413" => format!(
            r#"{{"pools": [{{"name": "{}", "model_column_ids": ["x"], "is_default": true}}]}}"#,
            repeat(3_145_728)
        )
        .into_bytes(),
        "TestRecommendPromptOverCharLimit422" => {
            format!(r#"{{"workload":{{"prompt":"{}"}}}}"#, repeat(500_001)).into_bytes()
        }
        "TestRecommendPromptAtCharLimitSurfacesTruncated" => {
            format!(r#"{{"workload":{{"prompt":"{}"}}}}"#, repeat(500_000)).into_bytes()
        }
        other => panic!("{other}: no generator for a truncated body"),
    }
}

/// Every event, in `gseq` order.
pub fn load() -> Vec<Event> {
    let mut events: Vec<Event> = Vec::new();
    for row in blackbox("api-recorded.jsonl") {
        let test = row["test"].as_str().unwrap().to_owned();
        let request = &row["request"];
        let response = &row["response"];
        let response_body = match &response["body"] {
            Value::String(text) => text.clone(),
            other => panic!("{test}: truncated response body {other}"),
        };
        events.push(Event::Http(Box::new(Exchange {
            seq: row["seq"].as_u64().unwrap(),
            gseq: row["gseq"].as_u64().unwrap(),
            listener: match row["listener"].as_str().unwrap() {
                "public" => Listener::Public,
                "internal" => Listener::Internal,
                other => panic!("listener {other}"),
            },
            method: request["method"].as_str().unwrap().to_owned(),
            path: request["path"].as_str().unwrap().to_owned(),
            content_length: request["content_length"].as_i64().unwrap(),
            headers: headers(&request["headers"]),
            authorization: request["authorization"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            auth_kind: request["auth_kind"].as_str().unwrap().to_owned(),
            body: body(&test, &request["body"]),
            status: u16::try_from(response["status"].as_u64().unwrap()).unwrap(),
            response_headers: headers(&response["headers"]),
            response_body,
            test,
        })));
    }
    for row in blackbox("api-seed-sql.jsonl") {
        events.push(Event::Seed(Statement {
            gseq: row["gseq"].as_u64().unwrap(),
            test: row["test"].as_str().unwrap().to_owned(),
            name: row["sqlc_name"].as_str().unwrap().to_owned(),
            sql: row["sql"].as_str().unwrap().to_owned(),
            args: row["args"].as_array().cloned().unwrap_or_default(),
        }));
    }
    events.sort_by_key(Event::gseq);
    events
}
