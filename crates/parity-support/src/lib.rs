//! Parity fixture helpers for Model Maverick tests.
//!
//! The helpers in this crate load files from `parity/` without normalizing bytes or
//! JSON values. Tests in the engine crates use them to keep fixture lookup, float
//! comparison, and approved divergence markers consistent.

use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;
use thiserror::Error;

static REPO_ROOT: OnceLock<PathBuf> = OnceLock::new();

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("could not locate repository root from {start}")]
    RepoRoot { start: String },

    #[error("path escapes parity directory: {path}")]
    EscapingPath { path: String },

    #[error("unsupported parity anchor: {anchor}")]
    UnsupportedAnchor { anchor: String },

    #[error("anchor segment {segment:?} not found in {anchor}")]
    AnchorNotFound { anchor: String, segment: String },

    #[error("io error reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("json error reading {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("unknown divergence id {id}")]
    UnknownDivergence { id: String },
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Divergence {
    id: String,
}

impl Divergence {
    pub fn id(&self) -> &str {
        &self.id
    }
}

#[macro_export]
macro_rules! divergence {
    ($id:literal) => {{ $crate::divergence($id).expect(concat!("unknown parity divergence marker: ", $id)) }};
}

pub fn repo_root() -> Result<&'static Path> {
    if let Some(root) = REPO_ROOT.get() {
        return Ok(root.as_path());
    }

    let start = std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .ok_or_else(|| Error::RepoRoot {
            start: "<unknown>".to_string(),
        })?;

    for candidate in start.ancestors() {
        if candidate.join("Cargo.toml").is_file() && candidate.join("parity/README.md").is_file() {
            let root = candidate.to_path_buf();
            let _ = REPO_ROOT.set(root);
            return Ok(REPO_ROOT.get().expect("repo root initialized").as_path());
        }
    }

    Err(Error::RepoRoot {
        start: start.display().to_string(),
    })
}

pub fn parity_path(relative: impl AsRef<Path>) -> Result<PathBuf> {
    let relative = clean_relative(relative.as_ref())?;
    Ok(repo_root()?.join("parity").join(relative))
}

pub fn fixture_path(area: &str, file: impl AsRef<Path>) -> Result<PathBuf> {
    let area = clean_relative(Path::new(area))?;
    let file = clean_relative(file.as_ref())?;
    Ok(parity_path("fixtures")?.join(area).join(file))
}

pub fn fixture(area: &str, file: impl AsRef<Path>) -> Result<Value> {
    let path = fixture_path(area, file)?;
    read_json(path)
}

pub fn fixture_typed<T>(area: &str, file: impl AsRef<Path>) -> Result<T>
where
    T: DeserializeOwned,
{
    let path = fixture_path(area, file)?;
    read_json_typed(path)
}

pub fn fixture_bytes(area: &str, file: impl AsRef<Path>) -> Result<Vec<u8>> {
    let path = fixture_path(area, file)?;
    read_bytes(path)
}

pub fn api_testdata(relative: impl AsRef<Path>) -> Result<Vec<u8>> {
    fixture_bytes(
        "api",
        Path::new("testdata").join(clean_relative(relative.as_ref())?),
    )
}

pub fn measurement_io_testdata(relative: impl AsRef<Path>) -> Result<Vec<u8>> {
    fixture_bytes(
        "measurement-io",
        Path::new("testdata").join(clean_relative(relative.as_ref())?),
    )
}

pub fn case(anchor: &str) -> Result<Value> {
    let mut values = cases(anchor)?;
    if values.len() == 1 {
        Ok(values.remove(0))
    } else {
        Ok(Value::Array(values))
    }
}

pub fn case_typed<T>(anchor: &str) -> Result<T>
where
    T: DeserializeOwned,
{
    serde_json::from_value(case(anchor)?).map_err(|source| Error::Json {
        path: PathBuf::from(anchor),
        source,
    })
}

pub fn cases(anchor: &str) -> Result<Vec<Value>> {
    let mut values = Vec::new();

    for part in anchor.split('|') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let value = fixture_case(part)?;
        match value {
            Value::Array(items) => values.extend(items),
            other => values.push(other),
        }
    }

    if values.is_empty() {
        return Err(Error::UnsupportedAnchor {
            anchor: anchor.to_string(),
        });
    }

    Ok(values)
}

pub fn as_f32_widened(value: f64) -> f64 {
    f64::from(value as f32)
}

pub fn f64_exact_eq(left: f64, right: f64) -> bool {
    left.to_bits() == right.to_bits()
}

pub fn assert_f64_exact(actual: f64, expected: f64) {
    assert!(
        f64_exact_eq(actual, expected),
        "f64 mismatch: expected {expected:.17e} ({:#018x}), got {actual:.17e} ({:#018x})",
        expected.to_bits(),
        actual.to_bits()
    );
}

pub fn assert_f64_within(actual: f64, expected: f64, tolerance: f64) {
    let delta = (actual - expected).abs();
    assert!(
        delta <= tolerance,
        "f64 mismatch: expected {expected:.17e}, got {actual:.17e}, delta {delta:.17e}, tolerance {tolerance:.17e}"
    );
}

pub fn divergence(id: &str) -> Result<Divergence> {
    if known_divergences()?.contains(id) {
        Ok(Divergence { id: id.to_string() })
    } else {
        Err(Error::UnknownDivergence { id: id.to_string() })
    }
}

pub fn known_divergences() -> Result<BTreeSet<String>> {
    let path = parity_path("DIVERGENCES.md")?;
    let text = String::from_utf8(read_bytes(&path)?).map_err(|source| Error::Io {
        path: path.clone(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, source),
    })?;
    let mut ids = BTreeSet::new();

    for line in text.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("| D") {
            continue;
        }
        let mut columns = trimmed.split('|').map(str::trim);
        let _empty = columns.next();
        if let Some(id) = columns.next() {
            if is_divergence_id(id) {
                ids.insert(id.to_string());
            }
        }
    }

    Ok(ids)
}

fn fixture_case(anchor: &str) -> Result<Value> {
    let (file, fragment) = anchor
        .split_once('#')
        .ok_or_else(|| Error::UnsupportedAnchor {
            anchor: anchor.to_string(),
        })?;

    if !file.starts_with("fixtures/") || !file.ends_with(".json") {
        return Err(Error::UnsupportedAnchor {
            anchor: anchor.to_string(),
        });
    }

    let path = parity_path(file)?;
    let value = read_json(path)?;
    select_fragment(value, fragment, anchor)
}

fn select_fragment(mut value: Value, fragment: &str, anchor: &str) -> Result<Value> {
    for segment in split_fragment(fragment) {
        value = select_segment(&value, segment, anchor)?;
    }
    Ok(value)
}

fn split_fragment(fragment: &str) -> Vec<&str> {
    fragment
        .split(['/', '#'])
        .filter(|segment| !segment.is_empty())
        .collect()
}

fn select_segment(value: &Value, segment: &str, anchor: &str) -> Result<Value> {
    match value {
        Value::Object(map) => {
            if let Some(value) = map.get(segment) {
                return Ok(value.clone());
            }

            for key in ["cases", "tests", "scenarios"] {
                if let Some(Value::Array(items)) = map.get(key) {
                    if let Some(value) = select_from_array(items, segment) {
                        return Ok(value);
                    }
                }
            }

            for value in map.values() {
                if let Value::Array(items) = value {
                    if let Some(value) = select_from_array(items, segment) {
                        return Ok(value);
                    }
                }
            }

            Err(Error::AnchorNotFound {
                anchor: anchor.to_string(),
                segment: segment.to_string(),
            })
        }
        Value::Array(items) => {
            select_from_array(items, segment).ok_or_else(|| Error::AnchorNotFound {
                anchor: anchor.to_string(),
                segment: segment.to_string(),
            })
        }
        _ => Err(Error::AnchorNotFound {
            anchor: anchor.to_string(),
            segment: segment.to_string(),
        }),
    }
}

fn select_from_array(items: &[Value], segment: &str) -> Option<Value> {
    if segment == "*" {
        return Some(Value::Array(items.to_vec()));
    }

    if let Ok(index) = segment.parse::<usize>() {
        return items.get(index).cloned();
    }

    if let Some(range) = parse_suffix_range(segment) {
        let selected: Vec<Value> = range
            .map(|id| find_exact(items, &id))
            .collect::<Option<Vec<_>>>()?;
        return Some(Value::Array(selected));
    }

    if let Some(prefix) = segment.strip_suffix('*') {
        let selected: Vec<Value> = items
            .iter()
            .filter(|item| case_keys(item).iter().any(|key| key.starts_with(prefix)))
            .cloned()
            .collect();
        if selected.is_empty() {
            return None;
        }
        return Some(Value::Array(selected));
    }

    find_exact(items, segment)
}

fn find_exact(items: &[Value], segment: &str) -> Option<Value> {
    items
        .iter()
        .find(|item| case_keys(item).iter().any(|key| key == segment))
        .cloned()
}

fn case_keys(item: &Value) -> Vec<String> {
    let mut keys = Vec::new();
    let Some(map) = item.as_object() else {
        return keys;
    };

    for key in ["id", "name", "go_test", "test", "case"] {
        if let Some(value) = map.get(key).and_then(Value::as_str) {
            keys.push(value.to_string());
        }
    }

    if let Some(values) = map.get("go_tests").and_then(Value::as_array) {
        for value in values {
            if let Some(value) = value.as_str() {
                keys.push(value.to_string());
            }
        }
    }

    keys
}

fn parse_suffix_range(segment: &str) -> Option<impl Iterator<Item = String> + '_> {
    let (prefix, rest) = segment.rsplit_once('-')?;
    let (start, end) = rest.split_once("..")?;
    let start = start.parse::<usize>().ok()?;
    let end = end.parse::<usize>().ok()?;
    if start > end {
        return None;
    }
    Some((start..=end).map(move |index| format!("{prefix}-{index}")))
}

fn clean_relative(path: &Path) -> Result<PathBuf> {
    let mut clean = PathBuf::new();

    for component in path.components() {
        match component {
            Component::Normal(part) => clean.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(Error::EscapingPath {
                    path: path.display().to_string(),
                });
            }
        }
    }

    Ok(clean)
}

fn read_bytes(path: impl AsRef<Path>) -> Result<Vec<u8>> {
    let path = path.as_ref();
    std::fs::read(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn read_json(path: impl AsRef<Path>) -> Result<Value> {
    read_json_typed(path)
}

fn read_json_typed<T>(path: impl AsRef<Path>) -> Result<T>
where
    T: DeserializeOwned,
{
    let path = path.as_ref();
    let bytes = read_bytes(path)?;
    serde_json::from_slice(&bytes).map_err(|source| Error::Json {
        path: path.to_path_buf(),
        source,
    })
}

fn is_divergence_id(id: &str) -> bool {
    let Some(rest) = id.strip_prefix('D') else {
        return false;
    };
    !rest.is_empty() && rest.chars().all(|ch| ch.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_fixture_from_each_area() {
        let fixtures = [
            ("store", "config_load.json"),
            ("measurement-io", "tasks.json"),
            ("measurement-core", "catalog.json"),
            ("decision", "policy_evaluate.json"),
            ("api", "mcp.json"),
            ("proxy", "unit.json"),
        ];

        for (area, file) in fixtures {
            let value = fixture(area, file).expect("fixture loads");
            assert!(value.is_object(), "{area}/{file} should be a JSON object");
        }
    }

    #[test]
    fn selects_named_cases_and_prefixes() {
        let config_case = case("fixtures/store/config_load.json#defaults").expect("config case");
        assert_eq!(config_case["id"], "defaults");

        let cases = case("fixtures/decision/policy_evaluate.json#TestEvaluatePoolResolution*")
            .expect("cases");
        assert!(cases.as_array().expect("array").len() >= 2);

        let cost =
            case("fixtures/measurement-core/catalog.json#cost/one mtok each way").expect("cost");
        assert_eq!(cost["expected"], 18);

        let range = case("fixtures/proxy/unit.json#rewrite_model-1..4").expect("range");
        assert_eq!(range.as_array().expect("range array").len(), 4);
    }

    #[test]
    fn loads_raw_testdata_byte_for_byte() {
        let api = api_testdata("recommend_declared.golden.json").expect("api raw");
        let api_path =
            fixture_path("api", "testdata/recommend_declared.golden.json").expect("api path");
        assert_eq!(api, std::fs::read(api_path).expect("api fs read"));

        let measurement = measurement_io_testdata("internal/runner/testdata/assemble_basic.golden")
            .expect("measurement raw");
        let measurement_path = fixture_path(
            "measurement-io",
            "testdata/internal/runner/testdata/assemble_basic.golden",
        )
        .expect("measurement path");
        assert_eq!(
            measurement,
            std::fs::read(measurement_path).expect("measurement fs read")
        );
    }

    #[test]
    fn checks_float_helpers() {
        let widened = as_f32_widened(0.1);
        assert_f64_exact(widened, f64::from(0.1_f32));
        assert_f64_within(1.0 + 1e-13, 1.0, 1e-12);
    }

    #[test]
    fn validates_divergence_marker() {
        let marker = divergence!("D6");
        assert_eq!(marker.id(), "D6");
        assert!(matches!(
            divergence("D9999"),
            Err(Error::UnknownDivergence { .. })
        ));
    }

    #[test]
    fn rejects_paths_that_escape_parity() {
        assert!(matches!(
            fixture_bytes("api", "../DIVERGENCES.md"),
            Err(Error::EscapingPath { .. })
        ));
    }
}
