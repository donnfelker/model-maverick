//! Shared test observability for the Model Maverick engine.
//!
//! Every failing test should explain itself. This crate gives the engine crates four
//! tools that read the same way in every package:
//!
//! - [`init_test_tracing`] installs a per-test `tracing` subscriber that captures every
//!   log line and span at `DEBUG` in the engine's JSON log shape, honors `RUST_LOG`, and
//!   surfaces the captured lines only when the test fails.
//! - The capture layer is also a G5 secret-leak check: if a registered secret or the
//!   [`MAVERICK_TEST_CANARY`] appears in any captured log line, the test fails. This makes
//!   every test that opts in a small check that prompts, completions, and credentials do
//!   not reach logs (spec §14.3).
//! - [`step!`] logs a numbered step with elapsed time, so a failure shows the last step
//!   that completed.
//! - The diff reporters ([`format_json_diff`], [`bytes_diff`], [`float_diff`]) and the
//!   [`ReplayReport`] turn parity mismatches into readable messages with the fixture
//!   anchor and any approved-divergence tag.
//!
//! The helpers never log prompt or corpus content. See `README.md` for the conventions
//! every engine crate follows when it uses this crate.

mod diff;
mod replay;
mod step;
mod tracing_capture;

pub use diff::{
    FieldDiff, assert_bytes_eq, assert_float_eq, assert_json_eq, bytes_diff, float_diff,
    format_json_diff, format_json_diff_tagged, json_diff,
};
pub use replay::{ReplayOutcome, ReplayReport};
pub use tracing_capture::{
    MAVERICK_TEST_CANARY, SecretViolation, TestTracing, init_test_tracing,
    init_test_tracing_with_secrets,
};

// Re-exported for the `step!` macro expansion (`$crate::next_step` / `$crate::log_step`).
#[doc(hidden)]
pub use step::{log_step, next_step};
