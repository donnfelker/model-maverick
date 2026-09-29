//! Go `log/slog` JSON lines (reference Part 1 §7.2), as a `tracing` layer:
//! `{"time":"<RFC3339Nano>","level":"INFO|WARN|ERROR","msg":"...",<attrs in call order>}`.
//!
//! Only INFO and above from the engine's own crates (targets starting with `maverick`)
//! are written, as Go logged only the engine's own lines. Attributes keep the order of
//! the `tracing` macro call. Durations should be logged as integer nanoseconds, as Go
//! did.
//!
//! A [`maverick_api::audit::JsonField`] attribute (a list or an object) is written as
//! raw JSON, as `slog` wrote a slice or struct. [`dispatch_with`] adds the OpenTelemetry
//! layer (spec §14.3) under the same engine-target filter.
//!
//! Time: Go stamped `time.Now()` in the local zone. The engine runs with `TZ=UTC` (the
//! container default), so this writes UTC with a `Z` suffix; the fixtures mask the value.

use std::fmt::{self, Write as _};
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use maverick_api::audit::JSON_FIELD_MARK;
use maverick_domain::gojson;
use tracing::field::{Field, Visit};
use tracing::{Dispatch, Event, Level, Subscriber};
use tracing_subscriber::filter::{FilterFn, LevelFilter};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

/// A shared, cloneable output stream (Go's `io.Writer` argument to `run`).
#[derive(Clone)]
pub struct Output(Arc<Mutex<Box<dyn Write + Send>>>);

impl Output {
    pub fn new(writer: impl Write + Send + 'static) -> Self {
        Output(Arc::new(Mutex::new(Box::new(writer))))
    }
}

impl fmt::Debug for Output {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Output")
    }
}

impl Write for Output {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut inner = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut inner = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner.flush()
    }
}

impl<'a> MakeWriter<'a> for Output {
    type Writer = Output;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// The `tracing` layer that writes Go `slog` JSON lines.
pub struct GoJsonLayer {
    out: Output,
}

impl<S: Subscriber> Layer<S> for GoJsonLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let line = format_line(&now_rfc3339_nano(), *event.metadata().level(), event);
        let mut out = self.out.clone();
        let _ = out.write_all(line.as_bytes());
        let _ = out.flush();
    }
}

/// A dispatcher that writes the engine's INFO-and-above events to `out`.
pub fn dispatch(out: Output) -> Dispatch {
    dispatch_with(out, None)
}

/// [`dispatch`], plus export of the engine's spans and events through `otel` when set.
/// Both layers see only the engine's own targets at INFO and above, so no dependency's
/// span (a SQL statement, an HTTP client call) is exported.
pub fn dispatch_with(out: Output, otel: Option<&crate::otel::Otel>) -> Dispatch {
    let slog = GoJsonLayer { out }
        .with_filter(LevelFilter::INFO)
        .with_filter(engine_targets());
    let otel = otel.map(|otel| {
        otel.layer()
            .with_filter(LevelFilter::INFO)
            .with_filter(engine_targets())
    });
    Dispatch::new(tracing_subscriber::registry().with(slog).with(otel))
}

fn engine_targets() -> FilterFn<impl Fn(&tracing::Metadata<'_>) -> bool> {
    FilterFn::new(|metadata: &tracing::Metadata<'_>| metadata.target().starts_with("maverick"))
}

/// Runs `body` with a Go-shaped JSON logger writing to `out` as the current thread's
/// default. The multi-threaded `serve` runtime installs [`dispatch`] globally instead.
pub fn with_logger<T>(out: Output, body: impl FnOnce() -> T) -> T {
    tracing::dispatcher::with_default(&dispatch(out), body)
}

/// Formats one event as a JSON line.
fn format_line(time: &str, level: Level, event: &Event<'_>) -> String {
    let mut visitor = AttrVisitor::default();
    event.record(&mut visitor);
    let mut line = String::with_capacity(96);
    line.push_str("{\"time\":");
    push_json_string(&mut line, time);
    line.push_str(",\"level\":");
    push_json_string(&mut line, level_name(level));
    line.push_str(",\"msg\":");
    push_json_string(&mut line, &visitor.message);
    for (key, value) in &visitor.attrs {
        line.push(',');
        push_json_string(&mut line, key);
        line.push(':');
        line.push_str(value);
    }
    line.push_str("}\n");
    line
}

/// Go `slog` level names.
fn level_name(level: Level) -> &'static str {
    match level {
        Level::ERROR => "ERROR",
        Level::WARN => "WARN",
        Level::INFO => "INFO",
        _ => "DEBUG",
    }
}

/// Collects the message and the other fields (as JSON text) in call order.
#[derive(Default)]
struct AttrVisitor {
    message: String,
    attrs: Vec<(String, String)>,
}

impl AttrVisitor {
    fn push_string(&mut self, field: &Field, value: &str) {
        let mut json = String::with_capacity(value.len() + 2);
        push_json_string(&mut json, value);
        self.attrs.push((field.name().to_owned(), json));
    }
}

impl Visit for AttrVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_owned();
        } else {
            self.push_string(field, value);
        }
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.attrs
            .push((field.name().to_owned(), value.to_string()));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.attrs
            .push((field.name().to_owned(), value.to_string()));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.attrs
            .push((field.name().to_owned(), value.to_string()));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        // `slog` writes a float with `json.Marshal` (`1e+21`, `1e-7`).
        let text = if let Ok(json) = gojson::encode_f64(value) {
            json
        } else {
            // Go's JSON handler cannot encode these either; it logs them as strings.
            let mut json = String::new();
            push_json_string(&mut json, &value.to_string());
            json
        };
        self.attrs.push((field.name().to_owned(), text));
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.push_string(field, &value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let text = format!("{value:?}");
        if field.name() == "message" {
            self.message = text;
        } else if let Some(json) = text.strip_prefix(JSON_FIELD_MARK) {
            self.attrs.push((field.name().to_owned(), json.to_owned()));
        } else {
            self.push_string(field, &text);
        }
    }
}

/// Appends `s` as a JSON string the way Go `slog` does: `"` and `\` escaped, `\b`,
/// `\f`, `\n`, `\r`, `\t`, other control bytes as `\u00xx`, and U+2028/U+2029 as
/// escapes. Unlike `encoding/json`, `<`, `>`, and `&` are not escaped.
fn push_json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// The current time as Go's `time.RFC3339Nano` in UTC.
pub fn now_rfc3339_nano() -> String {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    rfc3339_nano_utc(
        i64::try_from(since_epoch.as_secs()).unwrap_or(i64::MAX),
        since_epoch.subsec_nanos(),
    )
}

/// `2006-01-02T15:04:05.999999999Z07:00` for a UTC instant: the fraction drops its
/// trailing zeros, and the dot too when the fraction is zero.
pub fn rfc3339_nano_utc(unix_seconds: i64, nanos: u32) -> String {
    let days = unix_seconds.div_euclid(86_400);
    let second_of_day = unix_seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let mut out = format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}",
        second_of_day / 3600,
        second_of_day % 3600 / 60,
        second_of_day % 60,
    );
    if nanos > 0 {
        let fraction = format!("{nanos:09}");
        out.push('.');
        out.push_str(fraction.trim_end_matches('0'));
    }
    out.push('Z');
    out
}

/// Days since 1970-01-01 to (year, month, day) in the proleptic Gregorian calendar
/// (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month as u32, day as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture(body: impl FnOnce()) -> String {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        with_logger(Output::new(SharedBuffer(buffer.clone())), body);
        let bytes = buffer.lock().expect("lock").clone();
        String::from_utf8(bytes).expect("utf8")
    }

    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBuffer {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("lock").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn mask_time(line: &str) -> String {
        let start = line.find("\"time\":\"").expect("time") + 8;
        let end = start + line[start..].find('"').expect("end");
        format!("{}<TS>{}", &line[..start], &line[end..])
    }

    #[test]
    fn lines_have_go_shape_and_call_order() {
        let out = capture(|| {
            tracing::error!(var = "MAVERICK_X", reason = %"bad", n = 3u64, ok = true, "invalid configuration");
            tracing::info!(elapsed_ns = 1_500i64, "done");
            tracing::debug!("hidden");
        });
        let lines: Vec<String> = out.lines().map(mask_time).collect();
        assert_eq!(
            lines,
            [
                r#"{"time":"<TS>","level":"ERROR","msg":"invalid configuration","var":"MAVERICK_X","reason":"bad","n":3,"ok":true}"#,
                r#"{"time":"<TS>","level":"INFO","msg":"done","elapsed_ns":1500}"#,
            ]
        );
    }

    /// The exact Go line for both listeners (the field capture is in maverick-api).
    #[test]
    fn request_line_has_go_order_and_float_ms() {
        let out = capture(|| {
            let log = maverick_api::observe::RequestLog::default();
            log.set_identity("ten_42", "key_42");
            log.set_recommendation_id("rec_7");
            maverick_api::observe::log_request(
                "POST",
                "/v1/recommend",
                201,
                std::time::Duration::from_micros(1_234),
                "01J00000000000000000000000",
                &log,
            );
            let internal = maverick_api::observe::RequestLog::default();
            internal.set_actor("ops");
            maverick_api::observe::log_request(
                "GET",
                "/internal/v1/alerts",
                200,
                std::time::Duration::from_micros(500),
                "rid",
                &internal,
            );
        });
        let lines: Vec<String> = out.lines().map(mask_time).collect();
        assert_eq!(
            lines,
            [
                r#"{"time":"<TS>","level":"INFO","msg":"request","method":"POST","path":"/v1/recommend","status":201,"ms":1.234,"request_id":"01J00000000000000000000000","tenant_id":"ten_42","key_id":"key_42","recommendation_id":"rec_7"}"#,
                r#"{"time":"<TS>","level":"INFO","msg":"request","method":"GET","path":"/internal/v1/alerts","status":200,"ms":0.5,"request_id":"rid","actor":"ops"}"#,
            ]
        );
    }

    #[test]
    fn audit_lines_carry_counts_and_json_fields() {
        let out = capture(|| {
            maverick_api::audit::tenant_create("ops", "ten_1", "Acme", 2);
            maverick_api::audit::weights_replace("rails", "ten_1", 1);
            maverick_api::audit::tenant_suspend("ops", "ten_1", "suspended");
            tracing::info!(
                pool = ?maverick_api::audit::JsonField::of(&["gemini-3.1-pro/direct"]),
                "judge pool unavailable"
            );
        });
        let lines: Vec<String> = out.lines().map(mask_time).collect();
        assert_eq!(
            lines,
            [
                r#"{"time":"<TS>","level":"INFO","msg":"internal provisioning write","audit":true,"actor":"ops","action":"tenant.create","tenant_id":"ten_1","name":"Acme","default_pool_columns":2}"#,
                r#"{"time":"<TS>","level":"INFO","msg":"internal provisioning write","audit":true,"actor":"rails","action":"tenant.weights.replace","tenant_id":"ten_1","weights":1}"#,
                r#"{"time":"<TS>","level":"INFO","msg":"internal provisioning write","audit":true,"actor":"ops","action":"tenant.suspend","tenant_id":"ten_1","status":"suspended"}"#,
                r#"{"time":"<TS>","level":"INFO","msg":"judge pool unavailable","pool":["gemini-3.1-pro/direct"]}"#,
            ]
        );
    }

    #[test]
    fn catalog_change_lines() {
        use maverick_store::catalog::{CatalogChange, log_change};
        let out = capture(|| {
            log_change(CatalogChange::Status, "col_1", "cold_start", "active");
            log_change(
                CatalogChange::Pricing,
                "col_1",
                r#"{"input_per_mtok":1}"#,
                r#"{"input_per_mtok":2}"#,
            );
            log_change(
                CatalogChange::ExecutionAliases,
                "col_1",
                "{}",
                r#"{"a":"b"}"#,
            );
        });
        let lines: Vec<String> = out.lines().map(mask_time).collect();
        assert_eq!(
            lines,
            [
                r#"{"time":"<TS>","level":"INFO","msg":"catalog_changes","change":"status","column_id":"col_1","old":"cold_start","new":"active"}"#,
                r#"{"time":"<TS>","level":"INFO","msg":"catalog_changes","change":"pricing","column_id":"col_1","old":"{\"input_per_mtok\":1}","new":"{\"input_per_mtok\":2}"}"#,
                r#"{"time":"<TS>","level":"INFO","msg":"catalog_changes","change":"execution_aliases","column_id":"col_1","old":"{}","new":"{\"a\":\"b\"}"}"#,
            ]
        );
    }

    #[test]
    fn floats_use_go_json_form() {
        let out = capture(|| tracing::info!(big = 1e21f64, small = 1e-7f64, "floats"));
        assert!(out.contains(r#""big":1e+21,"small":1e-7}"#), "{out}");
    }

    #[test]
    fn warn_level_name() {
        let out = capture(|| tracing::warn!("careful"));
        assert!(out.contains(r#""level":"WARN","msg":"careful""#), "{out}");
    }

    #[test]
    fn foreign_targets_are_not_logged() {
        let out = capture(|| tracing::error!(target: "sqlx::query", "slow statement"));
        assert_eq!(out, "");
    }

    #[test]
    fn json_strings_escape_like_slog() {
        let mut out = String::new();
        push_json_string(&mut out, "a\"b\\c\nd\te\u{1}f<g>&h\u{2028}§");
        assert_eq!(out, "\"a\\\"b\\\\c\\nd\\te\\u0001f<g>&h\\u2028§\"");
        let mut out = String::new();
        push_json_string(&mut out, "\u{8}\u{c}\r");
        assert_eq!(out, r#""\b\f\r""#);
    }

    #[test]
    fn rfc3339_nano_trims_the_fraction() {
        assert_eq!(rfc3339_nano_utc(0, 0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_nano_utc(0, 500_000_000), "1970-01-01T00:00:00.5Z");
        assert_eq!(
            rfc3339_nano_utc(0, 123_456_789),
            "1970-01-01T00:00:00.123456789Z"
        );
        assert_eq!(rfc3339_nano_utc(0, 1_000), "1970-01-01T00:00:00.000001Z");
        // 2026-09-29T12:34:56Z, a leap-year boundary, and a pre-epoch instant.
        assert_eq!(rfc3339_nano_utc(1_790_685_296, 0), "2026-09-29T12:34:56Z");
        assert_eq!(rfc3339_nano_utc(951_782_400, 0), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339_nano_utc(-1, 0), "1969-12-31T23:59:59Z");
    }

    #[test]
    fn now_is_well_formed() {
        let now = now_rfc3339_nano();
        assert_eq!(&now[4..5], "-");
        assert_eq!(&now[10..11], "T");
        assert!(now.ends_with('Z'));
    }
}
