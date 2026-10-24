//! A single-tenant replica through the binary (spec §14.4, D49): the first `serve` start
//! bootstraps one tenant and one key, prints the key once on stderr and writes the key
//! file, a later start creates nothing, and `maverick key create | list | revoke` manage
//! that tenant's keys with the internal API's audit lines.

use std::collections::HashSet;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use maverick_cli::{Output, run};
use maverick_domain::keys;
use maverick_store::tenancy;
use maverick_testdb::Schema;
use serde_json::Value;
use sqlx::PgPool;

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| {
    Schema::custom("engine + graphile_worker", |url| async move {
        maverick_store::migrate::migrate(&url).await?;
        Ok(())
    })
});

/// A port no other test in this binary was given (see `tests/serve.rs`).
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

/// A running replica `serve`, with stdout lines collected and stderr read at the end.
struct Replica {
    child: Child,
    lines: Arc<Mutex<Vec<String>>>,
    arrivals: Receiver<String>,
}

impl Replica {
    fn start(database_url: &str, key_file: &PathBuf) -> Replica {
        let mut child = Command::new(env!("CARGO_BIN_EXE_maverick"))
            .arg("serve")
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("TZ", "UTC")
            .env("MAVERICK_ROLE", "replica")
            .env("MAVERICK_DATABASE_URL", database_url)
            .env("MAVERICK_INTERNAL_TOKENS", "ops:alpha-token")
            .env("MAVERICK_CAPTURE_MASTER_KEY", "ab".repeat(32))
            .env("MAVERICK_PUBLIC_ADDR", format!("127.0.0.1:{}", free_port()))
            .env(
                "MAVERICK_INTERNAL_ADDR",
                format!("127.0.0.1:{}", free_port()),
            )
            .env("MAVERICK_TENANT_NAME", "Acme Corp")
            .env("MAVERICK_BOOTSTRAP_KEY_FILE", key_file)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
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
        Replica {
            child,
            lines,
            arrivals,
        }
    }

    fn output(&self) -> String {
        self.lines.lock().expect("lock").join("\n")
    }

    fn wait_for(&self, msg: &str, listener: Option<&str>) {
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
                return;
            }
        }
    }

    /// SIGTERM, then the exit code and everything the process wrote to stderr.
    fn stop(mut self) -> (i32, String, String) {
        let status = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .expect("kill");
        assert!(status.success());
        let code = self.child.wait().expect("wait").code().expect("exit code");
        std::thread::sleep(Duration::from_millis(100));
        let mut stderr = String::new();
        self.child
            .stderr
            .take()
            .expect("stderr")
            .read_to_string(&mut stderr)
            .expect("read stderr");
        (code, self.output(), stderr)
    }
}

impl Drop for Replica {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

fn key_file(test: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("single-tenant-{test}-{}.key", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

/// DIVERGENCE: D49. The first replica start creates one tenant, named by
/// `MAVERICK_TENANT_NAME`, and one key. The key is printed once on stderr, in no log
/// line, and written to the 0600 key file. A second start creates nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_first_replica_start_bootstraps_once() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let path = key_file("first-start");

    let replica = Replica::start(db.url(), &path);
    replica.wait_for("listening", Some("public"));
    let (code, stdout, stderr) = replica.stop();
    assert_eq!(code, 0, "{stdout}\n{stderr}");

    let tenants = tenancy::list_tenants(&pool).await.unwrap();
    assert_eq!(tenants.len(), 1, "{stdout}");
    assert_eq!(tenants[0].name, "Acme Corp");
    let stored = tenancy::list_api_keys_by_tenant(&pool, &tenants[0].id)
        .await
        .unwrap();
    assert_eq!(stored.len(), 1);

    let written = std::fs::read_to_string(&path).expect("key file");
    let raw = written.strip_suffix('\n').expect("one line");
    assert_eq!(keys::key_hash(raw).to_vec(), stored[0].key_hash);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let printed: Vec<&str> = stderr.lines().filter(|line| line.contains(raw)).collect();
    assert_eq!(
        printed,
        [format!(
            "maverick: created tenant \"Acme Corp\" ({}) and its first API key, shown only once: {raw}",
            tenants[0].id
        )],
        "{stderr}"
    );
    assert!(
        !stdout.contains(raw),
        "the key reached a log line:\n{stdout}"
    );
    let logged: Vec<Value> = stdout
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let actions: Vec<&str> = logged
        .iter()
        .filter(|line| line["msg"] == "internal provisioning write")
        .map(|line| {
            assert_eq!(line["actor"], "bootstrap", "{line}");
            line["action"].as_str().unwrap()
        })
        .collect();
    assert_eq!(actions, ["tenant.create", "api_key.issue"]);
    assert!(
        logged
            .iter()
            .any(|line| line["msg"] == "bootstrap key written"
                && line["path"] == path.to_str().unwrap()),
        "{stdout}"
    );

    // A later start creates nothing and prints no key.
    let replica = Replica::start(db.url(), &path);
    replica.wait_for("listening", Some("public"));
    let (code, stdout, stderr) = replica.stop();
    assert_eq!(code, 0, "{stdout}\n{stderr}");
    assert!(!stderr.contains("created tenant"), "{stderr}");
    assert!(!stdout.contains("internal provisioning write"), "{stdout}");
    assert_eq!(tenancy::list_tenants(&pool).await.unwrap().len(), 1);
    assert_eq!(
        tenancy::list_api_keys_by_tenant(&pool, &tenants[0].id)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), written);
    let _ = std::fs::remove_file(&path);
    db.close().await;
}

#[derive(Clone)]
struct Shared(Arc<Mutex<Vec<u8>>>);

impl Write for Shared {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Once per binary, a global subscriber, so a `key` command's scoped logger never misses
/// an audit callsite that a thread with no subscriber registered first (bead 3bgl).
fn every_callsite_enabled() {
    static GLOBAL: std::sync::Once = std::sync::Once::new();
    GLOBAL.call_once(|| {
        let _ = tracing::subscriber::set_global_default(tracing_subscriber::registry());
    });
}

/// Runs `maverick key ...` in-process with `MAVERICK_ROLE=role`: exit code, stdout,
/// stderr. `run` builds its own runtime, so it runs on a blocking thread.
async fn key(database_url: &str, role: &str, args: &[&str]) -> (i32, String, String) {
    let database_url = database_url.to_owned();
    let role = role.to_owned();
    let args: Vec<String> = std::iter::once("key")
        .chain(args.iter().copied())
        .map(str::to_owned)
        .collect();
    tokio::task::spawn_blocking(move || key_blocking(&database_url, &role, &args))
        .await
        .unwrap()
}

fn key_blocking(database_url: &str, role: &str, args: &[String]) -> (i32, String, String) {
    let out = Arc::new(Mutex::new(Vec::new()));
    let err = Arc::new(Mutex::new(Vec::new()));
    let env = [
        ("MAVERICK_ROLE", role.to_owned()),
        ("MAVERICK_DATABASE_URL", database_url.to_owned()),
        ("MAVERICK_INTERNAL_TOKENS", "ops:alpha-token".to_owned()),
        ("MAVERICK_CAPTURE_MASTER_KEY", "ab".repeat(32)),
        ("USER", "tester".to_owned()),
    ];
    let lookup = |name: &str| {
        env.iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.clone())
    };
    let code = run(
        args,
        &lookup,
        Output::new(Shared(out.clone())),
        Output::new(Shared(err.clone())),
    );
    let read = |buffer: &Arc<Mutex<Vec<u8>>>| {
        String::from_utf8(buffer.lock().expect("lock").clone()).expect("utf8")
    };
    (code, read(&out), read(&err))
}

/// The audit lines on a key command's stderr: (action, actor, api_key_id).
fn audits(stderr: &str) -> Vec<(String, String, String)> {
    stderr
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| line["msg"] == "internal provisioning write")
        .map(|line| {
            let text = |key: &str| line[key].as_str().unwrap_or_default().to_owned();
            (text("action"), text("actor"), text("api_key_id"))
        })
        .collect()
}

async fn bootstrap(pool: &PgPool) -> maverick_api::provision::Bootstrapped {
    let mut conn = pool.acquire().await.unwrap();
    maverick_api::provision::bootstrap(&mut conn, "default")
        .await
        .unwrap()
        .unwrap()
}

/// DIVERGENCE: D49. `key create | list | revoke` manage the one tenant's keys, with the
/// internal API's JSON and audit lines and actor `cli:<os user>`. Revoke is idempotent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn key_commands_manage_the_one_tenants_keys() {
    every_callsite_enabled();
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let url = db.url().to_owned();

    let (code, _, stderr) = key(&url, "replica", &["list"]).await;
    assert_eq!(code, 1);
    assert!(
        stderr.ends_with(
            "maverick key: this install has no tenant yet; start maverick serve once to create it\n"
        ),
        "{stderr}"
    );
    let first = bootstrap(&pool).await;

    let (code, stdout, stderr) = key(&url, "replica", &["create", "--name", "billing-app"]).await;
    assert_eq!(code, 0, "{stderr}");
    let issued: Value = serde_json::from_str(stdout.trim_end()).unwrap();
    let (id, raw) = (
        issued["id"].as_str().unwrap().to_owned(),
        issued["key"].as_str().unwrap().to_owned(),
    );
    assert_eq!(stdout, format!("{{\"id\":\"{id}\",\"key\":\"{raw}\"}}\n"));
    assert!(
        !stderr.contains(&raw),
        "the key reached a log line:\n{stderr}"
    );
    assert_eq!(
        audits(&stderr),
        [(
            "api_key.issue".to_owned(),
            "cli:tester".to_owned(),
            id.clone()
        )]
    );
    let stored = tenancy::get_api_key_by_hash(&pool, &keys::key_hash(&raw))
        .await
        .unwrap();
    assert_eq!(
        (stored.tenant_id.as_str(), stored.name.as_str()),
        (first.tenant.id.as_str(), "billing-app")
    );

    let (code, stdout, stderr) = key(&url, "replica", &["list"]).await;
    assert_eq!(code, 0, "{stderr}");
    assert!(
        !stdout.contains(&raw) && !stdout.contains(&first.key.raw_key),
        "{stdout}"
    );
    let listed: Value = serde_json::from_str(&stdout).unwrap();
    let listed: Vec<(&str, &str, &str)> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|key| {
            (
                key["id"].as_str().unwrap(),
                key["name"].as_str().unwrap(),
                key["status"].as_str().unwrap(),
            )
        })
        .collect();
    let mut want = vec![
        (first.key.id.as_str(), "bootstrap", "active"),
        (id.as_str(), "billing-app", "active"),
    ];
    want.sort_unstable();
    let mut got = listed.clone();
    got.sort_unstable();
    assert_eq!(got, want);

    // Revoke, then revoke again: the same revoked key, and an audit line each time.
    for _ in 0..2 {
        let (code, stdout, stderr) = key(&url, "replica", &["revoke", "--id", &id]).await;
        assert_eq!(code, 0, "{stderr}");
        let revoked: Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(
            (revoked["id"].as_str(), revoked["status"].as_str()),
            (Some(id.as_str()), Some("revoked"))
        );
        assert_eq!(
            audits(&stderr),
            [(
                "api_key.revoke".to_owned(),
                "cli:tester".to_owned(),
                id.clone()
            )]
        );
    }
    let (code, _, stderr) = key(&url, "replica", &["revoke", "--id", "key_missing"]).await;
    assert_eq!(code, 1);
    assert!(
        stderr.ends_with("maverick key: api key key_missing does not exist\n"),
        "{stderr}"
    );

    // Usage errors, and an origin has no implicit tenant.
    let (code, _, stderr) = key(&url, "replica", &["create"]).await;
    assert_eq!(
        (code, stderr.as_str()),
        (2, "maverick key create: --name is required\n")
    );
    let (code, _, stderr) = key(&url, "origin", &["list"]).await;
    assert_eq!(
        (code, stderr.as_str()),
        (1, "key commands need MAVERICK_ROLE=replica\n")
    );

    // A second tenant (an origin's database) is not a single-tenant install.
    maverick_api::provision::create_tenant(&pool, "test", "other")
        .await
        .unwrap();
    let (code, _, stderr) = key(&url, "replica", &["list"]).await;
    assert_eq!(code, 1);
    assert!(
        stderr.ends_with(
            "maverick key: this install has 2 tenants; key commands need a single-tenant replica\n"
        ),
        "{stderr}"
    );
    db.close().await;
}
