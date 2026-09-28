//! Postgres store layer for the engine.
//!
//! This is the only crate that holds SQL. It owns the sqlx pool, the migrations, the
//! compile-time checked queries, and the transaction boundaries. Everything reaches
//! Postgres through this crate (spec §2.2). Package 2b fills this in. The skeleton is
//! intentionally empty.
