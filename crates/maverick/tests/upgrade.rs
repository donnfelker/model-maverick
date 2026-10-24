//! DIVERGENCE: D50. `maverick upgrade` (spec §14.5) against real Postgres: preflight,
//! plan, apply, record, and verify; `--check`, `--yes`, `--json`; the upgrade lock and the
//! serve heartbeats. The Go engine had no upgrade command.

use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use chrono::{TimeDelta, Utc};
use maverick_cli::version::VERSION;
use maverick_cli::{Output, run};
use maverick_store::schema::{self, binary_schema_version};
use maverick_store::upgrade::{self as store, UPGRADE_LOCK_KEY};
use maverick_testdb::{Schema, TestDatabase};
use serde_json::Value;
use sqlx::{Connection, PgConnection};

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

/// Runs `maverick <args>` against `database_url` with the required settings.
fn invoke(args: &[&str], database_url: &str) -> (i32, String, String) {
    let out = Arc::new(Mutex::new(Vec::new()));
    let err = Arc::new(Mutex::new(Vec::new()));
    let env = [
        ("MAVERICK_ROLE", "origin"),
        ("MAVERICK_DATABASE_URL", database_url),
        ("MAVERICK_INTERNAL_TOKENS", "ops:alpha-token"),
        ("MAVERICK_CAPTURE_MASTER_KEY", &"ab".repeat(32)),
        ("USER", "tester"),
    ]
    .map(|(k, v)| (k.to_owned(), v.to_owned()));
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    let code = run(
        &args,
        &|name| env.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()),
        Output::new(Shared(out.clone())),
        Output::new(Shared(err.clone())),
    );
    let read = |buffer: &Arc<Mutex<Vec<u8>>>| {
        String::from_utf8(buffer.lock().expect("lock").clone()).expect("utf8")
    };
    (code, read(&out), read(&err))
}

/// `invoke` on a background thread: `run` blocks on its own runtime.
async fn upgrade(args: &[&str], db: &TestDatabase) -> (i32, String, String) {
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    let url = db.url().to_owned();
    tokio::task::spawn_blocking(move || {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        invoke(&args, &url)
    })
    .await
    .expect("join")
}

fn json(stdout: &str) -> Value {
    serde_json::from_str(stdout.trim()).unwrap_or_else(|e| panic!("{e}: {stdout}"))
}

async fn status(db: &TestDatabase) -> schema::Status {
    let mut conn = db.connect().await;
    let status = schema::status(&mut conn).await.expect("status");
    conn.close().await.expect("close");
    status
}

/// A database that `migrate` brought fully up to date (engine and job queue).
async fn current() -> Option<TestDatabase> {
    let db = maverick_testdb::database(&Schema::empty()).await?;
    maverick_store::migrate::migrate(db.url())
        .await
        .expect("migrate");
    Some(db)
}

/// Marks an applied engine migration as not applied, so it is pending again, without
/// touching its objects (for preflight-only tests).
async fn unmark(db: &TestDatabase, version: i64) {
    let mut conn = db.connect().await;
    sqlx::query("delete from _sqlx_migrations where version = $1")
        .bind(version)
        .execute(&mut conn)
        .await
        .expect("unmark");
    conn.close().await.expect("close");
}

fn plan_names(report: &Value) -> Vec<String> {
    report["plan"]
        .as_array()
        .expect("plan")
        .iter()
        .map(|step| step["migration"].as_str().expect("name").to_owned())
        .collect()
}

#[tokio::test]
async fn a_fresh_database_is_upgraded_recorded_and_then_current() {
    let Some(db) = maverick_testdb::database(&Schema::empty()).await else {
        return;
    };
    let before = status(&db).await;
    let (code, stdout, stderr) = upgrade(&["upgrade", "--yes", "--json"], &db).await;
    assert_eq!(code, 0, "{stdout}\n{stderr}");
    let report = json(&stdout);
    assert_eq!(report["result"], "upgraded", "{report}");
    assert_eq!(report["release"], VERSION);
    assert_eq!(report["database_schema"], 0);
    assert_eq!(report["release_schema"], binary_schema_version());
    assert_eq!(report["last_upgrade"], Value::Null);
    let checks: Vec<(&str, bool)> = report["checks"]
        .as_array()
        .expect("checks")
        .iter()
        .map(|c| (c["name"].as_str().unwrap(), c["ok"].as_bool().unwrap()))
        .collect();
    assert_eq!(
        checks,
        [
            ("newer database", true),
            ("upgrade window", true),
            ("bundle format", true),
            ("upgrade lock", true),
            ("serve processes", true),
        ]
    );
    let expected: Vec<String> = before
        .pending
        .iter()
        .map(|v| schema::migration_name(*v))
        .chain(
            before
                .queue_pending
                .iter()
                .map(|n| schema::queue_migration_name(*n)),
        )
        .collect();
    assert_eq!(plan_names(&report), expected);
    assert_eq!(
        report["plan"][4]["migration"],
        "0005_debug_captures_capture_key_version.sql"
    );
    assert_eq!(report["plan"][4]["class"], "needs-downtime");
    assert_eq!(report["applied"].as_array().unwrap().len(), expected.len());
    assert_eq!(report["verify"]["ok"], true);
    assert_eq!(report["error"], Value::Null);

    let after = status(&db).await;
    assert!(after.pending.is_empty() && after.queue_pending.is_empty());
    let pool = db.pool().await;
    let row = store::last_upgrade(&pool)
        .await
        .expect("read")
        .expect("a row");
    assert_eq!(row.from_release, None);
    assert_eq!(row.to_release, VERSION);
    assert_eq!(row.operator, "cli:tester");
    assert_eq!(
        serde_json::to_value(&row.migrations).unwrap(),
        report["applied"]
    );
    assert!(row.finished_at.is_some());
    assert_eq!(row.verify_failed, None);

    // A second run finds nothing to do and records nothing.
    let (code, stdout, _) = upgrade(&["upgrade", "--yes", "--json"], &db).await;
    assert_eq!(code, 0, "{stdout}");
    let again = json(&stdout);
    assert_eq!(again["result"], "up_to_date");
    assert_eq!(again["plan"], serde_json::json!([]));
    assert_eq!(again["last_upgrade"]["id"], Value::String(row.id.clone()));
    let latest = store::last_upgrade(&pool)
        .await
        .expect("read")
        .expect("row");
    assert_eq!(latest.id, row.id);
    db.close().await;
}

#[tokio::test]
async fn check_reports_the_plan_and_changes_nothing() {
    let Some(db) = maverick_testdb::database(&Schema::empty()).await else {
        return;
    };
    let before = status(&db).await;
    let (code, stdout, stderr) = upgrade(&["upgrade", "--check"], &db).await;
    assert_eq!(code, 0, "{stdout}\n{stderr}");
    assert!(
        stdout.starts_with(&format!(
            "maverick upgrade: release {VERSION}; database schema 0, this release's schema {}\n",
            binary_schema_version()
        )),
        "{stdout}"
    );
    assert!(stdout.contains("\nlast upgrade: none recorded\npreflight:\n  ok newer database\n"));
    assert!(stdout.contains("\nplan:\n  0001_baseline.sql (online-safe)\n"));
    assert!(stdout.contains("  0005_debug_captures_capture_key_version.sql (needs-downtime)\n"));
    assert!(stdout.ends_with("result: checked\n"), "{stdout}");
    assert_eq!(status(&db).await, before);
    db.close().await;
}

#[tokio::test]
async fn without_yes_and_without_a_terminal_nothing_is_applied() {
    let Some(db) = maverick_testdb::database(&Schema::empty()).await else {
        return;
    };
    let (code, stdout, _) = upgrade(&["upgrade", "--json"], &db).await;
    assert_eq!(code, 1, "{stdout}");
    let report = json(&stdout);
    assert_eq!(report["result"], "refused");
    assert_eq!(
        report["error"],
        "not applied: run with --yes to apply without a terminal"
    );
    assert_eq!(status(&db).await.applied, Vec::<i64>::new());
    db.close().await;
}

#[tokio::test]
async fn a_second_upgrade_waits_for_the_lock() {
    let Some(db) = maverick_testdb::database(&Schema::empty()).await else {
        return;
    };
    let mut holder = PgConnection::connect(db.url()).await.expect("connect");
    sqlx::query("select pg_advisory_lock($1)")
        .bind(UPGRADE_LOCK_KEY)
        .execute(&mut holder)
        .await
        .expect("lock");
    let (code, stdout, _) = upgrade(&["upgrade", "--yes", "--json"], &db).await;
    assert_eq!(code, 1, "{stdout}");
    let report = json(&stdout);
    assert_eq!(report["result"], "refused");
    let last = report["checks"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["name"], "upgrade lock");
    assert_eq!(last["ok"], false);
    assert_eq!(
        report["error"],
        "upgrade lock: another maverick upgrade holds the upgrade lock"
    );
    assert!(status(&db).await.applied.is_empty());
    holder.close().await.expect("close");
    db.close().await;
}

#[tokio::test]
async fn a_live_serve_blocks_a_needs_downtime_plan_but_not_an_online_safe_one() {
    let Some(db) = current().await else {
        return;
    };
    let pool = db.pool().await;
    store::beat(&pool, "srv_live", "0.9.0", "host-a", Utc::now())
        .await
        .expect("beat");

    // Only 0012 pending: online-safe, so a live serve does not block the plan.
    unmark(&db, 12).await;
    let (code, stdout, _) = upgrade(&["upgrade", "--check", "--json"], &db).await;
    assert_eq!(code, 0, "{stdout}");
    let report = json(&stdout);
    assert_eq!(report["result"], "checked");
    assert_eq!(plan_names(&report), ["0012_upgrades.sql"]);
    let serve = report["checks"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(serve["name"], "serve processes");
    assert_eq!(serve["ok"], true);
    assert_eq!(
        serve["detail"],
        "running: host-a (0.9.0); every pending migration is online-safe"
    );

    // 0005 pending too: it needs downtime, and serve still runs.
    unmark(&db, 5).await;
    let (code, stdout, _) = upgrade(&["upgrade", "--check", "--json"], &db).await;
    assert_eq!(code, 1, "{stdout}");
    let report = json(&stdout);
    assert_eq!(report["result"], "refused");
    assert_eq!(
        report["error"],
        "serve processes: 0005_debug_captures_capture_key_version.sql needs downtime, and \
         serve still runs on host-a (0.9.0); stop every serve process first"
    );

    // A heartbeat older than two minutes is a stopped process.
    store::beat(
        &pool,
        "srv_live",
        "0.9.0",
        "host-a",
        Utc::now() - TimeDelta::minutes(3),
    )
    .await
    .expect("beat");
    let (code, stdout, _) = upgrade(&["upgrade", "--check", "--json"], &db).await;
    assert_eq!(code, 0, "{stdout}");
    db.close().await;
}

#[tokio::test]
async fn upgrade_refuses_a_newer_database_and_takes_no_arguments() {
    let Some(db) = current().await else {
        return;
    };
    let mut conn = db.connect().await;
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
         VALUES (9001, 'from a newer release', true, '\\x00', 0)",
    )
    .execute(&mut conn)
    .await
    .expect("insert");
    conn.close().await.expect("close");
    let (code, stdout, _) = upgrade(&["upgrade", "--yes", "--json"], &db).await;
    assert_eq!(code, 1, "{stdout}");
    let report = json(&stdout);
    assert_eq!(report["result"], "refused");
    assert_eq!(report["checks"].as_array().unwrap().len(), 1);
    assert!(
        report["error"]
            .as_str()
            .unwrap()
            .starts_with("newer database: the database schema is 9001, newer than this release's"),
        "{report}"
    );

    let (code, stdout, stderr) = upgrade(&["upgrade", "now"], &db).await;
    assert_eq!((code, stdout.as_str()), (2, ""));
    assert!(stderr.starts_with("maverick: upgrade takes no arguments\n\nUsage: maverick"));
    db.close().await;
}
