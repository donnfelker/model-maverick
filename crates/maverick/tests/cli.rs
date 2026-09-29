//! Parity: `parity/fixtures/store/cli.json` (`cmd/maverick` `run` black-box cases).
//!
//! Every row whose command stops before a database is exercised here with exact
//! streams and exit codes. Rows that need the database (or a corpus directory) belong
//! to the beads that write those command bodies; they are listed in `OTHER_BEADS` so
//! none is dropped silently. On `*-unreachable-db` rows the `error` text comes from the
//! Go database driver, and the fixture marks it informative: it is masked after checking
//! it is present.

use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use maverick_cli::{Output, run};
use maverick_domain::gojson::{self, JsonValue};

const FIXTURE: &str = include_str!("../../../parity/fixtures/store/cli.json");

/// Rows whose command body is another bead's.
const OTHER_BEADS: [(&str, &str); 0] = [];

/// The Go `corpus load` cases point `--dir` at the Go task testdata; the Rust tree keeps that
/// testdata under `parity/fixtures/measurement-io/testdata/`. Rewrite the path in both the args
/// and the expected output so the `rejected <dir>` line still matches.
const GO_TESTDATA: &str = "../../internal/tasks/testdata";
const RUST_TESTDATA: &str = "../../parity/fixtures/measurement-io/testdata/internal/tasks/testdata";

fn field<'a>(value: &'a JsonValue, key: &str) -> &'a JsonValue {
    match value {
        JsonValue::Object(members) => members
            .iter()
            .rev()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value)
            .unwrap_or_else(|| panic!("missing {key}")),
        other => panic!("{key}: not an object: {other:?}"),
    }
}

fn text(value: &JsonValue) -> &str {
    match value {
        JsonValue::String(s) => s,
        other => panic!("not a string: {other:?}"),
    }
}

fn items(value: &JsonValue) -> &[JsonValue] {
    match value {
        JsonValue::Array(items) => items,
        other => panic!("not an array: {other:?}"),
    }
}

fn env(value: &JsonValue) -> Vec<(String, String)> {
    match value {
        JsonValue::Null => Vec::new(),
        JsonValue::Object(members) => members
            .iter()
            .map(|(name, value)| (name.clone(), text(value).to_owned()))
            .collect(),
        other => panic!("env: {other:?}"),
    }
}

#[derive(Clone)]
struct Shared(Arc<Mutex<Vec<u8>>>);

impl Write for Shared {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn invoke(args: &[String], env: &[(String, String)]) -> (i32, String, String) {
    let out = Arc::new(Mutex::new(Vec::new()));
    let err = Arc::new(Mutex::new(Vec::new()));
    let lookup = |name: &str| {
        env.iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    };
    let code = run(
        args,
        &lookup,
        Output::new(Shared(out.clone())),
        Output::new(Shared(err.clone())),
    );
    let read = |buffer: &Arc<Mutex<Vec<u8>>>| {
        String::from_utf8(buffer.lock().expect("lock").clone()).expect("utf8")
    };
    (code, read(&out), read(&err))
}

/// Masks each JSON log timestamp as `<TS>`, checking it is RFC3339Nano first.
fn mask_times(stream: &str) -> String {
    const KEY: &str = "{\"time\":\"";
    let mut out = String::with_capacity(stream.len());
    let mut rest = stream;
    while let Some(at) = rest.find(KEY) {
        let start = at + KEY.len();
        let len = rest[start..].find('"').expect("closing quote");
        let stamp = &rest[start..start + len];
        assert!(
            stamp.len() >= 20
                && stamp.as_bytes()[4] == b'-'
                && stamp.as_bytes()[10] == b'T'
                && stamp.ends_with('Z'),
            "not RFC3339Nano: {stamp}"
        );
        out.push_str(&rest[..start]);
        out.push_str("<TS>");
        rest = &rest[start + len..];
    }
    out.push_str(rest);
    out
}

/// Masks each JSON log `error` value as `<ERR>`, checking it is not empty first.
fn mask_error(stream: &str) -> String {
    const KEY: &str = ",\"error\":\"";
    let mut out = String::with_capacity(stream.len());
    let mut rest = stream;
    while let Some(at) = rest.find(KEY) {
        let start = at + KEY.len();
        let bytes = rest.as_bytes();
        let mut end = start;
        while bytes[end] != b'"' {
            end += if bytes[end] == b'\\' { 2 } else { 1 };
        }
        assert!(end > start, "empty error text: {stream}");
        out.push_str(&rest[..start]);
        out.push_str("<ERR>");
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

#[test]
fn mask_error_keeps_everything_but_the_error_text() {
    assert_eq!(
        mask_error("{\"msg\":\"x\",\"error\":\"a \\\"b\\\" c\",\"n\":1}\n"),
        "{\"msg\":\"x\",\"error\":\"<ERR>\",\"n\":1}\n"
    );
    assert_eq!(mask_error("{\"msg\":\"x\"}"), "{\"msg\":\"x\"}");
}

// parity: main.TestRunNoCommandPrintsUsage
// parity: main.TestRunUnknownCommand
// parity: main.TestRunCorpusWithoutSubcommand
// parity: main.TestRunCorpusUnknownSubcommand
// parity: main.TestRunCorpusLoadRequiresVersionAndDir
// parity: main.TestRunCorpusActivateRequiresVersion
// parity: main.TestRunMCPStdioRequiresAPIKey
// parity: main.TestRunMCPStdioFailsFastNamingMissingVarsOnStderr
// parity: main.TestRunMCPStdioFailsOnUnreachableDatabase
// parity: main.TestRunServeFailsFastNamingMissingVars
// parity: main.TestRunServeFailsFastNamingMalformedVar
// parity: main.TestRunServeLogsJSON
// parity: main.TestRunServeRejectsConflictingFlags
// parity: main.TestRunMigrateFailsFastOnMissingConfig
// parity: main.TestRunMigrateFailsOnUnreachableDatabase
#[test]
fn cli_rows_match_go() {
    let fixture = gojson::unmarshal(FIXTURE).expect("cli.json parses");
    let cases = items(field(&fixture, "cases"));
    assert_eq!(cases.len(), 24);

    let ids: Vec<&str> = cases.iter().map(|case| text(field(case, "id"))).collect();
    for (id, _) in OTHER_BEADS {
        assert!(ids.contains(&id), "deferred row {id} is not in the fixture");
    }

    let mut checked = 0;
    for case in cases {
        let id = text(field(case, "id"));
        if OTHER_BEADS.iter().any(|(other, _)| *other == id) {
            continue;
        }
        let args: Vec<String> = items(field(case, "args"))
            .iter()
            .map(|arg| text(arg).replace(GO_TESTDATA, RUST_TESTDATA))
            .collect();
        let expected_code: i32 = match field(case, "exit_code") {
            JsonValue::Number(n) => n.parse().expect("exit code"),
            other => panic!("exit_code: {other:?}"),
        };

        let informative = id.ends_with("-unreachable-db");
        let mask = |stream: &str| {
            if informative {
                mask_error(stream)
            } else {
                stream.to_owned()
            }
        };

        let (code, stdout, stderr) = invoke(&args, &env(field(case, "env")));
        assert_eq!(code, expected_code, "{id}: exit code");
        assert_eq!(
            mask(&mask_times(&stdout)),
            mask(&text(field(case, "stdout")).replace(GO_TESTDATA, RUST_TESTDATA)),
            "{id}: stdout was {stdout}"
        );
        assert_eq!(
            mask(&mask_times(&stderr)),
            mask(&text(field(case, "stderr")).replace(GO_TESTDATA, RUST_TESTDATA)),
            "{id}: stderr was {stderr}"
        );
        checked += 1;
    }
    assert_eq!(checked, 24);
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_owned()).collect()
}

#[test]
fn demo_run_is_hidden_but_parses_its_flags() {
    let (code, stdout, stderr) = invoke(&strings(&["demo-run", "-h"]), &[]);
    assert_eq!(code, 2);
    assert_eq!(stdout, "");
    assert_eq!(
        stderr,
        "Usage of demo-run:\n  -fake-fixtures string\n    \tdirectory of deterministic \
         fake-provider fixtures (default \"scripts/demo/fixtures\")\n  -seed string\n    \tSQL \
         seed fixture executed before the batch (default \"scripts/demo/seed.sql\")\n"
    );
    assert!(!maverick_cli::MAIN_USAGE.contains("demo-run"));
}

#[test]
fn flag_errors_exit_two_before_any_logging() {
    for args in [
        &["migrate", "--nope"][..],
        &["mcp-stdio", "--api-key"],
        &["corpus", "status", "-x"],
        &["corpus", "load", "--strict=maybe"],
    ] {
        let (code, stdout, stderr) = invoke(&strings(args), &[]);
        assert_eq!(code, 2, "{args:?}");
        assert_eq!(stdout, "", "{args:?}");
        assert!(stderr.contains("Usage of "), "{args:?}: {stderr}");
    }
}

/// A valid config reaches `demo-run`'s body (bead 3.13), which migrates first: an
/// unreachable database fails there, loudly, on one JSON line.
#[test]
fn valid_config_reaches_the_demo_body_and_fails_loudly() {
    let env = [
        ("MAVERICK_DATABASE_URL", "postgres://127.0.0.1:1/maverick"),
        ("MAVERICK_INTERNAL_TOKENS", "rails:s3cret"),
        (
            "MAVERICK_CAPTURE_MASTER_KEY",
            "abababababababababababababababababababababababababababababababab",
        ),
    ]
    .map(|(k, v)| (k.to_owned(), v.to_owned()));
    let (code, stdout, stderr) = invoke(&strings(&["demo-run"]), &env);
    assert_eq!(code, 1);
    assert_eq!(stderr, "");
    assert_eq!(
        mask_error(&mask_times(&stdout)),
        "{\"time\":\"<TS>\",\"level\":\"ERROR\",\"msg\":\"migrate failed\",\"error\":\"<ERR>\"}\n"
    );
}
