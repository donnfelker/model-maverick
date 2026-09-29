//! Command dispatch and exit codes (reference Part 1 §7.1).

use std::io::Write;

use maverick_config::{Config, ConfigError};
use maverick_domain::gofmt::quote;

use crate::flags::{FlagSet, Parsed};
use crate::logging::{Output, with_logger};

/// The main usage text (exact; `demo-run` is hidden).
pub const MAIN_USAGE: &str = "Usage: maverick <command> [flags]

Commands:
  serve       run the HTTP API, MCP server, and workers (--api-only | --worker-only)
  migrate     run database migrations
  corpus      load, activate, and inspect corpus versions, and print the tier
              calibration report (load | activate | status | calibrate)
  mcp-stdio   stdio MCP transport for local development (--api-key mk_live_...)
";

/// The `corpus` usage text (exact).
pub const CORPUS_USAGE: &str = "Usage: maverick corpus <subcommand> [flags]

Subcommands:
  load      --version <v> --dir <path> [--description <text>] [--strict]
            validate the task files in <path> and store them under <v>
            (inactive until activated); --strict also fails on corpus-shape
            warnings (20-25 tasks and exactly 5 smoke members per capability)
  activate  --version <v>
            atomically make <v> the active corpus version
  status    list corpus versions and the active flag
  calibrate print the tier calibration report (capability-matrix design 5.3):
            per-capability, per-column tier pass rates from the latest
            measured aggregates, with monotonicity inversions and >15-point
            tier-trend outliers; a report, never a gate
";

/// Exit code for a runtime or configuration failure.
const EXIT_FAILURE: i32 = 1;
/// Exit code for a usage or flag error.
const EXIT_USAGE: i32 = 2;

/// Go `run(args, lookup, stdout, stderr)`: runs one command and returns its exit code.
pub fn run(
    args: &[String],
    lookup: &dyn Fn(&str) -> Option<String>,
    stdout: Output,
    mut stderr: Output,
) -> i32 {
    let Some(command) = args.first() else {
        let _ = stderr.write_all(MAIN_USAGE.as_bytes());
        return EXIT_USAGE;
    };
    let rest = &args[1..];
    match command.as_str() {
        "serve" => serve(rest, lookup, stdout, stderr),
        "migrate" => migrate(rest, lookup, stdout, stderr),
        "demo-run" => demo_run(rest, lookup, stdout, stderr),
        "corpus" => corpus(rest, lookup, stdout, stderr),
        "mcp-stdio" => mcp_stdio(rest, lookup, stderr),
        other => {
            let _ = write!(
                stderr,
                "maverick: unknown command {}\n\n{MAIN_USAGE}",
                quote(other)
            );
            EXIT_USAGE
        }
    }
}

/// Parses a command's flags; any error or `-h` is exit 2 (the usage is already printed).
fn parse_flags(set: &FlagSet, args: &[String], stderr: &mut Output) -> Option<Parsed> {
    set.parse(args, stderr).ok()
}

/// Loads the configuration, logging one ERROR line per issue on failure (§7.2).
fn load_config(lookup: &dyn Fn(&str) -> Option<String>) -> Result<Config, ConfigError> {
    let loaded = maverick_config::load_with(lookup);
    if let Err(error) = &loaded {
        for issue in error.issues() {
            tracing::error!(
                var = issue.var.as_str(),
                reason = issue.reason.as_str(),
                "invalid configuration"
            );
        }
    }
    loaded
}

/// The stop for a command body that another bead still has to write.
fn not_implemented(command: &str, bead: &str) -> i32 {
    tracing::error!(bead, "{command}: not implemented yet");
    EXIT_FAILURE
}

/// `serve [--api-only] [--worker-only]` (§7.3). Logs to stdout.
fn serve(
    args: &[String],
    lookup: &dyn Fn(&str) -> Option<String>,
    stdout: Output,
    mut stderr: Output,
) -> i32 {
    let set = FlagSet::new("serve")
        .bool(
            "api-only",
            "serve only the HTTP API (no background workers)",
        )
        .bool("worker-only", "run only background workers (no public API)");
    let Some(flags) = parse_flags(&set, args, &mut stderr) else {
        return EXIT_USAGE;
    };
    let (api_only, worker_only) = (flags.bool("api-only"), flags.bool("worker-only"));
    let loaded = with_logger(stdout.clone(), || {
        if api_only && worker_only {
            tracing::error!("serve: --api-only and --worker-only are mutually exclusive");
            return Err(EXIT_USAGE);
        }
        load_config(lookup).map_err(|_| EXIT_FAILURE)
    });
    match loaded {
        Ok(config) => crate::serve::serve(config, api_only, worker_only, stdout),
        Err(code) => code,
    }
}

/// `migrate` (§7.7): loads the FULL config (QUIRK: only the database URL is used), then
/// runs the engine and graphile_worker migrations (`maverick_store::migrate`).
fn migrate(
    args: &[String],
    lookup: &dyn Fn(&str) -> Option<String>,
    stdout: Output,
    mut stderr: Output,
) -> i32 {
    let set = FlagSet::new("migrate");
    if parse_flags(&set, args, &mut stderr).is_none() {
        return EXIT_USAGE;
    }
    with_logger(stdout, || {
        let Ok(config) = load_config(lookup) else {
            return EXIT_FAILURE;
        };
        let error: Box<dyn std::error::Error> =
            match block_on(maverick_store::migrate::migrate(&config.database_url)) {
                Ok(Ok(_)) => {
                    tracing::info!("migrations complete");
                    return 0;
                }
                Ok(Err(error)) => error.into(),
                Err(error) => error.into(),
            };
        tracing::error!(error = %error, "migrate failed");
        EXIT_FAILURE
    })
}

/// Runs `future` to completion on a runtime for this thread only, so its log events
/// reach the logger that `with_logger` installed for this thread.
fn block_on<F: Future>(future: F) -> std::io::Result<F::Output> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    Ok(runtime.block_on(future))
}

/// `demo-run [--seed path] [--fake-fixtures dir]` (§7.6), hidden from the usage.
fn demo_run(
    args: &[String],
    lookup: &dyn Fn(&str) -> Option<String>,
    stdout: Output,
    mut stderr: Output,
) -> i32 {
    let set = FlagSet::new("demo-run")
        .string(
            "seed",
            "scripts/demo/seed.sql",
            "SQL seed fixture executed before the batch",
        )
        .string(
            "fake-fixtures",
            "scripts/demo/fixtures",
            "directory of deterministic fake-provider fixtures",
        );
    if parse_flags(&set, args, &mut stderr).is_none() {
        return EXIT_USAGE;
    }
    with_logger(stdout, || {
        if load_config(lookup).is_err() {
            return EXIT_FAILURE;
        }
        not_implemented("demo-run", "model-maverick-rust-port-3hk.3.13")
    })
}

/// `corpus <load|activate|status|calibrate>` (§7.5).
fn corpus(
    args: &[String],
    lookup: &dyn Fn(&str) -> Option<String>,
    stdout: Output,
    mut stderr: Output,
) -> i32 {
    let Some(sub) = args.first() else {
        let _ = stderr.write_all(CORPUS_USAGE.as_bytes());
        return EXIT_USAGE;
    };
    let rest = &args[1..];
    match sub.as_str() {
        "load" => {
            let set = FlagSet::new("corpus load")
                .string("version", "", "corpus version to store the tasks under")
                .string("dir", "", "directory of task files to validate and load")
                .string("description", "", "free-text description of the version")
                .bool(
                    "strict",
                    "also fail on corpus-shape warnings (task counts and smoke members)",
                );
            let Some(flags) = parse_flags(&set, rest, &mut stderr) else {
                return EXIT_USAGE;
            };
            if flags.string("version").is_empty() || flags.string("dir").is_empty() {
                let _ = writeln!(
                    stderr,
                    "maverick corpus load: --version and --dir are required"
                );
                return EXIT_USAGE;
            }
            corpus_body("corpus load", lookup, stdout)
        }
        "activate" => {
            let set = FlagSet::new("corpus activate").string(
                "version",
                "",
                "corpus version to make active",
            );
            let Some(flags) = parse_flags(&set, rest, &mut stderr) else {
                return EXIT_USAGE;
            };
            if flags.string("version").is_empty() {
                let _ = writeln!(stderr, "maverick corpus activate: --version is required");
                return EXIT_USAGE;
            }
            corpus_body("corpus activate", lookup, stdout)
        }
        "status" | "calibrate" => {
            let name = format!("corpus {sub}");
            let set = FlagSet::new(name.clone());
            if parse_flags(&set, rest, &mut stderr).is_none() {
                return EXIT_USAGE;
            }
            corpus_body(&name, lookup, stdout)
        }
        other => {
            let _ = write!(
                stderr,
                "maverick corpus: unknown subcommand {}\n\n{CORPUS_USAGE}",
                quote(other)
            );
            EXIT_USAGE
        }
    }
}

/// The corpus bodies (bead 3.10). Their database helper logs to stdout.
fn corpus_body(command: &str, lookup: &dyn Fn(&str) -> Option<String>, stdout: Output) -> i32 {
    with_logger(stdout, || {
        if load_config(lookup).is_err() {
            return EXIT_FAILURE;
        }
        not_implemented(command, "model-maverick-rust-port-3hk.3.10")
    })
}

/// `mcp-stdio --api-key <key>` (§7.4). Logs to STDERR: stdout is the MCP channel.
fn mcp_stdio(args: &[String], lookup: &dyn Fn(&str) -> Option<String>, mut stderr: Output) -> i32 {
    let set = FlagSet::new("mcp-stdio").string(
        "api-key",
        "",
        "tenant API key (mk_live_...) the stdio session acts as",
    );
    let Some(flags) = parse_flags(&set, args, &mut stderr) else {
        return EXIT_USAGE;
    };
    let api_key = flags.string("api-key").to_owned();
    let loaded = with_logger(stderr.clone(), || {
        if api_key.is_empty() {
            tracing::error!("mcp-stdio: --api-key is required (spec §12)");
            return Err(EXIT_USAGE);
        }
        load_config(lookup).map_err(|_| EXIT_FAILURE)
    });
    match loaded {
        Ok(config) => crate::serve::mcp_stdio(config, &api_key, stderr),
        Err(code) => code,
    }
}
