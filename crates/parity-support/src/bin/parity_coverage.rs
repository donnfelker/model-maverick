//! parity-coverage: the manifest coverage checker (bead 3hk.1.5).
//!
//! It reads `parity/manifest/*.json` and, for each Go test, reports whether a Rust test
//! claims it with a `parity:` tag or an approved divergence row explains why not. It fails
//! nothing by default (report mode, for incomplete packages); `--strict` exits non-zero when
//! any Go test is still missing or a tag names a Go test that is in no manifest (a typo).
//!
//! A Rust test claims a Go test with a comment tag naming the exact `go_test` string, either
//! area-qualified or bare:
//!
//! ```text
//! // parity: decision/policy.TestEvaluatePoolRestriction
//! // parity: policy.TestEvaluatePoolRestriction
//! ```
//!
//! A Go test is divergence-justified when a `parity/DIVERGENCES.md` row (`| D<n> | ... |`)
//! names it (for example D37 names `TestMigrationsUpDownUpClean`).
//!
//! It also audits the tags and the divergence rows (bead 9.1). `--strict` fails when:
//! - a `parity:` tag sits outside test code (a `tests/` directory, or after `#[cfg(test)]`
//!   in a source file), so it cannot claim a test;
//! - a test (a `parity_support::divergence!("D<n>")` marker, or a one- or two-digit `D<n>`
//!   mention in a test comment) or `parity/blackbox/replay-allowlist.json` cites a row that
//!   does not exist;
//! - a DIVERGENCES row is named by no test: every approved difference is asserted
//!   somewhere.
//!
//! `--list-missing` prints each uncovered Go test; `--list-rows` prints each DIVERGENCES row
//! with its test citations.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use parity_support::{parity_path, repo_root};
use serde_json::Value;

/// One Go test from a manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GoTest {
    area: String,
    /// The full `go_test` string, e.g. `policy.TestValidateWeights`.
    name: String,
    /// The manifest `where` pointer: a fixture path (`fixtures/...`), a reference-doc anchor
    /// (`reference/...`), or a blackbox recording. It tells a closeout whether a missing row
    /// is a pure fixture to own now or a reference or blackbox row a later package owns.
    where_: String,
}

impl GoTest {
    /// The bare test name after the last `.`, e.g. `TestValidateWeights`.
    fn test_name(&self) -> &str {
        self.name.rsplit('.').next().unwrap_or(&self.name)
    }
}

/// How one Go test is (or is not) covered.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Coverage {
    Tagged,
    Divergence(String),
    Missing,
}

/// Per-area counts for the report.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct AreaCounts {
    total: usize,
    tagged: usize,
    divergence: usize,
    missing: usize,
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let strict = args.iter().any(|arg| arg == "--strict");
    let list_missing = args.iter().any(|arg| arg == "--list-missing");
    let list_rows = args.iter().any(|arg| arg == "--list-rows");

    let root = match repo_root() {
        Ok(root) => root.to_path_buf(),
        Err(error) => {
            eprintln!("parity-coverage: {error}");
            return ExitCode::FAILURE;
        }
    };

    match run(&root, strict, list_missing, list_rows) {
        Ok(exit) => exit,
        Err(error) => {
            eprintln!("parity-coverage: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(root: &Path, strict: bool, list_missing: bool, list_rows: bool) -> Result<ExitCode, String> {
    let manifest_dir = parity_path("manifest").map_err(|error| error.to_string())?;
    let go_tests = load_manifest_go_tests(&manifest_dir)?;

    let scan = scan_sources(&root.join("crates"))?;
    let tags = scan.tags;

    let divergences_md = parity_path("DIVERGENCES.md").map_err(|error| error.to_string())?;
    let divergences_text = fs::read_to_string(&divergences_md)
        .map_err(|error| format!("read {}: {error}", divergences_md.display()))?;
    let divergence_refs = divergence_refs(&divergences_text, &go_tests);

    let rows: BTreeSet<String> = divergences_text
        .lines()
        .filter_map(|line| row_divergence_id(line.trim_start()))
        .collect();
    let mut cited = scan.divergence_mentions;
    let allowlist =
        parity_path("blackbox/replay-allowlist.json").map_err(|error| error.to_string())?;
    for id in allowlist_divergences(&allowlist)? {
        cited
            .entry(id)
            .or_default()
            .push(allowlist.display().to_string());
    }
    let row_audit = audit_rows(&rows, &cited);

    let valid_specs = valid_specs(&go_tests);
    let unknown_tags: BTreeMap<String, Vec<String>> = tags
        .iter()
        .filter(|(spec, _)| !valid_specs.contains(spec.as_str()))
        .map(|(spec, locations)| (spec.clone(), locations.clone()))
        .collect();

    let mut per_area: BTreeMap<String, AreaCounts> = BTreeMap::new();
    let mut missing: BTreeMap<String, Vec<GoTest>> = BTreeMap::new();
    for go_test in &go_tests {
        let counts = per_area.entry(go_test.area.clone()).or_default();
        counts.total += 1;
        match coverage_of(go_test, &tags, &divergence_refs) {
            Coverage::Tagged => counts.tagged += 1,
            Coverage::Divergence(_) => counts.divergence += 1,
            Coverage::Missing => {
                counts.missing += 1;
                missing
                    .entry(go_test.area.clone())
                    .or_default()
                    .push(go_test.clone());
            }
        }
    }

    print_report(&per_area, &unknown_tags, strict);
    print_audit(&scan.untested_tags, &row_audit);

    if list_missing {
        print_missing(&missing);
    }
    if list_rows {
        print_rows(&rows, &cited);
    }

    let total_missing: usize = per_area.values().map(|counts| counts.missing).sum();
    let audit_failed = !scan.untested_tags.is_empty()
        || !row_audit.unknown.is_empty()
        || !row_audit.unasserted.is_empty();
    if strict && (total_missing > 0 || !unknown_tags.is_empty() || audit_failed) {
        return Ok(ExitCode::FAILURE);
    }
    Ok(ExitCode::SUCCESS)
}

/// The divergence rows cited by tests and the replay allowlist, checked against
/// `DIVERGENCES.md`.
#[derive(Debug, Default, PartialEq, Eq)]
struct RowAudit {
    /// Cited ids with no row, and where they are cited.
    unknown: BTreeMap<String, Vec<String>>,
    /// Rows no test names.
    unasserted: Vec<String>,
    /// How many citations each row has.
    citations: BTreeMap<String, usize>,
}

fn audit_rows(rows: &BTreeSet<String>, cited: &BTreeMap<String, Vec<String>>) -> RowAudit {
    let mut audit = RowAudit::default();
    for (id, places) in cited {
        if rows.contains(id) {
            audit.citations.insert(id.clone(), places.len());
        } else {
            audit.unknown.insert(id.clone(), places.clone());
        }
    }
    let mut unasserted: Vec<String> = rows
        .iter()
        .filter(|row| !cited.contains_key(*row))
        .cloned()
        .collect();
    unasserted.sort_by_key(|id| id[1..].parse::<u32>().unwrap_or(u32::MAX));
    audit.unasserted = unasserted;
    audit
}

/// Every `divergences` id in the replay allowlist (rules and entries).
fn allowlist_divergences(path: &Path) -> Result<Vec<String>, String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    let value: Value = serde_json::from_str(&text)
        .map_err(|error| format!("parse {}: {error}", path.display()))?;
    let mut ids = Vec::new();
    for key in ["rules", "entries"] {
        for item in value
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            for id in item
                .get("divergences")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(id) = id.as_str() {
                    ids.push(id.to_string());
                }
            }
        }
    }
    Ok(ids)
}

/// Decide how a Go test is covered. A `parity:` tag wins over a divergence row.
fn coverage_of(
    go_test: &GoTest,
    tags: &BTreeMap<String, Vec<String>>,
    divergence_refs: &BTreeMap<String, String>,
) -> Coverage {
    let qualified = format!("{}/{}", go_test.area, go_test.name);
    if tags.contains_key(&qualified) || tags.contains_key(&go_test.name) {
        Coverage::Tagged
    } else if let Some(id) = divergence_refs.get(&go_test.name) {
        Coverage::Divergence(id.clone())
    } else {
        Coverage::Missing
    }
}

/// The set of tag specs that name a real Go test: both the area-qualified and bare forms.
fn valid_specs(go_tests: &[GoTest]) -> BTreeSet<String> {
    let mut specs = BTreeSet::new();
    for go_test in go_tests {
        specs.insert(format!("{}/{}", go_test.area, go_test.name));
        specs.insert(go_test.name.clone());
    }
    specs
}

/// Load every `{ "go_test": ... }` entry from `parity/manifest/*.json`.
fn load_manifest_go_tests(manifest_dir: &Path) -> Result<Vec<GoTest>, String> {
    let mut files: Vec<PathBuf> = fs::read_dir(manifest_dir)
        .map_err(|error| format!("read {}: {error}", manifest_dir.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect();
    files.sort();

    let mut go_tests = Vec::new();
    for file in files {
        let text = fs::read_to_string(&file)
            .map_err(|error| format!("read {}: {error}", file.display()))?;
        let value: Value = serde_json::from_str(&text)
            .map_err(|error| format!("parse {}: {error}", file.display()))?;
        let area = value
            .get("area")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("{}: missing string \"area\"", file.display()))?
            .to_string();
        let tests = value
            .get("tests")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("{}: missing array \"tests\"", file.display()))?;
        for test in tests {
            if let Some(name) = test.get("go_test").and_then(Value::as_str) {
                let where_ = test
                    .get("where")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                go_tests.push(GoTest {
                    area: area.clone(),
                    name: name.to_string(),
                    where_,
                });
            }
        }
    }
    Ok(go_tests)
}

/// What the source scan found.
#[derive(Debug, Default)]
struct Scan {
    /// Every `parity:` tag spec in test code, with its locations.
    tags: BTreeMap<String, Vec<String>>,
    /// Tags outside test code (they claim nothing).
    untested_tags: Vec<String>,
    /// Every divergence id a test cites: a `divergence!("D<n>")` marker or a `D<n>` in a
    /// test comment, with its locations.
    divergence_mentions: BTreeMap<String, Vec<String>>,
}

/// True for a file under a `tests/` or `benches/` directory of a crate.
fn is_test_file(path: &Path) -> bool {
    path.components()
        .any(|part| matches!(part.as_os_str().to_str(), Some("tests" | "benches")))
}

/// Walk `crates/` for `.rs` files: the `parity:` tags and the divergence citations in test
/// code. A source file's test code starts at its first `#[cfg(test)]`.
fn scan_sources(crates_dir: &Path) -> Result<Scan, String> {
    let mut sources = Vec::new();
    collect_rs_files(crates_dir, &mut sources)?;
    sources.sort();

    let mut scan = Scan::default();
    for source in sources {
        // This checker's own examples are not evidence for any row.
        if source.ends_with("parity-support/src/bin/parity_coverage.rs") {
            continue;
        }
        let text = fs::read_to_string(&source)
            .map_err(|error| format!("read {}: {error}", source.display()))?;
        let mut in_test = is_test_file(&source);
        for (index, line) in text.lines().enumerate() {
            if line.trim_start().starts_with("#[cfg(test)]") {
                in_test = true;
            }
            let place = format!("{}:{}", source.display(), index + 1);
            if let Some(spec) = tag_spec(line) {
                if in_test {
                    scan.tags
                        .entry(spec.to_string())
                        .or_default()
                        .push(place.clone());
                } else {
                    scan.untested_tags.push(format!("{spec}  ({place})"));
                }
            }
            if in_test {
                for id in divergence_citations(line) {
                    scan.divergence_mentions
                        .entry(id)
                        .or_default()
                        .push(place.clone());
                }
            }
        }
    }
    Ok(scan)
}

/// The divergence ids a test line cites: a `divergence!("D<n>")` marker anywhere on the
/// line, or a `D<n>` token in its comment.
fn divergence_citations(line: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let mut rest = line;
    while let Some(at) = rest.find("divergence!(\"") {
        let after = &rest[at + "divergence!(\"".len()..];
        if let Some(id) = divergence_token(after) {
            ids.push(id);
        }
        rest = after;
    }
    // In prose only one- and two-digit ids count, so hex such as a UTF-16 unit `D800` is not
    // read as a citation. Markers and the allowlist are checked at any length.
    if let Some(at) = line.find("//") {
        let comment = &line[at..];
        let bytes = comment.as_bytes();
        for (index, _) in comment.match_indices('D') {
            let boundary = index == 0 || !bytes[index - 1].is_ascii_alphanumeric();
            if boundary
                && let Some(id) = divergence_token(&comment[index..])
                && id.len() <= 3
            {
                ids.push(id);
            }
        }
    }
    ids.dedup();
    ids
}

/// `D<digits>` at the start of `text`, ending at a non-alphanumeric character.
fn divergence_token(text: &str) -> Option<String> {
    let digits: String = text
        .strip_prefix('D')?
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    let next = text[1 + digits.len()..].chars().next();
    if digits.is_empty() || next.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_') {
        None
    } else {
        Some(format!("D{digits}"))
    }
}

/// Extract a tag spec from a comment whose body starts with `parity:`. Returns the trimmed
/// spec, or `None` when the line is not such a comment. Requiring the comment body to start
/// with `parity:` ignores prose that merely mentions the marker and doc examples that show a
/// nested `// parity:` line (like this file's own header).
fn tag_spec(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    let body = trimmed
        .strip_prefix("///")
        .or_else(|| trimmed.strip_prefix("//!"))
        .or_else(|| trimmed.strip_prefix("//"))
        .or_else(|| trimmed.strip_prefix('*'))?
        .trim_start();
    let after = body.strip_prefix("parity:")?.trim();
    let spec = after.split_whitespace().next()?;
    if spec.is_empty() { None } else { Some(spec) }
}

/// Recursively collect `.rs` files under `dir` (skipping `target/`).
fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("read {}: {error}", dir.display())),
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == "target") {
                continue;
            }
            collect_rs_files(&path, out)?;
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

/// Map each Go test to the divergence row (`D<n>`) that names it, if any.
fn divergence_refs(divergences_md: &str, go_tests: &[GoTest]) -> BTreeMap<String, String> {
    let mut names: BTreeMap<&str, &GoTest> = BTreeMap::new();
    for go_test in go_tests {
        names.insert(go_test.test_name(), go_test);
    }

    let mut refs = BTreeMap::new();
    for line in divergences_md.lines() {
        let trimmed = line.trim_start();
        let Some(id) = row_divergence_id(trimmed) else {
            continue;
        };
        for (test_name, go_test) in &names {
            if line.contains(test_name) {
                refs.insert(go_test.name.clone(), id.clone());
            }
        }
    }
    refs
}

/// If a line is a DIVERGENCES table row (`| D<n> | ... |`), return its `D<n>` id.
fn row_divergence_id(trimmed: &str) -> Option<String> {
    let rest = trimmed.strip_prefix('|')?.trim_start();
    let candidate = rest.strip_prefix('D')?;
    let digits: String = candidate.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        None
    } else {
        Some(format!("D{digits}"))
    }
}

fn print_report(
    per_area: &BTreeMap<String, AreaCounts>,
    unknown_tags: &BTreeMap<String, Vec<String>>,
    strict: bool,
) {
    let mode = if strict { "strict" } else { "report" };
    println!("== parity coverage ({mode} mode) ==");

    let mut total = AreaCounts::default();
    for (area, counts) in per_area {
        println!(
            "  {area:<18} {:>4} total | {:>4} tagged | {:>4} divergence | {:>4} missing",
            counts.total, counts.tagged, counts.divergence, counts.missing
        );
        total.total += counts.total;
        total.tagged += counts.tagged;
        total.divergence += counts.divergence;
        total.missing += counts.missing;
    }
    println!(
        "  {:<18} {:>4} total | {:>4} tagged | {:>4} divergence | {:>4} missing",
        "TOTAL", total.total, total.tagged, total.divergence, total.missing
    );

    if !unknown_tags.is_empty() {
        println!(
            "\n!! {} parity tag(s) name a Go test in no manifest (typo?):",
            unknown_tags.len()
        );
        for (spec, locations) in unknown_tags {
            println!("  {spec}  ({})", locations.join(", "));
        }
    }
}

/// Each DIVERGENCES row with the number of test citations and the first one.
fn print_rows(rows: &BTreeSet<String>, cited: &BTreeMap<String, Vec<String>>) {
    println!("\n== divergence rows (row: citations, first) ==");
    let mut ordered: Vec<&String> = rows.iter().collect();
    ordered.sort_by_key(|id| id[1..].parse::<u32>().unwrap_or(u32::MAX));
    for id in ordered {
        match cited.get(id.as_str()) {
            Some(places) => println!("  {id}: {} ({})", places.len(), places[0]),
            None => println!("  {id}: 0"),
        }
    }
}

fn print_audit(untested_tags: &[String], audit: &RowAudit) {
    println!(
        "\n== divergence rows: {} cited by tests, {} named by no test ==",
        audit.citations.len(),
        audit.unasserted.len()
    );
    if !audit.unasserted.is_empty() {
        println!("!! rows no test names: {}", audit.unasserted.join(", "));
    }
    if !audit.unknown.is_empty() {
        println!("!! cited ids with no DIVERGENCES row:");
        for (id, places) in &audit.unknown {
            println!("  {id}  ({})", places.join(", "));
        }
    }
    if !untested_tags.is_empty() {
        println!(
            "!! {} parity tag(s) outside test code:",
            untested_tags.len()
        );
        for tag in untested_tags {
            println!("  {tag}");
        }
    }
}

/// List every uncovered Go test with its manifest `where` pointer, grouped by area. A closeout
/// reads this to sort missing rows: a `fixtures/...` pointer is a pure-fixture gap to own now,
/// while a `reference/...` or `blackbox/...` pointer is a row a later package owns.
fn print_missing(missing: &BTreeMap<String, Vec<GoTest>>) {
    if missing.is_empty() {
        println!("\n== missing rows: none ==");
        return;
    }
    println!("\n== missing rows (area: go_test -> where) ==");
    for (area, rows) in missing {
        println!("  [{area}] {} missing", rows.len());
        for go_test in rows {
            let where_ = if go_test.where_.is_empty() {
                "(no where)"
            } else {
                go_test.where_.as_str()
            };
            println!("    {} -> {where_}", go_test.name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn go_tests() -> Vec<GoTest> {
        vec![
            GoTest {
                area: "decision".into(),
                name: "policy.TestValidateWeights".into(),
                where_: "fixtures/decision/policy_validate.json#TestValidateWeights/*".into(),
            },
            GoTest {
                area: "store".into(),
                name: "store.TestMigrationsUpDownUpClean".into(),
                where_: "reference/engine-reference.md (Part 4)#migrations".into(),
            },
        ]
    }

    #[test]
    fn tag_spec_parses_qualified_and_bare_and_ignores_non_comments() {
        assert_eq!(
            tag_spec("    /// parity: decision/policy.TestValidateWeights"),
            Some("decision/policy.TestValidateWeights")
        );
        assert_eq!(
            tag_spec("// parity: policy.TestFoo extra words"),
            Some("policy.TestFoo")
        );
        assert_eq!(tag_spec("    let parity: u8 = 1; // not a tag"), None);
        assert_eq!(tag_spec("// parity:"), None);
    }

    #[test]
    fn row_divergence_id_reads_table_rows_only() {
        assert_eq!(
            row_divergence_id("| D37 | Migrations ... |"),
            Some("D37".to_string())
        );
        assert_eq!(row_divergence_id("| Name | Header |"), None);
        assert_eq!(row_divergence_id("plain text D12"), None);
    }

    #[test]
    fn divergence_refs_maps_named_tests() {
        let md = "| D37 | no down migrations | TestMigrationsUpDownUpClean is replaced | owner |";
        let refs = divergence_refs(md, &go_tests());
        assert_eq!(
            refs.get("store.TestMigrationsUpDownUpClean"),
            Some(&"D37".to_string())
        );
        assert!(!refs.contains_key("policy.TestValidateWeights"));
    }

    #[test]
    fn coverage_prefers_tag_then_divergence_then_missing() {
        let tests = go_tests();
        let mut tags: BTreeMap<String, Vec<String>> = BTreeMap::new();
        tags.insert(
            "decision/policy.TestValidateWeights".into(),
            vec!["a.rs:1".into()],
        );
        let mut refs = BTreeMap::new();
        refs.insert(
            "store.TestMigrationsUpDownUpClean".to_string(),
            "D37".to_string(),
        );

        assert_eq!(coverage_of(&tests[0], &tags, &refs), Coverage::Tagged);
        assert_eq!(
            coverage_of(&tests[1], &tags, &refs),
            Coverage::Divergence("D37".into())
        );

        let empty_tags = BTreeMap::new();
        let empty_refs = BTreeMap::new();
        assert_eq!(
            coverage_of(&tests[0], &empty_tags, &empty_refs),
            Coverage::Missing
        );
    }

    #[test]
    fn bare_tag_covers_the_test_in_any_area() {
        let tests = go_tests();
        let mut tags: BTreeMap<String, Vec<String>> = BTreeMap::new();
        tags.insert("policy.TestValidateWeights".into(), vec!["a.rs:1".into()]);
        assert_eq!(
            coverage_of(&tests[0], &tags, &BTreeMap::new()),
            Coverage::Tagged
        );
    }

    #[test]
    fn valid_specs_holds_both_forms() {
        let specs = valid_specs(&go_tests());
        assert!(specs.contains("decision/policy.TestValidateWeights"));
        assert!(specs.contains("policy.TestValidateWeights"));
        assert!(!specs.contains("policy.TestUnknown"));
    }

    #[test]
    fn divergence_citations_read_markers_and_comment_tokens() {
        assert_eq!(
            divergence_citations(r#"    let _d16 = parity_support::divergence!("D16");"#),
            ["D16"]
        );
        assert_eq!(
            divergence_citations("    // D21/D22: sqlx and graphile"),
            ["D21", "D22"]
        );
        assert_eq!(
            divergence_citations("/// DIVERGENCE: D16 and D40."),
            ["D16", "D40"]
        );
        // Not a comment, not a marker, or not a bare token.
        assert!(divergence_citations(r#"    let d = "D16";"#).is_empty());
        assert!(divergence_citations("    // ID12 D12x AD3 D_4").is_empty());
        // Hex in prose is not a citation; a marker is checked at any length.
        assert!(divergence_citations("    // U+10000 is D800 DC00 in UTF-16").is_empty());
        assert_eq!(divergence_citations(r#"divergence!("D400")"#), ["D400"]);
    }

    #[test]
    fn row_audit_finds_unknown_and_unasserted_rows() {
        let rows: BTreeSet<String> = ["D1", "D2", "D10"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let mut cited: BTreeMap<String, Vec<String>> = BTreeMap::new();
        cited.insert("D2".into(), vec!["a.rs:1".into()]);
        cited.insert("D99".into(), vec!["b.rs:2".into()]);
        let audit = audit_rows(&rows, &cited);
        assert_eq!(audit.unasserted, ["D1", "D10"]);
        assert_eq!(audit.unknown.keys().collect::<Vec<_>>(), ["D99"]);
        assert_eq!(audit.citations.get("D2"), Some(&1));
    }

    #[test]
    fn test_code_is_a_tests_dir_or_after_cfg_test() {
        assert!(is_test_file(Path::new("crates/x/tests/a.rs")));
        assert!(is_test_file(Path::new("crates/x/tests/replay/mod.rs")));
        assert!(!is_test_file(Path::new("crates/x/src/lib.rs")));
    }

    #[test]
    fn manifests_and_repo_scan_are_consistent() {
        // Runs against the real repository: every manifest test lands in exactly one bucket.
        let root = repo_root().expect("repo root");
        let manifest_dir = parity_path("manifest").expect("manifest dir");
        let go_tests = load_manifest_go_tests(&manifest_dir).expect("manifests parse");
        assert!(
            go_tests.len() > 600,
            "expected the full Go test corpus, got {}",
            go_tests.len()
        );

        let tags = scan_sources(&root.join("crates"))
            .expect("scan crates")
            .tags;
        let divergences_text =
            fs::read_to_string(parity_path("DIVERGENCES.md").expect("divergences path"))
                .expect("read divergences");
        let refs = divergence_refs(&divergences_text, &go_tests);

        for go_test in &go_tests {
            // Every test resolves to exactly one coverage state without panicking.
            let _ = coverage_of(go_test, &tags, &refs);
        }
    }
}
