//! Go `rewriteModel` (bead 6.7): every `unit.json#rewrite_model` row, byte-exact.

use maverick_proxy::rewrite::{Normalizer, NotAnObject, rewrite_model};
use serde_json::Value;

fn normalizer(name: &str) -> Normalizer {
    match name {
        "none" => Normalizer::None,
        "chat" => Normalizer::Chat,
        "messages" => Normalizer::Messages,
        other => panic!("unknown normalizer {other}"),
    }
}

/// Every row: the output body bytes, or the error that skips the attempt.
// parity: proxy.TestRewriteModel
// parity: proxy.TestEffortNormalization_LeavesEverythingElseAlone
// parity: proxy.TestEffortNormalization_LeavesEverythingElseAlone/messages:_effort_high_untouched
// parity: proxy.TestEffortNormalization_LeavesEverythingElseAlone/messages:_absent_output_config_stays_absent
// parity: proxy.TestEffortNormalization_LeavesEverythingElseAlone/chat:_reasoning_effort_medium_untouched
// parity: proxy.TestEffortNormalization_LeavesEverythingElseAlone/chat:_absent_reasoning_effort_stays_absent
#[test]
fn rewrite_model_rows() {
    let unit = parity_support::fixture("proxy", "unit.json").expect("unit.json");
    let rows = unit["rewrite_model"]
        .as_array()
        .expect("rewrite_model rows");
    assert_eq!(rows.len(), 19);
    for row in rows {
        let id = row["id"].as_str().expect("id");
        let got = rewrite_model(
            row["input_body"].as_str().expect("input_body").as_bytes(),
            row["model"].as_str().expect("model"),
            normalizer(row["normalizer"].as_str().expect("normalizer")),
        );
        if row.get("error").and_then(Value::as_bool) == Some(true) {
            assert_eq!(got, Err(NotAnObject), "{id}");
        } else {
            let got = String::from_utf8(got.expect(id)).expect("utf-8");
            assert_eq!(
                got,
                row["output_body"].as_str().expect("output_body"),
                "{id}: {}",
                row["name"]
            );
        }
    }
}

/// Invalid UTF-8 becomes U+FFFD, as the proxy's model decode does.
#[test]
fn invalid_utf8_is_replaced() {
    let got = rewrite_model(b"{\"s\":\"a\xffb\"}", "m", Normalizer::None).expect("rewrite");
    assert_eq!(got, "{\"model\":\"m\",\"s\":\"a\u{fffd}b\"}".as_bytes());
}

/// Messages normalization fires only for an object whose `effort` is the string `xhigh`,
/// including an escaped spelling of it. The re-encoded `output_config` is itself compacted
/// and HTML-escaped.
#[test]
fn messages_normalization_edges() {
    let rewrite = |body: &str| {
        String::from_utf8(
            rewrite_model(body.as_bytes(), "c", Normalizer::Messages).expect("rewrite"),
        )
        .expect("utf-8")
    };
    assert_eq!(
        rewrite("{\"output_config\":{\"effort\":\"x\\u0068igh\",\"z\":\"<b>\",\"a\":[ 1 ]}}"),
        "{\"model\":\"c\",\"output_config\":{\"a\":[1],\"effort\":\"max\",\"z\":\"\\u003cb\\u003e\"}}"
    );
    for untouched in [
        r#"{"output_config":{"effort":"max"}}"#,
        r#"{"output_config":{"effort":1}}"#,
        r#"{"output_config":[{"effort":"xhigh"}]}"#,
        r#"{"output_config":"xhigh"}"#,
        r#"{"output_config":{"Effort":"xhigh"}}"#,
    ] {
        let want = untouched.replacen('{', r#"{"model":"c","#, 1);
        assert_eq!(rewrite(untouched), want, "{untouched}");
    }
    // Chat: a JSON string only, exact case.
    let chat = |body: &str| {
        String::from_utf8(rewrite_model(body.as_bytes(), "g", Normalizer::Chat).expect("rewrite"))
            .expect("utf-8")
    };
    assert_eq!(
        chat("{\"reasoning_effort\":\"x\\u0068igh\"}"),
        r#"{"model":"g","reasoning_effort":"high"}"#
    );
    assert_eq!(
        chat(r#"{"reasoning_effort":["xhigh"]}"#),
        r#"{"model":"g","reasoning_effort":["xhigh"]}"#
    );
}
