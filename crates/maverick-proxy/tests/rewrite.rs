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

// ---- D14 settings and D29 normalization (bead 6.8) ----

use maverick_core::normalize::{ParamConstraints, WireFormat, parse_constraints};
use maverick_proxy::rewrite::{AttemptParams, Rewritten, rewrite_attempt};
use serde_json::{Map, json};

fn settings(value: Value) -> Map<String, Value> {
    value.as_object().expect("settings object").clone()
}

fn effort_constraint(allowed: &[&str]) -> ParamConstraints {
    parse_constraints(&json!({
        "reasoning_effort": {
            "allowed": allowed,
            "order": ["minimal", "low", "medium", "high", "xhigh", "max"],
        }
    }))
}

/// Rewrites and logs input, expected, and actual bytes on a mismatch.
fn attempt(
    body: &str,
    format: WireFormat,
    settings: Option<&Map<String, Value>>,
    constraints: &ParamConstraints,
) -> (String, bool) {
    let Rewritten {
        body: out,
        settings_applied,
    } = rewrite_attempt(
        body.as_bytes(),
        "m",
        &AttemptParams {
            format,
            settings,
            constraints,
        },
    )
    .unwrap_or_else(|e| panic!("rewrite of {body}: {e}"));
    (String::from_utf8(out).expect("utf-8"), settings_applied)
}

fn check(got: (String, bool), want_body: &str, want_applied: bool, input: &str) {
    assert_eq!(
        got.0, want_body,
        "input {input}\nexpected {want_body}\nactual   {}",
        got.0
    );
    assert_eq!(got.1, want_applied, "settings_applied for {input}");
}

/// Chat completions (OpenAI, and Google and xAI through the same wire format): the
/// `reasoning` setting is `reasoning_effort`; `thinking` has no chat field.
#[test]
fn settings_on_chat_completions() {
    let none = ParamConstraints::new();
    let s = settings(json!({"reasoning": "high", "thinking": true}));
    let input = r#"{"messages":[],"reasoning_effort":"low","temperature":0.50}"#;
    check(
        attempt(input, WireFormat::ChatCompletions, Some(&s), &none),
        r#"{"messages":[],"model":"m","reasoning_effort":"high","temperature":0.50}"#,
        true,
        input,
    );
    let input = r#"{"messages":[]}"#;
    check(
        attempt(input, WireFormat::ChatCompletions, Some(&s), &none),
        r#"{"messages":[],"model":"m","reasoning_effort":"high"}"#,
        true,
        input,
    );
    let only_thinking = settings(json!({"thinking": true}));
    check(
        attempt(
            input,
            WireFormat::ChatCompletions,
            Some(&only_thinking),
            &none,
        ),
        r#"{"messages":[],"model":"m"}"#,
        false,
        input,
    );
    let empty = Map::new();
    check(
        attempt(input, WireFormat::ChatCompletions, Some(&empty), &none),
        r#"{"messages":[],"model":"m"}"#,
        false,
        input,
    );
}

/// Anthropic Messages: `reasoning` is `output_config.effort` (the object is created, or
/// re-encoded with sorted keys and its siblings' raw bytes); `thinking` is the `thinking`
/// object.
#[test]
fn settings_on_messages() {
    let none = ParamConstraints::new();
    let s = settings(json!({"reasoning": "high", "thinking": true}));
    let input = r#"{"max_tokens":10,"output_config":{"z":0.50, "effort":"low"},"thinking":{"type":"disabled"}}"#;
    check(
        attempt(input, WireFormat::Messages, Some(&s), &none),
        r#"{"max_tokens":10,"model":"m","output_config":{"effort":"high","z":0.50},"thinking":{"type":"adaptive"}}"#,
        true,
        input,
    );
    let input = r#"{"max_tokens":10}"#;
    check(
        attempt(input, WireFormat::Messages, Some(&s), &none),
        r#"{"max_tokens":10,"model":"m","output_config":{"effort":"high"},"thinking":{"type":"adaptive"}}"#,
        true,
        input,
    );
    // A non-object `output_config` is left alone; the setting is not applied.
    let reasoning = settings(json!({"reasoning": "high"}));
    let input = r#"{"output_config":"custom"}"#;
    check(
        attempt(input, WireFormat::Messages, Some(&reasoning), &none),
        r#"{"model":"m","output_config":"custom"}"#,
        false,
        input,
    );
    // An explicit thinking object is sent as stored.
    let explicit = settings(json!({"thinking": {"type": "enabled", "budget_tokens": 2048}}));
    check(
        attempt(r#"{}"#, WireFormat::Messages, Some(&explicit), &none),
        r#"{"model":"m","thinking":{"budget_tokens":2048,"type":"enabled"}}"#,
        true,
        "{}",
    );
}

/// `?settings=caller`: the caller's values stay; only the model and normalization change.
#[test]
fn settings_caller_leaves_the_callers_values() {
    let input = r#"{"output_config":{"effort":"low"},"thinking":{"type":"disabled"}}"#;
    check(
        attempt(input, WireFormat::Messages, None, &ParamConstraints::new()),
        r#"{"model":"m","output_config":{"effort":"low"},"thinking":{"type":"disabled"}}"#,
        false,
        input,
    );
}

/// D29 with a catalog constraint: exact, nearest down, nearest up, and an unknown value.
/// The constraint replaces the Go mapping for its field.
#[test]
fn catalog_constraints_normalize_to_the_nearest_accepted_value() {
    let c = effort_constraint(&["low", "medium", "high"]);
    for (value, want) in [
        ("medium", "medium"),
        ("xhigh", "high"),
        ("minimal", "low"),
        ("turbo", "turbo"),
    ] {
        let input = format!(r#"{{"reasoning_effort":"{value}"}}"#);
        check(
            attempt(&input, WireFormat::ChatCompletions, None, &c),
            &format!(r#"{{"model":"m","reasoning_effort":"{want}"}}"#),
            false,
            &input,
        );
        let input = format!(r#"{{"output_config":{{"effort":"{value}", "b" : 1}}}}"#);
        let want_body = if value == want {
            // Unchanged: `output_config` is only compacted, its key order kept.
            format!(r#"{{"model":"m","output_config":{{"effort":"{want}","b":1}}}}"#)
        } else {
            format!(r#"{{"model":"m","output_config":{{"b":1,"effort":"{want}"}}}}"#)
        };
        check(
            attempt(&input, WireFormat::Messages, None, &c),
            &want_body,
            false,
            &input,
        );
    }
    // The model accepts `xhigh`: the Go mapping does not apply.
    let accepts = effort_constraint(&["high", "xhigh"]);
    let input = r#"{"reasoning_effort":"xhigh"}"#;
    check(
        attempt(input, WireFormat::ChatCompletions, None, &accepts),
        r#"{"model":"m","reasoning_effort":"xhigh"}"#,
        false,
        input,
    );
    // No constraint: the Go mappings.
    let none = ParamConstraints::new();
    check(
        attempt(input, WireFormat::ChatCompletions, None, &none),
        r#"{"model":"m","reasoning_effort":"high"}"#,
        false,
        input,
    );
    let input = r#"{"output_config":{"effort":"xhigh"}}"#;
    check(
        attempt(input, WireFormat::Messages, None, &none),
        r#"{"model":"m","output_config":{"effort":"max"}}"#,
        false,
        input,
    );
}

/// Normalization never adds a missing field, and settings are applied first, so the sent
/// value is always one the model accepts.
#[test]
fn settings_then_normalization_and_never_add() {
    let c = effort_constraint(&["low", "high"]);
    let input = r#"{"messages":[]}"#;
    check(
        attempt(input, WireFormat::ChatCompletions, None, &c),
        r#"{"messages":[],"model":"m"}"#,
        false,
        input,
    );
    let s = settings(json!({"reasoning": "xhigh"}));
    check(
        attempt(input, WireFormat::ChatCompletions, Some(&s), &c),
        r#"{"messages":[],"model":"m","reasoning_effort":"high"}"#,
        true,
        input,
    );
    check(
        attempt(
            r#"{}"#,
            WireFormat::Messages,
            Some(&s),
            &ParamConstraints::new(),
        ),
        r#"{"model":"m","output_config":{"effort":"max"}}"#,
        true,
        "{} (Go mapping after the setting)",
    );
}

/// Property: the rewrite never changes a body's parsed value except `model`, and a
/// rewritten body is a fixed point. A seeded generator keeps the run reproducible.
#[test]
fn compaction_never_changes_the_parsed_value() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for case in 0..2000 {
        let body = rng.body();
        for format in [WireFormat::ChatCompletions, WireFormat::Messages] {
            let (out, _) = attempt(&body, format, None, &ParamConstraints::new());
            let mut want: Value = serde_json::from_str(&body).expect("generated JSON parses");
            want["model"] = json!("m");
            let got: Value = serde_json::from_str(&out)
                .unwrap_or_else(|e| panic!("case {case}: output does not parse ({e}): {out}"));
            assert_eq!(got, want, "case {case}\ninput  {body}\noutput {out}");
            let (again, _) = attempt(&out, format, None, &ParamConstraints::new());
            assert_eq!(again, out, "case {case}: not a fixed point");
            // HTML characters and U+2028 only ever appear as `\u` escapes.
            assert!(
                !out.contains(['<', '>', '&', '\u{2028}']),
                "case {case}: an unescaped HTML character: {out}"
            );
        }
    }
}

/// xorshift64: enough randomness for body shapes, no dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> usize {
        usize::try_from(self.next() % n).expect("small")
    }

    fn ws(&mut self) -> &'static str {
        [" ", "", "\n\t", "  ", ""][self.below(5)]
    }

    fn string(&mut self) -> String {
        const PIECES: [&str; 10] = [
            "a", "<b>", "&", "\\\"", "\\\\", "\\u00e9", "é", "\u{2028}", "x y", "\\n",
        ];
        let mut out = String::from("\"");
        for _ in 0..self.below(4) {
            out.push_str(PIECES[self.below(10)]);
        }
        out.push('"');
        out
    }

    fn value(&mut self, depth: u32) -> String {
        let pick = if depth > 2 {
            self.below(4)
        } else {
            self.below(6)
        };
        match pick {
            0 => self.string(),
            1 => ["0.50", "1e2", "-3", "0", "12.25E-1"][self.below(5)].to_owned(),
            2 => ["true", "false", "null"][self.below(3)].to_owned(),
            3 => self.string(),
            4 => {
                let items: Vec<String> =
                    (0..self.below(4)).map(|_| self.value(depth + 1)).collect();
                let ws = self.ws();
                format!("[{ws}{}{ws}]", items.join(&format!("{ws},{}", self.ws())))
            }
            _ => self.object(depth + 1, false),
        }
    }

    fn object(&mut self, depth: u32, top: bool) -> String {
        const KEYS: [&str; 6] = ["a", "b", "<k>", "é", "z z", "B"];
        let mut members = Vec::new();
        for _ in 0..self.below(if top { 6 } else { 4 }) {
            let key = KEYS[self.below(6)];
            let value = self.value(depth);
            let (a, b) = (self.ws(), self.ws());
            members.push(format!("\"{key}\"{a}:{b}{value}"));
        }
        let ws = self.ws();
        format!("{{{ws}{}{ws}}}", members.join(","))
    }

    fn body(&mut self) -> String {
        self.object(0, true)
    }
}
