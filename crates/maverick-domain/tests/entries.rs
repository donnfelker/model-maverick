//! Parity: `parity/fixtures/decision/reasons.json` (`internal/recommend` entries and
//! reasons; an extra fixture without a Go test), plus the D8/D9 entry fields.

use maverick_domain::entries::{
    BELOW_CEILING, EntryColumn, FALLBACK, RankedColumn, RecommendationEntry, recommendations,
};
use maverick_domain::policy::RetentionMode;
use serde_json::{Value, json};

/// Numbers compare by value: the fixtures write `88` where Rust serializes `88.0`.
fn normalize(value: Value) -> Value {
    match value {
        Value::Number(n) => Value::from(n.as_f64().expect("finite number")),
        Value::Array(items) => Value::Array(items.into_iter().map(normalize).collect()),
        Value::Object(map) => {
            Value::Object(map.into_iter().map(|(k, v)| (k, normalize(v))).collect())
        }
        other => other,
    }
}

/// Go's entry shape: drops the D8/D9 fields after checking their neutral values.
fn go_shape(entries: &[RecommendationEntry]) -> Value {
    let _ = parity_support::divergence!("D8");
    let _ = parity_support::divergence!("D9");
    let mut value = serde_json::to_value(entries).expect("entries serialize");
    for entry in value.as_array_mut().expect("array") {
        let entry = entry.as_object_mut().expect("entry");
        assert_eq!(entry.remove("settings"), Some(json!({})));
        assert_eq!(entry.remove("retention_mode"), Some(json!("unknown")));
        assert_eq!(entry.remove("low_availability"), Some(json!(false)));
    }
    normalize(value)
}

#[test]
fn entries_match_go_reasons() {
    let fixture = parity_support::fixture("decision", "reasons.json").expect("fixture loads");
    let cases = fixture["cases"].as_array().expect("cases");
    assert_eq!(cases.len(), 4);
    for case in cases {
        let id = case["id"].as_str().expect("id");
        assert_eq!(case["function"], "recommendations", "{id}");
        let input = &case["input"];
        let columns: Vec<EntryColumn> =
            serde_json::from_value(input["columns"].clone()).expect("columns");
        let ranked: Vec<RankedColumn> =
            serde_json::from_value(input["ranked"].clone()).expect("ranked");
        let got = recommendations(
            &columns,
            &ranked,
            input["mode"].as_str().expect("mode"),
            input["capability"].as_str().expect("capability"),
            i32::try_from(input["difficulty"].as_i64().expect("difficulty")).expect("i32"),
        );
        assert_eq!(
            go_shape(&got),
            normalize(case["expected"].clone()),
            "case {id}"
        );
    }
}

fn column(id: &str, provider: &str) -> EntryColumn {
    EntryColumn {
        id: id.to_owned(),
        model_id: format!("model-{id}"),
        provider: provider.to_owned(),
        endpoint: "direct".to_owned(),
        execution_aliases: json!({}),
    }
}

fn ranked(id: &str, family: &str, quality: Option<f64>) -> RankedColumn {
    RankedColumn {
        column_id: id.to_owned(),
        family: family.to_owned(),
        quality,
        clears_difficulty: true,
        low_availability: false,
        settings: json!({}),
        retention_mode: RetentionMode::Unknown,
    }
}

#[test]
fn entry_fields_serialize_in_response_order() {
    let _ = parity_support::divergence!("D9");
    let entries = recommendations(
        &[column("col_a", "anthropic")],
        &[ranked("col_a", "anthropic", Some(90.0))],
        "balanced",
        "factual_qa",
        3,
    );
    let text = serde_json::to_string(&entries[0]).expect("serializes");
    let keys = [
        "\"provider\"",
        "\"model\"",
        "\"endpoint\"",
        "\"column_id\"",
        "\"execution_aliases\"",
        "\"settings\"",
        "\"retention_mode\"",
        "\"score\"",
        "\"clears_difficulty\"",
        "\"low_availability\"",
        "\"reason\"",
    ];
    let positions: Vec<usize> = keys
        .iter()
        .map(|key| text.find(key).unwrap_or_else(|| panic!("{key} in {text}")))
        .collect();
    assert!(
        positions.windows(2).all(|pair| pair[0] < pair[1]),
        "field order: {text}"
    );
}

#[test]
fn entries_carry_d8_and_d9_fields() {
    let _ = parity_support::divergence!("D8");
    let _ = parity_support::divergence!("D9");
    let mut pick = ranked("col_a", "anthropic", Some(91.26));
    pick.settings = json!({"thinking": true});
    pick.retention_mode = RetentionMode::Zero;
    let mut slow = ranked("col_b", "anthropic", Some(80.0));
    slow.low_availability = true;
    let entries = recommendations(
        &[column("col_a", "anthropic"), column("col_b", "anthropic")],
        &[pick, slow],
        "quality",
        "code_generation",
        4,
    );
    assert_eq!(entries[0].settings, json!({"thinking": true}));
    assert_eq!(entries[0].retention_mode, RetentionMode::Zero);
    assert_eq!(entries[0].score, Some(91.3));
    assert!(!entries[0].low_availability);
    assert!(entries[1].low_availability);
    assert_eq!(entries[1].reason, FALLBACK);
}

#[test]
fn equal_quality_is_not_an_escalation() {
    let entries = recommendations(
        &[column("col_a", "anthropic"), column("col_b", "anthropic")],
        &[
            ranked("col_a", "anthropic", Some(88.0)),
            ranked("col_b", "anthropic", Some(88.0)),
        ],
        "balanced",
        "factual_qa",
        3,
    );
    assert_eq!(entries[1].reason, FALLBACK);
}

#[test]
fn below_ceiling_wins_over_cost_mode_and_the_dash_is_an_em_dash() {
    let mut pick = ranked("col_a", "anthropic", Some(70.0));
    pick.clears_difficulty = false;
    let entries = recommendations(
        &[column("col_a", "anthropic")],
        &[pick],
        "cost",
        "factual_qa",
        5,
    );
    assert_eq!(entries[0].reason, BELOW_CEILING);
    assert!(BELOW_CEILING.contains(" \u{2014} "));
}

#[test]
fn no_ranked_columns_yield_no_entries() {
    assert!(recommendations(&[column("col_a", "x")], &[], "balanced", "factual_qa", 3).is_empty());
}

#[test]
fn a_column_missing_from_the_catalog_keeps_its_slot() {
    let entries = recommendations(
        &[],
        &[ranked("col_ghost", "anthropic", None)],
        "balanced",
        "factual_qa",
        3,
    );
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].provider, "");
    assert_eq!(entries[0].execution_aliases, json!({}));
    assert_eq!(entries[0].score, None);
}
