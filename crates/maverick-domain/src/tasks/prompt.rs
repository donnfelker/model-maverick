//! `PromptText`: the exact bytes a benchmarked model sees (reference Part 2 §5.1).
//!
//! The prompt is the same for every column, so result differences come from the model.
//! The judge grader shows the judge the same text.

use super::{Attachment, Task};
use crate::gojson::{self, JsonValue};

/// The task prompt followed by each attachment as
/// `"\n\n--- attachment: <name> ---\n<content>"`, in order. Nothing is trimmed and no
/// newline is added; an empty name or content still emits the header.
pub fn prompt_text(prompt: &str, attachments: &[Attachment]) -> String {
    let mut out = String::with_capacity(
        prompt.len()
            + attachments
                .iter()
                .map(|a| a.name.len() + a.content.len() + 24)
                .sum::<usize>(),
    );
    out.push_str(prompt);
    for attachment in attachments {
        out.push_str("\n\n--- attachment: ");
        out.push_str(&attachment.name);
        out.push_str(" ---\n");
        out.push_str(&attachment.content);
    }
    out
}

impl Task {
    /// [`prompt_text`] of this task.
    pub fn prompt_text(&self) -> String {
        prompt_text(&self.prompt, &self.attachments)
    }
}

/// Decodes the `attachments` column like Go's `json.Unmarshal` into
/// `[]struct{Name, Content string}`: empty text or `null` is no attachments, field names
/// match case-insensitively, unknown fields are ignored, a `null` element or field is
/// the zero value. Errors are `parsing attachments: <json error>`; the text after the
/// prefix is library wording (D36).
pub fn parse_attachments(raw: &str) -> Result<Vec<Attachment>, String> {
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    let error = |e: String| format!("parsing attachments: {e}");
    let items = match gojson::unmarshal(raw).map_err(|e| error(e.0))? {
        JsonValue::Null => return Ok(Vec::new()),
        JsonValue::Array(items) => items,
        other => {
            return Err(error(format!(
                "json: cannot unmarshal {} into Go value of type []runner.attachment",
                other.kind_name()
            )));
        }
    };
    items
        .iter()
        .map(|item| decode_attachment(item).map_err(error))
        .collect()
}

fn decode_attachment(item: &JsonValue) -> Result<Attachment, String> {
    let mut attachment = Attachment {
        name: String::new(),
        content: String::new(),
    };
    let members = match item {
        JsonValue::Null => return Ok(attachment),
        JsonValue::Object(members) => members,
        other => {
            return Err(format!(
                "json: cannot unmarshal {} into Go value of type runner.attachment",
                other.kind_name()
            ));
        }
    };
    for (key, value) in members {
        let Some(field) = gojson::match_field(key, &["name", "content"]) else {
            continue;
        };
        let text = match value {
            JsonValue::String(s) => s.clone(),
            JsonValue::Null => continue,
            other => {
                return Err(format!(
                    "json: cannot unmarshal {} into Go struct field attachment.{field} of type string",
                    other.kind_name()
                ));
            }
        };
        if field == "name" {
            attachment.name = text;
        } else {
            attachment.content = text;
        }
    }
    Ok(attachment)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attachment(name: &str, content: &str) -> Attachment {
        Attachment {
            name: name.to_owned(),
            content: content.to_owned(),
        }
    }

    #[test]
    fn prompt_text_appends_each_attachment() {
        assert_eq!(prompt_text("P", &[]), "P");
        assert_eq!(
            prompt_text("P", &[attachment("a.txt", "A"), attachment("", "")]),
            "P\n\n--- attachment: a.txt ---\nA\n\n--- attachment:  ---\n"
        );
    }

    #[test]
    fn task_method_uses_the_task_attachments() {
        let corpus = crate::tasks::load_documents(vec![(
            "t.yaml".to_owned(),
            b"prompt: P\nattachments: [{name: a, content: b}]\n".to_vec(),
        )]);
        assert_eq!(
            corpus.tasks[0].prompt_text(),
            "P\n\n--- attachment: a ---\nb"
        );
    }

    #[test]
    fn attachments_decode_like_go() {
        assert_eq!(parse_attachments(""), Ok(Vec::new()));
        assert_eq!(parse_attachments("null"), Ok(Vec::new()));
        assert_eq!(parse_attachments("[]"), Ok(Vec::new()));
        assert_eq!(
            parse_attachments(
                r#"[{"NAME":"a","Content":"b","extra":1},null,{"name":null,"content":"c","name":"n2"}]"#
            ),
            Ok(vec![
                attachment("a", "b"),
                attachment("", ""),
                attachment("n2", "c")
            ])
        );
        assert_eq!(
            parse_attachments("{}").unwrap_err(),
            "parsing attachments: json: cannot unmarshal object into Go value of type []runner.attachment"
        );
        assert_eq!(
            parse_attachments("[1]").unwrap_err(),
            "parsing attachments: json: cannot unmarshal number into Go value of type runner.attachment"
        );
        assert_eq!(
            parse_attachments(r#"[{"name":1}]"#).unwrap_err(),
            "parsing attachments: json: cannot unmarshal number into Go struct field attachment.name of type string"
        );
        assert_eq!(
            parse_attachments("[").unwrap_err(),
            "parsing attachments: unexpected end of JSON input"
        );
    }
}
