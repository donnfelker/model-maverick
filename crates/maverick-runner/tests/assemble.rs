//! Parity: `parity/fixtures/measurement-io/runner.json#assemble`
//! (`internal/runner/assemble_test.go`).

use maverick_runner::{BenchColumn, BenchTask, assemble};
use serde_json::Value;

fn rows() -> Vec<Value> {
    parity_support::fixture("measurement-io", "runner.json").expect("runner.json loads")["assemble"]
        .as_array()
        .expect("assemble rows")
        .clone()
}

fn row(go_test: &str) -> Value {
    rows()
        .into_iter()
        .find(|r| r["go_test"] == go_test)
        .unwrap_or_else(|| panic!("no row {go_test}"))
}

fn str_or<'a>(value: &'a Value, default: &'a str) -> &'a str {
    value.as_str().unwrap_or(default)
}

fn task(value: &Value) -> BenchTask<'_> {
    BenchTask {
        id: str_or(&value["id"], "task_assemble_001"),
        prompt: str_or(&value["prompt"], ""),
        attachments_json: str_or(&value["attachments_json"], ""),
        temperature: value["temperature"].as_f64().unwrap_or(0.0) as f32,
        max_tokens: value["max_tokens"].as_i64().unwrap_or(0) as i32,
    }
}

fn column(value: &Value) -> BenchColumn<'_> {
    BenchColumn {
        id: str_or(&value["id"], "col_assemble_001"),
        model_id: str_or(&value["model_id"], ""),
        settings_json: str_or(&value["settings_json"], ""),
    }
}

fn golden(path: &str) -> String {
    let relative = path.strip_prefix("testdata/").expect("testdata path");
    String::from_utf8(parity_support::measurement_io_testdata(relative).expect("golden"))
        .expect("utf-8 golden")
}

/// parity: runner.TestAssembleGoldenBasic
#[test]
fn golden_basic() {
    let row = row("runner.TestAssembleGoldenBasic");
    let request = assemble(&task(&row["task"]), &column(&row["column"])).expect("assembles");
    let expect = &row["expect"];
    assert_eq!(request.model, expect["model"].as_str().expect("model"));
    assert_eq!(request.system, expect["system"].as_str().expect("system"));
    assert_eq!(
        request.prompt,
        expect["prompt_exact"].as_str().expect("prompt")
    );
    assert_eq!(
        request.prompt,
        golden(expect["prompt_golden_file"].as_str().expect("file"))
    );
    assert_eq!(request.prompt.len(), 69);
    assert_eq!(
        f64::from(request.temperature),
        expect["temperature"].as_f64().expect("t")
    );
    assert_eq!(request.max_tokens as u64, expect["max_tokens"]);
    assert!(expect["settings"].is_null() && request.settings.is_empty());
}

/// parity: runner.TestAssembleGoldenAttachments
#[test]
fn golden_attachments() {
    let row = row("runner.TestAssembleGoldenAttachments");
    let column = BenchColumn {
        id: "col",
        model_id: "m",
        settings_json: "{}",
    };
    let request = assemble(&task(&row["task"]), &column).expect("assembles");
    let expect = &row["expect"];
    assert_eq!(
        request.prompt,
        expect["prompt_exact"].as_str().expect("prompt")
    );
    assert_eq!(
        request.prompt,
        golden(expect["prompt_golden_file"].as_str().expect("file"))
    );
}

/// parity: runner.TestAssembleByteIdenticalAcrossColumnsAndCalls
#[test]
fn byte_identical_across_columns_and_calls() {
    let row = row("runner.TestAssembleByteIdenticalAcrossColumnsAndCalls");
    let task = task(&row["task"]);
    let want = row["expect"]["prompt_exact_all_calls"]
        .as_str()
        .expect("prompt");
    let mut models = Vec::new();
    for column_row in row["columns"].as_array().expect("columns") {
        for _ in 0..3 {
            let request = assemble(&task, &column(column_row)).expect("assembles");
            assert_eq!(request.prompt, want);
            assert_eq!(request.system, "");
            models.push(request.model);
        }
    }
    models.dedup();
    assert_eq!(models.len() > 1, row["expect"]["models_differ"] == true);
}

/// parity: runner.TestAssembleColumnSettings
#[test]
fn column_settings() {
    let row = row("runner.TestAssembleColumnSettings");
    let column = BenchColumn {
        id: "col",
        model_id: "m",
        settings_json: row["column_settings_json"].as_str().expect("settings"),
    };
    let task = BenchTask {
        id: "t",
        prompt: "p",
        attachments_json: "[]",
        temperature: 0.0,
        max_tokens: 1,
    };
    let request = assemble(&task, &column).expect("assembles");
    assert_eq!(
        Value::Object(request.settings.clone()),
        row["expect_settings"]
    );
    // Numbers decode as float64, as in Go's map[string]any.
    assert!(request.settings["thinking_budget"].is_f64());
}

/// parity: runner.TestAssembleMalformedInputs
#[test]
fn malformed_inputs() {
    for case in row("runner.TestAssembleMalformedInputs")["cases"]
        .as_array()
        .expect("cases")
    {
        let task = BenchTask {
            id: "task_assemble_001",
            prompt: "p",
            attachments_json: case["attachments_json"].as_str().expect("attachments"),
            temperature: 0.0,
            max_tokens: 1,
        };
        let column = BenchColumn {
            id: "col_assemble_001",
            model_id: "m",
            settings_json: case["settings_json"].as_str().expect("settings"),
        };
        let error = assemble(&task, &column).expect_err("malformed");
        // These tails happen to match Go's text exactly; D36 would allow them to differ.
        assert_eq!(error.to_string(), case["error"].as_str().expect("error"));
    }
}

/// The two probe rows: an empty attachment still emits its header; `null` settings are
/// none; array settings are an error.
#[test]
fn probes() {
    let probes: Vec<Value> = rows().into_iter().filter(|r| r["probe"] == true).collect();
    assert_eq!(probes.len(), 2);

    let request = assemble(&task(&probes[0]["task"]), &column(&probes[0]["column"])).expect("ok");
    let expect = &probes[0]["expect"];
    assert_eq!(request.model, expect["Model"].as_str().expect("model"));
    assert_eq!(request.system, "");
    assert_eq!(request.prompt, expect["Prompt"].as_str().expect("prompt"));
    assert_eq!(request.temperature, 0.2_f32);
    assert_eq!(request.max_tokens as u64, expect["MaxTokens"]);
    assert!(request.settings.is_empty());

    let error = assemble(&task(&probes[1]["task"]), &column(&probes[1]["column"]))
        .expect_err("array settings");
    assert_eq!(
        error.to_string(),
        probes[1]["expect"]["err"].as_str().expect("err")
    );
}
