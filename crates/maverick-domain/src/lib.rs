//! Pure domain logic for the Model Maverick engine.
//!
//! This crate has no I/O and no SQL. It holds ids, task and grader types, graders,
//! scoring (EWMA and Welford), ranking, policy, ceilings, bands, checks, reasons,
//! prediction and spend math, catalog lifecycle, capture crypto, fingerprints, key
//! format, pure classifier logic, and telemetry derivation. `cargo-mutants` runs on
//! this crate, so it stays fast to build.
//!
//! Package 2a fills this in, one module per work package.

pub mod capture;
pub mod gofmt;
pub mod gojson;
pub mod gostrconv;
pub mod gostrings;
pub mod graders;
pub mod ids;
pub mod keys;
pub mod policy;
pub mod predictions;
pub mod pricing;
pub mod scoring;
pub mod spend;
pub mod tasks;
