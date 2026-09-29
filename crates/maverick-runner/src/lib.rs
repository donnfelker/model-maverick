//! The measurement runner.
//!
//! Drives corpus tasks through provider clients to produce `runs` rows: prompt assembly,
//! per-provider worker pools and pacing, the status taxonomy, and cost from the column's
//! price snapshot. Persistence goes through a [`RunSink`], so this crate needs no store.

pub mod assemble;
pub mod cancel;
pub mod pacer;
pub mod run;

pub use assemble::{AssembleError, BenchColumn, BenchTask, assemble};
pub use cancel::{Cancellation, Cancelled};
pub use pacer::{Pacer, interval_for_rpm};
pub use run::{
    Batch, BatchColumn, BatchTask, Clients, DEFAULT_MAX_CONCURRENCY, DEFAULT_REPEATS,
    DEFAULT_TIMEOUT, ProviderLimits, RUNNABLE_STATUSES, RunKey, RunRow, RunSink, Runner,
    RunnerConfig, SinkError, SinkFuture, Summary, error_text, status_for_error,
};
