//! The trial proxy (`maverick-proxy`).
//!
//! A separate HTTP service with no database. A client points its base URL here and sets
//! `model` to a `maverick/*` alias. Per request the proxy parses the alias, projects the
//! workload, calls the engine's `POST /v1/recommend`, and walks the returned chain against
//! the providers with the caller's own credential. It never uses MM-owned provider keys
//! for customer traffic and never translates between provider wire formats.
//!
//! Reference part 6; spec v1 §8.1 "Trial proxy behavior"; master plan §3.5.
//!
//! - [`config`]: environment configuration and the deployment mode (D12, D13, D38).
//! - [`credentials`]: provider credential resolution and upstream headers (D12, D30).
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
pub mod envelope;
pub mod otel;
pub mod ratelimit;
pub mod relay;
pub mod reports;
pub mod rewrite;
pub mod server;
pub mod surface;
