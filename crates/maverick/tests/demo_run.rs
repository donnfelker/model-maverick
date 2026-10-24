//! Bead 3.13: the hidden `maverick demo-run` (reference Part 1 §7.6) against real Postgres
//! (testcontainers), starting from an empty database so its own migration runs. Skips when
//! Docker is not available.

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use maverick_cli::{Output, run};
use maverick_testdb::{Schema, TestDatabase, database};
use serde_json::Value;

/// The demo fixtures, relative to the crate directory (the test working directory).
const SEED: &str = "../../scripts/demo/seed.sql";
const FIXTURES: &str = "../../scripts/demo/fixtures";

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

/// One CLI invocation. `run` builds its own current-thread runtime, so it runs on a
/// blocking thread, outside the test's runtime.
async fn cli(args: &[&str], env: Vec<(String, String)>) -> (i32, String, String) {
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    tokio::task::spawn_blocking(move || {
        let out = Arc::new(Mutex::new(Vec::new()));
        let err = Arc::new(Mutex::new(Vec::new()));
        let lookup = |name: &str| {
            env.iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };
        let code = run(
            &args,
            &lookup,
            Output::new(Shared(out.clone())),
            Output::new(Shared(err.clone())),
        );
        let read = |buffer: &Arc<Mutex<Vec<u8>>>| {
            String::from_utf8(buffer.lock().expect("lock").clone()).expect("utf8")
        };
        (code, read(&out), read(&err))
    })
    .await
    .expect("join")
}

fn env(db: &TestDatabase) -> Vec<(String, String)> {
    vec![
        ("MAVERICK_ROLE".to_owned(), "origin".to_owned()),
        ("MAVERICK_DATABASE_URL".to_owned(), db.url().to_owned()),
        ("MAVERICK_CAPTURE_MASTER_KEY".to_owned(), "ab".repeat(32)),
        (
            "MAVERICK_INTERNAL_TOKENS".to_owned(),
            "rails:s3cret".to_owned(),
        ),
        ("MAVERICK_RUNNER_REPEATS".to_owned(), "2".to_owned()),
    ]
}

/// A seed file of its own under the system temp dir.
fn seed_file(name: &str, sql: &str) -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("maverick-demo-{}-{name}.sql", std::process::id()));
    std::fs::write(&path, sql).expect("write seed");
    path
}

/// The `demo-run failed` log line's `error`.
fn failure(out: &str) -> String {
    let line = out
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|line| line["msg"] == "demo-run failed")
        .unwrap_or_else(|| panic!("no `demo-run failed` line in: {out}"));
    assert_eq!(line["level"], "ERROR");
    line["error"].as_str().expect("error").to_owned()
}

#[tokio::test]
async fn demo_run_migrates_seeds_runs_one_batch_and_prints_the_runs() {
    let Some(db) = database(&Schema::empty()).await else {
        return;
    };
    let args = ["demo-run", "--seed", SEED, "--fake-fixtures", FIXTURES];
    let (code, out, err) = cli(&args, env(&db)).await;
    assert_eq!(code, 0, "stdout: {out}\nstderr: {err}");

    let lines: Vec<&str> = out.lines().collect();
    let summary = lines
        .iter()
        .position(|line| line.starts_with("batch bat_"))
        .unwrap_or_else(|| panic!("no summary line: {out}"));
    let batch_id = lines[summary]
        .strip_prefix("batch ")
        .and_then(|rest| rest.split(':').next())
        .unwrap()
        .to_owned();
    assert!(
        lines[summary].ends_with(": 2 runs (2 ok)"),
        "{}",
        lines[summary]
    );
    assert_eq!(
        lines[summary + 1],
        "RUN ID                          STATUS  LATENCY_MS  TOKENS_IN  TOKENS_OUT  COST_USD"
    );
    let rows = &lines[summary + 2..];
    assert_eq!(rows.len(), 2, "{out}");
    for row in rows {
        let cells: Vec<&str> = row.split_whitespace().collect();
        assert!(cells[0].starts_with("run_"), "{row}");
        // ok, the latency, 120 in, 18 out, and 120 × $3 + 18 × $15 per million tokens.
        assert_eq!(cells[1], "ok", "{row}");
        assert!(cells[2].parse::<i32>().is_ok(), "{row}");
        assert_eq!(&cells[3..], ["120", "18", "0.000630"], "{row}");
    }

    let pool = db.pool().await;
    let stored: Vec<(String, String, String)> = sqlx::query_as(
        "select task_id, model_column_id, matrix_version from runs where batch_id = $1",
    )
    .bind(&batch_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(stored.len(), 2);
    for row in &stored {
        assert_eq!(
            row,
            &(
                "task_demo_001".to_owned(),
                "col_demo_claude".to_owned(),
                "2026-07-01".to_owned()
            )
        );
    }

    // The seed is idempotent, so the demo runs again: a second batch of its own.
    let (code, out, _) = cli(&args, env(&db)).await;
    assert_eq!(code, 0, "{out}");
    assert!(!out.contains(&batch_id), "a new batch: {out}");
    let runs: i64 = sqlx::query_scalar("select count(*) from runs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(runs, 4);
}

#[tokio::test]
async fn a_seed_without_active_tasks_fails() {
    let Some(db) = database(&Schema::empty()).await else {
        return;
    };
    let seed = seed_file("no-tasks", "select 1;");
    let seed_arg = seed.to_str().unwrap();
    let (code, out, _) = cli(
        &["demo-run", "--seed", seed_arg, "--fake-fixtures", FIXTURES],
        env(&db),
    )
    .await;
    assert_eq!(code, 1, "{out}");
    assert_eq!(
        failure(&out),
        format!("seed fixture {seed_arg} produced no active tasks")
    );
}

#[tokio::test]
async fn a_seed_without_runnable_columns_fails() {
    let Some(db) = database(&Schema::empty()).await else {
        return;
    };
    // The demo seed with its column retired.
    let sql = std::fs::read_to_string(SEED)
        .unwrap()
        .replace("'{}', 'active',", "'{}', 'retired',");
    let seed = seed_file("retired", &sql);
    let seed_arg = seed.to_str().unwrap();
    let (code, out, _) = cli(
        &["demo-run", "--seed", seed_arg, "--fake-fixtures", FIXTURES],
        env(&db),
    )
    .await;
    assert_eq!(code, 1, "{out}");
    assert_eq!(
        failure(&out),
        format!("seed fixture {seed_arg} produced no runnable columns")
    );
}

#[tokio::test]
async fn a_missing_seed_or_fixture_dir_fails() {
    let Some(db) = database(&Schema::empty()).await else {
        return;
    };
    let (code, out, _) = cli(
        &[
            "demo-run",
            "--seed",
            "no/such/seed.sql",
            "--fake-fixtures",
            FIXTURES,
        ],
        env(&db),
    )
    .await;
    assert_eq!(code, 1);
    assert!(
        failure(&out).starts_with("reading seed fixture no/such/seed.sql: "),
        "{out}"
    );
    let (code, out, _) = cli(
        &["demo-run", "--seed", SEED, "--fake-fixtures", "no/such/dir"],
        env(&db),
    )
    .await;
    assert_eq!(code, 1);
    assert!(
        failure(&out).starts_with("fake provider: reading fixture dir: "),
        "{out}"
    );
}
