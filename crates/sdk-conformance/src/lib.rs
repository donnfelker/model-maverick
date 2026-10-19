//! The SDK conformance harness (sdk-v0 plan §2.3; master plan §3.5, §7.2 N2).
//!
//! One core is specified once and implemented four times (TypeScript, Ruby, Go, Python),
//! and the Rust proxy runs the same behavior. This crate is the thing that keeps them
//! honest: a mock MM engine and a mock provider that run scripted scenarios, plus a
//! language-neutral control API so a test in any language can drive them.
//!
//! The servers are dumb and deterministic. A [`Scenario`] scripts the exact HTTP replies and
//! the servers record what they received, so the provider wire shapes live in the scenario
//! JSON (frozen by bead 8.2), never in translation code here.
//!
//! # Control API (HTTP + JSON)
//!
//! The engine server carries the control endpoints:
//! - `POST /control/scenario` loads a [`Scenario`] and clears the recorders.
//! - `GET /control/recorded` returns the [`Recorded`] requests as JSON.
//! - `POST /control/reset` clears the recorders and reply cursors.
//!
//! # Example (Rust)
//!
//! ```no_run
//! # async fn run() -> std::io::Result<()> {
//! use sdk_conformance::{Harness, Scenario};
//! let harness = Harness::start().await?;
//! let scenario = Scenario::from_json(br#"{"name":"noop"}"#).unwrap();
//! harness.load(scenario);
//! let engine = harness.engine_base_url();
//! // Drive the SDK core against `engine`, then read `harness.recorded()`.
//! # let _ = engine;
//! # Ok(())
//! # }
//! ```

pub mod scenario;
pub mod server;

pub use scenario::{EngineScript, ProviderScript, Reply, Scenario, reply_at};
pub use server::{Harness, Recorded, RecordedRequest};
