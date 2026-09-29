//! Every named Go query has its Rust store function and a store test (bead 3.14).
//!
//! The Go queries are `parity/schema/queries/*.sql` (`-- name: <Name> :<kind>`). A store
//! function names the query it replaces in the first line of its doc comment, as
//! `` /// `<Name>` ``. This test reads both and checks that each query maps to exactly one
//! function, and that a test under `crates/maverick-store/tests/` calls that function.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn files(dir: &Path, extension: &str) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("{}: {error}", dir.display()))
        .map(|entry| entry.expect("dir entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == extension))
        .collect();
    files.sort();
    files
}

/// The query names in `-- name: <Name> :<kind>` lines.
fn go_queries() -> Vec<String> {
    let dir = crate_dir().join("../../parity/schema/queries");
    files(&dir, "sql")
        .iter()
        .flat_map(|path| {
            fs::read_to_string(path)
                .expect("query file")
                .lines()
                .filter_map(|line| {
                    let rest = line.trim().strip_prefix("--")?.trim();
                    let rest = rest.strip_prefix("name:")?.trim();
                    rest.split_whitespace().next().map(str::to_owned)
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

/// `pub fn` name after `pub fn ` or `pub async fn `, if the line declares one.
fn declared_fn(line: &str) -> Option<&str> {
    let rest = line
        .strip_prefix("pub async fn ")
        .or_else(|| line.strip_prefix("pub fn "))?;
    let end = rest
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    Some(&rest[..end])
}

/// Go query name -> the store functions whose doc comment starts with it.
fn store_functions() -> BTreeMap<String, Vec<String>> {
    let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for path in files(&crate_dir().join("src"), "rs") {
        let text = fs::read_to_string(&path).expect("source");
        let lines: Vec<&str> = text.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            let Some(function) = declared_fn(line) else {
                continue;
            };
            let mut first = index;
            while first > 0 && {
                let above = lines[first - 1];
                above.starts_with("///") || above.starts_with("#[")
            } {
                first -= 1;
            }
            let Some(doc) = lines.get(first).filter(|_| first < index) else {
                continue;
            };
            let Some(name) = doc
                .strip_prefix("/// `")
                .and_then(|rest| rest.split('`').next())
            else {
                continue;
            };
            map.entry(name.to_owned())
                .or_default()
                .push(function.to_owned());
        }
    }
    map
}

fn calls(text: &str, function: &str) -> bool {
    text.match_indices(function).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + function.len()..].trim_start().chars().next();
        !before.is_some_and(|c| c.is_alphanumeric() || c == '_') && after == Some('(')
    })
}

#[test]
fn every_go_query_has_one_store_function_and_a_store_test() {
    let queries = go_queries();
    assert!(queries.len() >= 90, "{} queries found", queries.len());
    let functions = store_functions();
    let tests: String = files(&crate_dir().join("tests"), "rs")
        .iter()
        .map(|path| fs::read_to_string(path).expect("test source"))
        .collect();

    let mut problems = Vec::new();
    for query in &queries {
        match functions.get(query).map(Vec::as_slice) {
            None | Some([]) => problems.push(format!("{query}: no store function")),
            Some([function]) => {
                if !calls(&tests, function) {
                    problems.push(format!("{query}: no store test calls `{function}`"));
                }
            }
            Some(many) => problems.push(format!("{query}: several functions {many:?}")),
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn the_scanner_reads_declarations_and_calls() {
    assert_eq!(
        declared_fn("pub async fn get_tenant<'e>("),
        Some("get_tenant")
    );
    assert_eq!(declared_fn("pub fn is_not_found(err"), Some("is_not_found"));
    assert_eq!(declared_fn("fn private()"), None);
    assert!(calls("x = store::get_tenant(&pool)", "get_tenant"));
    assert!(calls("get_tenant (\n", "get_tenant"));
    assert!(!calls("try_get_tenant(&pool)", "get_tenant"));
    assert!(!calls("get_tenant_constraints(&pool)", "get_tenant"));
}
