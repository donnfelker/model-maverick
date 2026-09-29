//! Shared pieces of the process-level tests (`tests/full_stack.rs`, `tests/latency.rs`):
//! where the workspace and the built binaries are, numbered step logging, logged HTTP
//! exchanges, and small database polls.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use reqwest::header::HeaderMap;
use serde_json::Value;
use sqlx::PgPool;

/// The workspace root.
///
/// # Panics
/// Never for this crate's layout (`crates/e2e`).
#[must_use]
pub fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the workspace root")
        .to_path_buf()
}

/// The `maverick` and `maverick-proxy` binaries: in `MAVERICK_E2E_BIN_DIR` (set by the
/// scripts after building them), or in the profile directory above the running test
/// binary (`target/<profile>/deps/..`).
///
/// # Panics
/// When a binary is missing, with the command that builds it.
#[must_use]
pub fn binaries() -> (PathBuf, PathBuf) {
    let dir = std::env::var_os("MAVERICK_E2E_BIN_DIR").map_or_else(
        || {
            std::env::current_exe()
                .expect("test binary path")
                .parent()
                .and_then(Path::parent)
                .expect("target dir")
                .to_path_buf()
        },
        PathBuf::from,
    );
    let engine = dir.join("maverick");
    let proxy = dir.join("maverick-proxy");
    for bin in [&engine, &proxy] {
        assert!(
            bin.exists(),
            "{} is missing: run the test's script (scripts/e2e.sh or scripts/latency.sh), or \
             `cargo build -p maverick -p maverick-proxy` in the same profile first",
            bin.display()
        );
    }
    (engine, proxy)
}

/// Numbered steps with elapsed times. `MAVERICK_E2E_BREAK=<n>` fails step `n` on purpose.
pub struct Steps {
    number: usize,
    started: Instant,
    step_started: Instant,
    broken: Option<usize>,
}

impl Default for Steps {
    fn default() -> Self {
        Self::new()
    }
}

impl Steps {
    /// Starts the clock.
    #[must_use]
    pub fn new() -> Self {
        Self {
            number: 0,
            started: Instant::now(),
            step_started: Instant::now(),
            broken: std::env::var("MAVERICK_E2E_BREAK")
                .ok()
                .and_then(|step| step.parse().ok()),
        }
    }

    /// Closes the previous step and prints the next header.
    ///
    /// # Panics
    /// When this is the step `MAVERICK_E2E_BREAK` names.
    pub fn begin(&mut self, title: &str) {
        if self.number > 0 {
            eprintln!(
                "    ok in {:.2}s",
                self.step_started.elapsed().as_secs_f64()
            );
        }
        self.number += 1;
        self.step_started = Instant::now();
        eprintln!(
            "\n=== [{:02}] {title}  (t+{:.1}s)",
            self.number,
            self.started.elapsed().as_secs_f64()
        );
        assert!(
            self.broken != Some(self.number),
            "step {} broken on purpose (MAVERICK_E2E_BREAK)",
            self.number
        );
    }

    /// One indented line under the current step.
    pub fn note(&self, text: impl AsRef<str>) {
        eprintln!("    {}", text.as_ref());
    }

    /// Time since the run started.
    #[must_use]
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }
}

/// One HTTP exchange.
pub struct Reply {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: String,
}

impl Reply {
    /// The body as JSON.
    ///
    /// # Panics
    /// When it is not JSON.
    #[must_use]
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|error| panic!("not JSON ({error}): {}", self.body))
    }

    /// A response header.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }

    /// The JSON-RPC message of an MCP reply (one SSE frame).
    ///
    /// # Panics
    /// When the body is not one SSE frame of JSON.
    #[must_use]
    pub fn frame(&self) -> Value {
        let data = self
            .body
            .strip_prefix("event: message\ndata: ")
            .and_then(|rest| rest.strip_suffix("\n\n"))
            .unwrap_or_else(|| panic!("not one SSE frame: {:?}", self.body));
        serde_json::from_str(data).expect("SSE JSON")
    }
}

/// One request, logged under the current step with its status and time.
///
/// # Panics
/// When the request cannot be sent.
pub async fn http(
    steps: &Steps,
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<&Value>,
) -> Reply {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("http client");
    let mut request = client.request(method.parse().expect("method"), url);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    if let Some(body) = body {
        request = request
            .header("content-type", "application/json")
            .body(body.to_string());
    }
    let started = Instant::now();
    let response = request
        .send()
        .await
        .unwrap_or_else(|error| panic!("{method} {url}: {error}"));
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let body = response.text().await.expect("body");
    steps.note(format!(
        "{method} {url} -> {status} in {} ms",
        started.elapsed().as_millis()
    ));
    Reply {
        status,
        headers,
        body,
    }
}

/// Asserts a reply's status, showing the body when it differs.
///
/// # Panics
/// When the status differs.
pub fn expect(reply: &Reply, status: u16, what: &str) {
    assert_eq!(reply.status, status, "{what}: {}", reply.body);
}

/// `select count(*) ...`.
///
/// # Panics
/// When the query fails.
pub async fn count(pool: &PgPool, sql: &str) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .fetch_one(pool)
        .await
        .unwrap_or_else(|error| panic!("{sql}: {error}"))
}

/// Polls `check` until it holds or `timeout` passes.
pub async fn eventually<F, Fut>(timeout: Duration, mut check: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if check().await {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}
