//! `instrument-lint <path>...`: fails if any `#[instrument]` under the paths lacks
//! `skip_all`. See the library docs for the rules.
//!
//! Exit codes: 0 when clean, 1 when there are violations, 2 on usage or read errors.

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let roots: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    if roots.is_empty() {
        eprintln!("usage: instrument-lint <path>...");
        return ExitCode::from(2);
    }

    let report = instrument_lint::check_paths(&roots);
    for (path, finding) in &report.findings {
        println!(
            "{}:{}:{}: {}",
            path.display(),
            finding.line,
            finding.column,
            finding.violation.message()
        );
    }
    for (path, error) in &report.errors {
        eprintln!("{}: {error}", path.display());
    }

    if !report.errors.is_empty() {
        eprintln!(
            "instrument-lint: {} file(s) could not be checked",
            report.errors.len()
        );
        ExitCode::from(2)
    } else if !report.findings.is_empty() {
        eprintln!(
            "instrument-lint: {} violation(s) in {} file(s) checked",
            report.findings.len(),
            report.files_checked
        );
        ExitCode::from(1)
    } else if report.files_checked == 0 {
        eprintln!("instrument-lint: no .rs files found");
        ExitCode::from(2)
    } else {
        println!(
            "instrument-lint: {} file(s) checked, no violations",
            report.files_checked
        );
        ExitCode::SUCCESS
    }
}
