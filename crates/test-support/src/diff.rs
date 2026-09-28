//! Readable diff reporters for parity comparisons.
//!
//! - [`json_diff`] / [`format_json_diff`] produce field-path diffs such as
//!   `$.recommendations[1].score: expected 94.1, got 94.2`, with the fixture anchor and any
//!   approved-divergence tag.
//! - [`bytes_diff`] shows a hex-and-text window around the first differing byte (wire bodies,
//!   SSE frames, rewritten bodies).
//! - [`float_diff`] shows both values in full `f64` precision and as `f32` bits, for the
//!   float comparisons the ranking and scoring fixtures need.

use serde_json::Value;

/// One differing leaf between two JSON values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldDiff {
    /// JSONPath-style location, e.g. `$.recommendations[1].score`.
    pub path: String,
    /// The expected value rendered as text (`(missing)` when absent).
    pub expected: String,
    /// The actual value rendered as text (`(missing)` when absent).
    pub actual: String,
}

/// Compare two JSON values and return every differing leaf, in a stable order.
#[must_use]
pub fn json_diff(expected: &Value, actual: &Value) -> Vec<FieldDiff> {
    let mut out = Vec::new();
    diff_value("$", expected, actual, &mut out);
    out
}

fn diff_value(path: &str, expected: &Value, actual: &Value, out: &mut Vec<FieldDiff>) {
    match (expected, actual) {
        (Value::Object(expected_map), Value::Object(actual_map)) => {
            let mut keys: Vec<&String> = expected_map.keys().chain(actual_map.keys()).collect();
            keys.sort_unstable();
            keys.dedup();
            for key in keys {
                let child = format!("{path}.{key}");
                diff_child(&child, expected_map.get(key), actual_map.get(key), out);
            }
        }
        (Value::Array(expected_items), Value::Array(actual_items)) => {
            let len = expected_items.len().max(actual_items.len());
            for index in 0..len {
                let child = format!("{path}[{index}]");
                diff_child(
                    &child,
                    expected_items.get(index),
                    actual_items.get(index),
                    out,
                );
            }
        }
        _ => {
            if expected != actual {
                out.push(FieldDiff {
                    path: path.to_string(),
                    expected: render(expected),
                    actual: render(actual),
                });
            }
        }
    }
}

fn diff_child(
    path: &str,
    expected: Option<&Value>,
    actual: Option<&Value>,
    out: &mut Vec<FieldDiff>,
) {
    match (expected, actual) {
        (Some(expected), Some(actual)) => diff_value(path, expected, actual, out),
        (Some(expected), None) => out.push(FieldDiff {
            path: path.to_string(),
            expected: render(expected),
            actual: "(missing)".to_string(),
        }),
        (None, Some(actual)) => out.push(FieldDiff {
            path: path.to_string(),
            expected: "(missing)".to_string(),
            actual: render(actual),
        }),
        (None, None) => {}
    }
}

fn render(value: &Value) -> String {
    match value {
        Value::String(text) => format!("{text:?}"),
        other => other.to_string(),
    }
}

/// Format a readable JSON diff, or `None` when the values are equal.
#[must_use]
pub fn format_json_diff(expected: &Value, actual: &Value, anchor: Option<&str>) -> Option<String> {
    format_json_diff_tagged(expected, actual, anchor, None)
}

/// Like [`format_json_diff`], and also names an approved divergence (`D12`) in the header
/// when one explains the difference.
#[must_use]
pub fn format_json_diff_tagged(
    expected: &Value,
    actual: &Value,
    anchor: Option<&str>,
    divergence: Option<&str>,
) -> Option<String> {
    let diffs = json_diff(expected, actual);
    if diffs.is_empty() {
        return None;
    }

    let mut header = String::from("parity mismatch");
    if let Some(anchor) = anchor {
        header.push_str(&format!(" at {anchor}"));
    }
    if let Some(divergence) = divergence {
        header.push_str(&format!(" [{divergence}]"));
    }
    header.push(':');

    let mut lines = vec![header];
    for diff in &diffs {
        lines.push(format!(
            "  {}: expected {}, got {}",
            diff.path, diff.expected, diff.actual
        ));
    }
    Some(lines.join("\n"))
}

/// Assert two JSON values are equal, panicking with a readable field-path diff otherwise.
pub fn assert_json_eq(expected: &Value, actual: &Value, anchor: &str) {
    if let Some(report) = format_json_diff(expected, actual, Some(anchor)) {
        panic!("{report}");
    }
}

/// Diff two byte strings, returning a hex-and-text window around the first mismatch, or
/// `None` when they are equal.
#[must_use]
pub fn bytes_diff(expected: &[u8], actual: &[u8]) -> Option<String> {
    if expected == actual {
        return None;
    }

    let first = expected
        .iter()
        .zip(actual.iter())
        .position(|(left, right)| left != right)
        .unwrap_or_else(|| expected.len().min(actual.len()));

    let window = 16usize;
    let start = first.saturating_sub(window / 2);

    let lines = [
        format!(
            "byte mismatch at offset {first} (expected {} bytes, got {} bytes):",
            expected.len(),
            actual.len()
        ),
        format!("  expected: {}", hex_and_text(expected, start, window)),
        format!("  actual:   {}", hex_and_text(actual, start, window)),
        format!("            {}", caret(first, start, window)),
    ];
    Some(lines.join("\n"))
}

fn hex_and_text(bytes: &[u8], start: usize, window: usize) -> String {
    let end = (start + window).min(bytes.len());
    let mut hex = String::new();
    let mut text = String::new();
    for &byte in &bytes[start.min(bytes.len())..end] {
        hex.push_str(&format!("{byte:02x} "));
        if byte.is_ascii_graphic() || byte == b' ' {
            text.push(byte as char);
        } else {
            text.push('.');
        }
    }
    format!("{hex}|{text}|")
}

fn caret(first: usize, start: usize, window: usize) -> String {
    if first < start || first >= start + window {
        return String::new();
    }
    let column = 3 * (first - start);
    format!("{}^^ first difference", " ".repeat(column))
}

/// Assert two byte strings are equal, panicking with a hex-and-text diff otherwise.
pub fn assert_bytes_eq(expected: &[u8], actual: &[u8]) {
    if let Some(report) = bytes_diff(expected, actual) {
        panic!("{report}");
    }
}

/// Diff two `f64` values by bit pattern, showing both in full precision and as `f32` bits,
/// or `None` when they are bit-identical.
#[must_use]
pub fn float_diff(expected: f64, actual: f64) -> Option<String> {
    if expected.to_bits() == actual.to_bits() {
        return None;
    }
    let expected_f32 = expected as f32;
    let actual_f32 = actual as f32;
    Some(format!(
        "float mismatch:\n  expected: {expected:.17e} (f64 bits {:#018x}, f32 {expected_f32:.9e} bits {:#010x})\n  actual:   {actual:.17e} (f64 bits {:#018x}, f32 {actual_f32:.9e} bits {:#010x})",
        expected.to_bits(),
        expected_f32.to_bits(),
        actual.to_bits(),
        actual_f32.to_bits(),
    ))
}

/// Assert two `f64` values are bit-identical, panicking with a full-precision diff otherwise.
pub fn assert_float_eq(expected: f64, actual: f64) {
    if let Some(report) = float_diff(expected, actual) {
        panic!("{report}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_diff_reports_field_path_and_anchor() {
        let expected = json!({ "recommendations": [{ "score": 94.1 }] });
        let actual = json!({ "recommendations": [{ "score": 94.2 }] });
        let report = format_json_diff(
            &expected,
            &actual,
            Some("fixtures/decision/rank.json#case-7"),
        )
        .expect("a mismatch is reported");
        assert!(
            report.contains("$.recommendations[0].score: expected 94.1, got 94.2"),
            "{report}"
        );
        assert!(report.contains("rank.json#case-7"), "{report}");
    }

    #[test]
    fn json_diff_tags_divergence_and_reports_missing_fields() {
        let expected = json!({ "a": 1, "b": 2 });
        let actual = json!({ "a": 1 });
        let report = format_json_diff_tagged(&expected, &actual, None, Some("D12"))
            .expect("a mismatch is reported");
        assert!(report.contains("[D12]"), "{report}");
        assert!(
            report.contains("$.b: expected 2, got (missing)"),
            "{report}"
        );
    }

    #[test]
    fn json_diff_is_empty_when_equal() {
        assert!(format_json_diff(&json!({ "a": 1 }), &json!({ "a": 1 }), None).is_none());
    }

    #[test]
    #[should_panic(expected = "expected 1, got 2")]
    fn assert_json_eq_panics_with_diff() {
        assert_json_eq(&json!({ "a": 1 }), &json!({ "a": 2 }), "anchor");
    }

    #[test]
    fn bytes_diff_shows_offset_and_window() {
        let report = bytes_diff(b"hello world", b"hello xorld").expect("a mismatch is reported");
        assert!(report.contains("offset 6"), "{report}");
        assert!(report.contains("first difference"), "{report}");
        assert!(bytes_diff(b"same", b"same").is_none());
    }

    #[test]
    fn float_diff_shows_precision_and_bits() {
        let report = float_diff(94.1, 94.2).expect("a mismatch is reported");
        assert!(report.contains("f64 bits"), "{report}");
        assert!(report.contains("f32"), "{report}");
        assert!(float_diff(1.0, 1.0).is_none());
    }

    #[test]
    #[should_panic(expected = "float mismatch")]
    fn assert_float_eq_panics_with_diff() {
        assert_float_eq(1.0, 1.0 + f64::EPSILON);
    }
}
