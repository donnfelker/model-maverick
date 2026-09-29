//! Postgres store layer for the engine.
//!
//! This is the only crate that holds SQL. It owns the sqlx pool, the migrations, the
//! compile-time checked queries, and the transaction boundaries. Everything reaches
//! Postgres through this crate (spec §2.2).
//!
//! Query functions follow the Go sqlc catalog (reference Part 7): the SQL text, the
//! ordering, and the NULL handling are the Go ones. Each takes any [`sqlx::PgExecutor`],
//! so it runs on a pool, a connection, or a transaction. A `:one` query that finds no
//! row returns [`sqlx::Error::RowNotFound`] (see [`is_not_found`]).
//!
//! Column types (reference Part 1 §4): `real` is `f32`, `numeric` is [`Decimal`], and
//! `jsonb` is raw text ([`Json`]).

use sqlx::migrate::Migrator;

pub mod alerts;
pub mod batches;
pub mod capture;
pub mod catalog;
pub mod corpus;
pub mod decisions;
pub mod health;
pub mod measurement;
pub mod tenancy;

pub use rust_decimal::Decimal;
pub use sqlx::types::JsonRawValue;

/// A raw `jsonb` value. Writes send the text as given. Reads return the Postgres
/// normalized text (keys sorted, canonical spacing), so a read is not byte-identical to
/// the write. The Go engine behaves the same way.
pub type Json = Box<JsonRawValue>;

/// Every engine migration in `migrations/`, embedded at compile time.
///
/// Up only (D37). The graphile_worker job queue applies its own schema; it is not here.
pub static MIGRATOR: Migrator = sqlx::migrate!();

/// The version of `migrations/0001_baseline.sql`: the Go schema at goose head (D21).
/// Later migrations are additive and have higher versions.
pub const BASELINE_VERSION: i64 = 1;

/// True when a `:one` query found no row (pgx `ErrNoRows` in the Go engine).
pub fn is_not_found(err: &sqlx::Error) -> bool {
    matches!(err, sqlx::Error::RowNotFound)
}

/// The SQLSTATE of a database error, for example `23505` for a unique violation.
pub fn sqlstate(err: &sqlx::Error) -> Option<String> {
    err.as_database_error()
        .and_then(|db| db.code())
        .map(|code| code.into_owned())
}

/// Converts a `numeric` to the nearest `f64`, like pgx `Numeric.Float64Value`, which
/// parses the decimal text.
pub fn numeric_to_f64(value: Decimal) -> f64 {
    value
        .to_string()
        .parse()
        .expect("a decimal's text is a valid f64 literal")
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::{Decimal, numeric_to_f64};

    #[test]
    fn numeric_converts_to_the_nearest_f64() {
        assert_eq!(
            numeric_to_f64(Decimal::from_str("0.012345").unwrap()),
            0.012345
        );
        assert_eq!(numeric_to_f64(Decimal::from_str("3.000000").unwrap()), 3.0);
        assert_eq!(numeric_to_f64(Decimal::from_str("-0.1").unwrap()), -0.1);
    }
}
