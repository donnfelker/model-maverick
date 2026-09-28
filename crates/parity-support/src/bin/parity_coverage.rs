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
    let strict = std::env::args().skip(1).any(|arg| arg == "--strict");

    let root = match repo_root() {
        Ok(root) => root.to_path_buf(),
        Err(error) => {
            eprintln!("parity-coverage: {error}");
            return ExitCode::FAILURE;
        }
    };

    match run(&root, strict) {
        Ok(exit) => exit,
        Err(error) => {
            eprintln!("parity-coverage: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(root: &Path, strict: bool) -> Result<ExitCode, String> {
    let manifest_dir = parity_path("manifest").map_err(|error| error.to_string())?;
    let go_tests = load_manifest_go_tests(&manifest_dir)?;

    let tags = scan_parity_tags(&root.join("crates"))?;

    let divergences_md = parity_path("DIVERGENCES.md").map_err(|error| error.to_string())?;
    let divergences_text = fs::read_to_string(&divergences_md)
        .map_err(|error| format!("read {}: {error}", divergences_md.display()))?;
    let divergence_refs = divergence_refs(&divergences_text, &go_tests);

    let valid_specs = valid_specs(&go_tests);
    let unknown_tags: BTreeMap<String, Vec<String>> = tags
        .iter()
        .filter(|(spec, _)| !valid_specs.contains(spec.as_str()))
        .map(|(spec, locations)| (spec.clone(), locations.clone()))
        .collect();

    let mut per_area: BTreeMap<String, AreaCounts> = BTreeMap::new();
    let mut missing: BTreeMap<String, Vec<String>> = BTreeMap::new();
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
                    .push(go_test.name.clone());
            }
        }
    }

    print_report(&per_area, &unknown_tags, strict);

    let total_missing: usize = per_area.values().map(|counts| counts.missing).sum();
    if strict && (total_missing > 0 || !unknown_tags.is_empty()) {
        return Ok(ExitCode::FAILURE);
    }
    Ok(ExitCode::SUCCESS)
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
                go_tests.push(GoTest {
                    area: area.clone(),
                    name: name.to_string(),
                });
            }
        }
    }
    Ok(go_tests)
}

/// Walk `crates/` for `.rs` files and collect every `parity:` tag spec with its locations.
fn scan_parity_tags(crates_dir: &Path) -> Result<BTreeMap<String, Vec<String>>, String> {
    let mut sources = Vec::new();
    collect_rs_files(crates_dir, &mut sources)?;
    sources.sort();

    let mut tags: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for source in sources {
        let text = fs::read_to_string(&source)
            .map_err(|error| format!("read {}: {error}", source.display()))?;
        for (index, line) in text.lines().enumerate() {
            if let Some(spec) = tag_spec(line) {
                tags.entry(spec.to_string()).or_default().push(format!(
                    "{}:{}",
                    source.display(),
                    index + 1
                ));
            }
        }
    }
    Ok(tags)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn go_tests() -> Vec<GoTest> {
        vec![
            GoTest {
                area: "decision".into(),
                name: "policy.TestValidateWeights".into(),
            },
            GoTest {
                area: "store".into(),
                name: "store.TestMigrationsUpDownUpClean".into(),
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

        let tags = scan_parity_tags(&root.join("crates")).expect("scan crates");
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
