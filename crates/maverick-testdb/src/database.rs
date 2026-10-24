//! One isolated database per test, cloned from the schema template.

use std::time::{Duration, SystemTime};

use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::{AssertSqlSafe, Connection, PgConnection};
use tokio::sync::Mutex;
use ulid::Ulid;

use crate::schema::Schema;
use crate::server::{Server, Unavailable, server};
use crate::url::with_database;
use crate::{KEEP_DB_VAR, REQUIRE_DB_VAR, env_flag};

/// Prefix of every test database. The rest of the name is a lowercase ULID.
const DB_PREFIX: &str = "mvt_";
/// A test database older than this is left over from a crashed run and is dropped.
const STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);
/// Serializes template builds across processes that share one server.
const TEMPLATE_LOCK_KEY: i64 = 0x6d76_7464_625f_746c;
/// Connections per test pool. A test binary runs one test per core in parallel.
const POOL_SIZE: u32 = 5;

/// Templates that this process has already built or found.
static READY_TEMPLATES: Mutex<Vec<String>> = Mutex::const_new(Vec::new());

/// A database that belongs to one test. Dropping it drops the database.
pub struct TestDatabase {
    name: String,
    url: String,
    admin_url: String,
    container_id: Option<String>,
    closed: bool,
}

/// Returns a new database with `schema` applied.
///
/// Returns `None` and prints `skipping store-backed tests: <reason>` when Docker is
/// unavailable, like the Go harness. Panics instead when `MAVERICK_TEST_REQUIRE_DB` or
/// `CI` is set, or when the server fails for any other reason.
pub async fn database(schema: &Schema) -> Option<TestDatabase> {
    let server = match server().await {
        Ok(server) => server,
        Err(Unavailable::Skip(reason)) => {
            if env_flag(REQUIRE_DB_VAR) || env_flag("CI") {
                panic!("store-backed tests require Postgres: {reason}");
            }
            eprintln!("skipping store-backed tests: {reason}");
            return None;
        }
        Err(Unavailable::Fatal(message)) => panic!("{message}"),
    };
    match create(server, schema).await {
        Ok(db) => Some(db),
        Err(message) => panic!("{message}"),
    }
}

impl TestDatabase {
    /// The database name, `mvt_<ulid>`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The connection URL. Use it to restart a worker against the same database.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Opens a new pool on this database.
    pub async fn pool(&self) -> PgPool {
        PgPoolOptions::new()
            .max_connections(POOL_SIZE)
            .connect(&self.url)
            .await
            .unwrap_or_else(|err| panic!("connect to test database {}: {err}", self.name))
    }

    /// Opens one connection to this database.
    pub async fn connect(&self) -> PgConnection {
        PgConnection::connect(&self.url)
            .await
            .unwrap_or_else(|err| panic!("connect to test database {}: {err}", self.name))
    }

    /// Returns `pg_dump --schema-only --no-owner --no-privileges` of this database.
    ///
    /// With the test container, pg_dump runs inside it through `docker exec`, so its
    /// version is the server version. With `MAVERICK_TEST_DATABASE_URL`, the `pg_dump` on
    /// `PATH` runs; its output can differ from the container's.
    pub async fn dump_schema(&self) -> String {
        self.pg_dump(&["--schema-only", "--no-owner", "--no-privileges"])
            .await
    }

    /// Returns a full `pg_dump --no-owner --no-privileges` of this database: schema and
    /// data, the migration history included. [`TestDatabase::restore`] loads it.
    pub async fn dump(&self) -> String {
        self.pg_dump(&["--no-owner", "--no-privileges"]).await
    }

    /// Runs `sql` with `psql -v ON_ERROR_STOP=1` (in the test container when there is
    /// one), so a plain `pg_dump` output loads as psql would load it. Panics on the first
    /// failing statement.
    pub async fn restore(&self, sql: &str) {
        use tokio::io::AsyncWriteExt;
        const FLAGS: [&str; 3] = ["--quiet", "--no-psqlrc", "-v"];
        let mut command = match &self.container_id {
            Some(id) => {
                let mut command = tokio::process::Command::new("docker");
                command
                    .args(["exec", "-i", id.as_str(), "psql"])
                    .args(FLAGS)
                    .args(["ON_ERROR_STOP=1", "--username=maverick", "--dbname"])
                    .arg(self.name.as_str());
                command
            }
            None => {
                let mut command = tokio::process::Command::new("psql");
                command
                    .args(FLAGS)
                    .args(["ON_ERROR_STOP=1", "--dbname"])
                    .arg(self.url.as_str());
                command
            }
        };
        let mut child = command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap_or_else(|err| panic!("run psql for {}: {err}", self.name));
        let mut stdin = child.stdin.take().expect("psql stdin");
        stdin
            .write_all(sql.as_bytes())
            .await
            .unwrap_or_else(|err| panic!("write to psql for {}: {err}", self.name));
        drop(stdin);
        let output = child
            .wait_with_output()
            .await
            .unwrap_or_else(|err| panic!("wait for psql for {}: {err}", self.name));
        assert!(
            output.status.success(),
            "psql restore into {} failed: {}",
            self.name,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    async fn pg_dump(&self, flags: &[&str]) -> String {
        let mut command = match &self.container_id {
            Some(id) => {
                let mut command = tokio::process::Command::new("docker");
                command
                    .args(["exec", id.as_str(), "pg_dump"])
                    .args(flags)
                    .args(["--username=maverick", "--dbname", self.name.as_str()]);
                command
            }
            None => {
                let mut command = tokio::process::Command::new("pg_dump");
                command.args(flags).args(["--dbname", self.url.as_str()]);
                command
            }
        };
        let output = command
            .output()
            .await
            .unwrap_or_else(|err| panic!("run pg_dump for {}: {err}", self.name));
        assert!(
            output.status.success(),
            "pg_dump for {} failed: {}",
            self.name,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("pg_dump output is UTF-8")
    }

    /// Drops the database now. Open connections to it are terminated.
    pub async fn close(mut self) {
        self.closed = true;
        if env_flag(KEEP_DB_VAR) {
            eprintln!("kept test database: {}", self.url);
            return;
        }
        if let Err(message) = drop_database(&self.admin_url, &self.name).await {
            panic!("{message}");
        }
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        if env_flag(KEEP_DB_VAR) {
            eprintln!("kept test database: {}", self.url);
            return;
        }
        // Drop has no async. The test runtime may be single threaded and blocked right
        // here, so the drop runs on its own thread and runtime.
        let admin_url = self.admin_url.clone();
        let name = self.name.clone();
        let outcome = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|err| format!("build runtime: {err}"))?;
            runtime.block_on(drop_database(&admin_url, &name))
        })
        .join();
        // Never panic here: this can run while a failed test unwinds.
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(message)) => eprintln!("{message}"),
            Err(_) => eprintln!("could not drop test database {}", self.name),
        }
    }
}

async fn create(server: &Server, schema: &Schema) -> Result<TestDatabase, String> {
    let template = ensure_template(server, schema).await?;
    let name = format!("{DB_PREFIX}{}", Ulid::generate().to_string().to_lowercase());
    let mut admin = admin_connection(&server.admin_url).await?;
    execute(
        &mut admin,
        format!("CREATE DATABASE {name} TEMPLATE {template}"),
    )
    .await?;
    let _ = admin.close().await;
    Ok(TestDatabase {
        url: with_database(&server.admin_url, &name),
        admin_url: server.admin_url.clone(),
        container_id: server.container_id.clone(),
        name,
        closed: false,
    })
}

/// Builds the template for `schema` once per server and returns its name.
async fn ensure_template(server: &Server, schema: &Schema) -> Result<String, String> {
    let name = schema.template_name();
    let mut ready = READY_TEMPLATES.lock().await;
    if ready.contains(&name) {
        return Ok(name);
    }
    let mut admin = admin_connection(&server.admin_url).await?;
    if ready.is_empty() {
        drop_stale_databases(&mut admin).await;
    }
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(TEMPLATE_LOCK_KEY)
        .execute(&mut admin)
        .await
        .map_err(|err| format!("lock template build: {err}"))?;
    let built = build_template(&mut admin, server, schema, &name).await;
    let _ = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(TEMPLATE_LOCK_KEY)
        .execute(&mut admin)
        .await;
    let _ = admin.close().await;
    built?;
    ready.push(name.clone());
    Ok(name)
}

/// Applies the schema to `<name>_build`, then renames it to `name`. A crash during the
/// build leaves only the `_build` database, which the next build replaces.
async fn build_template(
    admin: &mut PgConnection,
    server: &Server,
    schema: &Schema,
    name: &str,
) -> Result<(), String> {
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)")
            .bind(name)
            .fetch_one(&mut *admin)
            .await
            .map_err(|err| format!("look up template {name}: {err}"))?;
    if exists {
        return Ok(());
    }
    let building = format!("{name}_build");
    execute(
        admin,
        format!("DROP DATABASE IF EXISTS {building} WITH (FORCE)"),
    )
    .await?;
    execute(
        admin,
        format!("CREATE DATABASE {building} TEMPLATE template0"),
    )
    .await?;
    schema
        .apply(&with_database(&server.admin_url, &building))
        .await
        .map_err(|err| format!("apply schema to template {building}: {err}"))?;
    rename_database(admin, &building, name).await
}

/// Renames a database. The rename fails while any session is still connected, and a
/// terminated session takes a moment to exit, so this retries for up to five seconds.
async fn rename_database(admin: &mut PgConnection, from: &str, to: &str) -> Result<(), String> {
    let mut attempts = 0;
    loop {
        let _ = sqlx::query(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
             WHERE datname = $1 AND pid <> pg_backend_pid()",
        )
        .bind(from)
        .execute(&mut *admin)
        .await;
        match execute(admin, format!("ALTER DATABASE {from} RENAME TO {to}")).await {
            Ok(()) => return Ok(()),
            Err(message) if attempts >= 50 => return Err(message),
            Err(_) => {
                attempts += 1;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// Drops test databases that a crashed run left on a shared server.
async fn drop_stale_databases(admin: &mut PgConnection) {
    let Ok(names) = sqlx::query_scalar::<_, String>("SELECT datname FROM pg_database")
        .fetch_all(&mut *admin)
        .await
    else {
        return;
    };
    let now = SystemTime::now();
    for name in names.into_iter().filter(|name| is_stale(name, now)) {
        let _ = execute(
            admin,
            format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"),
        )
        .await;
    }
}

fn is_stale(name: &str, now: SystemTime) -> bool {
    let Some(id) = name.strip_prefix(DB_PREFIX) else {
        return false;
    };
    let Ok(ulid) = Ulid::from_string(id) else {
        return false;
    };
    now.duration_since(ulid.datetime())
        .is_ok_and(|age| age > STALE_AFTER)
}

async fn drop_database(admin_url: &str, name: &str) -> Result<(), String> {
    let mut admin = admin_connection(admin_url).await?;
    execute(
        &mut admin,
        format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"),
    )
    .await?;
    let _ = admin.close().await;
    Ok(())
}

async fn admin_connection(admin_url: &str) -> Result<PgConnection, String> {
    PgConnection::connect(admin_url)
        .await
        .map_err(|err| format!("connect to test Postgres server: {err}"))
}

/// Runs one statement. Every identifier in `sql` is generated by this crate.
async fn execute(conn: &mut PgConnection, sql: String) -> Result<(), String> {
    sqlx::raw_sql(AssertSqlSafe(sql.clone()))
        .execute(conn)
        .await
        .map(|_| ())
        .map_err(|err| format!("{sql}: {err}"))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use ulid::Ulid;

    use super::{DB_PREFIX, STALE_AFTER, is_stale};

    fn db_name(created: SystemTime) -> String {
        let ulid = Ulid::from_datetime(created);
        format!("{DB_PREFIX}{}", ulid.to_string().to_lowercase())
    }

    #[test]
    fn only_old_test_databases_are_stale() {
        let now = SystemTime::now();
        let old = now - STALE_AFTER - Duration::from_secs(60);
        assert!(is_stale(&db_name(old), now));
        assert!(!is_stale(&db_name(now), now));
    }

    #[test]
    fn other_databases_are_never_stale() {
        let now = SystemTime::now();
        assert!(!is_stale("maverick", now));
        assert!(!is_stale("mvtpl_0123456789abcdef", now));
        assert!(!is_stale("mvt_not-a-ulid", now));
    }
}
