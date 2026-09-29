//! Corpus tasks: the task type, the YAML loader, and the corpus validator (spec §5.4,
//! §5.6; reference Part 2 §13).
//!
//! [`load`] reads a directory of task YAML files. [`validate`] checks the loaded
//! corpus and returns every violation in one [`Report`]. Persistence (the `tasks` and
//! `corpus_versions` upserts and the activation transaction) is in the store crate;
//! [`Task`] provides the JSON encodings of its columns.
//!
//! Fixture tasks are synthetic. The private corpus never enters this repository (G7).

mod load;
mod prompt;
mod validate;

use std::collections::BTreeMap;

use crate::gojson::{self, JsonValue};

pub use load::{LoadError, load, load_documents};
pub use prompt::{parse_attachments, prompt_text};
pub use validate::{Report, Rule, Violation, validate};

/// The ten v1 capabilities, in the order the `unknown_capability` message lists them.
pub const CAPABILITIES: [&str; 10] = [
    "code_generation",
    "structured_extraction",
    "summarization_synthesis",
    "multi_step_reasoning",
    "instruction_schema_following",
    "long_context_qa",
    "factual_qa",
    "rewriting_editing",
    "translation_multilingual",
    "critique_and_judging",
];

/// The default `pass_threshold` when a task does not set one.
pub const DEFAULT_PASS_THRESHOLD: f64 = 0.7;

/// The largest estimated input a smoke-set task may have, in tokens (spec §5.6).
pub const SMOKE_INPUT_TOKEN_LIMIT: i64 = 32_768;

/// A named attachment appended to the task prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub name: String,
    pub content: String,
}

/// One corpus task as loaded from YAML. Nothing here is validated; see [`validate`].
#[derive(Debug, Clone, PartialEq)]
pub struct Task {
    /// The file path relative to the loaded directory, with `/` separators.
    pub file: String,
    pub id: String,
    pub capability: String,
    pub domain_tags: Vec<String>,
    /// `None` when the YAML has no `difficulty_tier`.
    pub difficulty_tier: Option<i64>,
    /// The authored knob settings. `None` when absent; stored as `{}`.
    pub difficulty_knobs: Option<BTreeMap<String, serde_norway::Value>>,
    pub prompt: String,
    pub attachments: Vec<Attachment>,
    pub temperature: f32,
    /// `None` when the YAML has no `max_tokens`.
    pub max_tokens: Option<i64>,
    /// Each grader spec envelope re-encoded as compact JSON (keys sorted). `None` where
    /// the re-encoding failed; the loader reported that as a `grader_envelope`
    /// violation.
    pub grader_specs: Vec<Option<String>>,
    pub dimension_weights: BTreeMap<String, f64>,
    /// [`DEFAULT_PASS_THRESHOLD`] when the YAML does not set one.
    pub pass_threshold: f64,
    pub smoke_member: bool,
}

impl Task {
    /// The difficulty tier, or 0 when absent (Go's zero value, used by the corpus
    /// shape checks).
    pub fn tier_or_zero(&self) -> i64 {
        self.difficulty_tier.unwrap_or(0)
    }

    /// `(len(prompt bytes) + sum(len(attachment content bytes)) + 3) / 4`, that is the
    /// byte count divided by 4, rounded up.
    pub fn estimated_input_tokens(&self) -> i64 {
        let bytes = self.prompt.len()
            + self
                .attachments
                .iter()
                .map(|a| a.content.len())
                .sum::<usize>();
        i64::try_from(bytes.div_ceil(4)).unwrap_or(i64::MAX)
    }

    /// The `attachments` column: `[{"name":...,"content":...}]`, `[]` when none.
    pub fn attachments_json(&self) -> String {
        let items = self
            .attachments
            .iter()
            .map(|a| {
                JsonValue::Object(vec![
                    ("name".to_owned(), JsonValue::String(a.name.clone())),
                    ("content".to_owned(), JsonValue::String(a.content.clone())),
                ])
            })
            .collect();
        gojson::marshal(&JsonValue::Array(items))
    }

    /// The `difficulty_knobs` column: the knobs with sorted keys, `{}` when absent.
    pub fn difficulty_knobs_json(&self) -> Result<String, String> {
        let Some(knobs) = &self.difficulty_knobs else {
            return Ok("{}".to_owned());
        };
        let mapping: serde_norway::Mapping = knobs
            .iter()
            .map(|(k, v)| (serde_norway::Value::String(k.clone()), v.clone()))
            .collect();
        let value = gojson::from_yaml(&serde_norway::Value::Mapping(mapping))?;
        Ok(gojson::marshal(&value))
    }

    /// The `dimension_weights` column: sorted keys, Go float encoding.
    pub fn dimension_weights_json(&self) -> Result<String, String> {
        let mut members = Vec::with_capacity(self.dimension_weights.len());
        for (dim, &w) in &self.dimension_weights {
            members.push((dim.clone(), JsonValue::Number(gojson::encode_f64(w)?)));
        }
        Ok(gojson::marshal(&JsonValue::Object(members)))
    }

    /// The `grader_specs` column: a JSON array of the re-encoded envelopes.
    pub fn grader_specs_json(&self) -> String {
        let specs: Vec<&str> = self
            .grader_specs
            .iter()
            .map(|s| s.as_deref().unwrap_or("null"))
            .collect();
        format!("[{}]", specs.join(","))
    }
}

/// A loaded corpus: the tasks in load order and the violations found while loading.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Corpus {
    pub tasks: Vec<Task>,
    /// `yaml_invalid` per file that did not decode, and `grader_envelope` per grader
    /// spec that could not be re-encoded as JSON.
    pub load_violations: Vec<Violation>,
}
