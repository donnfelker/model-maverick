//! The golden-fixture harness for grader kinds (Go `graders/testutil`, reference Part 2
//! §12).
//!
//! A grader kind's tests run every fixture in its directory:
//!
//! ```ignore
//! mod common;
//! let dir = parity_support::fixture_path("measurement-io", "testdata/internal/graders/golden/testdata")?;
//! common::golden::run_dir(Arc::new(GoldenAnswer), &dir);
//! ```
//!
//! Fixture format (YAML, unknown keys rejected): `description`, exactly one of `spec`
//! (the payload) or `envelope` (a whole §5.4 envelope whose `type` must equal the
//! grader kind; its `weight_dimensions` are not applied), `output`, optional
//! `task {id, capability, prompt}`, optional `run {status (default ok),
//! model_column_id, tokens_in, tokens_out, latency_ms}`, optional `epsilon` (default
//! 1e-6), and `expect {dimensions, detail_contains, error, error_contains}`.
//!
//! Per fixture: (1) it must expect dimensions or an error. (2) `validate_spec`: a
//! failure passes only if the fixture expects an error whose text contains
//! `error_contains`. (3) Grade through a fresh [`Registry`] with a counting failure
//! counter. An expected error must be a grader failure containing `error_contains`,
//! with the counter at 1. Otherwise the counter stays at 0, the dimension names match
//! exactly, each score is within epsilon, and the detail contains every
//! `detail_contains` substring.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use maverick_domain::gojson::{self, JsonValue};
use maverick_domain::graders::{
    FailureCounter, GradeError, GradeInput, GradeResult, GradeTask, Grader, Registry, RunMeta,
    parse_envelope_value,
};
use serde::Deserialize;

use super::block_on;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fixture {
    #[serde(default)]
    pub description: String,
    pub spec: Option<serde_norway::Value>,
    pub envelope: Option<serde_norway::Value>,
    #[serde(default)]
    pub output: String,
    pub task: Option<FixtureTask>,
    pub run: Option<FixtureRun>,
    pub epsilon: Option<f64>,
    pub expect: Expect,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureTask {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub capability: String,
    #[serde(default)]
    pub prompt: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureRun {
    #[serde(default = "ok")]
    pub status: String,
    #[serde(default)]
    pub model_column_id: String,
    #[serde(default)]
    pub tokens_in: i64,
    #[serde(default)]
    pub tokens_out: i64,
    #[serde(default)]
    pub latency_ms: i64,
}

fn ok() -> String {
    "ok".to_owned()
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    pub dimensions: Option<BTreeMap<String, f64>>,
    #[serde(default)]
    pub detail_contains: Vec<String>,
    #[serde(default)]
    pub error: bool,
    #[serde(default)]
    pub error_contains: String,
}

/// The default score tolerance.
pub const EPSILON: f64 = 1e-6;

/// A failure counter that counts.
#[derive(Debug, Default)]
pub struct Counting(AtomicUsize);

impl Counting {
    pub fn count(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

impl FailureCounter for Counting {
    fn grader_failure(&self, _kind: &str) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// The fixture files of `dir`: `.yaml` / `.yml`, not recursive, sorted by name.
pub fn fixture_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|entry| entry.expect("dir entry").path())
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .is_some_and(|ext| ext == "yaml" || ext == "yml")
        })
        .collect();
    files.sort();
    files
}

/// Runs every fixture in `dir` against `grader` and panics with the list of failing
/// cases. Returns the case names (file names without the extension). An empty
/// directory fails.
pub fn run_dir(grader: Arc<dyn Grader>, dir: &Path) -> Vec<String> {
    let files = fixture_files(dir);
    assert!(!files.is_empty(), "no golden fixtures in {}", dir.display());
    let mut names = Vec::new();
    let mut failures = Vec::new();
    for path in files {
        let name = path
            .file_stem()
            .expect("file name")
            .to_string_lossy()
            .into_owned();
        if let Err(message) = run_file(grader.clone(), &path) {
            failures.push(format!("{name}: {message}"));
        }
        names.push(name);
    }
    assert!(
        failures.is_empty(),
        "golden fixtures failed in {}:\n{}",
        dir.display(),
        failures.join("\n")
    );
    names
}

/// Loads one fixture and resolves its spec payload.
pub fn load_fixture(grader_kind: &str, path: &Path) -> Result<(Fixture, JsonValue), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("read: {e}"))?;
    let fixture: Fixture = serde_norway::from_str(&text).map_err(|e| format!("fixture: {e}"))?;
    let spec = match (&fixture.spec, &fixture.envelope) {
        (Some(spec), None) => gojson::from_yaml(spec)?,
        (None, Some(envelope)) => {
            let envelope = parse_envelope_value(envelope).map_err(|e| e.to_string())?;
            if envelope.kind.as_str() != grader_kind {
                return Err(format!(
                    "envelope type {} is not the grader kind {grader_kind}",
                    envelope.kind
                ));
            }
            envelope.spec
        }
        _ => return Err("fixture needs exactly one of spec or envelope".to_owned()),
    };
    Ok((fixture, spec))
}

/// Grades a fixture's output through a fresh registry. Returns the result and the
/// failure count.
pub fn grade_fixture(
    grader: Arc<dyn Grader>,
    fixture: &Fixture,
    spec: &JsonValue,
) -> (Result<GradeResult, GradeError>, usize) {
    let counter = Arc::new(Counting::default());
    let mut registry = Registry::with_failure_counter(counter.clone());
    let kind = grader.kind().to_owned();
    registry.must_register(grader);
    let task = fixture.task.as_ref().map(|t| GradeTask {
        id: t.id.clone(),
        capability: t.capability.clone(),
        prompt: t.prompt.clone(),
        attachments_json: String::new(),
    });
    let run = match &fixture.run {
        Some(run) => RunMeta {
            run_id: String::new(),
            model_column_id: run.model_column_id.clone(),
            status: run.status.clone(),
            tokens_in: run.tokens_in,
            tokens_out: run.tokens_out,
            latency_ms: run.latency_ms,
        },
        None => RunMeta {
            status: ok(),
            ..RunMeta::default()
        },
    };
    let input = GradeInput {
        task: task.as_ref(),
        output: &fixture.output,
        run: Some(&run),
    };
    let result = block_on(registry.grade(&kind, spec, input));
    (result, counter.count())
}

/// Runs one fixture file through the harness procedure.
pub fn run_file(grader: Arc<dyn Grader>, path: &Path) -> Result<(), String> {
    let (fixture, spec) = load_fixture(grader.kind(), path)?;
    let expect = &fixture.expect;
    if expect.dimensions.is_none() && !expect.error {
        return Err("fixture must expect dimensions or an error".to_owned());
    }

    if let Err(error) = grader.validate_spec(&spec) {
        if expect.error && error.contains(&expect.error_contains) {
            return Ok(());
        }
        return Err(format!("validate_spec: {error}"));
    }

    let (result, failures) = grade_fixture(grader, &fixture, &spec);
    if expect.error {
        return match result {
            Err(GradeError::Failed(error)) => {
                let text = error.to_string();
                if !text.contains(&expect.error_contains) {
                    Err(format!("error {text:?} lacks {:?}", expect.error_contains))
                } else if failures != 1 {
                    Err(format!("failure counter {failures}, want 1"))
                } else {
                    Ok(())
                }
            }
            Err(other) => Err(format!("error is not a grader failure: {other}")),
            Ok(result) => Err(format!("expected an error, got {result:?}")),
        };
    }

    let result = result.map_err(|e| format!("grade: {e}"))?;
    if failures != 0 {
        return Err(format!("failure counter {failures}, want 0"));
    }
    let want = expect.dimensions.as_ref().expect("checked above");
    let got_names: Vec<&String> = result.dimensions.keys().collect();
    let want_names: Vec<&String> = want.keys().collect();
    if got_names != want_names {
        return Err(format!("dimensions {got_names:?}, want {want_names:?}"));
    }
    let epsilon = fixture.epsilon.unwrap_or(EPSILON);
    for (dim, &want_score) in want {
        let got = result.dimensions[dim];
        if (got - want_score).abs() > epsilon {
            return Err(format!(
                "{dim} = {got}, want {want_score} (epsilon {epsilon})"
            ));
        }
    }
    for needle in &expect.detail_contains {
        if !result.detail.contains(needle.as_str()) {
            return Err(format!("detail {:?} lacks {needle:?}", result.detail));
        }
    }
    Ok(())
}
