//! Parity: `parity/fixtures/decision/history_telemetry.json` and the item rows of
//! `parity/fixtures/api/telemetry.json`, with D16 (`completed_unchecked`) and D14
//! (`settings_applied`).

use maverick_domain::telemetry::{
    DEFAULT_ENDPOINT, ENDPOINTS, ERROR_CLASSES, MAX_ITEMS, decode_item, outcome,
};
use serde_json::Value;

/// A 2xx without `check_pass` was `success` in Go; Rust derives `completed_unchecked`.
fn expected_outcome(go_outcome: &str, item: &Value) -> &'static str {
    let status = item["status"].as_i64().unwrap_or(-1);
    if go_outcome == outcome::SUCCESS
        && (200..=299).contains(&status)
        && item.get("check_pass").is_none_or(Value::is_null)
    {
        let _ = parity_support::divergence!("D16");
        return outcome::COMPLETED_UNCHECKED;
    }
    match go_outcome {
        "success" => outcome::SUCCESS,
        "parse_failure" => outcome::PARSE_FAILURE,
        "latency_exceeded" => outcome::LATENCY_EXCEEDED,
        "provider_error" => outcome::PROVIDER_ERROR,
        "other" => outcome::OTHER,
        other => panic!("outcome {other}"),
    }
}

#[test]
fn history_telemetry_rows() {
    let doc = parity_support::fixture("decision", "history_telemetry.json").expect("fixture");
    assert_eq!(doc["max_items"], MAX_ITEMS);
    for case in doc["cases"].as_array().expect("cases") {
        let input = case["input"].as_str().expect("input");
        match decode_item(input) {
            Ok(item) => {
                assert_eq!(case["accepted"], true, "{input}: accepted");
                let raw: Value = serde_json::from_str(input).expect("json");
                assert_eq!(
                    item.outcome(),
                    expected_outcome(case["outcome"].as_str().expect("outcome"), &raw),
                    "{input}"
                );
                assert_eq!(item.endpoint_used, case["endpoint_used"], "{input}");
                assert_eq!(item.details_json(), case["details_json_exact"], "{input}");
            }
            Err(dropped) => assert_eq!(case["accepted"], false, "{input}: {dropped:?}"),
        }
    }
}

fn api_fixture() -> Value {
    parity_support::fixture("api", "telemetry.json").expect("fixture")
}

/// The item a derivation row posts: the owned recommendation plus the row's fields.
fn item_json(fields: &Value) -> String {
    let mut item = serde_json::json!({
        "recommendation_id": "rec_owned",
        "model_used": "claude-sonnet-4-6",
    });
    for (k, v) in fields.as_object().expect("item") {
        item[k] = v.clone();
    }
    item.to_string()
}

/// parity: api.TestTelemetryDerivationTable
/// parity: api.TestTelemetryDerivationTable/timeout_beats_a_5xx_status
/// parity: api.TestTelemetryDerivationTable/timeout_beats_status_0
/// parity: api.TestTelemetryDerivationTable/connect_error_beats_a_2xx_status
/// parity: api.TestTelemetryDerivationTable/provider_error_error_class
/// parity: api.TestTelemetryDerivationTable/status_0_(transport_failure)
/// parity: api.TestTelemetryDerivationTable/status_429
/// parity: api.TestTelemetryDerivationTable/status_500
/// parity: api.TestTelemetryDerivationTable/status_503
/// parity: api.TestTelemetryDerivationTable/2xx_with_check_pass_false
/// parity: api.TestTelemetryDerivationTable/201_with_check_pass_false
/// parity: api.TestTelemetryDerivationTable/2xx_success
/// parity: api.TestTelemetryDerivationTable/2xx_with_check_pass_true
/// parity: api.TestTelemetryDerivationTable/semantic_4xx_is_other
/// parity: api.TestTelemetryDerivationTable/3xx_is_other
#[test]
fn derivation_rows() {
    for row in api_fixture()["derivation"].as_array().expect("rows") {
        let item = decode_item(&item_json(&row["item"])).expect("accepted");
        assert_eq!(
            item.outcome(),
            expected_outcome(
                row["expected_outcome"].as_str().expect("outcome"),
                &row["item"]
            ),
            "{}",
            row["id"]
        );
    }
}

/// parity: api.TestTelemetryDropsInvalidItems
/// parity: api.TestTelemetryDropsInvalidItems/missing_recommendation_id
/// parity: api.TestTelemetryDropsInvalidItems/empty_recommendation_id
/// parity: api.TestTelemetryDropsInvalidItems/missing_model_used
/// parity: api.TestTelemetryDropsInvalidItems/missing_status
/// parity: api.TestTelemetryDropsInvalidItems/mistyped_status_(string)
/// parity: api.TestTelemetryDropsInvalidItems/negative_status
/// parity: api.TestTelemetryDropsInvalidItems/negative_latency_ms
/// parity: api.TestTelemetryDropsInvalidItems/negative_tokens_in
/// parity: api.TestTelemetryDropsInvalidItems/negative_tokens_out
/// parity: api.TestTelemetryDropsInvalidItems/invalid_error_class
/// parity: api.TestTelemetryDropsInvalidItems/invalid_endpoint_used
/// parity: api.TestTelemetryDropsInvalidItems/mistyped_check_pass
/// parity: api.TestTelemetryDropsInvalidItems/item_is_not_an_object
#[test]
fn drop_rows() {
    for row in api_fixture()["drops"].as_array().expect("rows") {
        let raw = row["item"]
            .to_string()
            .replace("<seeded rec id>", "rec_owned");
        assert!(decode_item(&raw).is_err(), "{}: {raw}", row["id"]);
        assert_eq!(
            row["expected_response_body_exact"],
            "{\"accepted\":0,\"dropped\":1}\n"
        );
    }
}

#[test]
fn enums_match_the_fixture() {
    let enums = &api_fixture()["enums_exact"];
    assert_eq!(serde_json::json!(ERROR_CLASSES), enums["error_class"]);
    assert_eq!(serde_json::json!(ENDPOINTS), enums["endpoint_used"]);
    assert_eq!(enums["endpoint_used_default"], DEFAULT_ENDPOINT);
    assert_eq!(enums["max_items"], MAX_ITEMS);
}

/// D14: `settings_applied` is kept in `details`; D16: the three `check_pass` states.
#[test]
fn settings_applied_and_check_states() {
    let base = r#""recommendation_id":"rec_1","model_used":"m","status":200"#;
    let applied = decode_item(&format!("{{{base},\"settings_applied\":true}}")).expect("item");
    let _ = parity_support::divergence!("D14");
    assert_eq!(
        applied.details_json(),
        r#"{"settings_applied":true,"status":200}"#
    );
    assert_eq!(applied.outcome(), outcome::COMPLETED_UNCHECKED);
    assert!(decode_item(&format!("{{{base},\"settings_applied\":\"yes\"}}")).is_err());
    for (check, want) in [
        ("true", outcome::SUCCESS),
        ("false", outcome::PARSE_FAILURE),
        ("null", outcome::COMPLETED_UNCHECKED),
    ] {
        let item = decode_item(&format!("{{{base},\"check_pass\":{check}}}")).expect("item");
        assert_eq!(item.outcome(), want, "{check}");
    }
}

#[test]
fn lenient_decoding_rules() {
    // Case-insensitive keys, the last duplicate wins, and null leaves a string field.
    let item = decode_item(
        r#"{"Recommendation_ID":"rec_1","model_used":"a","model_used":"b","model_used":null,"STATUS":201,"endpoint_used":null}"#,
    )
    .expect("item");
    assert_eq!(item.recommendation_id, "rec_1");
    assert_eq!(item.model_used, "b");
    assert_eq!(item.status, 201);
    assert_eq!(item.endpoint_used, "direct");
    // A null item decodes to zero values and is dropped for the missing id.
    assert!(decode_item("null").is_err());
    // Other fields, all present.
    let full = decode_item(
        r#"{"recommendation_id":"r","model_used":"m","status":500,"latency_ms":0,"tokens_in":0,"tokens_out":7,"finish_reason":"<len>","error_class":"other","endpoint_used":"vertex"}"#,
    )
    .expect("item");
    assert_eq!(full.outcome(), outcome::PROVIDER_ERROR);
    assert_eq!(full.endpoint_used, "vertex");
    assert_eq!(
        full.details_json(),
        r#"{"error_class":"other","finish_reason":"\u003clen\u003e","latency_ms":0,"status":500,"tokens_in":0,"tokens_out":7}"#
    );
    for bad in [
        r#"{"recommendation_id":1,"model_used":"m","status":200}"#,
        r#"{"recommendation_id":"r","model_used":true,"status":200}"#,
        r#"{"recommendation_id":"r","model_used":"m","status":200,"finish_reason":1}"#,
        r#"{"recommendation_id":"r","model_used":"m","status":200,"latency_ms":"1"}"#,
        r#"{"recommendation_id":"r","model_used":"m","status":99999999999999999999}"#,
        r#"{"recommendation_id":"r","model_used":"m","status":200,"error_class":""}"#,
        r#"{"recommendation_id":"r","model_used":"m","status":200,"endpoint_used":""}"#,
        "[1]",
        "",
    ] {
        assert!(decode_item(bad).is_err(), "{bad}");
    }
    // Every enum value is accepted.
    for class in ERROR_CLASSES {
        let raw = format!(
            r#"{{"recommendation_id":"r","model_used":"m","status":200,"error_class":"{class}"}}"#
        );
        assert!(decode_item(&raw).is_ok(), "{class}");
    }
    for endpoint in ENDPOINTS {
        let raw = format!(
            r#"{{"recommendation_id":"r","model_used":"m","status":200,"endpoint_used":"{endpoint}"}}"#
        );
        assert_eq!(decode_item(&raw).expect("item").endpoint_used, endpoint);
    }
}
