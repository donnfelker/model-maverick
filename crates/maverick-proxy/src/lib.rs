//! The proxy (`maverick-proxy`).
//!
//! A separate HTTP service with no database. A client points its base URL here. A
//! `maverick/*` route projects the workload, calls `POST /v1/recommend`, and walks the
//! returned chain. An explicit model calls its provider directly, without recommending
//! or reporting (D45). Both paths use the caller's credentials. The proxy never uses
//! MM-owned provider keys for customer traffic or translates provider wire formats.
//!
//! Reference part 6; spec v1 §8.1 "Proxy behavior"; master plan §3.5.
//!
//! - [`config`]: environment configuration and the deployment mode (D12, D13, D38).
//! - [`credentials`]: provider credential resolution and upstream headers (D12, D30).
//! - [`direct`]: explicit provider models without recommendations or telemetry (D45).
//! - [`envelope`]: the exact error envelope and routing responses.
//! - [`server`]: routing, the request pipeline, and the HTTP server.
//! - [`chain`]: the chain-walk seam that the chain executor plugs into.
//! - [`surface`]: the chat and messages surfaces on the `maverick-core` chain executor.
//! - [`rewrite`]: Go `rewriteModel`, the per-attempt upstream body.
//! - [`relay`]: the relay of the settled attempt (streaming or buffered).
//! - [`reports`]: each settled request's report to telemetry (D27).
//! - [`ratelimit`]: the per-tenant fixed-window rate limit (D28).

pub mod chain;
pub mod config;
pub mod credentials;
pub mod direct;
pub mod envelope;
pub mod otel;
pub mod ratelimit;
pub mod relay;
pub mod reports;
pub mod rewrite;
pub mod server;
pub mod surface;
