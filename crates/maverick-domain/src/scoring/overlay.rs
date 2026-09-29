//! The high-stakes overlay (reference Part 3 §1.4).
//!
//! Tasks tagged `medical`, `legal`, or `financial` score under overlay weights (factual
//! accuracy 0.40, calibration 0.20, citation honesty 0.15, task quality 0.25) and fail
//! outright when a grader records an autofail condition: a failed critical rule, or a
//! forbidden fact marked `autofail` that was found.

use std::collections::BTreeMap;

use super::run_quality::RunScore;
use crate::gojson::{self, JsonValue};

/// The standing calibration dimension, added when the task has no calibration class.
pub const CALIBRATION_SAFE_HEDGING: &str = "calibration_safe_hedging";
/// The standing citation dimension, added when the task has no citation class.
pub const CITATION_HONESTY: &str = "citation_honesty";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Class {
    FactualAccuracy,
    Calibration,
    CitationHonesty,
    TaskQuality,
}

impl Class {
    fn of(dimension: &str) -> Class {
        match dimension {
            "accuracy"
            | "field_accuracy"
            | "answer_correctness"
            | "final_answer_correctness"
            | "functional_correctness"
            | "adequacy"
            | "faithfulness"
            | "hallucination_free"
            | "meaning_preservation"
            | "precision"
            | "false_premise_detection"
            | "verdict_accuracy" => Class::FactualAccuracy,
            "calibrated_hedging" | "grounded_refusal" | "calibration_safe_hedging" => {
                Class::Calibration
            }
            "citation_honesty" => Class::CitationHonesty,
            _ => Class::TaskQuality,
        }
    }

    fn weight(self) -> f64 {
        match self {
            Class::FactualAccuracy => 0.40,
            Class::Calibration => 0.20,
            Class::CitationHonesty => 0.15,
            Class::TaskQuality => 0.25,
        }
    }
}

/// `HighStakes(tags)`: any tag exactly `medical`, `legal`, or `financial`.
pub fn high_stakes<S: AsRef<str>>(domain_tags: &[S]) -> bool {
    domain_tags
        .iter()
        .any(|t| matches!(t.as_ref(), "medical" | "legal" | "financial"))
}

/// `OverlayWeights(taskWeights)`: each positive-weight dimension gets its class weight
/// split in proportion to its native weight within the class; non-positive weights are
/// dropped; a class with no members contributes nothing. A task with no calibration
/// dimension gets `calibration_safe_hedging = 0.20`, and one with no citation dimension
/// gets `citation_honesty = 0.15`.
pub fn overlay_weights(task_weights: &BTreeMap<String, f64>) -> BTreeMap<String, f64> {
    let mut sums: BTreeMap<Class, f64> = BTreeMap::new();
    for (dim, &w) in task_weights {
        // Only positive weights count (a negative or NaN weight adds 0).
        *sums.entry(Class::of(dim)).or_default() += w.max(0.0);
    }
    let class_sum = |class: Class| sums.get(&class).copied().unwrap_or(0.0);
    let mut out = BTreeMap::new();
    for (dim, &w) in task_weights {
        // A positive weight makes its class sum positive, so the division is safe.
        if w > 0.0 {
            let class = Class::of(dim);
            out.insert(dim.clone(), class.weight() * w / class_sum(class));
        }
    }
    if class_sum(Class::Calibration) <= 0.0 {
        out.insert(
            CALIBRATION_SAFE_HEDGING.to_owned(),
            Class::Calibration.weight(),
        );
    }
    if class_sum(Class::CitationHonesty) <= 0.0 {
        out.insert(CITATION_HONESTY.to_owned(), Class::CitationHonesty.weight());
    }
    out
}

/// The autofail markers of one grader detail, decoded like Go's `json.Unmarshal` into
/// `{critical_failed bool, forbidden_facts [{found, autofail bool}]}`. `None` when the
/// blob does not decode (empty, malformed, or a wrong type in those fields); `null`
/// decodes to no marker.
fn detail_autofails(detail: &str) -> Option<bool> {
    let members = match gojson::unmarshal(detail).ok()? {
        JsonValue::Null => return Some(false),
        JsonValue::Object(members) => members,
        _ => return None,
    };
    let mut critical_failed = false;
    let mut forbidden_hit = false;
    for (key, value) in &members {
        match gojson::match_field(key, &["critical_failed", "forbidden_facts"]) {
            Some("critical_failed") => match value {
                JsonValue::Bool(b) => critical_failed = *b,
                JsonValue::Null => {}
                _ => return None,
            },
            Some(_) => match value {
                JsonValue::Null => forbidden_hit = false,
                JsonValue::Array(facts) => {
                    forbidden_hit = false;
                    for fact in facts {
                        forbidden_hit |= fact_autofails(fact)?;
                    }
                }
                _ => return None,
            },
            None => {}
        }
    }
    Some(critical_failed || forbidden_hit)
}

/// One forbidden fact: `found && autofail`; `None` on a type error.
fn fact_autofails(fact: &JsonValue) -> Option<bool> {
    let members = match fact {
        JsonValue::Null => return Some(false),
        JsonValue::Object(members) => members,
        _ => return None,
    };
    let (mut found, mut autofail) = (false, false);
    for (key, value) in members {
        let slot = match gojson::match_field(key, &["found", "autofail"]) {
            Some("found") => &mut found,
            Some(_) => &mut autofail,
            None => continue,
        };
        match value {
            JsonValue::Bool(b) => *slot = *b,
            JsonValue::Null => {}
            _ => return None,
        }
    }
    Some(found && autofail)
}

/// `Autofail(details)`: true when any decodable grader detail (in grader order) has
/// `critical_failed` or a found forbidden fact marked `autofail`. `None` entries and
/// blobs that do not decode are skipped.
pub fn autofail(details: &[Option<&str>]) -> bool {
    details
        .iter()
        .flatten()
        .any(|detail| detail_autofails(detail) == Some(true))
}

/// `ApplyAutofail(rs, details)`: on autofail the run scores 0 and fails; gradability and
/// dimensions stay (an autofailed run is a real zero that enters the mean). A run with
/// no `q_run` (D6) keeps none.
pub fn apply_autofail(score: &mut RunScore, details: &[Option<&str>]) {
    if autofail(details) {
        score.q = score.q.map(|_| 0.0);
        score.pass = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_and_negative_weights_are_dropped() {
        let weights: BTreeMap<String, f64> =
            [("accuracy", 0.0), ("field_accuracy", 1.0), ("style", -1.0)]
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v))
                .collect();
        let out = overlay_weights(&weights);
        assert_eq!(
            out.keys().collect::<Vec<_>>(),
            [
                "calibration_safe_hedging",
                "citation_honesty",
                "field_accuracy"
            ]
        );
        assert_eq!(out["field_accuracy"], 0.40);
    }

    #[test]
    fn decoding_edges() {
        assert_eq!(detail_autofails("null"), Some(false));
        assert_eq!(detail_autofails(""), None);
        assert_eq!(detail_autofails("[]"), None);
        assert_eq!(detail_autofails(r#"{"Critical_Failed":true}"#), Some(true));
        assert_eq!(detail_autofails(r#"{"critical_failed":null}"#), Some(false));
        assert_eq!(
            detail_autofails(r#"{"critical_failed":true,"critical_failed":false}"#),
            Some(false)
        );
        assert_eq!(detail_autofails(r#"{"forbidden_facts":{}}"#), None);
        assert_eq!(detail_autofails(r#"{"forbidden_facts":[1]}"#), None);
        assert_eq!(
            detail_autofails(r#"{"forbidden_facts":[null]}"#),
            Some(false)
        );
        assert_eq!(
            detail_autofails(r#"{"forbidden_facts":[{"found":true,"autofail":"yes"}]}"#),
            None
        );
        assert_eq!(
            detail_autofails(r#"{"forbidden_facts":[{"FOUND":true,"Autofail":true,"id":1}]}"#),
            Some(true)
        );
        assert_eq!(
            detail_autofails(r#"{"forbidden_facts":[{"found":true,"autofail":null}]}"#),
            Some(false)
        );
        // A later forbidden_facts field replaces the earlier list (Go decodes a fresh slice).
        assert_eq!(
            detail_autofails(
                r#"{"forbidden_facts":[{"found":true,"autofail":true}],"forbidden_facts":[]}"#
            ),
            Some(false)
        );
        assert_eq!(
            detail_autofails(
                r#"{"forbidden_facts":[{"found":true,"autofail":true}],"forbidden_facts":null}"#
            ),
            Some(false)
        );
        // Unknown fields of any type are ignored.
        assert_eq!(
            detail_autofails(r#"{"rules":"x","critical_failed":true}"#),
            Some(true)
        );
    }

    #[test]
    fn autofail_never_gives_an_error_run_a_score() {
        let mut score = RunScore {
            q: None,
            pass: false,
            gradable: false,
            dimensions: BTreeMap::new(),
            weight_covered: 0.0,
        };
        apply_autofail(&mut score, &[Some(r#"{"critical_failed":true}"#)]);
        assert_eq!(score.q, None);
    }
}
