//! Repeat and grader-disagreement penalties (spec §6.2, §6.3; reference Part 3 §1.5,
//! §1.6).
//!
//! A (task, column) pair's task score is
//! `AmbiguityPenalty(VariancePenalty(q_runs).score, Ambiguity(spreads), λ_amb)`.

use std::collections::BTreeMap;

use crate::gojson::{self, JsonValue};
use crate::graders::Envelope;

/// The default variance weight (`MAVERICK_SCORING_LAMBDA_VAR`).
pub const LAMBDA_VAR: f64 = 0.5;
/// The default ambiguity weight (`MAVERICK_SCORING_LAMBDA_AMB`).
pub const LAMBDA_AMB: f64 = 0.25;

/// Go `math.Max(0, x)`: NaN stays NaN and `-0` becomes `0`.
fn floor_zero(x: f64) -> f64 {
    if x.is_nan() || x > 0.0 { x } else { 0.0 }
}

/// The mean and the population standard deviation (two-pass: mean first).
fn mean_and_stddev(values: &[f64]) -> (f64, f64) {
    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    let variance = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
    (mean, variance.sqrt())
}

/// The result of [`variance_penalty`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Variance {
    /// `max(0, mu - λ_var·sigma)`.
    pub score: f64,
    pub mu: f64,
    /// Population standard deviation (0 for one repeat).
    pub sigma: f64,
    pub n: usize,
    /// False when there were no gradable repeats: the pair has no task score.
    pub ok: bool,
}

/// `VariancePenalty(qRuns, λ_var)` over the gradable repeats' `q_run` values.
pub fn variance_penalty(q_runs: &[f64], lambda_var: f64) -> Variance {
    if q_runs.is_empty() {
        return Variance {
            score: 0.0,
            mu: 0.0,
            sigma: 0.0,
            n: 0,
            ok: false,
        };
    }
    let (mu, sigma) = mean_and_stddev(q_runs);
    Variance {
        score: floor_zero(mu - lambda_var * sigma),
        mu,
        sigma,
        n: q_runs.len(),
        ok: true,
    }
}

/// `SharedDimensionSpread(graderDims)` for one run: the mean, over dimensions with at
/// least two observations across graders, of their population standard deviation.
/// `None` when no dimension was observed twice. Dimensions without task weight count.
pub fn shared_dimension_spread(grader_dims: &[Option<BTreeMap<String, f64>>]) -> Option<f64> {
    let mut observations: BTreeMap<&str, Vec<f64>> = BTreeMap::new();
    for dims in grader_dims.iter().flatten() {
        for (dim, &score) in dims {
            observations.entry(dim.as_str()).or_default().push(score);
        }
    }
    let spreads: Vec<f64> = observations
        .values()
        .filter(|scores| scores.len() >= 2)
        .map(|scores| mean_and_stddev(scores).1)
        .collect();
    if spreads.is_empty() {
        None
    } else {
        Some(spreads.iter().sum::<f64>() / spreads.len() as f64)
    }
}

/// `Ambiguity(spreads)`: the mean of the per-repeat spreads (the repeats that had one),
/// 0 when there are none.
pub fn ambiguity(spreads: &[f64]) -> f64 {
    if spreads.is_empty() {
        0.0
    } else {
        spreads.iter().sum::<f64>() / spreads.len() as f64
    }
}

/// `AmbiguityPenalty(taskScore, a, λ_amb) = max(0, taskScore - λ_amb·a)`.
pub fn ambiguity_penalty(task_score: f64, ambiguity: f64, lambda_amb: f64) -> f64 {
    floor_zero(task_score - lambda_amb * ambiguity)
}

/// `OrderSwapObservations(detail)`: the per-call score maps of a judge detail
/// (`{"order_swap": [{"scores": {...}}, ...]}`), decoded like Go's `json.Unmarshal`.
/// `None` for a missing or empty detail, a decode failure (including `order_swap` not
/// an array, or elements that are not objects), fewer than two calls, or any call
/// without scores.
pub fn order_swap_observations(detail: Option<&str>) -> Option<Vec<BTreeMap<String, f64>>> {
    let detail = detail.filter(|d| !d.is_empty())?;
    let JsonValue::Object(members) = gojson::unmarshal(detail).ok()? else {
        return None;
    };
    let mut calls: Option<Vec<Option<BTreeMap<String, f64>>>> = None;
    for (key, value) in &members {
        if gojson::match_field(key, &["order_swap"]).is_none() {
            continue;
        }
        calls = match value {
            JsonValue::Null => None,
            JsonValue::Array(items) => Some(items.iter().map(decode_call).collect::<Option<_>>()?),
            _ => return None,
        };
    }
    let calls = calls.filter(|calls| calls.len() >= 2)?;
    calls
        .into_iter()
        .map(|scores| scores.filter(|s| !s.is_empty()))
        .collect()
}

/// One call object: `Some(scores)` (`None` inside for a missing or null `scores`), or
/// `None` when it does not decode. (A `null` call has no scores, which ends the same
/// way as a decode failure: no observations.)
fn decode_call(call: &JsonValue) -> Option<Option<BTreeMap<String, f64>>> {
    let JsonValue::Object(members) = call else {
        return None;
    };
    let mut scores = None;
    for (key, value) in members {
        if gojson::match_field(key, &["scores"]).is_none() {
            continue;
        }
        match value {
            JsonValue::Null => scores = None,
            JsonValue::Object(entries) => {
                let map: &mut BTreeMap<String, f64> = scores.get_or_insert_with(BTreeMap::new);
                for (dim, score) in entries {
                    let score = match score {
                        JsonValue::Null => 0.0,
                        JsonValue::Number(literal) => {
                            literal.parse::<f64>().ok().filter(|v| v.is_finite())?
                        }
                        _ => return None,
                    };
                    map.insert(dim.clone(), score);
                }
            }
            _ => return None,
        }
    }
    Some(scores)
}

/// The score maps one grader contributes to [`shared_dimension_spread`] for a run: a
/// judge's per-call maps (each mapped through the envelope's `weight_dimensions`), so a
/// judge alone gives two observations of its dimension; otherwise, or when expansion
/// fails, the grader's persisted mean map. `envelope` is `None` when the grader index
/// has no parsed envelope.
pub fn grader_observations(
    envelope: Option<&Envelope>,
    detail: Option<&str>,
    mean: Option<&BTreeMap<String, f64>>,
) -> Vec<BTreeMap<String, f64>> {
    let expanded = envelope
        .zip(order_swap_observations(detail))
        .and_then(|(envelope, calls)| {
            calls
                .iter()
                .map(|call| {
                    envelope
                        .map_dimensions(Some(call))
                        .ok()
                        .filter(|mapped| !mapped.is_empty())
                })
                .collect::<Option<Vec<_>>>()
        });
    expanded.unwrap_or_else(|| mean.cloned().into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graders::parse_envelope;

    fn map(pairs: &[(&str, f64)]) -> BTreeMap<String, f64> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect()
    }

    #[test]
    fn floors_follow_go_max() {
        assert_eq!(floor_zero(-0.0).to_bits(), 0.0f64.to_bits());
        assert_eq!(floor_zero(-1.0), 0.0);
        assert_eq!(floor_zero(0.5), 0.5);
        assert!(floor_zero(f64::NAN).is_nan());
        assert!(variance_penalty(&[f64::NAN], LAMBDA_VAR).score.is_nan());
        assert!(ambiguity_penalty(f64::NAN, 0.0, LAMBDA_AMB).is_nan());
    }

    #[test]
    fn call_decoding_edges() {
        let calls = |detail: &str| order_swap_observations(Some(detail));
        assert_eq!(
            calls(r#"{"ORDER_SWAP":[{"Scores":{"a":1}},{"scores":{"a":null}}]}"#),
            Some(vec![map(&[("a", 1.0)]), map(&[("a", 0.0)])])
        );
        assert_eq!(calls(r#"{"order_swap":[{"scores":{"a":1}},null]}"#), None);
        assert_eq!(
            calls(r#"{"order_swap":[{"scores":{"a":1}},{"scores":null}]}"#),
            None
        );
        assert_eq!(
            calls(r#"{"order_swap":[{"scores":{"a":1}},{"scores":{"a":"x"}}]}"#),
            None
        );
        assert_eq!(
            calls(r#"{"order_swap":[{"scores":{"a":1}},{"scores":[1]}]}"#),
            None
        );
        assert_eq!(
            calls(r#"{"order_swap":[{"scores":{"a":1}},{"scores":{"a":1e400}}]}"#),
            None
        );
        assert_eq!(calls(r#"{"order_swap":null}"#), None);
        assert_eq!(calls("[]"), None);
        // A later order_swap replaces the earlier one; a repeated scores merges.
        assert_eq!(
            calls(
                r#"{"order_swap":[{"scores":{"a":1}},{"scores":{"a":1}}],"order_swap":[{"scores":{"b":1}}]}"#
            ),
            None
        );
        assert_eq!(
            calls(r#"{"order_swap":[{"scores":{"a":1},"scores":{"b":0.5}},{"scores":{"a":0}}]}"#),
            Some(vec![map(&[("a", 1.0), ("b", 0.5)]), map(&[("a", 0.0)])])
        );
        // A null field followed by a real one keeps the real one, as in Go.
        assert_eq!(
            calls(r#"{"order_swap":null,"order_swap":[{"scores":{"a":1}},{"scores":{"a":0}}]}"#),
            Some(vec![map(&[("a", 1.0)]), map(&[("a", 0.0)])])
        );
        assert_eq!(
            calls(r#"{"order_swap":[{"scores":null,"scores":{"a":1}},{"scores":{"a":0}}]}"#),
            Some(vec![map(&[("a", 1.0)]), map(&[("a", 0.0)])])
        );
        assert_eq!(order_swap_observations(Some("")), None);
        assert_eq!(order_swap_observations(None), None);
    }

    fn envelope(weights: &str) -> Envelope {
        parse_envelope(&format!(
            r#"{{"type":"judge_rubric","version":1,"weight_dimensions":{weights},"spec":{{}}}}"#
        ))
        .expect("envelope")
    }

    #[test]
    fn grader_observations_expand_or_fall_back() {
        let detail = r#"{"order_swap":[{"scores":{"overall":0.8}},{"scores":{"overall":0.6}}]}"#;
        let mean = map(&[("style", 0.7)]);
        // Renamed through weight_dimensions: overall -> style.
        assert_eq!(
            grader_observations(Some(&envelope(r#"{"style":1}"#)), Some(detail), Some(&mean)),
            [map(&[("style", 0.8)]), map(&[("style", 0.6)])]
        );
        // No envelope, no detail, a mapping error, or an empty mapping: the mean map.
        assert_eq!(
            grader_observations(None, Some(detail), Some(&mean)),
            std::slice::from_ref(&mean)
        );
        assert_eq!(
            grader_observations(Some(&envelope(r#"{"style":1}"#)), None, Some(&mean)),
            std::slice::from_ref(&mean)
        );
        assert_eq!(
            grader_observations(
                Some(&envelope(r#"{"x":1,"y":1}"#)),
                Some(detail),
                Some(&mean)
            ),
            std::slice::from_ref(&mean)
        );
        let two = r#"{"order_swap":[{"scores":{"a":0.8,"b":1}},{"scores":{"a":0.6,"b":1}}]}"#;
        assert_eq!(
            grader_observations(Some(&envelope(r#"{"c":1,"d":1}"#)), Some(two), Some(&mean)),
            std::slice::from_ref(&mean)
        );
        // A grader with no mean map (it errored) contributes nothing.
        assert!(grader_observations(None, None, None).is_empty());
    }

    #[test]
    fn spread_uses_every_emitted_dimension() {
        assert_eq!(shared_dimension_spread(&[]), None);
        assert_eq!(
            shared_dimension_spread(&[Some(map(&[("a", 1.0)])), Some(map(&[("a", 0.0)]))]),
            Some(0.5)
        );
        assert_eq!(
            shared_dimension_spread(&[
                Some(map(&[("a", 1.0), ("b", 0.2)])),
                Some(map(&[("a", 0.0), ("b", 0.2)]))
            ]),
            Some(0.25)
        );
    }
}
