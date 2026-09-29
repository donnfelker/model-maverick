//! Parity: `parity/fixtures/measurement-core/spend.json` (`internal/jobs`, the G8 spend
//! guard's pure parts).

use chrono::NaiveDate;
use maverick_domain::spend::{
    SpendCaps, SpendCheck, SpendColumn, SpendTask, estimate_input_tokens, project_spend,
};
use serde_json::Value;

fn fixture() -> Value {
    parity_support::fixture("measurement-core", "spend.json").expect("fixture loads")
}

fn number(value: &Value, key: &str) -> f64 {
    value[key]
        .as_f64()
        .unwrap_or_else(|| panic!("{key}: {value}"))
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key]
        .as_str()
        .unwrap_or_else(|| panic!("{key}: {value}"))
}

/// parity: jobs.TestEstimateInputTokens
#[test]
fn estimate_input_tokens_rows() {
    let doc = fixture();
    for case in doc["estimate_input_tokens"]["cases"]
        .as_array()
        .expect("cases")
    {
        let input = match case.get("text") {
            Some(text) => text.as_str().expect("text").to_owned(),
            None => {
                "x".repeat(usize::try_from(case["chars"].as_u64().expect("chars")).expect("fits"))
            }
        };
        assert_eq!(
            estimate_input_tokens(&input),
            case["expected"].as_i64().expect("expected"),
            "{case}"
        );
    }
}

/// The spec and the reference use `task.max_tokens` for output and 600 as the input
/// overhead; the master plan's "600 output tokens per run" is not the Go behavior.
///
/// parity: jobs.TestProjectSpendGolden
#[test]
fn project_spend_rows() {
    let doc = fixture();
    let spend = &doc["project_spend"];
    let tolerance = spend["tolerance"].as_f64().expect("tolerance");
    for case in spend["cases"].as_array().expect("cases") {
        let id = text(case, "id");
        let tasks: Vec<SpendTask<'_>> = case["tasks"]
            .as_array()
            .expect("tasks")
            .iter()
            .map(|t| {
                assert_eq!(
                    t["prompt"].as_str().expect("prompt").chars().count(),
                    usize::try_from(t["prompt_chars"].as_u64().expect("chars")).expect("fits")
                );
                SpendTask {
                    id: text(t, "id"),
                    prompt: text(t, "prompt"),
                    attachments: text(t, "attachments"),
                    max_tokens: t["max_tokens"].as_i64().expect("max_tokens"),
                }
            })
            .collect();
        let columns: Vec<SpendColumn<'_>> = case["columns"]
            .as_array()
            .expect("columns")
            .iter()
            .map(|c| SpendColumn {
                id: text(c, "id"),
                pricing: text(c, "pricing"),
            })
            .collect();
        let repeats = case["repeats"].as_i64().expect("repeats");
        let projection = project_spend(&tasks, &columns, repeats).expect(id);
        let expected = &case["expected"];
        parity_support::assert_f64_within(
            projection.projected_usd,
            number(expected, "projected_usd"),
            tolerance,
        );
        assert_eq!(
            projection.runs,
            expected["runs"].as_i64().expect("runs"),
            "{id}"
        );
    }
}

fn night(value: &Value) -> NaiveDate {
    text(value, "night").parse().expect("night")
}

/// The alert messages rendered with the exact Go format strings.
#[test]
fn message_rows() {
    let doc = fixture();
    for case in doc["messages"]["cases"].as_array().expect("cases") {
        let projected = number(case, "projected_usd");
        let check = SpendCheck {
            job: text(case, "job"),
            night: night(case),
            projected_usd: projected,
            planned_runs: 0,
            night_spent_usd: number(case, "night_total") - projected,
            week_spent_usd: number(case, "week_total") - projected,
            caps: SpendCaps {
                nightly_usd: number(case, "nightly_cap_usd"),
                weekly_usd: number(case, "weekly_cap_usd"),
            },
            apply_nightly_cap: case["apply_nightly_cap"].as_bool().expect("apply"),
        };
        assert_eq!(check.refusal_message(), text(case, "refusal_message"));
        assert_eq!(check.warning_message(), text(case, "warning_message"));
    }
}

/// The alert subject as Go's `json.Marshal` of the map writes it.
#[test]
fn subject_example() {
    let doc = fixture();
    let example = &doc["subject_example"];
    let want: Value = serde_json::from_str(text(example, "json")).expect("json");
    let check = SpendCheck {
        job: text(&want, "job"),
        night: night(&want),
        projected_usd: number(&want, "projected_usd"),
        planned_runs: want["planned_runs"].as_i64().expect("runs"),
        night_spent_usd: number(&want, "night_spent_usd"),
        week_spent_usd: number(&want, "week_spent_usd"),
        caps: SpendCaps {
            nightly_usd: number(&want, "nightly_cap_usd"),
            weekly_usd: number(&want, "weekly_cap_usd"),
        },
        apply_nightly_cap: true,
    };
    assert_eq!(check.subject().as_deref(), Ok(text(example, "json")));
}
