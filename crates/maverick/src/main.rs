//! The engine binary and CLI entry point.
//!
//! One binary, three shapes: `serve` (everything), `serve --api-only` (stateless
//! replica), `serve --worker-only` (exactly one). Subcommands: `migrate`,
//! `corpus load | activate | status | calibrate`, `mcp-stdio --api-key`, `demo-run`.
//! Postgres is the whole state, including the job queue.

use maverick_cli::{Output, run};

fn main() {
    // Go reads arguments as bytes; a non-UTF-8 argument becomes lossy text here.
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let code = run(
        &args,
        &|name| std::env::var(name).ok(),
        Output::new(std::io::stdout()),
        Output::new(std::io::stderr()),
    );
    std::process::exit(code);
}
