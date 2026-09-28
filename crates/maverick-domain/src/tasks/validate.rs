//! `Validate(corpus)`: every violation in one report, in Go's exact order and wording
//! (reference Part 2 §13.2).

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::{CAPABILITIES, Corpus, SMOKE_INPUT_TOKEN_LIMIT, Task};
use crate::gofmt;
use crate::graders::{self, EnvelopeErrorKind, ValidationRegistry};

/// A violation rule name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Rule {
    YamlInvalid,
    GraderEnvelope,
    MissingField,
    BadTaskId,
    FilenameMismatch,
    UnknownCapability,
    TierOutOfRange,
    InvalidField,
    WeightsSum,
    GraderUnsupported,
    UnknownGraderType,
    GraderSpecInvalid,
    DimensionMismatch,
    DimensionUnfed,
    SmokeInputTooLarge,
    DuplicateTaskId,
    SmokeAnchorMissing,
    CapabilityTaskCount,
    CapabilityTierSpan,
    SmokeCount,
}

impl Rule {
    pub fn as_str(self) -> &'static str {
        match self {
            Rule::YamlInvalid => "yaml_invalid",
            Rule::GraderEnvelope => "grader_envelope",
            Rule::MissingField => "missing_field",
            Rule::BadTaskId => "bad_task_id",
            Rule::FilenameMismatch => "filename_mismatch",
            Rule::UnknownCapability => "unknown_capability",
            Rule::TierOutOfRange => "tier_out_of_range",
            Rule::InvalidField => "invalid_field",
            Rule::WeightsSum => "weights_sum",
            Rule::GraderUnsupported => "grader_unsupported",
            Rule::UnknownGraderType => "unknown_grader_type",
            Rule::GraderSpecInvalid => "grader_spec_invalid",
            Rule::DimensionMismatch => "dimension_mismatch",
            Rule::DimensionUnfed => "dimension_unfed",
            Rule::SmokeInputTooLarge => "smoke_input_too_large",
            Rule::DuplicateTaskId => "duplicate_task_id",
            Rule::SmokeAnchorMissing => "smoke_anchor_missing",
            Rule::CapabilityTaskCount => "capability_task_count",
            Rule::CapabilityTierSpan => "capability_tier_span",
            Rule::SmokeCount => "smoke_count",
        }
    }
}

impl std::fmt::Display for Rule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One violation. `file` and `task_id` are empty for corpus-shape violations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub file: String,
    pub task_id: String,
    pub rule: Rule,
    pub message: String,
}

impl std::fmt::Display for Violation {
    /// `[<file>: ][task <task_id>: ]<rule>: <message>`
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if !self.file.is_empty() {
            write!(f, "{}: ", self.file)?;
        }
        if !self.task_id.is_empty() {
            write!(f, "task {}: ", self.task_id)?;
        }
        write!(f, "{}: {}", self.rule, self.message)
    }
}

/// The validation result. The corpus is valid when `errors` is empty. Warnings are the
/// spec §5.6 shape targets that Go reports but does not enforce.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub errors: Vec<Violation>,
    pub warnings: Vec<Violation>,
}

impl Report {
    pub fn is_valid(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Validates a loaded corpus. Errors are the load violations, then per task in load
/// order its own violations and the duplicate-id check, then the corpus-shape errors.
/// Grader specs are checked through `registry` (normally [`ValidationRegistry::v1`]).
pub fn validate(corpus: &Corpus, registry: &ValidationRegistry) -> Report {
    let mut report = Report {
        errors: corpus.load_violations.clone(),
        warnings: Vec::new(),
    };
    let mut first_file_by_id: HashMap<&str, &str> = HashMap::new();
    for task in &corpus.tasks {
        validate_task(task, registry, &mut report.errors);
        if task.id.is_empty() {
            continue;
        }
        match first_file_by_id.get(task.id.as_str()) {
            Some(first) => report.errors.push(task_violation(
                task,
                Rule::DuplicateTaskId,
                format!("id already used by {first}"),
            )),
            None => {
                first_file_by_id.insert(&task.id, &task.file);
            }
        }
    }
    check_shape(corpus, &mut report);
    report
}

fn task_violation(task: &Task, rule: Rule, message: String) -> Violation {
    Violation {
        file: task.file.clone(),
        task_id: task.id.clone(),
        rule,
        message,
    }
}

fn shape_violation(rule: Rule, message: String) -> Violation {
    Violation {
        file: String::new(),
        task_id: String::new(),
        rule,
        message,
    }
}

/// `^task_[a-z0-9]+(_[a-z0-9]+)*$`
fn is_authored_slug(id: &str) -> bool {
    id.strip_prefix("task_").is_some_and(|rest| {
        rest.split('_').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
    })
}

fn file_stem(file: &str) -> &str {
    let base = file.rsplit('/').next().unwrap_or(file);
    let base = base.strip_suffix(".yaml").unwrap_or(base);
    base.strip_suffix(".yml").unwrap_or(base)
}

fn validate_task(task: &Task, registry: &ValidationRegistry, out: &mut Vec<Violation>) {
    let mut push = |rule: Rule, message: String| out.push(task_violation(task, rule, message));

    // 1. id
    if task.id.is_empty() {
        push(Rule::MissingField, "id is required".to_owned());
    } else {
        if !is_authored_slug(&task.id) {
            push(
                Rule::BadTaskId,
                format!(
                    "id {} is not an authored slug matching ^task_[a-z0-9]+(_[a-z0-9]+)*$",
                    gofmt::quote(&task.id)
                ),
            );
        }
        if file_stem(&task.file) != task.id {
            push(
                Rule::FilenameMismatch,
                format!(
                    "file {} must be named after its task id {}",
                    task.file,
                    gofmt::quote(&task.id)
                ),
            );
        }
    }

    // 2. capability
    if task.capability.is_empty() {
        push(Rule::MissingField, "capability is required".to_owned());
    } else if !CAPABILITIES.contains(&task.capability.as_str()) {
        push(
            Rule::UnknownCapability,
            format!(
                "capability {} is not one of the v1 capabilities: {}",
                gofmt::quote(&task.capability),
                CAPABILITIES.join(", ")
            ),
        );
    }

    // 3. difficulty_tier
    match task.difficulty_tier {
        None => push(Rule::MissingField, "difficulty_tier is required".to_owned()),
        Some(tier) if !(1..=5).contains(&tier) => push(
            Rule::TierOutOfRange,
            format!("difficulty_tier {tier} is not in 1..5"),
        ),
        Some(_) => {}
    }

    // 4. prompt (Rust `trim` and Go `strings.TrimSpace` share one whitespace set)
    if task.prompt.trim().is_empty() {
        push(Rule::MissingField, "prompt is required".to_owned());
    }

    // 5. max_tokens
    match task.max_tokens {
        None => push(Rule::MissingField, "max_tokens is required".to_owned()),
        Some(n) if n < 1 => push(Rule::InvalidField, format!("max_tokens {n} must be >= 1")),
        Some(_) => {}
    }

    // 6. temperature
    if !task.temperature.is_finite() || task.temperature < 0.0 {
        push(
            Rule::InvalidField,
            format!(
                "temperature {} must be a finite number >= 0",
                gofmt::fmt_v_f32(task.temperature)
            ),
        );
    }

    // 7. pass_threshold
    if !(task.pass_threshold > 0.0 && task.pass_threshold <= 1.0) {
        push(
            Rule::InvalidField,
            format!(
                "pass_threshold {} must be in (0, 1]",
                gofmt::fmt_v_f64(task.pass_threshold)
            ),
        );
    }

    // 8. attachments
    for (i, attachment) in task.attachments.iter().enumerate() {
        if attachment.name.is_empty() || attachment.content.is_empty() {
            push(
                Rule::InvalidField,
                format!("attachments[{i}] needs both name and content"),
            );
        }
    }

    // 9. dimension_weights, in sorted key order (the map is a BTreeMap)
    let weights = &task.dimension_weights;
    if weights.is_empty() {
        push(
            Rule::MissingField,
            "dimension_weights is required".to_owned(),
        );
    } else {
        let mut sum = 0.0;
        for (dim, &w) in weights {
            if !(w > 0.0 && w <= 1.0) {
                push(
                    Rule::InvalidField,
                    format!(
                        "dimension_weights[{}] = {} must be in (0, 1]",
                        gofmt::quote(dim),
                        gofmt::fmt_v_f64(w)
                    ),
                );
            }
            sum += w;
        }
        // Go: `math.Abs(sum-1) > 0.001`. A NaN sum compares as unordered and is not
        // reported here (its NaN weight already was).
        if (sum - 1.0_f64).abs().partial_cmp(&0.001) == Some(Ordering::Greater) {
            push(
                Rule::WeightsSum,
                format!(
                    "dimension_weights sum to {}; they must sum to 1 (spec 5.6)",
                    gofmt::fmt_v_f64(sum)
                ),
            );
        }
    }

    // 10. grader specs
    if task.grader_specs.is_empty() {
        push(Rule::MissingField, "grader_specs is required".to_owned());
    } else {
        let mut all_parsed = true;
        let mut fed: BTreeSet<&str> = BTreeSet::new();
        for (i, raw) in task.grader_specs.iter().enumerate() {
            let Some(raw) = raw else {
                // Already reported by the loader.
                all_parsed = false;
                continue;
            };
            let envelope = match graders::parse_envelope(raw) {
                Ok(envelope) => envelope,
                Err(error) => {
                    all_parsed = false;
                    let rule = if error.kind == EnvelopeErrorKind::Unsupported {
                        Rule::GraderUnsupported
                    } else if !graders::is_known_type(&graders::raw_type(raw)) {
                        // QUIRK Q-T2: a missing type is an unknown type.
                        Rule::UnknownGraderType
                    } else {
                        Rule::GraderEnvelope
                    };
                    push(rule, format!("grader_specs[{i}]: {error}"));
                    continue;
                }
            };
            if let Err(error) = envelope.validate_spec(registry) {
                push(
                    Rule::GraderSpecInvalid,
                    format!("grader_specs[{i}]: {error}"),
                );
            }
            for dim in envelope.weight_dimensions.keys() {
                match weights.get_key_value(dim) {
                    Some((key, _)) => {
                        fed.insert(key.as_str());
                    }
                    None => push(
                        Rule::DimensionMismatch,
                        format!(
                            "grader_specs[{i}]: weight_dimensions names {}, which is not in the task's dimension_weights",
                            gofmt::quote(dim)
                        ),
                    ),
                }
            }
        }
        if all_parsed && !weights.is_empty() {
            for dim in weights.keys() {
                if !fed.contains(dim.as_str()) {
                    push(
                        Rule::DimensionUnfed,
                        format!(
                            "dimension_weights[{}] is fed by no grader's weight_dimensions",
                            gofmt::quote(dim)
                        ),
                    );
                }
            }
        }
    }

    // 11. smoke input size
    if task.smoke_member {
        let tokens = task.estimated_input_tokens();
        if tokens > SMOKE_INPUT_TOKEN_LIMIT {
            push(
                Rule::SmokeInputTooLarge,
                format!(
                    "smoke task input is ~{tokens} tokens; smoke-set inputs must be <= {SMOKE_INPUT_TOKEN_LIMIT} (spec 5.6)"
                ),
            );
        }
    }
}

#[derive(Default)]
struct CapabilityShape {
    tasks: usize,
    tiers: BTreeSet<i64>,
    smoke: usize,
    smoke_easy: bool,
    smoke_hard: bool,
}

/// Spec §5.6 corpus shape, per known capability in sorted order. Anchor gaps are
/// errors; counts and tier span are warnings.
fn check_shape(corpus: &Corpus, report: &mut Report) {
    let mut shapes: BTreeMap<&str, CapabilityShape> = BTreeMap::new();
    for task in &corpus.tasks {
        if !CAPABILITIES.contains(&task.capability.as_str()) {
            continue;
        }
        let shape = shapes.entry(task.capability.as_str()).or_default();
        let tier = task.tier_or_zero();
        shape.tasks += 1;
        shape.tiers.insert(tier);
        if task.smoke_member {
            shape.smoke += 1;
            shape.smoke_easy |= tier <= 2;
            shape.smoke_hard |= tier >= 4;
        }
    }

    for (capability, shape) in &shapes {
        if shape.smoke > 0 {
            if !shape.smoke_easy {
                report.errors.push(shape_violation(
                    Rule::SmokeAnchorMissing,
                    format!(
                        "capability {capability}: smoke set has no easy anchor (tier <= 2) (spec 5.6)"
                    ),
                ));
            }
            if !shape.smoke_hard {
                report.errors.push(shape_violation(
                    Rule::SmokeAnchorMissing,
                    format!(
                        "capability {capability}: smoke set has no hard anchor (tier >= 4) (spec 5.6)"
                    ),
                ));
            }
        }
        if !(20..=25).contains(&shape.tasks) {
            report.warnings.push(shape_violation(
                Rule::CapabilityTaskCount,
                format!(
                    "capability {capability} has {} tasks; spec 5.6 requires 20-25",
                    shape.tasks
                ),
            ));
        }
        let missing: Vec<String> = (1..=5)
            .filter(|tier| !shape.tiers.contains(tier))
            .map(|tier| tier.to_string())
            .collect();
        if !missing.is_empty() {
            report.warnings.push(shape_violation(
                Rule::CapabilityTierSpan,
                format!(
                    "capability {capability} has no tasks at tier(s) [{}]; tasks must span all five tiers (spec 5.6)",
                    missing.join(" ")
                ),
            ));
        }
        if shape.smoke != 5 {
            report.warnings.push(shape_violation(
                Rule::SmokeCount,
                format!(
                    "capability {capability} has {} smoke_member tasks; spec 5.6 requires exactly 5",
                    shape.smoke
                ),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::Attachment;

    /// A task that passes every per-task rule.
    fn valid_task() -> Task {
        Task {
            file: "task_b_001.yaml".to_owned(),
            id: "task_b_001".to_owned(),
            capability: "factual_qa".to_owned(),
            domain_tags: Vec::new(),
            difficulty_tier: Some(2),
            difficulty_knobs: None,
            prompt: "Q?".to_owned(),
            attachments: Vec::new(),
            temperature: 0.0,
            max_tokens: Some(16),
            grader_specs: vec![Some(
                r#"{"spec":{"answers":[{"value":"A"}]},"type":"golden_answer","version":1,"weight_dimensions":{"answer_correctness":1}}"#
                    .to_owned(),
            )],
            dimension_weights: BTreeMap::from([("answer_correctness".to_owned(), 1.0)]),
            pass_threshold: 0.7,
            smoke_member: false,
        }
    }

    fn task_errors(task: Task) -> Vec<String> {
        let mut out = Vec::new();
        validate_task(&task, &ValidationRegistry::v1(), &mut out);
        out.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn valid_task_has_no_errors() {
        assert_eq!(task_errors(valid_task()), Vec::<String>::new());
    }

    #[test]
    fn numeric_boundaries() {
        let mut task = valid_task();
        task.max_tokens = Some(1);
        task.pass_threshold = 1.0;
        task.temperature = 0.0;
        assert_eq!(task_errors(task), Vec::<String>::new());

        let mut task = valid_task();
        task.dimension_weights = BTreeMap::from([
            ("answer_correctness".to_owned(), 1.0),
            ("b".to_owned(), 0.0),
        ]);
        let errors = task_errors(task);
        assert!(
            errors.contains(
                &"task_b_001.yaml: task task_b_001: invalid_field: dimension_weights[\"b\"] = 0 must be in (0, 1]"
                    .to_owned()
            ),
            "{errors:?}"
        );

        let mut task = valid_task();
        task.dimension_weights = BTreeMap::from([("answer_correctness".to_owned(), f64::NAN)]);
        let errors = task_errors(task);
        assert!(errors.iter().any(|e| e.contains("= NaN must be in (0, 1]")));
        assert!(
            !errors.iter().any(|e| e.contains("weights_sum")),
            "{errors:?}"
        );
    }

    #[test]
    fn smoke_input_limit_is_inclusive() {
        let limit = usize::try_from(SMOKE_INPUT_TOKEN_LIMIT).expect("fits");
        let mut task = valid_task();
        task.smoke_member = true;
        task.prompt = "p".repeat(limit * 4 - 10);
        task.attachments = vec![Attachment {
            name: "a.txt".to_owned(),
            content: "c".repeat(10),
        }];
        assert_eq!(task.estimated_input_tokens(), SMOKE_INPUT_TOKEN_LIMIT);
        assert_eq!(task_errors(task.clone()), Vec::<String>::new());

        task.attachments[0].content.push('c');
        assert_eq!(task.estimated_input_tokens(), SMOKE_INPUT_TOKEN_LIMIT + 1);
        assert_eq!(
            task_errors(task),
            [
                "task_b_001.yaml: task task_b_001: smoke_input_too_large: smoke task input is ~32769 tokens; smoke-set inputs must be <= 32768 (spec 5.6)"
            ]
        );
    }

    #[test]
    fn authored_slug_pattern() {
        for ok in ["task_a", "task_sx_017", "task_0", "task_a1_b2_c3"] {
            assert!(is_authored_slug(ok), "{ok}");
        }
        for bad in [
            "task_", "task__a", "task_a_", "Task_a", "task_A", "task-a", "tasks_a", "task_a b", "",
        ] {
            assert!(!is_authored_slug(bad), "{bad}");
        }
    }

    #[test]
    fn file_stem_strips_one_extension() {
        assert_eq!(file_stem("sub/dir/task_x.yml"), "task_x");
        assert_eq!(file_stem("task_x.yaml"), "task_x");
        assert_eq!(file_stem("task_x.yaml.yml"), "task_x.yaml");
        assert_eq!(file_stem("task_x.yml.yaml"), "task_x");
    }

    #[test]
    fn violation_display_omits_empty_parts() {
        let v = Violation {
            file: "a.yaml".to_owned(),
            task_id: String::new(),
            rule: Rule::YamlInvalid,
            message: "m".to_owned(),
        };
        assert_eq!(v.to_string(), "a.yaml: yaml_invalid: m");
        let v = Violation {
            file: String::new(),
            task_id: String::new(),
            rule: Rule::SmokeCount,
            message: "m".to_owned(),
        };
        assert_eq!(v.to_string(), "smoke_count: m");
    }
}
