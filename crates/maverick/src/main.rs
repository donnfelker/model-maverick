//! The engine binary and CLI entry point.
//!
//! One binary, three shapes: `serve` (everything), `serve --api-only` (stateless
//! replica), `serve --worker-only` (exactly one). Subcommands: `migrate`,
//! `corpus load | activate | status | calibrate`, `mcp-stdio --api-key`, `demo-run`.
//! Postgres is the whole state, including the job queue. Package 2b wires the CLI and
//! config; later packages wire the listeners and workers.

fn main() {
    // Skeleton entry point.
}
