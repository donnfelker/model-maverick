//! `maverick matrix status` on real Postgres (spec §14.4, D49): the active bundle's
//! matrix_version, its published and import times, and the days behind, on either role.

use std::io::{self, Write};
use std::sync::{Arc, LazyLock, Mutex};

use chrono::{DateTime, Duration, Utc};
use maverick_cli::{MATRIX_USAGE, NO_ACTIVE_BUNDLE, Output, run};
use maverick_store::MIGRATOR;
use maverick_testdb::Schema;
use sqlx::PgPool;

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

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

/// Runs `maverick <args>` in-process with `MAVERICK_ROLE=role`: exit code, stdout,
/// stderr. `run` builds its own runtime, so a database test calls it on a blocking thread.
fn maverick(database_url: &str, role: &str, args: &[&str]) -> (i32, String, String) {
    let out = Arc::new(Mutex::new(Vec::new()));
    let err = Arc::new(Mutex::new(Vec::new()));
    let env = [
        ("MAVERICK_ROLE", role.to_owned()),
        ("MAVERICK_DATABASE_URL", database_url.to_owned()),
        ("MAVERICK_INTERNAL_TOKENS", "ops:alpha-token".to_owned()),
        ("MAVERICK_CAPTURE_MASTER_KEY", "ab".repeat(32)),
    ];
    let lookup = |name: &str| {
        env.iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.clone())
    };
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
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
}

async fn status(database_url: &str, role: &str) -> (i32, String, String) {
    let url = database_url.to_owned();
    let role = role.to_owned();
    tokio::task::spawn_blocking(move || maverick(&url, &role, &["matrix", "status"]))
        .await
        .unwrap()
}

async fn bundle(
    pool: &PgPool,
    sha: u8,
    version: &str,
    published_at: DateTime<Utc>,
    imported_at: DateTime<Utc>,
    active: bool,
) {
    sqlx::query(
        "insert into matrix_imports (manifest_sha256, format, matrix_version, published_at,
             origin_release, signing_kids, active, created_at)
         values ($1, 'mmbundle1', $2, $3, 'v1.0.0', array['mm-2026a'], $4, $5)",
    )
    .bind(vec![sha; 32])
    .bind(version)
    .bind(published_at)
    .bind(active)
    .bind(imported_at)
    .execute(pool)
    .await
    .expect("bundle row");
}

fn seconds(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// DIVERGENCE: D49. With no imported bundle, both roles say so and exit 0. Then the
/// active bundle is the one printed, never an older inactive one, with its whole days
/// behind.
#[tokio::test]
async fn status_prints_the_active_bundle_and_its_days_behind() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    for role in ["replica", "origin"] {
        assert_eq!(
            status(db.url(), role).await,
            (0, NO_ACTIVE_BUNDLE.to_owned(), String::new()),
            "{role}"
        );
    }

    let now = Utc::now();
    let published = now - Duration::days(3) - Duration::hours(1);
    let imported = now - Duration::days(2);
    bundle(
        &pool,
        1,
        "2026-10-01",
        now - Duration::days(30),
        now - Duration::days(29),
        false,
    )
    .await;
    bundle(&pool, 2, "2026-10-20", published, imported, true).await;
    let (code, stdout, stderr) = status(db.url(), "replica").await;
    assert_eq!((code, stderr.as_str()), (0, ""), "{stdout}");
    assert_eq!(
        stdout,
        format!(
            "MATRIX_VERSION  PUBLISHED             IMPORTED              DAYS_BEHIND\n\
             2026-10-20      {}  {}  3\n",
            seconds(published),
            seconds(imported)
        )
    );
    pool.close().await;
    db.close().await;
}

/// An unreachable database exits 1 with the connect error logged on stderr. The host
/// fails name resolution at once (a refused port is retried until the pool's timeout).
#[tokio::test]
async fn status_fails_when_the_database_is_unreachable() {
    let (code, stdout, stderr) =
        status("postgres://u:p@maverick-test.invalid:5432/db", "replica").await;
    assert_eq!((code, stdout.as_str()), (1, ""));
    assert!(stderr.contains("connecting to database"), "{stderr}");
}

#[test]
fn matrix_usage_errors_exit_2() {
    let url = "postgres://u:p@127.0.0.1:1/db";
    assert_eq!(
        maverick(url, "replica", &["matrix"]),
        (2, String::new(), MATRIX_USAGE.to_owned())
    );
    assert_eq!(
        maverick(url, "replica", &["matrix", "nope"]),
        (
            2,
            String::new(),
            format!("maverick matrix: unknown subcommand \"nope\"\n\n{MATRIX_USAGE}")
        )
    );
    let (code, stdout, stderr) = maverick(url, "replica", &["matrix", "status", "--all"]);
    assert_eq!((code, stdout.as_str()), (2, ""));
    assert!(
        stderr.contains("flag provided but not defined: -all"),
        "{stderr}"
    );
    assert!(maverick_cli::MAIN_USAGE.contains("\n  matrix      "));
}
