//! Command dispatch and exit codes (reference Part 1 §7.1).

use std::cell::RefCell;
use std::io::{IsTerminal, Write};

use maverick_config::{CompatWarning, Config, ConfigError};
use maverick_domain::gofmt::quote;

use crate::flags::{FlagSet, Parsed};
use crate::logging::{Output, with_logger};

/// The main usage text (Go's text plus the D50 `upgrade` command and `--version` flag;
/// `demo-run` and `spend` are hidden).
pub const MAIN_USAGE: &str = "Usage: maverick <command> [flags]

Commands:
  serve       run the HTTP API, MCP server, and workers (--api-only | --worker-only)
  upgrade     check and apply the schema for this release (--check | --yes | --json)
  migrate     run database migrations (low level; operators use upgrade)
  corpus      load, activate, and inspect corpus versions, and print the tier
              calibration report (load | activate | status | calibrate)
  mcp-stdio   stdio MCP transport for local development (--api-key mk_live_...)

Flags:
  --version   print the release, build, schema, and bundle formats
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

/// The `spend` usage text (bead 9.15). The command is hidden: [`MAIN_USAGE`] is Go's
/// exact text and does not list it.
pub const SPEND_USAGE: &str =
    "Usage: maverick spend preview [--job nightly_smoke|weekly_sweep] [--night YYYY-MM-DD]

Subcommands:
  preview   print what the batch would project and what the spend guard would
            decide (allow, warn, or refuse), per group and per column, from the
            active corpus, the runnable columns, and the runs already costed in
            the night and week windows; read only: no lease, no alert, no run
";

/// Exit code for a runtime or configuration failure.
const EXIT_FAILURE: i32 = 1;
/// Exit code for a usage or flag error.
const EXIT_USAGE: i32 = 2;

thread_local! {
    /// The process environment's variable names while [`run_with_env`] runs (D50). Empty
    /// under [`run`], so a test that calls `run` sees no compatibility warning.
    static ENV_NAMES: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// [`run`] for the binary. `env_names` are the process environment's variable names, so a
/// command that loads the configuration warns once about each unknown or renamed
/// `MAVERICK_*` setting (spec §14.5, D50).
pub fn run_with_env(
    args: &[String],
    lookup: &dyn Fn(&str) -> Option<String>,
    env_names: Vec<String>,
    stdout: Output,
    stderr: Output,
) -> i32 {
    ENV_NAMES.with(|names| *names.borrow_mut() = env_names);
    let code = run(args, lookup, stdout, stderr);
    ENV_NAMES.with(|names| names.borrow_mut().clear());
    code
}

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
        "upgrade" => upgrade(rest, lookup, stdout, stderr),
        "demo-run" => demo_run(rest, lookup, stdout, stderr),
        "corpus" => corpus(rest, lookup, stdout, stderr),
        "spend" => spend(rest, lookup, stdout, stderr),
        "mcp-stdio" => mcp_stdio(rest, lookup, stderr),
        "--version" => version(rest, stdout, stderr),
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

/// `--version` (spec §14.5, D50): prints the version text to stdout. It takes no arguments.
fn version(args: &[String], mut stdout: Output, mut stderr: Output) -> i32 {
    if !args.is_empty() {
        let _ = write!(
            stderr,
            "maverick: --version takes no arguments\n\n{MAIN_USAGE}"
        );
        return EXIT_USAGE;
    }
    let _ = stdout.write_all(crate::version::text().as_bytes());
    0
}

/// Parses a command's flags; any error or `-h` is exit 2 (the usage is already printed).
fn parse_flags(set: &FlagSet, args: &[String], stderr: &mut Output) -> Option<Parsed> {
    set.parse(args, stderr).ok()
}

/// Loads the configuration, logging one ERROR line per issue on failure (§7.2), and one
/// WARN line per unknown or renamed setting (D50). A warning names variables only, never a
/// value, and never stops the command.
pub(crate) fn load_config(lookup: &dyn Fn(&str) -> Option<String>) -> Result<Config, ConfigError> {
    ENV_NAMES.with(|names| {
        for warning in maverick_config::compatibility_warnings(&names.borrow(), lookup) {
            match warning {
                CompatWarning::Unknown { name } => {
                    tracing::warn!(var = name.as_str(), "unknown setting ignored");
                }
                CompatWarning::Renamed { old, new } => {
                    tracing::warn!(
                        var = old,
                        replacement = new,
                        "renamed setting, use the new name"
                    );
                }
                CompatWarning::OldIgnored { old, new } => {
                    tracing::warn!(
                        var = old,
                        replacement = new,
                        "renamed setting ignored, the new name is set"
                    );
                }
            }
        }
    });
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

/// `upgrade [--check] [--yes] [--json]` (spec §14.5, D50). The report goes to stdout;
/// log lines and the confirmation prompt go to stderr.
fn upgrade(
    args: &[String],
    lookup: &dyn Fn(&str) -> Option<String>,
    mut stdout: Output,
    mut stderr: Output,
) -> i32 {
    let set = FlagSet::new("upgrade")
        .bool(
            "check",
            "run the preflight and the plan only, and change nothing",
        )
        .bool("yes", "apply without asking")
        .bool("json", "print the report as one JSON object");
    let Some(flags) = parse_flags(&set, args, &mut stderr) else {
        return EXIT_USAGE;
    };
    if !flags.args().is_empty() {
        let _ = write!(
            stderr,
            "maverick: upgrade takes no arguments\n\n{MAIN_USAGE}"
        );
        return EXIT_USAGE;
    }
    let options = crate::upgrade::Options {
        check: flags.bool("check"),
        yes: flags.bool("yes"),
        json: flags.bool("json"),
        interactive: std::io::stdin().is_terminal(),
    };
    let operator = format!(
        "cli:{}",
        lookup("USER")
            .filter(|user| !user.is_empty())
            .unwrap_or_else(|| "unknown".to_owned())
    );
    let mut prompt = stderr.clone();
    with_logger(stderr, || {
        let Ok(config) = load_config(lookup) else {
            return EXIT_FAILURE;
        };
        let stdin = std::io::stdin();
        let mut input = stdin.lock();
        let ran = block_on(crate::upgrade::run(
            &config.database_url,
            &operator,
            options,
            &mut input,
            &mut prompt,
            &mut stdout,
        ));
        ran.unwrap_or_else(|error| {
            tracing::error!(error = %error, "upgrade failed");
            EXIT_FAILURE
        })
    })
}

/// Runs `future` to completion on a runtime for this thread only, so its log events
/// reach the logger that `with_logger` installed for this thread.
pub(crate) fn block_on<F: Future>(future: F) -> std::io::Result<F::Output> {
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
    let Some(flags) = parse_flags(&set, args, &mut stderr) else {
        return EXIT_USAGE;
    };
    crate::demo::run(
        flags.string("seed"),
        flags.string("fake-fixtures"),
        lookup,
        stdout,
    )
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
            crate::corpus::load(
                flags.string("version"),
                flags.string("dir"),
                flags.string("description"),
                flags.bool("strict"),
                lookup,
                stdout,
                stderr,
            )
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
            crate::corpus::activate(flags.string("version"), lookup, stdout, stderr)
        }
        "status" => {
            let set = FlagSet::new("corpus status");
            if parse_flags(&set, rest, &mut stderr).is_none() {
                return EXIT_USAGE;
            }
            crate::corpus::status(lookup, stdout, stderr)
        }
        "calibrate" => {
            let set = FlagSet::new("corpus calibrate");
            if parse_flags(&set, rest, &mut stderr).is_none() {
                return EXIT_USAGE;
            }
            crate::corpus::calibrate(lookup, stdout, stderr)
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

/// `spend preview [--job <kind>] [--night <YYYY-MM-DD>]` (bead 9.15), hidden from the usage.
fn spend(
    args: &[String],
    lookup: &dyn Fn(&str) -> Option<String>,
    stdout: Output,
    mut stderr: Output,
) -> i32 {
    match args.first().map(String::as_str) {
        Some("preview") => {}
        Some(other) => {
            let _ = write!(
                stderr,
                "maverick spend: unknown subcommand {}\n\n{SPEND_USAGE}",
                quote(other)
            );
            return EXIT_USAGE;
        }
        None => {
            let _ = stderr.write_all(SPEND_USAGE.as_bytes());
            return EXIT_USAGE;
        }
    }
    let set = FlagSet::new("spend preview")
        .string(
            "job",
            "nightly_smoke",
            "the batch to preview: nightly_smoke or weekly_sweep",
        )
        .string(
            "night",
            "",
            "the batch's night, YYYY-MM-DD in UTC (default: today)",
        );
    let Some(flags) = parse_flags(&set, &args[1..], &mut stderr) else {
        return EXIT_USAGE;
    };
    if !flags.args().is_empty() {
        let _ = writeln!(
            stderr,
            "maverick spend preview: unexpected positional arguments"
        );
        return EXIT_USAGE;
    }
    let job = match flags.string("job") {
        "nightly_smoke" => maverick_jobs::JobKind::NightlySmoke,
        "weekly_sweep" => maverick_jobs::JobKind::WeeklySweep,
        other => {
            let _ = writeln!(
                stderr,
                "maverick spend preview: --job must be nightly_smoke or weekly_sweep, not {}",
                quote(other)
            );
            return EXIT_USAGE;
        }
    };
    let night = match flags.string("night") {
        "" => chrono::Utc::now().date_naive(),
        text => match chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d") {
            Ok(night) if text.len() == 10 && night.format("%Y-%m-%d").to_string() == text => night,
            _ => {
                let _ = writeln!(
                    stderr,
                    "maverick spend preview: --night must be YYYY-MM-DD, not {}",
                    quote(text)
                );
                return EXIT_USAGE;
            }
        },
    };
    crate::spend::preview(job, night, lookup, stdout, stderr)
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
