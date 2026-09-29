//! Real Postgres for store and job tests.
//!
//! AGENTS.md: store tests and job tests use real Postgres (testcontainers), never mocks.
//! Float32 `real` storage, `timestamptz` behavior, and transaction semantics only match on
//! real Postgres.
//!
//! Each test binary starts one `postgres:16-alpine` container. The image, the user, and
//! the readiness rule are the ones of the Go `storetest` harness (reference Part 1 §5).
//! The container publishes its port on 127.0.0.1 only and gets a random password, because
//! Docker-published ports bypass host firewalls such as ufw.
//! A [`Schema`] is applied once to a template database. Every test gets its own database,
//! cloned from that template, so tests never share rows.
//!
//! ```ignore
//! static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&maverick_store::MIGRATOR));
//!
//! #[tokio::test]
//! async fn inserts_a_tenant() {
//!     let Some(db) = maverick_testdb::database(&SCHEMA).await else { return };
//!     let pool = db.pool().await;
//!     // ...
//!     db.close().await;
//! }
//! ```
//!
//! When Docker is unavailable, [`database`] prints the Go skip line and returns `None`, so
//! the test passes without running. Under CI the same condition fails the test.
//!
//! # Environment
//!
//! | Variable | Effect |
//! |---|---|
//! | `MAVERICK_TEST_DATABASE_URL` | Use this server instead of a container. The role must be allowed to create databases. |
//! | `MAVERICK_TEST_REQUIRE_DB` or `CI` | A missing Docker daemon fails the test instead of skipping it. |
//! | `MAVERICK_TEST_KEEP_DB` | Keep each test database after the test and print its URL. |
//!
//! A present but empty value is treated as unset, the same rule as the engine config.

mod database;
mod schema;
mod server;
mod url;

pub use database::{TestDatabase, database};
pub use schema::{BoxError, Schema};

/// Environment variable that points the helper at an existing Postgres server.
pub const DATABASE_URL_VAR: &str = "MAVERICK_TEST_DATABASE_URL";
/// Environment variable that turns a missing Docker daemon into a test failure.
pub const REQUIRE_DB_VAR: &str = "MAVERICK_TEST_REQUIRE_DB";
/// Environment variable that keeps test databases after each test.
pub const KEEP_DB_VAR: &str = "MAVERICK_TEST_KEEP_DB";

/// Reads an environment variable. A present but empty value counts as unset.
fn env_value(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// Reads a boolean flag. Unset, `0`, and `false` are off; any other value is on.
fn env_flag(name: &str) -> bool {
    env_value(name).is_some_and(|value| value != "0" && !value.eq_ignore_ascii_case("false"))
}
