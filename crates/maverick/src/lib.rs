//! The `maverick` command line (reference Part 1 §7): dispatch, Go-compatible flags,
//! usage text, exit codes, and Go `slog`-shaped JSON logging.
//!
//! `run` takes the arguments, an environment lookup, and the two output streams, so
//! tests drive every command without a process. Subcommand bodies that need a database
//! or listeners are filled in by their own beads (`serve` 3.12, `corpus` 3.10,
//! `demo-run` 3.13, `mcp-stdio` the MCP stdio bead); until then they stop after config
//! loading with an explicit "not implemented yet" error.

mod cli;
mod corpus;
mod demo;
pub mod flags;
pub mod logging;
pub mod otel;
mod rewatch;
mod serve;
mod spend;

pub use cli::{CORPUS_USAGE, MAIN_USAGE, SPEND_USAGE, run};
pub use logging::Output;
