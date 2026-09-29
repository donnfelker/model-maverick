//! `parity/blackbox/replay-allowlist.json`: the reviewed differences, each tied to a
//! DIVERGENCES row.
//!
//! - A **rule** matches exchanges by method, path, and recorded status, and turns an
//!   approved difference into an exact comparison: `strip` removes the Rust-only members
//!   (additive rows such as D8) from our body, `recorded_replace` rewrites the recorded
//!   body where a row changes a value (such as D16), and `mask` rewrites both where a row
//!   leaves text to a library (D36). With `nested_text`, `strip` also applies inside an MCP
//!   tool result's `text` string. The rest must still match byte for byte.
//! - An **entry** names one recorded exchange (test and seq) whose listed parts differ
//!   as a whole, for example a status the Rust engine changed.

use std::path::PathBuf;

use maverick_domain::gojson;
use regex::Regex;
use serde::Deserialize;

use super::Diff;

#[derive(Debug, Deserialize)]
pub struct Match {
    pub method: String,
    /// The path without its query.
    pub path: String,
    /// The recorded status.
    pub status: u16,
}

#[derive(Debug, Deserialize)]
pub struct Rule {
    pub divergences: Vec<String>,
    pub reason: String,
    #[serde(rename = "match")]
    pub matches: Vec<Match>,
    /// A regex the recorded body must match for the rule to apply.
    #[serde(default)]
    pub when: Option<String>,
    #[serde(default)]
    pub strip: Vec<String>,
    #[serde(default)]
    pub nested_text: bool,
    #[serde(default)]
    pub recorded_replace: Vec<(String, String)>,
    #[serde(default)]
    pub mask: Vec<(String, String)>,
}

#[derive(Debug, Deserialize)]
pub struct Entry {
    pub test: String,
    pub seq: u64,
    /// `status`, `body`, or `header <name>`.
    pub parts: Vec<String>,
    pub divergences: Vec<String>,
    pub reason: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct Allowlist {
    #[serde(default)]
    pub rules: Vec<Rule>,
    #[serde(default)]
    pub entries: Vec<Entry>,
}

fn skip_string(b: &[u8], mut i: usize) -> usize {
    i += 1;
    while b[i] != b'"' {
        i += if b[i] == b'\\' { 2 } else { 1 };
    }
    i + 1
}

fn skip_value(b: &[u8], i: usize) -> usize {
    let mut out = Vec::new();
    copy_value(b, i, &mut out, &[], false)
}

/// Copies the JSON value at `b[i..]` into `out` without the object members named in
/// `names` (at any depth), and returns the index after the value. Compact JSON only.
fn copy_value(
    b: &[u8],
    mut i: usize,
    out: &mut Vec<u8>,
    names: &[String],
    nested_text: bool,
) -> usize {
    match b[i] {
        b'{' => {
            out.push(b'{');
            i += 1;
            let mut first = true;
            loop {
                if b[i] == b'}' {
                    out.push(b'}');
                    return i + 1;
                }
                let key_end = skip_string(b, i);
                let key = &b[i + 1..key_end - 1];
                let value = key_end + 1;
                if names.iter().any(|n| n.as_bytes() == key) {
                    i = skip_value(b, value);
                } else {
                    if !first {
                        out.push(b',');
                    }
                    out.extend_from_slice(&b[i..value]);
                    i = if nested_text && key == b"text" && b[value] == b'"' {
                        copy_nested_text(b, value, out, names)
                    } else {
                        copy_value(b, value, out, names, nested_text)
                    };
                    first = false;
                }
                if b[i] == b',' {
                    i += 1;
                }
            }
        }
        b'[' => {
            out.push(b'[');
            i += 1;
            let mut first = true;
            loop {
                if b[i] == b']' {
                    out.push(b']');
                    return i + 1;
                }
                if !first {
                    out.push(b',');
                }
                i = copy_value(b, i, out, names, nested_text);
                first = false;
                if b[i] == b',' {
                    i += 1;
                }
            }
        }
        b'"' => {
            let end = skip_string(b, i);
            out.extend_from_slice(&b[i..end]);
            end
        }
        _ => {
            let start = i;
            while i < b.len() && !matches!(b[i], b',' | b'}' | b']' | b'\n') {
                i += 1;
            }
            out.extend_from_slice(&b[start..i]);
            i
        }
    }
}

/// A string value holding JSON (an MCP tool result's text): decoded, stripped, and
/// re-escaped as the server escapes it.
fn copy_nested_text(b: &[u8], i: usize, out: &mut Vec<u8>, names: &[String]) -> usize {
    let end = skip_string(b, i);
    let literal = std::str::from_utf8(&b[i..end]).expect("utf-8");
    let decoded: String = serde_json::from_str(literal).expect("string literal");
    if decoded.starts_with('{') {
        let stripped = strip(&decoded, names, true);
        let mut encoded = String::new();
        gojson::write_string(&mut encoded, &stripped);
        out.extend_from_slice(encoded.as_bytes());
    } else {
        out.extend_from_slice(&b[i..end]);
    }
    end
}

/// `text` without the named members. Text around the JSON (an SSE frame's `event:` and
/// `data:` lines, a trailing newline) is kept.
pub fn strip(text: &str, names: &[String], nested_text: bool) -> String {
    let b = text.as_bytes();
    let Some(start) = text.find(['{', '[']) else {
        return text.to_owned();
    };
    let mut out = b[..start].to_vec();
    let end = copy_value(b, start, &mut out, names, nested_text);
    out.extend_from_slice(&b[end..]);
    String::from_utf8(out).expect("utf-8")
}

/// A rule path: `{id}` matches any one segment.
fn path_matches(pattern: &str, path: &str) -> bool {
    let pattern: Vec<&str> = pattern.split('/').collect();
    let path: Vec<&str> = path.split('/').collect();
    pattern.len() == path.len()
        && pattern
            .iter()
            .zip(&path)
            .all(|(p, s)| *p == "{id}" || p == s)
}

fn replace_all(text: &str, pairs: &[(String, String)]) -> String {
    pairs
        .iter()
        .fold(text.to_owned(), |text, (pattern, replacement)| {
            Regex::new(pattern)
                .unwrap_or_else(|e| panic!("allowlist regex {pattern}: {e}"))
                .replace_all(&text, replacement.as_str())
                .into_owned()
        })
}

impl Allowlist {
    pub fn load() -> Self {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../parity/blackbox/replay-allowlist.json");
        let text = std::fs::read_to_string(path).expect("replay-allowlist.json");
        serde_json::from_str(&text).expect("replay-allowlist.json parses")
    }

    /// The DIVERGENCES rows that explain `diff`, if a rule or an entry covers it.
    /// `recorded_status` is the exchange's recorded status.
    pub fn explain(&self, diff: &Diff, recorded_status: u16) -> Option<String> {
        if let Some(entry) = self
            .entries
            .iter()
            .find(|e| e.test == diff.test && e.seq == diff.seq && e.parts.contains(&diff.part))
        {
            return Some(format!(
                "{}: {}",
                entry.divergences.join(", "),
                entry.reason
            ));
        }
        if diff.part != "body" {
            return None;
        }
        let path = diff.path.split('?').next().unwrap_or_default();
        let rules: Vec<&Rule> = self
            .rules
            .iter()
            .filter(|rule| {
                rule.matches.iter().any(|m| {
                    m.method == diff.method
                        && path_matches(&m.path, path)
                        && m.status == recorded_status
                }) && rule.when.as_ref().is_none_or(|when| {
                    Regex::new(when)
                        .unwrap_or_else(|e| panic!("allowlist regex {when}: {e}"))
                        .is_match(&diff.expected)
                })
            })
            .collect();
        if rules.is_empty() {
            return None;
        }
        let mut expected = diff.expected.clone();
        let mut actual = diff.actual.clone();
        for rule in &rules {
            expected = replace_all(&expected, &rule.recorded_replace);
            if !rule.strip.is_empty() {
                actual = strip(&actual, &rule.strip, rule.nested_text);
            }
            expected = replace_all(&expected, &rule.mask);
            actual = replace_all(&actual, &rule.mask);
        }
        (expected == actual).then(|| {
            let mut rows: Vec<&str> = rules
                .iter()
                .flat_map(|rule| rule.divergences.iter().map(String::as_str))
                .collect();
            rows.dedup();
            let reasons: Vec<&str> = rules.iter().map(|rule| rule.reason.as_str()).collect();
            format!("{}: {}", rows.join(", "), reasons.join(" "))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn strip_removes_members_at_any_depth_and_keeps_the_rest_byte_exact() {
        let text = "{\"a\":1,\"x\":{\"b\":[1,{\"x\":2,\"c\":\"}\"}]},\"d\":0.800000011920929,\"x\":null}\n";
        assert_eq!(
            strip(text, &names(&["x"]), false),
            "{\"a\":1,\"d\":0.800000011920929}\n"
        );
        assert_eq!(strip("{\"x\":1}", &names(&["x"]), false), "{}");
        assert_eq!(
            strip("[{\"x\":1,\"y\":\"a,b\"}]", &names(&["x"]), false),
            "[{\"y\":\"a,b\"}]"
        );
    }

    #[test]
    fn nested_text_is_stripped_inside_the_string() {
        let text = "event: message\ndata: {\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"{\\\"a\\\":1,\\\"x\\\":2}\"}],\"structuredContent\":{\"a\":1,\"x\":2}}}\n\n";
        assert_eq!(
            strip(text, &names(&["x"]), true),
            "event: message\ndata: {\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"{\\\"a\\\":1}\"}],\"structuredContent\":{\"a\":1}}}\n\n"
        );
    }
}
