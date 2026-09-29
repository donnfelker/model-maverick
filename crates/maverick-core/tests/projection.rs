//! Workload projection `p1` (D5): the replaced Go rows, golden scenarios, and the
//! conformance vectors the four SDK cores run.
//!
//! - `d5_extract_last_user_text_rows`: every `unit.json#extract_last_user_text-*` row with
//!   its `p1` expected output, computed by hand. Go's last-user-text rule is replaced (D5).
//! - `scenario_goldens`: `insta` goldens for single-turn, multi-turn, system and developer
//!   instructions, tool use with results, response-format schemas, binary content, the
//!   conversation bound, and oversize truncation, on both request surfaces.
//! - `conformance_vectors_are_current` / `conformance_vectors_replay`: the same cases as
//!   `crates/sdk-conformance/vectors/projection.p1.json`. Regenerate the file with
//!   `MAVERICK_UPDATE_VECTORS=1 cargo test -p maverick-core --test projection`.
//!
//! Every mismatch prints the input, the expected and actual projection, and the first
//! differing line. Long runs of one character print as `⟦c×n⟧`.

use std::fmt::Write as _;
use std::path::PathBuf;

use maverick_core::projection::{
    self, DeclaredWorkload, HEAD_CHARS, PROJECTION_VERSION, Projection, TAIL_CHARS,
    TRUNCATION_MARKER, WorkloadHeaderError, parse_workload_header,
};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------------------
// Texts built from segments, so generated bodies stay small in the vectors file.

#[derive(Debug, Clone, PartialEq, Eq)]
enum Seg {
    Text(String),
    Repeat(char, usize),
}

fn text(value: &str) -> Seg {
    Seg::Text(value.to_owned())
}

fn build(segments: &[Seg]) -> String {
    let mut out = String::new();
    for segment in segments {
        match segment {
            Seg::Text(value) => out.push_str(value),
            Seg::Repeat(ch, count) => out.extend(std::iter::repeat_n(*ch, *count)),
        }
    }
    out
}

/// Runs of at least this many equal characters become `Repeat` segments.
const RUN: usize = 32;

fn compress(value: &str) -> Vec<Seg> {
    let chars: Vec<char> = value.chars().collect();
    let mut segments = Vec::new();
    let mut literal = String::new();
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        let run = chars[index..]
            .iter()
            .take_while(|next| **next == ch)
            .count();
        if run >= RUN {
            if !literal.is_empty() {
                segments.push(Seg::Text(std::mem::take(&mut literal)));
            }
            segments.push(Seg::Repeat(ch, run));
        } else {
            literal.extend(std::iter::repeat_n(ch, run));
        }
        index += run;
    }
    if !literal.is_empty() {
        segments.push(Seg::Text(literal));
    }
    segments
}

/// A readable form for failure messages and snapshots.
fn show(value: &str) -> String {
    compress(value)
        .iter()
        .map(|segment| match segment {
            Seg::Text(literal) => literal.clone(),
            Seg::Repeat(ch, count) => format!("⟦{ch}×{count}⟧"),
        })
        .collect()
}

fn render(projection: Option<&Projection>) -> String {
    match projection {
        None => "<none: declared fallback>".to_owned(),
        Some(projection) => format!(
            "truncated: {}\n---\n{}",
            projection.truncated,
            show(&projection.text)
        ),
    }
}

/// Compare a projection with its expectation and explain any mismatch.
fn check(id: &str, body: &str, want: Option<&Projection>) {
    let got = projection::project(body.as_bytes());
    if got.as_ref() == want {
        return;
    }
    let (want_text, got_text) = (render(want), render(got.as_ref()));
    let first_diff = want_text
        .lines()
        .zip(got_text.lines())
        .position(|(left, right)| left != right)
        .map_or_else(
            || "one side has extra lines".to_owned(),
            |line| format!("first difference at rendered line {}", line + 1),
        );
    panic!(
        "projection mismatch for {id} ({first_diff})\n\
         == input body ==\n{}\n== expected ==\n{want_text}\n== actual ==\n{got_text}\n",
        show(body)
    );
}

// ---------------------------------------------------------------------------------------
// D5: the Go extractLastUserText rows, with p1 expected outputs.

/// Hand-computed `p1` projections for the recorded rows. `None` is "no projection"; the
/// caller then sends the declared fallback workload, as Go did for `ok=false`.
const D5_EXPECTED: [(&str, Option<&str>); 21] = [
    (
        "extract_last_user_text-0",
        Some(
            "[system]\nsys\n\n[conversation]\nuser: first question\nassistant: answer\nuser: second question, the real one",
        ),
    ),
    (
        "extract_last_user_text-1",
        Some("[conversation]\nuser: part A \npart B"),
    ),
    // Go: ok=false. p1 keeps the system text and the assistant turn.
    (
        "extract_last_user_text-2",
        Some("[system]\nsys\n\n[conversation]\nassistant: hi"),
    ),
    ("extract_last_user_text-3", None),
    ("extract_last_user_text-4", None),
    ("extract_last_user_text-5", None),
    ("extract_last_user_text-6", None),
    // Go: ok=false (never looks past the last user message). p1 drops the empty turn.
    (
        "extract_last_user_text-7",
        Some("[conversation]\nuser: earlier"),
    ),
    (
        "extract_last_user_text-8",
        Some("[conversation]\nuser: q\nassistant: a"),
    ),
    ("extract_last_user_text-9", None),
    // Go matched keys case-insensitively and returned "k". p1 keys are exact.
    ("extract_last_user_text-10", None),
    ("extract_last_user_text-11", None),
    ("extract_last_user_text-12", None),
    ("extract_last_user_text-13", Some("[conversation]\nuser: z")),
    // Go voided the whole parse on one bad element. p1 ignores the element.
    (
        "extract_last_user_text-14",
        Some("[conversation]\nuser: ok"),
    ),
    ("extract_last_user_text-15", None),
    (
        "extract_last_user_text-16",
        Some("[conversation]\nuser:    "),
    ),
    ("extract_last_user_text-17", None),
    // Go ignored tool_result blocks. p1 makes them tool turns.
    ("extract_last_user_text-18", Some("[conversation]\ntool: r")),
    (
        "extract_last_user_text-19",
        Some("[conversation]\nuser: a\u{fffd}b"),
    ),
    (
        "extract_last_user_text-20",
        Some("[conversation]\nuser: \u{e9}\u{1f600}"),
    ),
];

/// `[conversation]\nuser: ` is 21 scalar values.
const USER_PREFIX: &str = "[conversation]\nuser: ";

fn generated_user_body(ch: char, count: usize) -> Vec<Seg> {
    vec![
        text("{\"messages\":[{\"role\":\"user\",\"content\":\""),
        Seg::Repeat(ch, count),
        text("\"}]}"),
    ]
}

/// `USER_PREFIX` + `ch` * `count`, cut to head + marker + tail.
fn truncated_user_text(ch: char) -> Vec<Seg> {
    vec![
        text(USER_PREFIX),
        Seg::Repeat(ch, HEAD_CHARS - USER_PREFIX.chars().count()),
        text(TRUNCATION_MARKER),
        Seg::Repeat(ch, TAIL_CHARS),
    ]
}

struct Case {
    id: String,
    note: String,
    body: Vec<Seg>,
    expect: Option<(Vec<Seg>, bool)>,
}

impl Case {
    fn expected(&self) -> Option<Projection> {
        self.expect
            .as_ref()
            .map(|(segments, truncated)| Projection {
                text: build(segments),
                truncated: *truncated,
            })
    }
}

fn d5_cases() -> Vec<Case> {
    let rows = parity_support::cases("fixtures/proxy/unit.json#extract_last_user_text")
        .expect("load unit.json extract_last_user_text rows");
    assert_eq!(rows.len(), 23, "recorded extract_last_user_text rows");
    rows.iter()
        .map(|row| {
            let id = row["id"].as_str().expect("row id").to_owned();
            let note = format!("D5 replaces Go row: {}", row["name"].as_str().unwrap_or(""));
            let (body, expect) = match id.as_str() {
                "extract_last_user_text-21" => (
                    generated_user_body('a', 101_000),
                    Some((truncated_user_text('a'), true)),
                ),
                "extract_last_user_text-22" => (
                    generated_user_body('\u{e9}', 100_005),
                    Some((truncated_user_text('\u{e9}'), true)),
                ),
                _ => {
                    let body = row["body"].as_str().expect("row body");
                    let want = D5_EXPECTED
                        .iter()
                        .find(|(row_id, _)| *row_id == id)
                        .unwrap_or_else(|| panic!("no hand-computed p1 expectation for {id}"))
                        .1;
                    (vec![text(body)], want.map(|want| (vec![text(want)], false)))
                }
            };
            Case {
                id,
                note,
                body,
                expect,
            }
        })
        .collect()
}

/// DIVERGENCE: D5. Go's `extractLastUserText` (last user message only, 100,000-rune
/// head cap) is replaced by projection p1. Each row asserts the Rust output.
// parity: proxy.TestExtractLastUserText
// parity: proxy.TestExtractLastUserText_Caps
// parity: proxy.TestExtractLastUserText/string_content,_last_user_message_wins
// parity: proxy.TestExtractLastUserText/array_content,_concatenates_text_parts_only
// parity: proxy.TestExtractLastUserText/no_user_message_at_all
// parity: proxy.TestExtractLastUserText/no_messages_field
// parity: proxy.TestExtractLastUserText/last_user_message_has_only_non-text_parts
// parity: proxy.TestExtractLastUserText/last_user_message_content_is_empty_string
// parity: proxy.TestExtractLastUserText/malformed_JSON
#[test]
fn d5_extract_last_user_text_rows() {
    let d5 = parity_support::divergence!("D5");
    assert_eq!(d5.id(), "D5");
    for case in d5_cases() {
        check(&case.id, &build(&case.body), case.expected().as_ref());
    }
}

// ---------------------------------------------------------------------------------------
// Golden scenarios (both surfaces).

fn scenario(id: &str, note: &str, body: &Value) -> (String, String, String) {
    (
        id.to_owned(),
        note.to_owned(),
        serde_json::to_string(body).expect("body json"),
    )
}

fn scenarios() -> Vec<(String, String, String)> {
    let long_system = "a".repeat(110_000);
    let long_question = format!("Q: {}", "b".repeat(100_500));
    let bulky = "c".repeat(6_000);
    vec![
        scenario(
            "chat_single_turn",
            "chat: one user message",
            &json!({"model": "maverick/auto", "messages": [
                {"role": "user", "content": "Extract the parties and the effective date from this contract."}
            ]}),
        ),
        scenario(
            "chat_multi_turn",
            "chat: earlier turns are kept, newest last",
            &json!({"model": "maverick/auto", "messages": [
                {"role": "user", "content": "Write a Python function that parses ISO 8601 dates."},
                {"role": "assistant", "content": "Here is a version that uses datetime.fromisoformat."},
                {"role": "user", "content": "It fails on the Z suffix. Fix it and add tests."}
            ]}),
        ),
        scenario(
            "chat_system_and_user",
            "chat: system instructions go to [system]",
            &json!({"model": "maverick/quality", "messages": [
                {"role": "system", "content": "You are a careful legal analyst. Cite clause numbers."},
                {"role": "user", "content": "Is the indemnity clause mutual?"}
            ]}),
        ),
        scenario(
            "chat_developer_role",
            "chat: developer messages, and system messages anywhere, go to [system] in order",
            &json!({"model": "maverick/auto", "messages": [
                {"role": "developer", "content": [{"type": "text", "text": "Answer in French."}]},
                {"role": "user", "content": "Summarize the release notes."},
                {"role": "system", "content": "Keep it under 100 words."}
            ]}),
        ),
        scenario(
            "chat_tool_use",
            "chat: tools, assistant tool_calls (arguments in JCS), tool results",
            &json!({"model": "maverick/auto",
            "tools": [
                {"type": "function", "function": {"name": "get_weather", "description": "Current weather for a city.", "parameters": {"type": "object"}}},
                {"type": "function", "function": {"name": "search_docs", "parameters": {"type": "object"}}}
            ],
            "messages": [
                {"role": "system", "content": "You are a travel assistant."},
                {"role": "user", "content": "Do I need an umbrella in Paris today?"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {"name": "get_weather", "arguments": "{\"units\": \"metric\", \"city\": \"Paris\"}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "{\"rain_probability\": 0.8}"},
                {"role": "assistant", "content": "Yes, rain is likely."},
                {"role": "user", "content": "And tomorrow?"}
            ]}),
        ),
        scenario(
            "chat_response_format_schema",
            "chat: json_schema response_format; the schema is written in JCS (sorted keys, ES numbers)",
            &json!({"model": "maverick/auto",
                "response_format": {"type": "json_schema", "json_schema": {
                    "name": "invoice",
                    "description": "Fields read from an invoice.",
                    "strict": true,
                    "schema": {"type": "object", "required": ["total", "currency"], "properties": {
                        "total": {"type": "number", "minimum": 0, "maximum": 1.5e6},
                        "currency": {"type": "string", "enum": ["EUR", "USD"]}
                    }}
                }},
                "messages": [{"role": "user", "content": "Read the invoice below.\nTotal: 1,200.00 EUR"}]}),
        ),
        scenario(
            "chat_binary_parts_excluded",
            "chat: image, audio, and file parts contribute nothing",
            &json!({"model": "maverick/auto", "messages": [{"role": "user", "content": [
                {"type": "text", "text": "What is in these?"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw0KGgo="}},
                {"type": "input_audio", "input_audio": {"data": "UklGRiQAAABXQVZF", "format": "wav"}},
                {"type": "file", "file": {"file_data": "JVBERi0xLjQK", "filename": "a.pdf"}},
                {"type": "text", "text": "Describe each briefly."}
            ]}]}),
        ),
        scenario(
            "chat_oversize_truncation",
            "chat: a 110,000-character system prompt; the head keeps the system text, the tail keeps the question",
            &json!({"model": "maverick/auto", "messages": [
                {"role": "system", "content": long_system},
                {"role": "user", "content": "What is this?"}
            ]}),
        ),
        scenario(
            "chat_conversation_bound",
            "chat: older turns past the 16,000 budget are dropped; the walk stops at the first misfit",
            &json!({"model": "maverick/auto", "messages": [
                {"role": "user", "content": "oldest question"},
                {"role": "assistant", "content": bulky},
                {"role": "user", "content": bulky},
                {"role": "assistant", "content": bulky},
                {"role": "user", "content": "latest question"}
            ]}),
        ),
        scenario(
            "messages_single_turn",
            "messages: one user message",
            &json!({"model": "maverick/cost", "max_tokens": 1024, "messages": [
                {"role": "user", "content": "Translate to German: Good morning."}
            ]}),
        ),
        scenario(
            "messages_system_blocks",
            "messages: top-level system as text blocks",
            &json!({"model": "maverick/auto", "max_tokens": 1024,
                "system": [
                    {"type": "text", "text": "You are a contracts analyst."},
                    {"type": "text", "text": "Cite clauses.", "cache_control": {"type": "ephemeral"}}
                ],
                "messages": [{"role": "user", "content": "Which party can terminate early?"}]}),
        ),
        scenario(
            "messages_tool_use",
            "messages: tool_use input in JCS; tool_result blocks become tool turns before the user text",
            &json!({"model": "maverick/auto", "max_tokens": 1024,
            "tools": [{"name": "lookup_order", "description": "Look up an order by id.", "input_schema": {"type": "object"}}],
            "messages": [
                {"role": "user", "content": "Where is order A-17?"},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "Let me look that up."},
                    {"type": "tool_use", "id": "toolu_1", "name": "lookup_order", "input": {"verbose": true, "order_id": "A-17"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": [{"type": "text", "text": "status: shipped"}]},
                    {"type": "text", "text": "When will it arrive?"}
                ]}
            ]}),
        ),
        scenario(
            "messages_output_config_format",
            "messages: output_config.format json_schema; output_config.effort is ignored",
            &json!({"model": "maverick/auto", "max_tokens": 1024,
                "output_config": {"effort": "high", "format": {"type": "json_schema", "schema": {
                    "type": "object", "properties": {"sentiment": {"type": "string", "enum": ["positive", "negative"]}, "score": {"type": "number"}}
                }}},
                "messages": [{"role": "user", "content": "Rate this review: great product."}]}),
        ),
        scenario(
            "messages_binary_excluded",
            "messages: image and document blocks and thinking blocks contribute nothing",
            &json!({"model": "maverick/auto", "max_tokens": 1024, "messages": [
                {"role": "user", "content": [
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo="}},
                    {"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "JVBERi0xLjQK"}},
                    {"type": "text", "text": "Summarize the attached report."}
                ]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "private reasoning", "signature": "c2ln"},
                    {"type": "text", "text": "Here is a summary."}
                ]},
                {"role": "user", "content": "Shorter, please."}
            ]}),
        ),
        scenario(
            "messages_oversize_truncation",
            "messages: one oversize user turn is kept in full, then cut to head + marker + tail",
            &json!({"model": "maverick/auto", "max_tokens": 1024, "messages": [
                {"role": "user", "content": long_question}
            ]}),
        ),
        scenario(
            "messages_prefill_keeps_user",
            "messages: an assistant prefill after the question does not push the question out",
            &json!({"model": "maverick/auto", "max_tokens": 1024, "messages": [
                {"role": "user", "content": "Classify this ticket as bug, feature, or question: the export button does nothing."},
                {"role": "assistant", "content": "{"}
            ]}),
        ),
    ]
}

#[test]
fn scenario_goldens() {
    for (id, note, body) in scenarios() {
        let projection = projection::project(body.as_bytes());
        insta::with_settings!({ description => note.as_str(), omit_expression => true }, {
            insta::assert_snapshot!(id.as_str(), render(projection.as_ref()));
        });
    }
}

#[test]
fn truncated_projections_stay_over_the_engine_cap() {
    for (id, _, body) in scenarios() {
        let Some(projection) = projection::project(body.as_bytes()) else {
            continue;
        };
        let length = projection.text.chars().count();
        if projection.truncated {
            assert_eq!(
                length,
                HEAD_CHARS + TRUNCATION_MARKER.chars().count() + TAIL_CHARS,
                "{id}"
            );
        } else {
            assert!(length <= projection::MAX_CHARS, "{id}");
        }
    }
}

// ---------------------------------------------------------------------------------------
// Conformance vectors.

fn vector_cases() -> Vec<Case> {
    let mut cases: Vec<Case> = scenarios()
        .into_iter()
        .map(|(id, note, body)| {
            let expect = projection::project(body.as_bytes())
                .map(|projection| (compress(&projection.text), projection.truncated));
            Case {
                id,
                note,
                body: compress(&body),
                expect,
            }
        })
        .collect();
    cases.extend(d5_cases());
    cases
}

const HEADER_CASES: [&str; 12] = [
    "code_generation",
    "code_generation:4",
    " \tstructured_extraction:2\t ",
    "factual_qa:9",
    "translation_multilingual:007",
    "",
    ":3",
    "code_generation:",
    "code_generation:hard",
    "code_generation:+3",
    "code_generation: 3",
    "code_generation:99999999999",
];

fn json_string(value: &str) -> String {
    serde_json::to_string(value).expect("string json")
}

fn segments_json(segments: &[Seg]) -> String {
    let items: Vec<String> = segments
        .iter()
        .map(|segment| match segment {
            Seg::Text(value) => format!("{{\"text\": {}}}", json_string(value)),
            Seg::Repeat(ch, count) => {
                format!(
                    "{{\"repeat\": {}, \"count\": {count}}}",
                    json_string(&ch.to_string())
                )
            }
        })
        .collect();
    format!("[{}]", items.join(", "))
}

/// The vectors document, written with a fixed layout and member order.
fn render_vectors() -> String {
    let mut out = String::new();
    out.push_str("{\n");
    let _ = writeln!(
        out,
        "  \"projection_version\": {},",
        json_string(PROJECTION_VERSION)
    );
    out.push_str("  \"source\": \"spec v1 §8.1 'Projection format p1'; crates/maverick-core/src/projection.rs; generated by crates/maverick-core/tests/projection.rs\",\n");
    out.push_str("  \"text_encoding\": \"A text is an array of segments, concatenated. {\\\"text\\\": s} is s. {\\\"repeat\\\": c, \\\"count\\\": n} is the character c repeated n times.\",\n");
    out.push_str("  \"how_to_run\": \"For each case, build the body text, parse it as JSON (or pass the bytes), and project it. expect null means no projection (send the declared fallback). Otherwise the projection text and the truncated flag must match exactly. For each workload_header case, parse the value; compare with expect or expect_error.\",\n");
    out.push_str("  \"cases\": [\n");
    let cases = vector_cases();
    for (index, case) in cases.iter().enumerate() {
        out.push_str("    {\n");
        let _ = writeln!(out, "      \"id\": {},", json_string(&case.id));
        let _ = writeln!(out, "      \"note\": {},", json_string(&case.note));
        let _ = writeln!(out, "      \"body\": {},", segments_json(&case.body));
        match &case.expect {
            None => out.push_str("      \"expect\": null\n"),
            Some((segments, truncated)) => {
                let _ = writeln!(
                    out,
                    "      \"expect\": {{\"truncated\": {truncated}, \"text\": {}}}",
                    segments_json(segments)
                );
            }
        }
        out.push_str(if index + 1 == cases.len() {
            "    }\n"
        } else {
            "    },\n"
        });
    }
    out.push_str("  ],\n");
    out.push_str("  \"workload_header\": [\n");
    for (index, value) in HEADER_CASES.iter().enumerate() {
        let outcome = match parse_workload_header(value) {
            Ok(DeclaredWorkload {
                capability,
                difficulty,
            }) => {
                let difficulty =
                    difficulty.map_or_else(|| "null".to_owned(), |tier| tier.to_string());
                format!(
                    "\"expect\": {{\"capability\": {}, \"difficulty\": {difficulty}}}",
                    json_string(&capability)
                )
            }
            Err(error) => format!("\"expect_error\": {}", json_string(&error.to_string())),
        };
        let comma = if index + 1 == HEADER_CASES.len() {
            ""
        } else {
            ","
        };
        let _ = writeln!(
            out,
            "    {{\"value\": {}, {outcome}}}{comma}",
            json_string(value)
        );
    }
    out.push_str("  ]\n}\n");
    out
}

fn vectors_path() -> PathBuf {
    parity_support::repo_root()
        .expect("repo root")
        .join("crates/sdk-conformance/vectors/projection.p1.json")
}

#[test]
fn conformance_vectors_are_current() {
    let path = vectors_path();
    let want = render_vectors();
    if std::env::var_os("MAVERICK_UPDATE_VECTORS").is_some() {
        std::fs::create_dir_all(path.parent().expect("vectors dir")).expect("create vectors dir");
        std::fs::write(&path, &want).expect("write vectors");
        return;
    }
    let have = std::fs::read_to_string(&path).unwrap_or_default();
    if have != want {
        let line = want
            .lines()
            .zip(have.lines())
            .position(|(left, right)| left != right)
            .map_or(0, |line| line + 1);
        panic!(
            "{} is stale (first difference near line {line}). Review the projection change, \
             bump PROJECTION_VERSION if the output bytes changed, then run \
             MAVERICK_UPDATE_VECTORS=1 cargo test -p maverick-core --test projection",
            path.display()
        );
    }
}

fn parse_segments(value: &Value) -> Vec<Seg> {
    value
        .as_array()
        .expect("segments array")
        .iter()
        .map(|segment| {
            if let Some(literal) = segment.get("text") {
                Seg::Text(literal.as_str().expect("text segment").to_owned())
            } else {
                let ch = segment["repeat"].as_str().expect("repeat char");
                let mut chars = ch.chars();
                let (Some(ch), None) = (chars.next(), chars.next()) else {
                    panic!("repeat must be one character: {ch:?}");
                };
                let count = segment["count"].as_u64().expect("repeat count");
                Seg::Repeat(ch, usize::try_from(count).expect("count fits"))
            }
        })
        .collect()
}

/// Run the committed vectors the way an SDK core's conformance runner does.
#[test]
fn conformance_vectors_replay() {
    let raw = std::fs::read_to_string(vectors_path()).expect("read vectors (generate them first)");
    let doc: Value = serde_json::from_str(&raw).expect("vectors json");
    assert_eq!(doc["projection_version"], PROJECTION_VERSION);

    let cases = doc["cases"].as_array().expect("cases");
    assert!(cases.len() >= 39, "scenario and D5 cases");
    for case in cases {
        let id = case["id"].as_str().expect("case id");
        let body = build(&parse_segments(&case["body"]));
        let want = match &case["expect"] {
            Value::Null => None,
            expect => Some(Projection {
                text: build(&parse_segments(&expect["text"])),
                truncated: expect["truncated"].as_bool().expect("truncated flag"),
            }),
        };
        check(id, &body, want.as_ref());
    }

    for case in doc["workload_header"].as_array().expect("workload_header") {
        let value = case["value"].as_str().expect("header value");
        match parse_workload_header(value) {
            Ok(declared) => {
                let expect = &case["expect"];
                assert_eq!(
                    expect["capability"],
                    declared.capability.as_str(),
                    "{value:?}"
                );
                assert_eq!(
                    expect["difficulty"].as_u64(),
                    declared.difficulty.map(u64::from),
                    "{value:?}"
                );
            }
            Err(error) => {
                let expected = case["expect_error"].as_str().expect("expect_error");
                assert_eq!(error.to_string(), expected, "{value:?}");
                assert!(matches!(
                    error,
                    WorkloadHeaderError::MissingCapability | WorkloadHeaderError::InvalidTier
                ));
            }
        }
    }
}
