//! DIVERGENCE: D49. The sealed-environment test (spec §14.4; self-hosted plan §6; bead
//! xq7v.12): a replica with every outbound setting at its default makes no outbound
//! connection. `scripts/sealed.sh` runs it in a network namespace that has only loopback,
//! and the test reaches Postgres through a unix socket, so the namespace needs no route.
//!
//! The namespace alone is not the check. A process in it can still reach the host through
//! a unix socket (nscd, systemd-resolved, systemd-userdb). So `maverick upgrade` and
//! `maverick serve` run under strace, and every socket address they connect or send to
//! must be the Postgres socket, or netlink, which stays in the kernel. One exception: sqlx
//! looks up the process's own user name (its default database user) whenever it parses a
//! database URL, and glibc tries the nscd cache first. A connect to an nscd socket that
//! does not exist carries nothing, so it passes; a connect that reaches nscd fails. `-s 0`
//! keeps every payload byte out of the traces.
//!
//! The run: `maverick upgrade --yes` on an empty database; a first `serve`, which
//! bootstraps the tenant and its key file; `maverick matrix import` of a test-signed
//! fixture bundle, which also completes the tenant's default pool; a declared recommend
//! with the bootstrap key, ranked on the bundle; and one firing of every cron trigger the
//! replica registered, so the hourly and daily jobs run sealed too.
//!
//! `MAVERICK_E2E_BIN_DIR` names the directory of a debug `maverick` built with the
//! `test-keys` feature, the only kind that accepts a test-signed bundle
//! (docs/matrix-bundles.md). `MAVERICK_SEALED_PG_SOCKET` names the Postgres socket file.

use std::fs;
use std::io;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use chrono::{SecondsFormat, Utc};
use maverick_domain::keys::KEY_PREFIX;
use maverick_e2e::harness::{Steps, count, eventually, expect, http, workspace};
use maverick_e2e::process::{Artifacts, FailureReport, Process, free_port, wait_ready};
use maverick_testdb::Schema;
use serde_json::{Value, json};

const INTERNAL_TOKEN: &str = "ops:sealed-internal-secret";
/// A format fixture (bead xq7v.12 item 4), signed by the test current and standby keys.
const FIXTURE: &str = "crates/maverick-bundle/tests/data/mmbundle1/current-and-standby.tar";
const FIXTURE_VERSION: &str = "fixture-v1";
/// The fixture's better column for structured_extraction tier 2 (quality 0.91 to 0.74).
const BEST_COLUMN: &str = "col_fixture_a";
/// A TEST-NET-1 address (RFC 5737): never routed, so a sealed namespace refuses it at once.
const PUBLIC_ADDR: &str = "192.0.2.1:443";
/// strace's flags before `-o <trace>`: follow every thread and child, each into its own
/// `<trace>.<tid>` file (so no line is split by another thread), no attach or exit lines, no
/// payload bytes, only the syscalls that take a socket address, no signal lines.
const STRACE_FLAGS: [&str; 8] = [
    "-ff",
    "-qq",
    "-s0",
    "-e",
    "trace=connect,sendto,sendmsg,sendmmsg",
    "-e",
    "signal=none",
    "-o",
];

/// The replica's settings: the role, the database, the two required secrets, the listen
/// addresses, and the bootstrap key file. None of them is an outbound setting, so every
/// outbound call (providers, models.dev, the matrix pull, alerts, OTel) is at its default.
fn replica_env(db: &str, public: u16, internal: u16, key_file: &Path) -> Vec<(String, String)> {
    [
        ("MAVERICK_ROLE", "replica".to_owned()),
        ("MAVERICK_DATABASE_URL", db.to_owned()),
        ("MAVERICK_INTERNAL_TOKENS", INTERNAL_TOKEN.to_owned()),
        ("MAVERICK_CAPTURE_MASTER_KEY", "5e".repeat(32)),
        ("MAVERICK_PUBLIC_ADDR", format!("127.0.0.1:{public}")),
        ("MAVERICK_INTERNAL_ADDR", format!("127.0.0.1:{internal}")),
        (
            "MAVERICK_BOOTSTRAP_KEY_FILE",
            key_file.display().to_string(),
        ),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value))
    .collect()
}

/// The first executable named `name` on `PATH`.
fn on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

/// Fails unless this process's network namespace has only loopback and no route out.
fn assert_sealed() {
    // /proc/net is this process's namespace; /sys/class/net would show the mounter's.
    let dev = fs::read_to_string("/proc/net/dev").expect("/proc/net/dev");
    let interfaces: Vec<&str> = dev
        .lines()
        .skip(2)
        .filter_map(|line| line.split_once(':'))
        .map(|(name, _)| name.trim())
        .collect();
    assert_eq!(
        interfaces,
        ["lo"],
        "run this test in a network namespace with only loopback (scripts/sealed.sh)"
    );
    let addr: SocketAddr = PUBLIC_ADDR.parse().expect("address");
    let error = TcpStream::connect_timeout(&addr, Duration::from_secs(2))
        .expect_err("a public address is reachable from the namespace");
    assert_eq!(error.kind(), io::ErrorKind::NetworkUnreachable, "{error}");
}

/// `program args` under strace, writing the trace to `trace`.
fn traced(
    artifacts: &Artifacts,
    name: &str,
    strace: &Path,
    trace: &Path,
    program: &Path,
    args: &[&str],
    env: &[(String, String)],
) -> Process {
    let trace = trace.display().to_string();
    let program = program.display().to_string();
    let mut argv = STRACE_FLAGS.to_vec();
    argv.extend([trace.as_str(), "--", program.as_str()]);
    argv.extend_from_slice(args);
    Process::spawn(artifacts, name, strace, &argv, env)
        .unwrap_or_else(|error| panic!("spawn {name} under strace: {error}"))
}

/// Sends SIGTERM to the program strace runs (the strace process's only child), so the
/// trace covers the shutdown too, and waits for strace to exit with the program's status.
async fn stop_traced(process: &mut Process, timeout: Duration) -> std::process::ExitStatus {
    let id = process.id();
    let children = fs::read_to_string(format!("/proc/{id}/task/{id}/children"))
        .unwrap_or_else(|error| panic!("{}: reading its children: {error}", process.name()));
    let child = children
        .split_whitespace()
        .next()
        .unwrap_or_else(|| panic!("{}: strace runs no program", process.name()));
    let sent = Command::new("kill")
        .args(["-TERM", child])
        .status()
        .expect("kill");
    assert!(sent.success(), "kill -TERM {child}: {sent}");
    process
        .wait(timeout)
        .await
        .unwrap_or_else(|error| panic!("{error}"))
}

/// glibc's name-service cache sockets.
const NSCD_SOCKETS: [&str; 2] = ["/var/run/nscd/socket", "/run/nscd/socket"];

/// The traced lines that name a socket address other than `allowed` (a unix socket path),
/// netlink, or an nscd socket that is not there: each one is an outbound connection
/// attempt or send.
fn outbound<'a>(trace: &'a str, allowed: &str) -> Vec<&'a str> {
    trace
        .lines()
        .filter(|line| {
            line.split("sa_family=").skip(1).any(|address| {
                let address = address.split('}').next().unwrap_or_default();
                match address.split(',').next().unwrap_or_default() {
                    "AF_NETLINK" => false,
                    "AF_UNIX" => match unix_path(address) {
                        Some(path) if path == allowed => false,
                        Some(path) if NSCD_SOCKETS.contains(&path) => !nothing_there(line),
                        _ => true,
                    },
                    _ => true,
                }
            })
        })
        .collect()
}

/// Whether a traced line is a connect that found no listener at its path.
fn nothing_there(line: &str) -> bool {
    is_connect(line) && (line.contains("= -1 ENOENT") || line.contains("= -1 ECONNREFUSED"))
}

/// Whether a traced line is a connect (with `-ff` a line has no pid prefix).
fn is_connect(line: &str) -> bool {
    line.starts_with("connect(") || line.contains(" connect(")
}

/// The `sun_path` of one traced unix address; `None` for an abstract or unnamed socket.
fn unix_path(address: &str) -> Option<&str> {
    let (_, rest) = address.split_once("sun_path=\"")?;
    rest.split_once('"').map(|(path, _)| path)
}

/// How many traced lines connect to `allowed`.
fn connects_to(trace: &str, allowed: &str) -> usize {
    let needle = format!("sun_path=\"{allowed}\"");
    trace
        .lines()
        .filter(|line| is_connect(line) && line.contains(&needle))
        .count()
}

/// Every `<trace>.<tid>` file strace wrote, joined.
fn read_traces(trace: &Path) -> String {
    let dir = trace.parent().expect("trace dir");
    let prefix = format!(
        "{}.",
        trace.file_name().expect("trace name").to_string_lossy()
    );
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .expect("trace dir")
        .map(|entry| entry.expect("trace entry").path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(&prefix))
        })
        .collect();
    files.sort();
    assert!(!files.is_empty(), "strace wrote no {prefix}<tid> file");
    files
        .iter()
        .map(|path| fs::read_to_string(path).expect("trace file"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Fails on any outbound line in `trace`'s files, and when they never reach Postgres (so
/// strace traced nothing).
fn assert_no_outbound(name: &str, trace: &Path, allowed: &str) {
    let text = read_traces(trace);
    let found = outbound(&text, allowed);
    assert!(
        found.is_empty(),
        "{name} tried {} outbound connection(s); trace {}:\n{}",
        found.len(),
        trace.display(),
        found.join("\n")
    );
    assert!(
        connects_to(&text, allowed) > 0,
        "{name}: the trace {} never connects to Postgres, so strace traced nothing",
        trace.display()
    );
}

/// The kinds the serve log says a cron firing enqueued.
fn enqueued_kinds(log: &str) -> Vec<String> {
    log.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| line["msg"] == "scheduled job enqueued")
        .filter_map(|line| line["kind"].as_str().map(str::to_owned))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "sealed environment: needs a loopback-only network namespace and strace; run scripts/sealed.sh"]
async fn a_replica_with_default_settings_makes_no_outbound_connection() {
    let mut steps = Steps::new();
    let artifacts = Artifacts::create_named(&workspace(), "sealed").expect("artifact dir");
    let _report = FailureReport::new(&artifacts);
    let socket = std::env::var("MAVERICK_SEALED_PG_SOCKET")
        .expect("MAVERICK_SEALED_PG_SOCKET: run scripts/sealed.sh");
    let engine = PathBuf::from(
        std::env::var_os("MAVERICK_E2E_BIN_DIR")
            .expect("MAVERICK_E2E_BIN_DIR: run scripts/sealed.sh"),
    )
    .join("maverick");
    let strace = on_path("strace").expect("strace is on PATH");
    let fixture = workspace().join(FIXTURE);
    assert!(fixture.is_file(), "{} is missing", fixture.display());
    eprintln!(
        "sealed: engine {}; artifacts {}",
        engine.display(),
        artifacts.dir().display()
    );

    steps.begin("The network namespace has only loopback and no route out");
    assert_sealed();

    steps.begin("Postgres over its unix socket, then `maverick upgrade --yes`");
    let Some(db) = maverick_testdb::database(&Schema::empty()).await else {
        panic!("no Postgres: set MAVERICK_TEST_DATABASE_URL (scripts/sealed.sh does)");
    };
    let pool = db.pool().await;
    let key_file = artifacts.dir().join("bootstrap-key");
    let (public, internal) = (free_port(), free_port());
    let env = replica_env(db.url(), public, internal, &key_file);
    let upgrade_trace = artifacts.dir().join("upgrade.strace");
    let mut upgrade = traced(
        &artifacts,
        "upgrade",
        &strace,
        &upgrade_trace,
        &engine,
        &["upgrade", "--yes"],
        &env,
    );
    let status = upgrade
        .wait(Duration::from_secs(120))
        .await
        .expect("upgrade exits");
    assert!(status.success(), "upgrade: {status}\n{}", upgrade.log());

    steps.begin("The first `maverick serve` bootstraps the tenant and its key");
    let serve_trace = artifacts.dir().join("serve.strace");
    let mut serve = traced(
        &artifacts,
        "serve",
        &strace,
        &serve_trace,
        &engine,
        &["serve"],
        &env,
    );
    let public_url = format!("http://127.0.0.1:{public}");
    wait_ready(&format!("{public_url}/up"), Duration::from_secs(60))
        .await
        .expect("serve /up");
    let written = eventually(Duration::from_secs(10), || async {
        fs::read_to_string(&key_file).is_ok_and(|key| !key.trim().is_empty())
    })
    .await;
    assert!(
        written,
        "the bootstrap wrote no key to {}",
        key_file.display()
    );
    let key = fs::read_to_string(&key_file).expect("key file");
    let key = key.trim();
    assert!(key.starts_with(KEY_PREFIX), "the key file holds no API key");

    steps.begin("`maverick matrix import` of a test-signed fixture bundle");
    let import_trace = artifacts.dir().join("import.strace");
    let path = fixture.display().to_string();
    let mut import = traced(
        &artifacts,
        "import",
        &strace,
        &import_trace,
        &engine,
        &["matrix", "import", &path],
        &env,
    );
    let status = import
        .wait(Duration::from_secs(60))
        .await
        .expect("import exits");
    assert!(status.success(), "import: {status}\n{}", import.log());
    assert!(
        import.log().contains("matrix bundle imported"),
        "{}",
        import.log()
    );

    steps.begin("A declared recommend with the bootstrap key ranks on the bundle");
    let bearer = format!("Bearer {key}");
    let recommend = http(
        &steps,
        "POST",
        &format!("{public_url}/v1/recommend"),
        &[("authorization", bearer.as_str())],
        Some(&json!({"workload": {"capability": "structured_extraction", "difficulty": 2}})),
    )
    .await;
    expect(&recommend, 200, "declared recommend");
    let body = recommend.json();
    assert_eq!(body["classification"]["method"], "declared");
    assert_eq!(body["matrix_version"], FIXTURE_VERSION);
    assert_eq!(body["measured"], true);
    assert_eq!(body["recommendations"][0]["column_id"], BEST_COLUMN);

    steps.begin("Every cron trigger the replica registered, fired once");
    let crons: Vec<String> = sqlx::query_scalar(
        "select identifier from graphile_worker._private_known_crontabs order by identifier",
    )
    .fetch_all(&pool)
    .await
    .expect("known crontabs");
    assert!(!crons.is_empty(), "the replica registered no cron trigger");
    steps.note(format!("cron triggers: {crons:?}"));
    // graphile_worker stores each crontab under its trigger's task identifier
    // (`cron.<kind>`), so the job to add is the identifier itself, with a firing's payload.
    let ts = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    for cron in &crons {
        sqlx::query("select graphile_worker.add_job(identifier => $1, payload => $2::json)")
            .bind(cron)
            .bind(sqlx::types::Json(
                json!({"_cron": {"ts": ts, "backfilled": false}}),
            ))
            .execute(&pool)
            .await
            .unwrap_or_else(|error| panic!("{cron}: {error}"));
    }
    // A finished job leaves the queue, and a failed one stays with its error.
    let drained = eventually(Duration::from_secs(120), || async {
        count(&pool, "select count(*) from graphile_worker._private_jobs").await == 0
    })
    .await;
    if !drained {
        let left: Vec<(String, i16, Option<String>)> = sqlx::query_as(
            "select tasks.identifier, jobs.attempts, jobs.last_error
             from graphile_worker._private_jobs as jobs
             join graphile_worker._private_tasks as tasks on tasks.id = jobs.task_id
             order by jobs.id",
        )
        .fetch_all(&pool)
        .await
        .expect("jobs");
        panic!("jobs still queued after 120 s: {left:#?}");
    }
    let enqueued = enqueued_kinds(&serve.log());
    for cron in &crons {
        let kind = cron.strip_prefix("cron.").unwrap_or(cron);
        assert!(
            enqueued.iter().any(|seen| seen == kind),
            "{cron} enqueued no job; the serve log names {enqueued:?}"
        );
    }

    steps.begin("Stop serve, then check the three traces");
    let status = stop_traced(&mut serve, Duration::from_secs(45)).await;
    assert!(status.success(), "serve: {status}");
    assert_no_outbound("maverick upgrade", &upgrade_trace, &socket);
    assert_no_outbound("maverick matrix import", &import_trace, &socket);
    assert_no_outbound("maverick serve", &serve_trace, &socket);
    steps.note(format!(
        "no outbound connection; traces in {}",
        artifacts.dir().display()
    ));

    pool.close().await;
    db.close().await;
}

/// The trace check on strace lines of every shape the run can produce, with and without
/// the pid prefix (`-f` and `-ff`).
#[test]
fn only_postgres_netlink_and_an_absent_nscd_are_allowed() {
    let socket = "/run/pg/.s.PGSQL.5432";
    let trace = r#"101 connect(3, {sa_family=AF_UNIX, sun_path="/run/pg/.s.PGSQL.5432"}, 110) = 0
101 sendto(3, ""..., 63, MSG_NOSIGNAL, NULL, 0) = 63
102 sendto(4, [{nlmsg_len=20, nlmsg_type=RTM_GETADDR}], 20, 0, {sa_family=AF_NETLINK, nl_pid=0, nl_groups=00000000}, 12) = 20
103 connect(5, {sa_family=AF_UNIX, sun_path="/var/run/nscd/socket"}, 110) = -1 ENOENT (No such file or directory)
connect(5, {sa_family=AF_UNIX, sun_path="/run/nscd/socket"}, 110) = -1 ECONNREFUSED (Connection refused)
104 connect(6, {sa_family=AF_INET, sin_port=htons(53), sin_addr=inet_addr("127.0.0.53")}, 16) = 0
105 connect(7, {sa_family=AF_INET6, sin6_port=htons(443), sin6_flowinfo=htonl(0), inet_pton(AF_INET6, "2001:db8::1", &sin6_addr), sin6_scope_id=0}, 28 <unfinished ...>
106 sendmsg(8, {msg_name={sa_family=AF_INET, sin_port=htons(443), sin_addr=inet_addr("192.0.2.1")}, msg_namelen=16, msg_iov=[{iov_base=""..., iov_len=40}], msg_iovlen=1, msg_controllen=0, msg_flags=0}, 0) = -1 ENETUNREACH (Network is unreachable)
107 connect(9, {sa_family=AF_UNIX, sun_path=@"/tmp/.X11-unix/X0"}, 20) = 0
108 sendmmsg(10, [{msg_hdr={msg_name=NULL, msg_namelen=0, msg_iov=[{iov_base=""..., iov_len=40}], msg_iovlen=1, msg_controllen=0, msg_flags=0}, msg_len=40}], 1, MSG_NOSIGNAL) = 1
105 <... connect resumed>) = -1 ENETUNREACH (Network is unreachable)
109 connect(11, {sa_family=AF_UNSPEC, sa_data="\0\0\0\0\0\0\0\0\0\0\0\0\0\0"}, 16) = 0
110 connect(12, {sa_family=AF_UNIX, sun_path="/var/run/nscd/socket"}, 110) = 0
111 connect(13, {sa_family=AF_UNIX, sun_path="/var/run/nscd/socket"}, 110 <unfinished ...>
112 sendto(14, ""..., 12, 0, {sa_family=AF_UNIX, sun_path="/var/run/nscd/socket"}, 110) = -1 ENOENT (No such file or directory)
connect(15, {sa_family=AF_UNIX, sun_path="/run/pg/.s.PGSQL.5432"}, 110) = 0
"#;
    let pids: Vec<&str> = outbound(trace, socket)
        .iter()
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    assert_eq!(
        pids,
        ["104", "105", "106", "107", "109", "110", "111", "112"]
    );
    assert_eq!(connects_to(trace, socket), 2);
    assert_eq!(connects_to(trace, "/run/pg"), 0);
    assert!(outbound("", socket).is_empty());
}
