//! The grading framework: the grader trait, the registry, and the failure convention
//! (spec §5.2, reference Part 2 §6.1).
//!
//! A grader turns one model output into scores per dimension in `[0, 1]` plus a JSON
//! detail that is stored in `scores.detail`. A grader **error** is not a low score:
//! the run's dimensions for that grader are absent, the failure counter goes up once,
//! and the scoring step renormalizes over the dimensions that remain.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use super::spec::SpecValidator;
use crate::gofmt;
use crate::gojson::JsonValue;

/// A boxed, sendable future. The judge grader awaits provider calls, so grading is
/// async; the deterministic graders return a ready future.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Any grader error.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The grader-facing view of one run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunMeta {
    pub run_id: String,
    pub model_column_id: String,
    pub status: String,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub latency_ms: i64,
}

/// The task fields a grader can read (a `tasks` row). The judge grader rebuilds the
/// benchmarked model's prompt from `prompt` and the raw `attachments_json`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GradeTask {
    pub id: String,
    pub capability: String,
    pub prompt: String,
    /// The `attachments` column as JSON text (empty means none).
    pub attachments_json: String,
}

/// What a grader sees: the task (for reference-anchored graders), the model output,
/// and the run.
#[derive(Debug, Clone, Copy)]
pub struct GradeInput<'a> {
    pub task: Option<&'a GradeTask>,
    pub output: &'a str,
    pub run: Option<&'a RunMeta>,
}

/// A successful grade: score per emitted dimension, and the detail JSON text stored in
/// `scores.detail` exactly as the grader wrote it.
#[derive(Debug, Clone, PartialEq)]
pub struct GradeResult {
    pub dimensions: BTreeMap<String, f64>,
    pub detail: String,
}

/// One grader kind. [`SpecValidator::validate_spec`] checks a spec at corpus load;
/// [`Grader::grade`] scores an output against a spec.
pub trait Grader: SpecValidator {
    /// Grades `input.output` against `spec` (the envelope's spec payload). An error
    /// means the dimensions are absent for this run, never zero.
    fn grade<'a>(
        &'a self,
        spec: &'a JsonValue,
        input: GradeInput<'a>,
    ) -> BoxFuture<'a, Result<GradeResult, BoxError>>;
}

/// A grader failure: `grader <kind>: <err>`. Its [`std::error::Error::source`] is the
/// grader's own error.
#[derive(Debug)]
pub struct GraderError {
    pub kind: String,
    pub source: BoxError,
}

impl GraderError {
    pub fn new(kind: impl Into<String>, source: impl Into<BoxError>) -> Self {
        GraderError {
            kind: kind.into(),
            source: source.into(),
        }
    }
}

impl std::fmt::Display for GraderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "grader {}: {}", self.kind, self.source)
    }
}

impl std::error::Error for GraderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Why [`Registry::grade`] failed.
#[derive(Debug)]
pub enum GradeError {
    /// No grader has this kind. The failure counter is not incremented.
    NotRegistered(String),
    /// The grader failed. The failure counter was incremented once.
    Failed(GraderError),
}

impl std::fmt::Display for GradeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GradeError::NotRegistered(kind) => write!(
                f,
                "graders: grade: no grader registered for kind {}",
                gofmt::quote(kind)
            ),
            GradeError::Failed(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for GradeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            GradeError::NotRegistered(_) => None,
            GradeError::Failed(error) => Some(error),
        }
    }
}

/// Counts grader failures per kind (a metric in production, a counter in tests).
pub trait FailureCounter: Send + Sync {
    fn grader_failure(&self, kind: &str);
}

/// A counter that discards failures.
struct NoCounter;

impl FailureCounter for NoCounter {
    fn grader_failure(&self, _kind: &str) {}
}

/// Why [`Registry::register`] refused a grader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterError {
    EmptyKind,
    Duplicate(String),
}

impl std::fmt::Display for RegisterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegisterError::EmptyKind => f.write_str("graders: register: empty kind"),
            RegisterError::Duplicate(kind) => {
                write!(
                    f,
                    "graders: register: duplicate kind {}",
                    gofmt::quote(kind)
                )
            }
        }
    }
}

impl std::error::Error for RegisterError {}

/// The graders by kind.
pub struct Registry {
    graders: BTreeMap<String, Arc<dyn Grader>>,
    counter: Arc<dyn FailureCounter>,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("kinds", &self.kinds())
            .finish_non_exhaustive()
    }
}

impl Registry {
    /// An empty registry that does not count failures.
    pub fn new() -> Self {
        Self::with_failure_counter(Arc::new(NoCounter))
    }

    /// An empty registry that reports each grader failure to `counter`.
    pub fn with_failure_counter(counter: Arc<dyn FailureCounter>) -> Self {
        Registry {
            graders: BTreeMap::new(),
            counter,
        }
    }

    /// Adds a grader. Its kind must be non-empty and not yet registered.
    pub fn register(&mut self, grader: Arc<dyn Grader>) -> Result<(), RegisterError> {
        let kind = grader.kind();
        if kind.is_empty() {
            return Err(RegisterError::EmptyKind);
        }
        if self.graders.contains_key(kind) {
            return Err(RegisterError::Duplicate(kind.to_owned()));
        }
        self.graders.insert(kind.to_owned(), grader);
        Ok(())
    }

    /// [`Registry::register`], panicking on an error (for static wiring).
    pub fn must_register(&mut self, grader: Arc<dyn Grader>) {
        if let Err(error) = self.register(grader) {
            panic!("{error}");
        }
    }

    pub fn lookup(&self, kind: &str) -> Option<&Arc<dyn Grader>> {
        self.graders.get(kind)
    }

    /// The registered kinds in ascending order.
    pub fn kinds(&self) -> Vec<&str> {
        self.graders.keys().map(String::as_str).collect()
    }

    /// Grades with the grader for `kind`. On a grader error the failure counter goes up
    /// once and the error is a [`GraderError`]: the grader's own `GraderError` as it is,
    /// any other error wrapped as `grader <kind>: <err>`.
    pub async fn grade(
        &self,
        kind: &str,
        spec: &JsonValue,
        input: GradeInput<'_>,
    ) -> Result<GradeResult, GradeError> {
        let Some(grader) = self.graders.get(kind) else {
            return Err(GradeError::NotRegistered(kind.to_owned()));
        };
        match grader.grade(spec, input).await {
            Ok(result) => Ok(result),
            Err(error) => {
                self.counter.grader_failure(kind);
                let error = match error.downcast::<GraderError>() {
                    Ok(grader_error) => *grader_error,
                    Err(other) => GraderError {
                        kind: kind.to_owned(),
                        source: other,
                    },
                };
                Err(GradeError::Failed(error))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Named(&'static str);

    impl SpecValidator for Named {
        fn kind(&self) -> &str {
            self.0
        }

        fn validate_spec(&self, _spec: &JsonValue) -> Result<(), String> {
            Ok(())
        }
    }

    impl Grader for Named {
        fn grade<'a>(
            &'a self,
            _spec: &'a JsonValue,
            _input: GradeInput<'a>,
        ) -> BoxFuture<'a, Result<GradeResult, BoxError>> {
            Box::pin(async { Err("unused".into()) })
        }
    }

    #[test]
    fn debug_lists_the_kinds() {
        let mut registry = Registry::default();
        assert_eq!(format!("{registry:?}"), "Registry { kinds: [], .. }");
        registry.must_register(Arc::new(Named("rule_scan")));
        registry.must_register(Arc::new(Named("golden_answer")));
        assert_eq!(
            format!("{registry:?}"),
            "Registry { kinds: [\"golden_answer\", \"rule_scan\"], .. }"
        );
    }
}
