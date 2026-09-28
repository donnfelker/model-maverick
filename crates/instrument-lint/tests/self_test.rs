//! Self-test: the binary must fail on the bad fixture and pass on the good one.

use std::path::PathBuf;
use std::process::Command;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn run(path: PathBuf) -> (i32, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_instrument-lint"))
        .arg(path)
        .output()
        .expect("instrument-lint runs");
    let stdout = String::from_utf8(output.stdout).expect("utf-8 output");
    (output.status.code().expect("exit code"), stdout)
}

#[test]
fn bad_fixture_fails_with_each_violation_located() {
    let (code, stdout) = run(fixture("bad"));
    assert_eq!(code, 1, "{stdout}");
    let locations: Vec<&str> = stdout
        .lines()
        .map(|line| {
            let rest = line.split("bare.rs:").nth(1).expect("path prefix");
            rest.split(": ").next().expect("location")
        })
        .collect();
    assert_eq!(locations, ["6:1", "11:1", "17:1"], "{stdout}");
    assert!(stdout.contains("bare #[instrument] records every argument"));
    assert!(stdout.contains("without skip_all"));
    assert!(stdout.contains("#[instrument(ret)] records the return value"));
}

#[test]
fn good_fixture_passes() {
    let (code, stdout) = run(fixture("good"));
    assert_eq!(code, 0, "{stdout}");
    assert!(
        stdout.contains("1 file(s) checked, no violations"),
        "{stdout}"
    );
}

#[test]
fn workspace_walk_skips_the_fixture_directory() {
    // Walking this crate must not report the deliberate violations in tests/fixtures.
    let (code, stdout) = run(PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    assert_eq!(code, 0, "{stdout}");
}

#[test]
fn missing_arguments_is_a_usage_error() {
    let output = Command::new(env!("CARGO_BIN_EXE_instrument-lint"))
        .output()
        .expect("instrument-lint runs");
    assert_eq!(output.status.code(), Some(2));
}
