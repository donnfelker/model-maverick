//! History pagination cursors (bead 2.27): every `history_cursor.json` row.

use chrono::NaiveDate;
use maverick_domain::history::{
    AggregatePosition, INVALID_CURSOR, INVALID_CURSOR_PARAMETER, InvalidCursor, KIND_AGGREGATE,
    KIND_OUTCOME, KIND_RECOMMENDATION, decode_aggregate_cursor, decode_cursor,
    encode_aggregate_cursor, encode_cursor,
};
use serde_json::Value;

fn fixture() -> Value {
    parity_support::fixture("decision", "history_cursor.json").expect("history_cursor.json")
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .expect("strings")
        .iter()
        .map(|item| item.as_str().expect("string").to_owned())
        .collect()
}

/// Encode rows: exact tokens. Decode rows: fields or the exact error.
// parity: history.TestCursorRoundTrip
// parity: history.TestCursorIsOpaque
// parity: history.TestCursorRejectsGarbage
// parity: history.TestCursorRejectsForeignKind
#[test]
fn history_cursor_rows() {
    let doc = fixture();
    assert_eq!(doc["version"], "v1");
    assert_eq!(doc["separator"], "|");
    assert_eq!(doc["invalid_cursor_error"]["message"], INVALID_CURSOR);
    assert_eq!(
        doc["invalid_cursor_error"]["details"]["parameter"],
        INVALID_CURSOR_PARAMETER
    );
    assert_eq!(InvalidCursor.to_string(), INVALID_CURSOR);

    let encode = doc["encode"].as_array().expect("encode rows");
    assert_eq!(encode.len(), 5);
    for row in encode {
        let kind = row["kind"].as_str().expect("kind");
        let fields = strings(&row["fields"]);
        let fields: Vec<&str> = fields.iter().map(String::as_str).collect();
        let token = encode_cursor(kind, &fields);
        assert_eq!(
            token,
            row["expected_token"].as_str().expect("token"),
            "encode {kind} {fields:?}"
        );
        // Opaque: URL alphabet, no padding.
        assert!(
            token
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'),
            "{token}"
        );
        assert_eq!(
            decode_cursor(&token, kind, fields.len()),
            Ok(strings(&row["fields"])),
            "round trip"
        );
    }

    let decode = doc["decode"].as_array().expect("decode rows");
    assert_eq!(decode.len(), 11);
    for row in decode {
        let id = row["id"].as_str().expect("id");
        let token = row["token"].as_str().expect("token");
        let kind = row["kind"].as_str().expect("kind");
        let count =
            usize::try_from(row["field_count"].as_u64().expect("field_count")).expect("count");
        let got = decode_cursor(token, kind, count);
        if row["ok"].as_bool().expect("ok") {
            assert_eq!(got, Ok(strings(&row["fields"])), "{id}");
        } else {
            assert_eq!(got, Err(InvalidCursor), "{id}");
            assert_eq!(row["error_message"], INVALID_CURSOR, "{id}");
        }
    }
}

#[test]
fn foreign_kind_is_rejected_both_ways() {
    let outcome = encode_cursor(KIND_OUTCOME, &["out_01ABC"]);
    assert_eq!(
        decode_cursor(&outcome, KIND_RECOMMENDATION, 1),
        Err(InvalidCursor)
    );
    assert_eq!(
        decode_cursor(&outcome, KIND_OUTCOME, 1),
        Ok(vec!["out_01ABC".to_owned()])
    );
    assert_eq!(
        decode_cursor(&outcome, KIND_OUTCOME, 2),
        Err(InvalidCursor),
        "field count"
    );
}

#[test]
fn aggregate_cursors_parse_date_and_tier() {
    let position = AggregatePosition {
        as_of: NaiveDate::from_ymd_opt(2026, 7, 1).expect("date"),
        capability: "structured_extraction".to_owned(),
        tier: 3,
    };
    let token = encode_aggregate_cursor(&position);
    assert_eq!(
        token,
        "djF8YWdnfDIwMjYtMDctMDF8c3RydWN0dXJlZF9leHRyYWN0aW9ufDM"
    );
    assert_eq!(decode_aggregate_cursor(&token), Ok(position));

    let negative = encode_cursor(KIND_AGGREGATE, &["2026-07-01", "factual_qa", "-32768"]);
    assert_eq!(
        decode_aggregate_cursor(&negative).map(|position| position.tier),
        Ok(-32768)
    );
    for bad in [
        ["2026-02-30", "factual_qa", "3"],
        ["2026-7-01", "factual_qa", "3"],
        ["2026-07-01", "factual_qa", "32768"],
        ["2026-07-01", "factual_qa", "three"],
        ["2026-07-01", "factual_qa", "3.0"],
    ] {
        let token = encode_cursor(KIND_AGGREGATE, &bad);
        assert_eq!(
            decode_aggregate_cursor(&token),
            Err(InvalidCursor),
            "{bad:?}"
        );
    }
    let wrong_kind = encode_cursor(KIND_RECOMMENDATION, &["2026-07-01", "factual_qa", "3"]);
    assert_eq!(decode_aggregate_cursor(&wrong_kind), Err(InvalidCursor));
}
