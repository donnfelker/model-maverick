//! Bead 3.10: the `maverick corpus` subcommands driven end to end through real Postgres
//! (testcontainers). Skips when Docker is not available.

use std::io::{self, Write};
use std::sync::{Arc, LazyLock, Mutex};

use maverick_cli::{Output, run};
use maverick_store::MIGRATOR;
use maverick_testdb::{Schema, database};

// parity: main.TestMain (maverick-testdb boots Postgres 16 once per binary and migrates)
static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

/// A minimal valid corpus, relative to the crate directory (the test working directory).
const VALID_DIR: &str =
    "../../parity/fixtures/measurement-io/testdata/internal/tasks/testdata/valid";

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

/// Run one CLI invocation with the given environment. `run` spins its own current-thread
/// runtime, so it must not run inside the test's async runtime: drive it on a blocking thread.
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

// parity: main.TestRunCorpusCalibrateEmptyAggregatesPrintsNoData (the calibrate step: no
// aggregates gives exit 0 and exactly `calibrate.json#report_empty`)
#[tokio::test]
async fn corpus_load_activate_status_calibrate_round_trip() {
    let Some(db) = database(&SCHEMA).await else {
        return;
    };
    let env = vec![
        ("MAVERICK_ROLE".to_owned(), "origin".to_owned()),
        ("MAVERICK_DATABASE_URL".to_owned(), db.url().to_owned()),
        ("MAVERICK_CAPTURE_MASTER_KEY".to_owned(), "ab".repeat(32)),
        (
            "MAVERICK_INTERNAL_TOKENS".to_owned(),
            "rails:s3cret".to_owned(),
        ),
    ];

    // load: a valid corpus stores inactive (warnings on stderr, success on stdout).
    let (code, out, err) = cli(
        &["corpus", "load", "--version", "v-test", "--dir", VALID_DIR],
        env.clone(),
    )
    .await;
    assert_eq!(code, 0, "load exit; stderr: {err}");
    assert!(
        out.contains("loaded corpus version v-test:"),
        "load stdout: {out}"
    );
    assert!(
        out.contains("(inactive; run `maverick corpus activate --version v-test`)"),
        "load stdout: {out}"
    );

    // status: the version is present and inactive.
    let (code, out, _) = cli(&["corpus", "status"], env.clone()).await;
    assert_eq!(code, 0);
    assert!(
        out.contains("VERSION") && out.contains("v-test") && out.contains("false"),
        "status: {out}"
    );

    // activate: the version becomes active, with the exact message.
    let (code, out, err) = cli(&["corpus", "activate", "--version", "v-test"], env.clone()).await;
    assert_eq!(code, 0, "activate exit; stderr: {err}");
    assert_eq!(
        out, "corpus version v-test is now active\n",
        "activate stdout"
    );

    // status: now active.
    let (code, out, _) = cli(&["corpus", "status"], env.clone()).await;
    assert_eq!(code, 0);
    assert!(
        out.contains("v-test") && out.contains("true"),
        "status active: {out}"
    );

    // calibrate: no measured aggregates yet, so the no-data report and exit 0.
    let (code, out, _) = cli(&["corpus", "calibrate"], env.clone()).await;
    assert_eq!(code, 0);
    assert_eq!(
        out,
        "tier calibration report (capability-matrix design 5.3)\nno data: no measured pass_rate aggregates\n",
        "calibrate no-data"
    );

    db.close().await;
}
