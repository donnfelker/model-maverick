//! `Load(dir)`: read every `*.yaml` / `*.yml` file under a directory into tasks.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Deserializer};

use super::validate::{Rule, Violation};
use super::{Attachment, Corpus, DEFAULT_PASS_THRESHOLD, Task};
use crate::gojson;

/// A load failure that aborts the whole load. The text is exact up to the OS error
/// detail (`tasks: load <dir>: ...`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadError(pub String);

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for LoadError {}

/// Loads the corpus under `dir`.
///
/// The walk is recursive in lexical order (Go `filepath.WalkDir`) and does not follow
/// symlinked directories. Every file whose name ends in `.yaml` or `.yml` is decoded;
/// its path is recorded relative to `dir`. A file that does not decode becomes one
/// `yaml_invalid` violation and yields no task. I/O errors abort the load. An empty
/// result (no tasks and no violations) is an error.
///
/// This is blocking `std::fs` I/O, read-only. Async callers run it on a blocking
/// thread.
pub fn load(dir: &Path) -> Result<Corpus, LoadError> {
    let shown = dir.display().to_string();
    let fail = |detail: String| LoadError(format!("tasks: load {shown}: {detail}"));

    let root = std::fs::symlink_metadata(dir)
        .map_err(|e| fail(format!("lstat {shown}: {}", go_io_error(&e))))?;
    let mut files = Vec::new();
    if root.is_dir() {
        walk(dir, "", &mut files).map_err(fail)?;
    } else if is_task_file(&dir.file_name().unwrap_or_default().to_string_lossy()) {
        // WalkDir visits a file root once; filepath.Rel(dir, dir) is ".".
        files.push((".".to_owned(), dir.to_path_buf()));
    }

    let mut documents = Vec::with_capacity(files.len());
    for (relative, path) in files {
        let bytes = std::fs::read(&path)
            .map_err(|e| fail(format!("open {}: {}", path.display(), go_io_error(&e))))?;
        documents.push((relative, bytes));
    }

    let corpus = load_documents(documents);
    if corpus.tasks.is_empty() && corpus.load_violations.is_empty() {
        return Err(fail("no *.yaml task files found".to_owned()));
    }
    Ok(corpus)
}

fn is_task_file(name: &str) -> bool {
    name.ends_with(".yaml") || name.ends_with(".yml")
}

/// Collects task files under `dir` in WalkDir order: entries sorted by name, each
/// directory walked where it sorts.
fn walk(
    dir: &Path,
    prefix: &str,
    files: &mut Vec<(String, std::path::PathBuf)>,
) -> Result<(), String> {
    let read_err = |e: std::io::Error| format!("open {}: {}", dir.display(), go_io_error(&e));
    let mut entries = std::fs::read_dir(dir)
        .map_err(read_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(read_err)?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let relative = format!("{prefix}{name}");
        let file_type = entry.file_type().map_err(read_err)?;
        if file_type.is_dir() {
            walk(&entry.path(), &format!("{relative}/"), files)?;
        } else if is_task_file(&name) {
            files.push((relative, entry.path()));
        }
    }
    Ok(())
}

/// Go's wording for the common OS errors (`no such file or directory`), else Rust's.
fn go_io_error(error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::NotFound => "no such file or directory".to_owned(),
        std::io::ErrorKind::PermissionDenied => "permission denied".to_owned(),
        std::io::ErrorKind::IsADirectory => "is a directory".to_owned(),
        _ => error.to_string(),
    }
}

/// Decodes task documents given as (relative path, bytes), in the given order. This is
/// the pure core of [`load`].
pub fn load_documents(documents: Vec<(String, Vec<u8>)>) -> Corpus {
    let mut corpus = Corpus::default();
    for (file, bytes) in documents {
        match decode_task(&file, &bytes) {
            Ok((task, violations)) => {
                corpus.load_violations.extend(violations);
                corpus.tasks.push(task);
            }
            Err(message) => corpus.load_violations.push(Violation {
                file,
                task_id: String::new(),
                rule: Rule::YamlInvalid,
                message,
            }),
        }
    }
    corpus
}

/// The YAML task shape. Unknown fields are errors. `null` leaves a field at its zero
/// value, as in Go.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct YamlTask {
    #[serde(deserialize_with = "nullable")]
    id: String,
    #[serde(deserialize_with = "nullable")]
    capability: String,
    #[serde(deserialize_with = "nullable")]
    domain_tags: Vec<String>,
    difficulty_tier: Option<i64>,
    difficulty_knobs: Option<BTreeMap<String, serde_norway::Value>>,
    #[serde(deserialize_with = "nullable")]
    prompt: String,
    #[serde(deserialize_with = "nullable")]
    attachments: Vec<YamlAttachment>,
    #[serde(deserialize_with = "nullable")]
    temperature: f32,
    max_tokens: Option<i64>,
    #[serde(deserialize_with = "nullable")]
    grader_specs: Vec<serde_norway::Value>,
    #[serde(deserialize_with = "nullable")]
    dimension_weights: BTreeMap<String, f64>,
    pass_threshold: Option<f64>,
    #[serde(deserialize_with = "nullable")]
    smoke_member: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct YamlAttachment {
    #[serde(deserialize_with = "nullable")]
    name: String,
    #[serde(deserialize_with = "nullable")]
    content: String,
}

fn nullable<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// Decodes one file. `Err` is the `yaml_invalid` message. `Ok` carries the task and any
/// `grader_envelope` violations from re-encoding its grader specs.
fn decode_task(file: &str, bytes: &[u8]) -> Result<(Task, Vec<Violation>), String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("invalid UTF-8: {e}"))?;
    // Go's strict decoder rejects a duplicate mapping key at any depth. The typed decode
    // below only catches duplicate task fields, so check the whole document first.
    serde_norway::from_str::<serde_norway::Value>(text).map_err(yaml_message)?;
    // An empty document decodes to the all-zero task.
    let yaml = serde_norway::from_str::<Option<YamlTask>>(text)
        .map_err(yaml_message)?
        .unwrap_or_default();

    let mut violations = Vec::new();
    let mut grader_specs = Vec::with_capacity(yaml.grader_specs.len());
    for (i, spec) in yaml.grader_specs.iter().enumerate() {
        match gojson::from_yaml(spec) {
            Ok(json) => grader_specs.push(Some(gojson::marshal(&json))),
            Err(e) => {
                violations.push(Violation {
                    file: file.to_owned(),
                    task_id: yaml.id.clone(),
                    rule: Rule::GraderEnvelope,
                    message: format!("grader_specs[{i}]: {e}"),
                });
                grader_specs.push(None);
            }
        }
    }

    let task = Task {
        file: file.to_owned(),
        id: yaml.id,
        capability: yaml.capability,
        domain_tags: yaml.domain_tags,
        difficulty_tier: yaml.difficulty_tier,
        difficulty_knobs: yaml.difficulty_knobs,
        prompt: yaml.prompt,
        attachments: yaml
            .attachments
            .into_iter()
            .map(|a| Attachment {
                name: a.name,
                content: a.content,
            })
            .collect(),
        temperature: yaml.temperature,
        max_tokens: yaml.max_tokens,
        grader_specs,
        dimension_weights: yaml.dimension_weights,
        pass_threshold: yaml.pass_threshold.unwrap_or(DEFAULT_PASS_THRESHOLD),
        smoke_member: yaml.smoke_member,
    };
    Ok((task, violations))
}

/// `[line:column] message`, like goccy's position prefix. The text after it is the
/// YAML library's wording (QUIRK Q-T1, D36).
fn yaml_message(error: serde_norway::Error) -> String {
    let text = error.to_string();
    match error.location() {
        Some(location) => {
            let suffix = format!(" at line {} column {}", location.line(), location.column());
            let text = text.strip_suffix(&suffix).unwrap_or(&text);
            format!("[{}:{}] {text}", location.line(), location.column())
        }
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_errors_use_go_wording() {
        use std::io::Error;
        // Real OS errors (ENOENT, EACCES, EISDIR), whose Rust text differs from Go's:
        // "No such file or directory (os error 2)".
        assert_eq!(
            go_io_error(&Error::from_raw_os_error(2)),
            "no such file or directory"
        );
        assert_eq!(
            go_io_error(&Error::from_raw_os_error(13)),
            "permission denied"
        );
        assert_eq!(go_io_error(&Error::from_raw_os_error(21)), "is a directory");
        assert_eq!(go_io_error(&Error::other("disk on fire")), "disk on fire");
    }

    fn doc(file: &str, text: &str) -> (String, Vec<u8>) {
        (file.to_owned(), text.as_bytes().to_vec())
    }

    #[test]
    fn empty_and_comment_only_documents_are_zero_tasks() {
        let corpus = load_documents(vec![doc("a.yaml", ""), doc("b.yaml", "# note\n")]);
        assert!(corpus.load_violations.is_empty());
        assert_eq!(corpus.tasks.len(), 2);
        assert_eq!(corpus.tasks[0].id, "");
        assert_eq!(corpus.tasks[0].pass_threshold, DEFAULT_PASS_THRESHOLD);
        assert_eq!(corpus.tasks[0].difficulty_tier, None);
    }

    #[test]
    fn nulls_leave_zero_values() {
        let corpus = load_documents(vec![doc(
            "t.yaml",
            "id: ~\ndomain_tags: ~\ndifficulty_tier: ~\npass_threshold: ~\ntemperature: ~\nattachments: [{name: ~, content: x}]\n",
        )]);
        assert!(
            corpus.load_violations.is_empty(),
            "{:?}",
            corpus.load_violations
        );
        let task = &corpus.tasks[0];
        assert_eq!(task.id, "");
        assert!(task.domain_tags.is_empty());
        assert_eq!(task.difficulty_tier, None);
        assert_eq!(task.pass_threshold, DEFAULT_PASS_THRESHOLD);
        assert_eq!(task.attachments[0].name, "");
    }

    #[test]
    fn duplicate_keys_are_rejected_at_any_depth() {
        let corpus = load_documents(vec![doc(
            "t.yaml",
            "id: t\ndimension_weights: {a: 1, a: 0.5}\n",
        )]);
        assert!(corpus.tasks.is_empty());
        assert_eq!(corpus.load_violations[0].rule, Rule::YamlInvalid);
        assert!(corpus.load_violations[0].message.contains("\"a\""));
    }

    #[test]
    fn unknown_nested_field_is_rejected() {
        let corpus = load_documents(vec![doc(
            "t.yaml",
            "attachments: [{name: a, content: b, mime: x}]\n",
        )]);
        assert!(corpus.tasks.is_empty());
        assert!(corpus.load_violations[0].message.contains("mime"));
        assert!(corpus.load_violations[0].message.starts_with("[1:"));
    }

    #[test]
    fn grader_spec_reencode_failure_keeps_the_task_with_a_hole() {
        let corpus = load_documents(vec![doc(
            "t.yaml",
            "id: task_a\ngrader_specs:\n  - {type: golden_answer, spec: {1: x}}\n  - {type: key_facts}\n",
        )]);
        assert_eq!(corpus.tasks.len(), 1);
        assert_eq!(
            corpus.tasks[0].grader_specs,
            [None, Some("{\"type\":\"key_facts\"}".to_owned())]
        );
        let violation = &corpus.load_violations[0];
        assert_eq!(violation.rule, Rule::GraderEnvelope);
        assert_eq!(violation.task_id, "task_a");
        assert_eq!(
            violation.message,
            "grader_specs[0]: graders: envelope: non-string map key 1"
        );
    }

    #[test]
    fn non_utf8_is_yaml_invalid() {
        let corpus = load_documents(vec![("t.yaml".to_owned(), vec![0xff, 0xfe])]);
        assert_eq!(corpus.load_violations[0].rule, Rule::YamlInvalid);
    }
}
