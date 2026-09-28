//! Postgres store layer for the engine.
//!
//! This is the only crate that holds SQL. It owns the sqlx pool, the migrations, the
//! compile-time checked queries, and the transaction boundaries. Everything reaches
//! Postgres through this crate (spec §2.2).

use sqlx::migrate::Migrator;

/// Every engine migration in `migrations/`, embedded at compile time.
///
/// Up only (D37). The graphile_worker job queue applies its own schema; it is not here.
pub static MIGRATOR: Migrator = sqlx::migrate!();

/// The version of `migrations/0001_baseline.sql`: the Go schema at goose head (D21).
/// Later migrations are additive and have higher versions.
pub const BASELINE_VERSION: i64 = 1;
