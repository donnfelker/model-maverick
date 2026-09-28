//! The HTTP API and MCP surface.
//!
//! Two axum listeners (public :8080, internal :8081), every endpoint, and the MCP
//! server over rmcp. This crate has no business logic: it validates, calls services,
//! and shapes responses. Recommend and policy hold no SQL; everything reaches Postgres
//! through the store. Package 2d fills this in. The skeleton is intentionally empty.
