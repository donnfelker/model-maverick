//! The measurement runner.
//!
//! Drives corpus tasks through provider clients and graders to produce runs and
//! scores. Package 2c fills this in.

pub mod assemble;

pub use assemble::{AssembleError, BenchColumn, BenchTask, assemble};
