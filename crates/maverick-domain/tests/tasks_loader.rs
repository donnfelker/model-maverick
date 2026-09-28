//! Parity: `parity/fixtures/measurement-io/tasks.json` (`internal/tasks/loader_test.go`).
//!
//! Each case is a fixture directory or an inline set of YAML documents, and the exact
//! violation list Go produced. Two kinds of message text come from third-party
//! libraries and are not reproduced (D36): `yaml_invalid` messages (goccy/go-yaml,
//! QUIRK Q-T1) and JSON-schema failure details (santhosh-tekuri/jsonschema, D24). For
//! those the test asserts the rule, file, task, the engine-authored prefix, and the
//! substrings the Go tests check.

use std::path::{Path, PathBuf};

use maverick_domain::graders::{ValidationRegistry, parse_envelope};
use maverick_domain::tasks::{self, Corpus, Report, Violation};
use serde_json::Value;

fn fixture() -> Value {
    parity_support::fixture("measurement-io", "tasks.json").expect("tasks.json loads")
}

fn case(id: &str) -> Value {
    fixture()["cases"]
        .as_array()
        .expect("cases")
        .iter()
        .find(|c| c["id"] == id)
        .unwrap_or_else(|| panic!("no case {id}"))
        .clone()
}

/// A fresh directory under the test binary's temp dir.
fn scratch_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("tasks_loader")
        .join(name.replace('/', "__"));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("clear scratch dir");
    }
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn write_docs(dir: &Path, docs: &[(String, String)]) {
    for (name, text) in docs {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, text).expect("write doc");
    }
}

/// The directory a case loads: a fixture directory, or its inline documents written to
/// a scratch directory.
fn case_dir(case: &Value) -> PathBuf {
    let id = case["id"].as_str().expect("id");
    if let Some(dir) = case["dir"].as_str() {
        return parity_support::fixture_path("measurement-io", dir).expect("fixture dir");
    }
    let docs: Vec<(String, String)> = match &case["docs"] {
        Value::Object(map) => map
            .iter()
            .map(|(name, text)| (name.clone(), text.as_str().expect("doc text").to_owned()))
            .collect(),
        // oversize_smoke describes its second document instead of embedding 137 KB.
        Value::String(_) => oversize_smoke_docs(case),
        other => panic!("{id}: unexpected docs {other}"),
    };
    let dir = scratch_dir(id);
    write_docs(&dir, &docs);
    dir
}

/// `task_lc_002.yaml` is the small doc with id `task_lc_002`, difficulty_tier 4, and the
/// attachment content `long context filler. ` repeated 6553 times after 6 spaces of
/// block indentation, followed by a newline.
fn oversize_smoke_docs(case: &Value) -> Vec<(String, String)> {
    let small = case["small_doc"].as_str().expect("small_doc");
    let content_line = "      Section 12 defines the term \"Filler\".\n";
    assert!(small.ends_with(content_line), "small_doc layout changed");
    let large = small
        .replace("id: task_lc_001\n", "id: task_lc_002\n")
        .replace("difficulty_tier: 2\n", "difficulty_tier: 4\n");
    let large = format!(
        "{}      {}\n",
        &large[..large.len() - content_line.len()],
        "long context filler. ".repeat(6553)
    );
    vec![
        ("task_lc_001.yaml".to_owned(), small.to_owned()),
        ("task_lc_002.yaml".to_owned(), large),
    ]
}

fn load_and_validate(case: &Value) -> (Corpus, Report) {
    let corpus = tasks::load(&case_dir(case)).expect("corpus loads");
    let report = tasks::validate(&corpus, &ValidationRegistry::v1());
    (corpus, report)
}

/// Extra substrings a `yaml_invalid` message must contain, per case (the Go tests'
/// `contains` checks and the offending token).
fn yaml_needles(case_id: &str) -> &'static [&'static str] {
    match case_id {
        "invalid/unknown_field" => &["notes"],
        "probe/duplicate_key" => &["\"id\""],
        "probe/wrong_type" => &["difficulty_tier", "two"],
        _ => &[],
    }
}

fn assert_violation(case_id: &str, got: &Violation, want: &Value) {
    let context = format!("{case_id}: {got}");
    assert_eq!(got.file, want["File"].as_str().expect("File"), "{context}");
    assert_eq!(
        got.task_id,
        want["TaskID"].as_str().expect("TaskID"),
        "{context}"
    );
    assert_eq!(
        got.rule.as_str(),
        want["Rule"].as_str().expect("Rule"),
        "{context}"
    );
    let want_message = want["Message"].as_str().expect("Message");
    let want_string = want["String"].as_str().expect("String");

    if got.rule.as_str() == "yaml_invalid" {
        let _ = parity_support::divergence!("D36");
        assert!(!got.message.is_empty(), "{context}");
        for needle in yaml_needles(case_id) {
            assert!(got.message.contains(needle), "{context}: missing {needle}");
        }
        assert!(
            want_message.starts_with('['),
            "{context}: goccy position prefix"
        );
        assert!(got.message.starts_with('['), "{context}: position prefix");
        return;
    }

    const SCHEMA_MARKER: &str = "jsonschema validation failed";
    if let Some(at) = want_message.find(SCHEMA_MARKER) {
        let _ = parity_support::divergence!("D24");
        let prefix = &want_message[..at + SCHEMA_MARKER.len()];
        assert!(
            got.message.starts_with(prefix),
            "{context}: want prefix {prefix:?}"
        );
        // The failing instance location matches ("- at '/answers': ...").
        if let Some(location) = want_message.split("- at '").nth(1) {
            let location = location.split('\'').next().expect("location");
            assert!(
                got.message.contains(&format!("'{location}'")),
                "{context}: want location {location}"
            );
        }
        return;
    }

    assert_eq!(got.message, want_message, "{case_id}");
    assert_eq!(got.to_string(), want_string, "{case_id}");
}

fn assert_violations(case_id: &str, got: &[Violation], want: &Value) {
    let want = want.as_array().map(Vec::as_slice).unwrap_or(&[]);
    let got_strings: Vec<String> = got.iter().map(ToString::to_string).collect();
    assert_eq!(
        got.len(),
        want.len(),
        "{case_id}: got {got_strings:#?}\nwant {want:#?}"
    );
    for (g, w) in got.iter().zip(want) {
        assert_violation(case_id, g, w);
    }
}

/// Runs one case: load order, then every error and warning.
fn check_case(id: &str) {
    let case = case(id);
    let (corpus, report) = load_and_validate(&case);

    let order: Vec<String> = corpus
        .tasks
        .iter()
        .map(|t| format!("{}#{}", t.file, t.id))
        .collect();
    // Absent: not recorded for this case. `null`: Go loaded no tasks.
    match case.get("tasks_in_load_order") {
        None => {}
        Some(Value::Array(want)) => {
            let want: Vec<&str> = want.iter().map(|v| v.as_str().expect("entry")).collect();
            assert_eq!(order, want, "{id}: load order");
        }
        Some(Value::Null) => {
            assert!(order.is_empty(), "{id}: expected no tasks, got {order:?}");
        }
        Some(other) => panic!("{id}: unexpected tasks_in_load_order {other}"),
    }

    assert_violations(id, &report.errors, &case["errors"]);
    // judge_rubric_valid records its warnings as a note ("not asserted").
    if case["warnings"].is_array() {
        assert_violations(id, &report.warnings, &case["warnings"]);
    }
}

/// parity: tasks.TestLoadValidCorpus
#[test]
fn load_valid_corpus() {
    let expect = fixture()["load_valid_corpus_expect"].clone();
    let (corpus, _) = load_and_validate(&case("valid"));
    assert_eq!(corpus.tasks.len() as u64, expect["task_count"]);
    let task = |id: &str| {
        corpus
            .tasks
            .iter()
            .find(|t| t.id == id)
            .unwrap_or_else(|| panic!("no task {id}"))
    };

    let sx017 = task("task_sx_017");
    let want = &expect["task_sx_017"];
    assert_eq!(sx017.capability, want["capability"]);
    assert_eq!(sx017.difficulty_tier, want["difficulty_tier"].as_i64());
    assert_eq!(sx017.domain_tags, ["legal"]);
    let knobs = sx017.difficulty_knobs.as_ref().expect("knobs");
    assert_eq!(
        knobs["nesting_depth"].as_i64(),
        want["difficulty_knobs.nesting_depth"].as_i64()
    );
    assert_eq!(sx017.attachments.len(), 1);
    assert_eq!(sx017.attachments[0].name, want["attachments"][0]["name"]);
    assert!(
        sx017.attachments[0].content.contains(
            want["attachments"][0]["content_contains"]
                .as_str()
                .expect("needle")
        )
    );
    assert_eq!(sx017.max_tokens, want["max_tokens"].as_i64());
    assert_eq!(
        f64::from(sx017.temperature),
        want["temperature"].as_f64().expect("t")
    );
    assert_eq!(
        sx017.pass_threshold,
        want["pass_threshold"].as_f64().expect("p")
    );
    assert_eq!(sx017.smoke_member, want["smoke_member"]);
    assert_eq!(sx017.grader_specs.len() as u64, want["grader_specs_count"]);
    let first = parse_envelope(sx017.grader_specs[0].as_deref().expect("spec 0")).expect("parses");
    assert_eq!(first.kind.as_str(), want["grader_specs[0]"]["type"]);
    assert_eq!(first.version, want["grader_specs[0]"]["version"]);
    let weights = want["dimension_weights"].as_object().expect("weights");
    assert_eq!(sx017.dimension_weights.len(), weights.len());
    for (dim, w) in weights {
        assert_eq!(
            sx017.dimension_weights[dim],
            w.as_f64().expect("weight"),
            "{dim}"
        );
    }

    let sx001 = task("task_sx_001");
    let want = &expect["task_sx_001"];
    assert_eq!(sx001.smoke_member, want["smoke_member"]);
    assert_eq!(sx001.difficulty_tier, want["difficulty_tier"].as_i64());
    // pass_threshold_source "default": the YAML sets none.
    assert_eq!(
        sx001.pass_threshold,
        want["pass_threshold"].as_f64().expect("p")
    );
    assert_eq!(sx001.pass_threshold, tasks::DEFAULT_PASS_THRESHOLD);

    let sx002 = task("task_sx_002");
    let want = &expect["task_sx_002"];
    assert_eq!(sx002.smoke_member, want["smoke_member"]);
    assert_eq!(sx002.difficulty_tier, want["difficulty_tier"].as_i64());

    for row in fixture()["estimated_input_tokens"]
        .as_array()
        .expect("rows")
    {
        let id = row["id"].as_str().expect("id");
        assert_eq!(
            task(id).estimated_input_tokens(),
            row["estimated_input_tokens"].as_i64().expect("tokens"),
            "{id}"
        );
    }
}

/// parity: tasks.TestLoadMissingDir
#[test]
fn load_missing_dir() {
    let want = &fixture()["load_errors"]["tasks.TestLoadMissingDir"];
    // Integration tests run in the crate directory, where `testdata/nope` does not
    // exist, so the relative path reproduces Go's text exactly.
    let dir = want["dir"].as_str().expect("dir");
    let error = tasks::load(Path::new(dir)).expect_err("missing dir");
    assert_eq!(error.to_string(), want["error"]);
}

/// parity: tasks.TestLoadEmptyDir
#[test]
fn load_empty_dir() {
    let want = fixture()["load_errors"]["tasks.TestLoadEmptyDir"]["error"]
        .as_str()
        .expect("error")
        .to_owned();
    let dir = scratch_dir("empty_dir");
    // Non-task files do not count.
    write_docs(&dir, &[("README.md".to_owned(), "# none".to_owned())]);
    let error = tasks::load(&dir).expect_err("empty dir");
    assert_eq!(
        error.to_string(),
        want.replace("<dir>", &dir.display().to_string())
    );
}

/// parity: tasks.TestValidateValidCorpus
#[test]
fn validate_valid_corpus() {
    check_case("valid");
    let (_, report) = load_and_validate(&case("valid"));
    assert!(report.is_valid());
}

/// parity: tasks.TestValidateRejectingFixtures
#[test]
fn validate_rejecting_fixtures() {
    for id in [
        "invalid/bad_id",
        "invalid/unknown_capability",
        "invalid/tier_range",
        "invalid/weights_sum",
        "invalid/unknown_grader",
        "invalid/test_suite",
        "invalid/bad_grader_payload",
        "invalid/smoke_anchors",
        "invalid/duplicate_id",
        "invalid/unknown_field",
    ] {
        check_case(id);
        let (_, report) = load_and_validate(&case(id));
        assert!(!report.is_valid(), "{id} must be rejected");
    }
}

/// parity: tasks.TestValidateTestSuiteUsesGraderUnsupportedWording
#[test]
fn validate_test_suite_uses_grader_unsupported_wording() {
    check_case("invalid/test_suite");
}

/// parity: tasks.TestValidateUnknownFieldNamesTheField
#[test]
fn validate_unknown_field_names_the_field() {
    check_case("invalid/unknown_field");
}

/// parity: tasks.TestValidateOversizeSmokeInput
#[test]
fn validate_oversize_smoke_input() {
    check_case("oversize_smoke");
}

/// parity: tasks.TestValidateReportsEveryViolationInOneRun
#[test]
fn validate_reports_every_violation_in_one_run() {
    check_case("multi_violation");
}

/// parity: tasks.TestValidateDimensionMismatch
#[test]
fn validate_dimension_mismatch() {
    check_case("dimension_mismatch");
}

/// parity: tasks.TestValidateJudgeRubricTasks
#[test]
fn validate_judge_rubric_tasks() {
    check_case("judge_rubric_valid");
    check_case("judge_rubric_invalid");
}

/// Probe rows (not Go tests): behavior the Go tests leave open, recorded by running the
/// Go code.
#[test]
fn validate_probe_cases() {
    let probes: Vec<String> = fixture()["cases"]
        .as_array()
        .expect("cases")
        .iter()
        .filter(|c| c["probe"] == true)
        .map(|c| c["id"].as_str().expect("id").to_owned())
        .collect();
    assert!(probes.len() >= 16, "{probes:?}");
    for id in probes {
        check_case(&id);
    }
}

/// Every case in the fixture is exercised by one of the tests above.
#[test]
fn every_case_is_covered() {
    let named = [
        "valid",
        "invalid/bad_id",
        "invalid/unknown_capability",
        "invalid/tier_range",
        "invalid/weights_sum",
        "invalid/unknown_grader",
        "invalid/test_suite",
        "invalid/bad_grader_payload",
        "invalid/smoke_anchors",
        "invalid/duplicate_id",
        "invalid/unknown_field",
        "multi_violation",
        "dimension_mismatch",
        "oversize_smoke",
        "judge_rubric_valid",
        "judge_rubric_invalid",
    ];
    for c in fixture()["cases"].as_array().expect("cases") {
        let id = c["id"].as_str().expect("id");
        assert!(
            c["probe"] == true || named.contains(&id),
            "case {id} is not exercised"
        );
    }
}

/// parity: tasks.TestGraderSpecsSurviveJSONRoundTrip
#[test]
fn grader_specs_survive_json_round_trip() {
    let expect = fixture()["upsert_params"].clone();
    let (corpus, _) = load_and_validate(&case("valid"));
    let task = |id: &str| corpus.tasks.iter().find(|t| t.id == id).expect("task");

    let sx017 = task("task_sx_017");
    let want = &expect["task_sx_017"];
    assert_eq!(sx017.id, want["id"]);
    assert_eq!(sx017.capability, want["capability"]);
    assert_eq!(sx017.difficulty_tier, want["difficulty_tier"].as_i64());
    assert_eq!(sx017.prompt, want["prompt"]);
    assert_eq!(sx017.max_tokens, want["max_tokens"].as_i64());
    assert_eq!(sx017.smoke_member, want["smoke_member"]);
    let domain_tags: Vec<&str> = want["domain_tags"]
        .as_array()
        .expect("tags")
        .iter()
        .map(|t| t.as_str().expect("tag"))
        .collect();
    assert_eq!(sx017.domain_tags, domain_tags);
    // Postgres `real` columns: the values travel as float32.
    assert_eq!(
        f64::from(sx017.pass_threshold as f32),
        parity_support::as_f32_widened(want["pass_threshold"].as_f64().expect("p"))
    );
    assert_eq!(
        f64::from(sx017.temperature),
        want["temperature"].as_f64().expect("t")
    );
    assert_eq!(sx017.attachments_json(), want["attachments"]);
    assert_eq!(
        sx017.difficulty_knobs_json().expect("knobs"),
        want["difficulty_knobs"]
    );
    assert_eq!(
        sx017.dimension_weights_json().expect("weights"),
        want["dimension_weights"]
    );
    assert_eq!(sx017.grader_specs_json(), want["grader_specs"]);

    let fq001 = task("task_fq_001");
    let want = &expect["task_fq_001_partial"];
    assert_eq!(fq001.attachments_json(), want["attachments"]);
    assert_eq!(
        fq001.difficulty_knobs_json().expect("knobs"),
        want["difficulty_knobs"]
    );
    assert!(fq001.domain_tags.is_empty());
    assert_eq!(want["domain_tags"], serde_json::json!([]));
    assert_eq!(fq001.grader_specs_json(), want["grader_specs"]);
}
