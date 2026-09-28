//! Per-test tracing capture and the G5 secret-leak guard.
//!
//! [`init_test_tracing`] installs a scoped subscriber for the current test thread. Each
//! event is rendered into the engine's JSON log shape (slog keys `time`, `level`, `msg`,
//! then the event fields; spec §14.3, §10.1) and pushed into an in-memory buffer. If a
//! registered secret appears in any rendered line, the guard fails the test when it drops.
//! Captured lines print only when the test fails, so passing tests stay quiet.

use std::sync::{Arc, Mutex, PoisonError};

use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry::LookupSpan;

/// A canary string a test can plant in a prompt or request body. If it ever reaches a
/// captured log line the guard fails the test. This mirrors the G5 acceptance canary
/// (spec §14.3): a prompt-form request carrying a canary, after which the canary must
/// appear in no log line.
pub const MAVERICK_TEST_CANARY: &str = "MAVERICK-CANARY-DO-NOT-LOG";

/// One captured line that contained a registered secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretViolation {
    /// The registered secret that leaked.
    pub secret: String,
    /// The full rendered log line that contained it.
    pub line: String,
}

struct CaptureState {
    lines: Vec<String>,
    secrets: Vec<String>,
    violations: Vec<SecretViolation>,
}

#[derive(Clone)]
struct CaptureLayer {
    state: Arc<Mutex<CaptureState>>,
}

impl<S> Layer<S> for CaptureLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let metadata = event.metadata();

        let mut fields = serde_json::Map::new();
        let mut visitor = JsonVisitor {
            fields: &mut fields,
            message: None,
        };
        event.record(&mut visitor);

        let mut spans = Vec::new();
        if let Some(scope) = ctx.event_scope(event) {
            for span in scope.from_root() {
                spans.push(serde_json::Value::String(span.name().to_string()));
            }
        }

        let mut object = serde_json::Map::new();
        object.insert(
            "time".to_string(),
            serde_json::Value::String(
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            ),
        );
        object.insert(
            "level".to_string(),
            serde_json::Value::String(metadata.level().to_string()),
        );
        object.insert(
            "target".to_string(),
            serde_json::Value::String(metadata.target().to_string()),
        );
        if let Some(message) = visitor.message {
            object.insert("msg".to_string(), serde_json::Value::String(message));
        }
        for (key, value) in fields {
            object.insert(key, value);
        }
        if !spans.is_empty() {
            object.insert("spans".to_string(), serde_json::Value::Array(spans));
        }

        let line = serde_json::Value::Object(object).to_string();

        let mut guard = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let state = &mut *guard;
        for secret in &state.secrets {
            if !secret.is_empty() && line.contains(secret) {
                state.violations.push(SecretViolation {
                    secret: secret.clone(),
                    line: line.clone(),
                });
            }
        }
        state.lines.push(line);
    }
}

/// Renders `tracing` event fields into a JSON map, lifting the `message` field into `msg`.
struct JsonVisitor<'a> {
    fields: &'a mut serde_json::Map<String, serde_json::Value>,
    message: Option<String>,
}

impl JsonVisitor<'_> {
    fn insert(&mut self, field: &Field, value: serde_json::Value) {
        self.fields.insert(field.name().to_string(), value);
    }
}

impl Visit for JsonVisitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let rendered = format!("{value:?}");
        if field.name() == "message" {
            self.message = Some(rendered);
        } else {
            self.insert(field, serde_json::Value::String(rendered));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_string());
        } else {
            self.insert(field, serde_json::Value::String(value.to_string()));
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.insert(field, serde_json::Value::from(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.insert(field, serde_json::Value::from(value));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.insert(field, serde_json::Value::from(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.insert(field, serde_json::Value::from(value));
    }
}

/// A scoped guard for one test. Drop it (at the end of the test) to reset the subscriber
/// and, if any registered secret leaked into the logs, fail the test.
pub struct TestTracing {
    state: Arc<Mutex<CaptureState>>,
    _guard: tracing::subscriber::DefaultGuard,
}

/// Install a per-test tracing subscriber with only the default canary registered.
///
/// The subscriber is scoped to the current thread and reset when the returned guard drops,
/// so tests running in parallel do not see each other's logs. `RUST_LOG` narrows the
/// captured levels; the default is `debug`.
#[must_use]
pub fn init_test_tracing() -> TestTracing {
    init_test_tracing_with_secrets(&[])
}

/// Install a per-test tracing subscriber and register extra secrets to watch for, in
/// addition to [`MAVERICK_TEST_CANARY`].
#[must_use]
pub fn init_test_tracing_with_secrets(secrets: &[&str]) -> TestTracing {
    crate::step::reset_steps();

    let mut registered = vec![MAVERICK_TEST_CANARY.to_string()];
    registered.extend(secrets.iter().map(|secret| (*secret).to_string()));

    let state = Arc::new(Mutex::new(CaptureState {
        lines: Vec::new(),
        secrets: registered,
        violations: Vec::new(),
    }));

    let layer = CaptureLayer {
        state: Arc::clone(&state),
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("debug"));
    let subscriber = tracing_subscriber::registry().with(layer.with_filter(filter));
    let guard = tracing::subscriber::set_default(subscriber);

    TestTracing {
        state,
        _guard: guard,
    }
}

impl TestTracing {
    fn lock(&self) -> std::sync::MutexGuard<'_, CaptureState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Register another secret to watch for. Register before the code under test logs;
    /// lines captured earlier are not rescanned.
    pub fn register_secret(&self, secret: &str) {
        self.lock().secrets.push(secret.to_string());
    }

    /// The rendered JSON log lines captured so far, oldest first.
    #[must_use]
    pub fn captured_lines(&self) -> Vec<String> {
        self.lock().lines.clone()
    }

    /// The captured lines parsed back into JSON values (lines that fail to parse are
    /// skipped, which should not happen for lines this layer wrote).
    #[must_use]
    pub fn captured_json(&self) -> Vec<serde_json::Value> {
        self.captured_lines()
            .iter()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    /// The secret leaks detected so far.
    #[must_use]
    pub fn secret_violations(&self) -> Vec<SecretViolation> {
        self.lock().violations.clone()
    }

    /// Remove and return the recorded violations. A test that intentionally exercises the
    /// leak check calls this so the drop guard does not then fail the test.
    pub fn take_violations(&self) -> Vec<SecretViolation> {
        std::mem::take(&mut self.lock().violations)
    }

    /// Print every captured line to stderr. Called automatically when a test fails.
    pub fn dump(&self) {
        for line in self.captured_lines() {
            eprintln!("{line}");
        }
    }
}

impl Drop for TestTracing {
    fn drop(&mut self) {
        let violations = self.secret_violations();

        if std::thread::panicking() {
            // The test is already failing; print the captured logs to explain why.
            self.dump();
            if !violations.is_empty() {
                eprintln!(
                    "test-support: {} secret leak(s) also detected during a failing test",
                    violations.len()
                );
            }
            return;
        }

        if !violations.is_empty() {
            self.dump();
            let detail: Vec<String> = violations
                .iter()
                .map(|violation| {
                    format!(
                        "  secret {:?} appeared in log line: {}",
                        violation.secret, violation.line
                    )
                })
                .collect();
            panic!(
                "test-support: {} secret(s) leaked into captured logs (G5 violation):\n{}",
                violations.len(),
                detail.join("\n")
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_events_in_slog_shape() {
        let capture = init_test_tracing();
        tracing::info!(
            method = "GET",
            path = "/v1/recommend",
            status = 200,
            "request"
        );

        let events = capture.captured_json();
        let request = events
            .iter()
            .find(|event| event["msg"] == "request")
            .expect("request line captured");
        assert_eq!(request["level"], "INFO");
        assert_eq!(request["method"], "GET");
        assert_eq!(request["path"], "/v1/recommend");
        assert_eq!(request["status"], 200);
        assert!(request.get("time").is_some(), "time field present");
        assert!(request.get("target").is_some(), "target field present");
    }

    #[test]
    fn flags_a_registered_secret_that_reaches_the_log() {
        let capture = init_test_tracing();
        capture.register_secret("mk_live_supersecretkey");
        tracing::info!(token = "mk_live_supersecretkey", "leaked a key");

        let violations = capture.take_violations();
        assert_eq!(violations.len(), 1, "one violation recorded");
        assert_eq!(violations[0].secret, "mk_live_supersecretkey");
        assert!(violations[0].line.contains("mk_live_supersecretkey"));
    }

    #[test]
    fn ignores_lines_without_a_secret() {
        let capture = init_test_tracing();
        tracing::info!(status = 200, "clean line");
        assert!(capture.secret_violations().is_empty());
    }

    #[test]
    fn default_canary_is_registered() {
        let capture = init_test_tracing();
        tracing::info!("prompt echoed: {}", MAVERICK_TEST_CANARY);
        let violations = capture.take_violations();
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].secret, MAVERICK_TEST_CANARY);
    }
}
