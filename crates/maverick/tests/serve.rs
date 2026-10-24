//! `maverick serve` as a real process (reference Part 1 §7.3; bead 3.12): the startup
//! lines, both listeners, the three process shapes, SIGTERM, and a busy port.

use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use maverick_testdb::Schema;
use serde_json::Value;

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| {
    Schema::custom("engine + graphile_worker", |url| async move {
        maverick_store::migrate::migrate(&url).await?;
        Ok(())
    })
});

const INTERNAL_TOKEN: &str = "ops:alpha-token";

/// A port no other test in this binary was given. `bind(:0)` can return a port that an
/// earlier call released, and `serve` logs `listening` before it binds (reference Part 1
/// §7.3 step 11). So two tests could share a port, and one engine's request reached the
/// other engine's router (bead model-maverick-yve4).
fn free_port() -> u16 {
    static GIVEN: LazyLock<Mutex<HashSet<u16>>> = LazyLock::new(Mutex::default);
    for _ in 0..100 {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("bind")
            .local_addr()
            .expect("addr")
            .port();
        if GIVEN.lock().expect("lock").insert(port) {
            return port;
        }
    }
    panic!("no unused port after 100 tries");
}

/// A running `maverick serve` with its stdout lines collected.
struct Engine {
    child: Child,
    lines: Arc<Mutex<Vec<String>>>,
    arrivals: Receiver<String>,
    public: u16,
    internal: u16,
}

impl Engine {
    fn start(database_url: &str, flags: &[&str], public: u16, internal: u16) -> Engine {
        Self::start_role(database_url, flags, public, internal, "origin")
    }

    fn start_role(
        database_url: &str,
        flags: &[&str],
        public: u16,
        internal: u16,
        role: &str,
    ) -> Engine {
        let mut child = Command::new(env!("CARGO_BIN_EXE_maverick"))
            .arg("serve")
            .args(flags)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("TZ", "UTC")
            .env("MAVERICK_ROLE", role)
            .env("MAVERICK_DATABASE_URL", database_url)
            .env("MAVERICK_INTERNAL_TOKENS", INTERNAL_TOKEN)
            .env("MAVERICK_CAPTURE_MASTER_KEY", "ab".repeat(32))
            .env("MAVERICK_PUBLIC_ADDR", format!("127.0.0.1:{public}"))
            .env("MAVERICK_INTERNAL_ADDR", format!("127.0.0.1:{internal}"))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn maverick");
        let stdout = child.stdout.take().expect("stdout");
        let lines = Arc::new(Mutex::new(Vec::new()));
        let (sender, arrivals) = channel();
        let collected = Arc::clone(&lines);
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                collected.lock().expect("lock").push(line.clone());
                let _ = sender.send(line);
            }
        });
        Engine {
            child,
            lines,
            arrivals,
            public,
            internal,
        }
    }

    /// Waits for a log line whose `msg` is `msg` (and, if given, whose `listener` is
    /// `listener`).
    fn wait_for(&self, msg: &str, listener: Option<&str>) -> Value {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = self
                .arrivals
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("no {msg:?} line; got:\n{}", self.output()));
            let Ok(entry) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if entry["msg"] == msg && listener.is_none_or(|name| entry["listener"] == name) {
                return entry;
            }
        }
    }

    fn output(&self) -> String {
        self.lines.lock().expect("lock").join("\n")
    }

    fn messages(&self) -> Vec<String> {
        self.lines
            .lock()
            .expect("lock")
            .iter()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter_map(|entry| entry["msg"].as_str().map(str::to_owned))
            .collect()
    }

    fn terminate(&mut self) -> i32 {
        let status = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .expect("kill");
        assert!(status.success());
        self.wait()
    }

    fn wait(&mut self) -> i32 {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = self.child.try_wait().expect("wait") {
                // Let the reader thread drain the pipe.
                std::thread::sleep(Duration::from_millis(100));
                return status.code().expect("exit code");
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                panic!("serve did not exit; output:\n{}", self.output());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

async fn get(url: &str, token: Option<&str>) -> Result<(u16, String), reqwest::Error> {
    let client = reqwest::Client::new();
    let mut request = client.get(url);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = request.send().await?;
    let status = response.status().as_u16();
    Ok((status, response.text().await?))
}

/// The full shape: both listeners (internal first), workers, `/up`, internal health, and
/// a clean SIGTERM shutdown.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serve_starts_both_listeners_and_stops_on_sigterm() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let mut engine = Engine::start(db.url(), &[], free_port(), free_port());
    let starting = engine.wait_for("starting maverick", None);
    assert_eq!(starting["api_only"], false);
    assert_eq!(starting["worker_only"], false);
    engine.wait_for("workers started", None);
    let internal = engine.wait_for("listening", Some("internal"));
    assert_eq!(internal["addr"], format!("127.0.0.1:{}", engine.internal));
    engine.wait_for("listening", Some("public"));

    let (status, body) = get(&format!("http://127.0.0.1:{}/up", engine.public), None)
        .await
        .expect("public /up");
    assert_eq!(status, 200, "{body}");
    let (status, body) = get(
        &format!("http://127.0.0.1:{}/health/detail", engine.internal),
        Some(INTERNAL_TOKEN),
    )
    .await
    .expect("internal health");
    assert!(status == 200 || status == 503, "{status} {body}");
    let health: Value = serde_json::from_str(&body).expect("health json");
    assert!(health.is_object(), "{body}");

    assert_eq!(engine.terminate(), 0, "{}", engine.output());
    let messages = engine.messages();
    let position = |msg: &str| {
        messages
            .iter()
            .position(|m| m == msg)
            .unwrap_or_else(|| panic!("no {msg:?} in {messages:?}"))
    };
    assert!(position("starting maverick") < position("workers started"));
    assert!(position("workers started") < position("listening"));
    assert!(position("listening") < position("shutdown signal received"));
    assert!(position("shutdown signal received") < position("stopped"));
    assert_eq!(messages.last().map(String::as_str), Some("stopped"));
}

/// `--worker-only` serves the internal listener (health and operator surfaces) but no
/// public API.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_only_serves_only_the_internal_listener() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let mut engine = Engine::start(db.url(), &["--worker-only"], free_port(), free_port());
    engine.wait_for("workers started", None);
    engine.wait_for("listening", Some("internal"));
    let (status, _) = get(
        &format!("http://127.0.0.1:{}/health/detail", engine.internal),
        Some(INTERNAL_TOKEN),
    )
    .await
    .expect("internal health");
    assert!(status == 200 || status == 503);
    assert!(
        get(&format!("http://127.0.0.1:{}/up", engine.public), None)
            .await
            .is_err(),
        "the public port must not be served"
    );
    assert_eq!(engine.terminate(), 0, "{}", engine.output());
    let listeners: Vec<String> = engine
        .lines
        .lock()
        .expect("lock")
        .iter()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|entry| entry["msg"] == "listening")
        .filter_map(|entry| entry["listener"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(listeners, ["internal"]);
}

/// `--api-only` runs no workers, and an unreachable database does not fail startup:
/// connections are lazy.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_only_runs_no_workers_and_starts_without_a_database() {
    let unreachable = format!(
        "postgres://maverick:maverick@127.0.0.1:{}/maverick",
        free_port()
    );
    let mut engine = Engine::start(&unreachable, &["--api-only"], free_port(), free_port());
    engine.wait_for("listening", Some("public"));
    let (status, _) = get(&format!("http://127.0.0.1:{}/up", engine.public), None)
        .await
        .expect("public /up");
    // The listener answers; `/up` reports the database it cannot reach (lazy pool).
    assert_eq!(status, 503);
    assert_eq!(engine.terminate(), 0, "{}", engine.output());
    let messages = engine.messages();
    assert!(
        !messages.iter().any(|m| m == "workers started"),
        "{messages:?}"
    );
    // The classifier lookup needs the catalog: it logs, and serving continues.
    assert!(
        messages.iter().any(|m| m
            == "classifier column unavailable; prompt-path requests will receive the fallback classification"),
        "{messages:?}"
    );
}

/// A port already in use is a listener failure: exit 1 after a clean shutdown of the rest.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_busy_port_is_a_listener_failure() {
    let busy = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let public = busy.local_addr().expect("addr").port();
    let unreachable = format!(
        "postgres://maverick:maverick@127.0.0.1:{}/maverick",
        free_port()
    );
    let mut engine = Engine::start(&unreachable, &["--api-only"], public, free_port());
    let failed = engine.wait_for("listener failed", Some("public"));
    assert!(failed["error"].as_str().is_some_and(|e| !e.is_empty()));
    assert_eq!(engine.wait(), 1, "{}", engine.output());
    assert_eq!(
        engine.messages().last().map(String::as_str),
        Some("stopped")
    );
    drop(busy);
}

/// DIVERGENCE: D49. A replica starts both listeners and maintenance workers without judges.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replica_starts_without_resolving_the_judge_pool() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let mut engine = Engine::start_role(db.url(), &[], free_port(), free_port(), "replica");
    engine.wait_for("workers started", None);
    engine.wait_for("listening", Some("public"));
    let (status, body) = get(
        &format!("http://127.0.0.1:{}/health/detail", engine.internal),
        Some(INTERNAL_TOKEN),
    )
    .await
    .unwrap();
    assert_eq!(status, 200, "{body}");
    let health: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(health["role"], "replica");
    assert_eq!(health["active_matrix_version"], "");
    assert_eq!(engine.terminate(), 0, "{}", engine.output());
    assert!(
        !engine
            .messages()
            .iter()
            .any(|m| m.starts_with("judge pool unavailable")),
        "{}",
        engine.output()
    );
    db.close().await;
}
