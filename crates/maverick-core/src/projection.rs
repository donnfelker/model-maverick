//! The versioned workload projection (D5).
//!
//! The proxy and every SDK core turn a chat request into the prompt-form workload
//! the engine classifies. This module is the one definition of that projection. It
//! replaces the Go proxy's `extractLastUserText` (last user message text only; reference
//! part 6 §6). See spec v1 §8.1 "Workload projection" and "Projection format p1", and
//! `parity/DIVERGENCES.md` D5.
//!
//! The caller sends [`PROJECTION_VERSION`] as `workload.projection_version`; the engine
//! folds it into the classification cache key (spec §8.3). Any change to the output
//! bytes is a version bump.
//!
//! # Format `p1`
//!
//! The input is the request body as JSON: an OpenAI chat-completions body or an Anthropic
//! Messages body. The same rules read both shapes, so the caller does not say which one
//! it has. A body that is not a JSON object projects to nothing. Keys match exactly
//! (case sensitive). A value of an unexpected JSON type is ignored.
//!
//! The **text of a content value** is the string itself, or, for an array, the `text` of
//! each element whose `type` is exactly `text`, in order, joined with `"\n"`. Empty pieces
//! are dropped. Nothing else contributes: images, audio, files, documents, base64 data,
//! and thinking blocks are excluded.
//!
//! Sections, in this order. Each is a header line followed by its lines. One blank line
//! separates sections. An empty section is omitted. There is no trailing newline.
//!
//! 1. `[system]`: the top-level `system` value (Messages), then the text of every message
//!    whose `role` is `system` or `developer` (chat), one entry each.
//! 2. `[response_format]`: the first object found at `response_format` (chat),
//!    `output_config.format`, or `output_format` (Messages), when its `type` is a
//!    non-empty string other than `text`. Lines: the type; `name: <name>` and
//!    `description: <description>` when non-empty strings; `schema: <schema>` when a
//!    schema is present. Chat nests these under `json_schema`.
//! 3. `[tools]`: one line per element of `tools` with a non-empty string `name`, read from
//!    the element's `function` object, else its `custom` object, else the element itself:
//!    `<name>`, or `<name>: <description>` when the description is a non-empty string.
//! 4. `[conversation]`: one line per turn, `user: `, `assistant: `, or `tool: ` followed by
//!    the turn text.
//!    - `user` message: its text as one `user` turn. In an array, each `tool_result`
//!      element first ends the pending `user` text, then becomes a `tool` turn with the
//!      text of its `content`.
//!    - `assistant` message: one `assistant` turn. Its pieces are the `text` elements and
//!      the `tool_use` elements in order, then each `tool_calls` element. A tool call is
//!      the piece `tool_call <name> <arguments>`. `tool_use.input` is written as JCS.
//!      `tool_calls[].function.arguments` (or `custom.input`) is a string; it is written
//!      as JCS when it parses as JSON, else as is. Missing arguments are left out.
//!    - `tool` message: its text as one `tool` turn.
//!    - A turn whose text is empty is dropped. Other roles are ignored.
//!
//!    The section keeps the most recent turns. The last `user` turn (or the last turn when
//!    there is none) is always kept in full. Then, newest first, each other turn is kept
//!    while the total cost stays within [`CONVERSATION_BUDGET`]; a turn costs its length
//!    plus one. The walk stops at the first turn that does not fit. Kept turns stay in
//!    their original order.
//!
//! Lengths are counted in Unicode scalar values. Embedded JSON values are written in
//! RFC 8785 canonical form (JCS). When the assembled text is empty there is no projection
//! and the caller uses the declared fallback workload. When it is longer than
//! [`MAX_CHARS`], it becomes the first [`HEAD_CHARS`], then [`TRUNCATION_MARKER`], then
//! the last [`TAIL_CHARS`]. The marker keeps the text over the engine's
//! `classifier.max_input_chars` (default 100,000), so the engine makes the same head and
//! tail cut, caps confidence at 0.8, and records `truncated: true` (spec §8.3).
//!
//! # Declared override
//!
//! [`parse_workload_header`] reads `X-Maverick-Workload: <capability>[:<tier>]`. When
//! the header is present the caller sends the declared form instead of a projection.

use serde_json::{Map, Value};

mod jcs;

/// The projection format version, sent as `workload.projection_version`.
pub const PROJECTION_VERSION: &str = "p1";

/// The conversation section budget, in Unicode scalar values.
pub const CONVERSATION_BUDGET: usize = 16_000;

/// The longest projection sent without truncation, in Unicode scalar values.
pub const MAX_CHARS: usize = 100_000;

/// Scalar values kept from the start of an oversize projection.
pub const HEAD_CHARS: usize = 80_000;

/// Scalar values kept from the end of an oversize projection.
pub const TAIL_CHARS: usize = 20_000;

/// The text placed between the head and the tail of a truncated projection.
pub const TRUNCATION_MARKER: &str = "\n[...]\n";

/// The declared-workload override header.
pub const WORKLOAD_HEADER: &str = "X-Maverick-Workload";

/// A workload projection: the prompt-form text the engine classifies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Projection {
    /// The projection text.
    pub text: String,
    /// True when the text was cut to head + marker + tail.
    pub truncated: bool,
}

/// Project a raw request body. Invalid UTF-8 is replaced with U+FFFD before the body is
/// read as JSON. Returns `None` when there is nothing to project.
#[must_use]
pub fn project(body: &[u8]) -> Option<Projection> {
    let text = String::from_utf8_lossy(body);
    let value: Value = serde_json::from_str(&text).ok()?;
    project_value(&value)
}

/// Project a request body that is already decoded. Returns `None` when there is nothing
/// to project.
#[must_use]
pub fn project_value(body: &Value) -> Option<Projection> {
    let body = body.as_object()?;
    let sections = [
        ("[system]", system_lines(body)),
        ("[response_format]", response_format_lines(body)),
        ("[tools]", tool_lines(body)),
        ("[conversation]", conversation_lines(body)),
    ];
    let mut text = String::new();
    for (header, lines) in sections {
        if lines.is_empty() {
            continue;
        }
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(header);
        for line in lines {
            text.push('\n');
            text.push_str(&line);
        }
    }
    if text.is_empty() {
        return None;
    }
    Some(truncate(text))
}

/// A declared workload read from the [`WORKLOAD_HEADER`] override.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredWorkload {
    /// The capability, passed through as written. The engine validates it.
    pub capability: String,
    /// The tier, when one was given. The engine validates the range 1 to 5.
    pub difficulty: Option<u32>,
}

/// Why an [`WORKLOAD_HEADER`] value cannot be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WorkloadHeaderError {
    /// The value has no capability before the optional `:<tier>`.
    #[error("missing_capability")]
    MissingCapability,
    /// The text after `:` is not a decimal integer.
    #[error("invalid_tier")]
    InvalidTier,
}

/// Read `X-Maverick-Workload: <capability>[:<tier>]`.
///
/// Spaces and tabs around the whole value are trimmed; nothing else is. The value splits
/// at the first `:`. The capability must be non-empty. The tier, when present, must be
/// ASCII digits that fit in a `u32`. Neither is checked against the v1 lists, because the
/// engine answers an unknown capability or an out-of-range tier with its own 422.
///
/// A caller that gets an error must not fall back to the projection: the header asked
/// that the prompt not leave the process.
pub fn parse_workload_header(value: &str) -> Result<DeclaredWorkload, WorkloadHeaderError> {
    let value = value.trim_matches([' ', '\t']);
    let (capability, tier) = match value.split_once(':') {
        Some((capability, tier)) => (capability, Some(tier)),
        None => (value, None),
    };
    if capability.is_empty() {
        return Err(WorkloadHeaderError::MissingCapability);
    }
    let difficulty = match tier {
        None => None,
        Some(tier) => {
            if tier.is_empty() || !tier.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(WorkloadHeaderError::InvalidTier);
            }
            Some(
                tier.parse::<u32>()
                    .map_err(|_| WorkloadHeaderError::InvalidTier)?,
            )
        }
    };
    Ok(DeclaredWorkload {
        capability: capability.to_owned(),
        difficulty,
    })
}

// ---------------------------------------------------------------------------------------
// Sections

fn system_lines(body: &Map<String, Value>) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(system) = body.get("system") {
        push_nonempty(&mut lines, content_text(system));
    }
    for message in messages(body) {
        if matches!(role(message), Some("system" | "developer")) {
            push_nonempty(&mut lines, message_text(message));
        }
    }
    lines
}

fn response_format_lines(body: &Map<String, Value>) -> Vec<String> {
    let format = object_at(body.get("response_format"))
        .or_else(|| {
            object_at(
                body.get("output_config")
                    .and_then(|config| config.get("format")),
            )
        })
        .or_else(|| object_at(body.get("output_format")));
    let Some(format) = format else {
        return Vec::new();
    };
    let Some(kind) = nonempty_str(format.get("type")).filter(|kind| *kind != "text") else {
        return Vec::new();
    };
    let detail = object_at(format.get("json_schema")).unwrap_or(format);
    let mut lines = vec![kind.to_owned()];
    if let Some(name) = nonempty_str(detail.get("name")) {
        lines.push(format!("name: {name}"));
    }
    if let Some(description) = nonempty_str(detail.get("description")) {
        lines.push(format!("description: {description}"));
    }
    if let Some(schema) = detail.get("schema") {
        lines.push(format!("schema: {}", jcs::canonicalize(schema)));
    }
    lines
}

fn tool_lines(body: &Map<String, Value>) -> Vec<String> {
    let Some(tools) = body.get("tools").and_then(Value::as_array) else {
        return Vec::new();
    };
    tools
        .iter()
        .filter_map(Value::as_object)
        .filter_map(|tool| {
            let definition = object_at(tool.get("function"))
                .or_else(|| object_at(tool.get("custom")))
                .unwrap_or(tool);
            let name = nonempty_str(definition.get("name"))?;
            Some(match nonempty_str(definition.get("description")) {
                Some(description) => format!("{name}: {description}"),
                None => name.to_owned(),
            })
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Label {
    User,
    Assistant,
    Tool,
}

impl Label {
    fn as_str(self) -> &'static str {
        match self {
            Label::User => "user",
            Label::Assistant => "assistant",
            Label::Tool => "tool",
        }
    }
}

struct Turn {
    label: Label,
    line: String,
}

fn conversation_lines(body: &Map<String, Value>) -> Vec<String> {
    let turns = turns(body);
    let Some(last) = turns.len().checked_sub(1) else {
        return Vec::new();
    };
    let pinned = turns
        .iter()
        .rposition(|turn| turn.label == Label::User)
        .unwrap_or(last);
    let cost = |turn: &Turn| turn.line.chars().count() + 1;

    let mut keep = vec![false; turns.len()];
    keep[pinned] = true;
    let mut used = cost(&turns[pinned]);
    for index in (0..turns.len()).rev().filter(|index| *index != pinned) {
        let next = used + cost(&turns[index]);
        if next > CONVERSATION_BUDGET {
            break;
        }
        used = next;
        keep[index] = true;
    }

    turns
        .into_iter()
        .zip(keep)
        .filter_map(|(turn, kept)| kept.then_some(turn.line))
        .collect()
}

fn turns(body: &Map<String, Value>) -> Vec<Turn> {
    let mut turns = Vec::new();
    for message in messages(body) {
        match role(message) {
            Some("user") => user_turns(message.get("content"), &mut turns),
            Some("assistant") => push_turn(&mut turns, Label::Assistant, assistant_pieces(message)),
            Some("tool") => push_turn(&mut turns, Label::Tool, vec![message_text(message)]),
            _ => {}
        }
    }
    turns
}

/// A user message's text, split around any `tool_result` elements.
fn user_turns(content: Option<&Value>, turns: &mut Vec<Turn>) {
    let Some(parts) = content.and_then(Value::as_array) else {
        let text = content.map(content_text).unwrap_or_default();
        push_turn(turns, Label::User, vec![text]);
        return;
    };
    let mut pending = Vec::new();
    for part in parts.iter().filter_map(Value::as_object) {
        match part_type(part) {
            Some("text") => pending.extend(part_text(part).map(str::to_owned)),
            Some("tool_result") => {
                push_turn(turns, Label::User, std::mem::take(&mut pending));
                let result = part.get("content").map(content_text).unwrap_or_default();
                push_turn(turns, Label::Tool, vec![result]);
            }
            _ => {}
        }
    }
    push_turn(turns, Label::User, pending);
}

fn assistant_pieces(message: &Map<String, Value>) -> Vec<String> {
    let mut pieces = Vec::new();
    match message.get("content") {
        Some(Value::String(text)) => pieces.push(text.clone()),
        Some(Value::Array(parts)) => {
            for part in parts.iter().filter_map(Value::as_object) {
                match part_type(part) {
                    Some("text") => pieces.extend(part_text(part).map(str::to_owned)),
                    Some("tool_use") => {
                        let input = part.get("input").map(jcs::canonicalize);
                        pieces.extend(tool_call(part.get("name"), input));
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in calls.iter().filter_map(Value::as_object) {
            let definition =
                object_at(call.get("function")).or_else(|| object_at(call.get("custom")));
            let Some(definition) = definition else {
                continue;
            };
            let arguments = definition
                .get("arguments")
                .or_else(|| definition.get("input"))
                .and_then(Value::as_str)
                .map(canonical_arguments);
            pieces.extend(tool_call(definition.get("name"), arguments));
        }
    }
    pieces
}

/// `tool_call <name>[ <arguments>]`, or nothing when the name is missing.
fn tool_call(name: Option<&Value>, arguments: Option<String>) -> Option<String> {
    let name = nonempty_str(name)?;
    Some(match arguments.filter(|arguments| !arguments.is_empty()) {
        Some(arguments) => format!("tool_call {name} {arguments}"),
        None => format!("tool_call {name}"),
    })
}

/// A tool-call arguments string: JCS when it parses as JSON, else as is.
fn canonical_arguments(arguments: &str) -> String {
    match serde_json::from_str::<Value>(arguments) {
        Ok(value) => jcs::canonicalize(&value),
        Err(_) => arguments.to_owned(),
    }
}

// ---------------------------------------------------------------------------------------
// Text helpers

fn messages(body: &Map<String, Value>) -> impl Iterator<Item = &Map<String, Value>> {
    body.get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
}

fn role(message: &Map<String, Value>) -> Option<&str> {
    message.get("role").and_then(Value::as_str)
}

fn message_text(message: &Map<String, Value>) -> String {
    message.get("content").map(content_text).unwrap_or_default()
}

/// The text of a content value: a string, or the `text` elements of an array.
fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(Value::as_object)
            .filter(|part| part_type(part) == Some("text"))
            .filter_map(part_text)
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn part_type(part: &Map<String, Value>) -> Option<&str> {
    part.get("type").and_then(Value::as_str)
}

fn part_text(part: &Map<String, Value>) -> Option<&str> {
    nonempty_str(part.get("text"))
}

fn object_at(value: Option<&Value>) -> Option<&Map<String, Value>> {
    value.and_then(Value::as_object)
}

fn nonempty_str(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

fn push_nonempty(lines: &mut Vec<String>, text: String) {
    if !text.is_empty() {
        lines.push(text);
    }
}

fn push_turn(turns: &mut Vec<Turn>, label: Label, pieces: Vec<String>) {
    let pieces: Vec<String> = pieces
        .into_iter()
        .filter(|piece| !piece.is_empty())
        .collect();
    if pieces.is_empty() {
        return;
    }
    turns.push(Turn {
        label,
        line: format!("{}: {}", label.as_str(), pieces.join("\n")),
    });
}

/// Cut an oversize projection to head + marker + tail.
fn truncate(text: String) -> Projection {
    let length = text.chars().count();
    if length <= MAX_CHARS {
        return Projection {
            text,
            truncated: false,
        };
    }
    let head_end = byte_offset(&text, HEAD_CHARS);
    let tail_start = byte_offset(&text, length - TAIL_CHARS);
    let mut cut =
        String::with_capacity(head_end + TRUNCATION_MARKER.len() + text.len() - tail_start);
    cut.push_str(&text[..head_end]);
    cut.push_str(TRUNCATION_MARKER);
    cut.push_str(&text[tail_start..]);
    Projection {
        text: cut,
        truncated: true,
    }
}

/// The byte offset of scalar value number `index` (which must be in range).
fn byte_offset(text: &str, index: usize) -> usize {
    text.char_indices()
        .nth(index)
        .map_or(text.len(), |(offset, _)| offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text_of(body: &Value) -> Option<String> {
        project_value(body).map(|projection| projection.text)
    }

    #[test]
    fn non_object_bodies_project_to_nothing() {
        for body in [json!(null), json!([]), json!("x"), json!(1), json!({})] {
            assert_eq!(project_value(&body), None, "{body}");
        }
        assert_eq!(project(b"not json"), None);
    }

    #[test]
    fn invalid_utf8_becomes_replacement_characters() {
        let body = b"{\"messages\":[{\"role\":\"user\",\"content\":\"a\xffb\"}]}";
        assert_eq!(
            project(body).map(|projection| projection.text).as_deref(),
            Some("[conversation]\nuser: a\u{fffd}b")
        );
    }

    #[test]
    fn sections_come_in_order_with_one_blank_line_between() {
        let body = json!({
            "messages": [{"role": "user", "content": "q"}],
            "tools": [{"name": "t"}],
            "output_format": {"type": "json_schema", "schema": {"type": "object"}},
            "system": "s"
        });
        assert_eq!(
            text_of(&body).as_deref(),
            Some(
                "[system]\ns\n\n[response_format]\njson_schema\nschema: {\"type\":\"object\"}\n\n[tools]\nt\n\n[conversation]\nuser: q"
            )
        );
    }

    #[test]
    fn response_format_precedence_and_text_type() {
        // The first object found wins, even when its type is `text`.
        let body = json!({
            "response_format": {"type": "text"},
            "output_config": {"format": {"type": "json_schema", "schema": {}}}
        });
        assert_eq!(text_of(&body), None);

        let body =
            json!({"output_config": {"effort": "high"}, "output_format": {"type": "json_object"}});
        assert_eq!(
            text_of(&body).as_deref(),
            Some("[response_format]\njson_object")
        );

        let body = json!({"output_config": {"format": {"type": "json_schema", "schema": true}}, "output_format": {"type": "json_object"}});
        assert_eq!(
            text_of(&body).as_deref(),
            Some("[response_format]\njson_schema\nschema: true")
        );
    }

    #[test]
    fn tool_lines_read_function_custom_or_the_element() {
        let body = json!({"tools": [
            {"type": "function", "function": {"name": "f", "description": "does f"}},
            {"type": "custom", "custom": {"name": "c", "description": ""}},
            {"name": "m", "description": "messages tool", "input_schema": {}},
            {"type": "function", "function": {"description": "no name"}},
            "not an object"
        ]});
        assert_eq!(
            text_of(&body).as_deref(),
            Some("[tools]\nf: does f\nc\nm: messages tool")
        );
    }

    #[test]
    fn tool_call_arguments_are_canonical_when_json() {
        let body = json!({"messages": [{"role": "assistant", "content": null, "tool_calls": [
            {"type": "function", "function": {"name": "a", "arguments": "{ \"b\": 2, \"a\": 1.0 }"}},
            {"type": "function", "function": {"name": "b", "arguments": "not json"}},
            {"type": "function", "function": {"name": "c", "arguments": ""}},
            {"type": "custom", "custom": {"name": "d", "input": "free text"}},
            {"type": "function", "function": {"arguments": "{}"}},
            {"type": "other"}
        ]}]});
        assert_eq!(
            text_of(&body).as_deref(),
            Some(
                "[conversation]\nassistant: tool_call a {\"a\":1,\"b\":2}\ntool_call b not json\ntool_call c\ntool_call d free text"
            )
        );
    }

    #[test]
    fn user_text_splits_around_tool_results() {
        let body = json!({"messages": [{"role": "user", "content": [
            {"type": "text", "text": "before"},
            {"type": "tool_result", "tool_use_id": "1", "content": "r1"},
            {"type": "tool_result", "tool_use_id": "2", "content": [{"type": "text", "text": "r2"}, {"type": "image"}]},
            {"type": "tool_result", "tool_use_id": "3", "content": []},
            {"type": "text", "text": "after"},
            {"type": "text", "text": "more"}
        ]}]});
        assert_eq!(
            text_of(&body).as_deref(),
            Some("[conversation]\nuser: before\ntool: r1\ntool: r2\nuser: after\nmore")
        );
    }

    #[test]
    fn budget_pins_the_last_user_turn_and_stops_at_the_first_misfit() {
        let long = "x".repeat(CONVERSATION_BUDGET);
        let body = json!({"messages": [
            {"role": "user", "content": "oldest"},
            {"role": "assistant", "content": long},
            {"role": "user", "content": "question"},
            {"role": "assistant", "content": "prefill"}
        ]});
        // "user: question" (14 + 1) and "assistant: prefill" (18 + 1) fit; the long
        // assistant turn does not, so the walk stops before "oldest".
        assert_eq!(
            text_of(&body).as_deref(),
            Some("[conversation]\nuser: question\nassistant: prefill")
        );

        // The pinned user turn is kept even when it alone is over budget.
        let body = json!({"messages": [
            {"role": "assistant", "content": "earlier"},
            {"role": "user", "content": long},
            {"role": "assistant", "content": "after"}
        ]});
        let text = text_of(&body).expect("projection");
        assert_eq!(text, format!("[conversation]\nuser: {long}"));
    }

    #[test]
    fn budget_is_inclusive() {
        // "user: " and "tool: " are both 6 scalar values, so each turn below costs
        // 6 + len + 1. Two turns of cost 8,000 fill the budget exactly.
        let exact = "y".repeat(8_000 - 6 - 1);
        let body = json!({"messages": [
            {"role": "user", "content": "dropped"},
            {"role": "tool", "content": exact},
            {"role": "user", "content": exact}
        ]});
        assert_eq!(
            text_of(&body),
            Some(format!("[conversation]\ntool: {exact}\nuser: {exact}"))
        );

        // One more scalar value in the tool turn and it no longer fits.
        let over = format!("{exact}y");
        let body = json!({"messages": [
            {"role": "tool", "content": over},
            {"role": "user", "content": exact}
        ]});
        assert_eq!(
            text_of(&body),
            Some(format!("[conversation]\nuser: {exact}"))
        );
    }

    #[test]
    fn truncation_keeps_head_marker_tail() {
        let at_limit = "z".repeat(MAX_CHARS);
        assert_eq!(
            truncate(at_limit.clone()),
            Projection {
                text: at_limit,
                truncated: false
            }
        );
        let over = format!("{}{}", "h".repeat(HEAD_CHARS), "t".repeat(TAIL_CHARS + 1));
        let cut = truncate(over);
        assert!(cut.truncated);
        assert_eq!(
            cut.text,
            format!(
                "{}{TRUNCATION_MARKER}{}",
                "h".repeat(HEAD_CHARS),
                "t".repeat(TAIL_CHARS)
            )
        );
        assert!(
            cut.text.chars().count() > MAX_CHARS,
            "the engine must still see an oversize prompt"
        );
    }

    #[test]
    fn workload_header_shapes() {
        let ok = |capability: &str, difficulty: Option<u32>| {
            Ok(DeclaredWorkload {
                capability: capability.to_owned(),
                difficulty,
            })
        };
        assert_eq!(
            parse_workload_header("code_generation"),
            ok("code_generation", None)
        );
        assert_eq!(
            parse_workload_header(" \tcode_generation:4\t "),
            ok("code_generation", Some(4))
        );
        assert_eq!(
            parse_workload_header("factual_qa:9"),
            ok("factual_qa", Some(9))
        );
        assert_eq!(parse_workload_header("x:007"), ok("x", Some(7)));
        assert_eq!(
            parse_workload_header("a:b:c").unwrap_err(),
            WorkloadHeaderError::InvalidTier
        );
        for bad in ["", "  ", ":3"] {
            assert_eq!(
                parse_workload_header(bad),
                Err(WorkloadHeaderError::MissingCapability),
                "{bad:?}"
            );
        }
        for bad in [
            "x:",
            "x:+3",
            "x:-1",
            "x: 3",
            "x:3.0",
            "x:hard",
            "x:99999999999",
        ] {
            assert_eq!(
                parse_workload_header(bad),
                Err(WorkloadHeaderError::InvalidTier),
                "{bad:?}"
            );
        }
    }
}
