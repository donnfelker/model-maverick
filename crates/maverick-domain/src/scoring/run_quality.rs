//! `RunQuality`: one run's quality and pass (spec §6.1, reference Part 3 §1.3).

use std::collections::BTreeMap;

/// A run's status (exact strings from the runner).
pub mod status {
    pub const OK: &str = "ok";
    pub const PROVIDER_ERROR: &str = "provider_error";
    pub const TIMEOUT: &str = "timeout";
    pub const PARSE_FAILURE: &str = "parse_failure";
}

/// One run's score.
#[derive(Debug, Clone, PartialEq)]
pub struct RunScore {
    /// `q_run`. `None` for a `provider_error` or `timeout` run: it has no gradable output
    /// and no `q_run` (D6; Go stored 0). A run whose weighted dimensions are all absent
    /// keeps Go's `Some(0.0)` with `gradable == false`.
    pub q: Option<f64>,
    pub pass: bool,
    /// False when the run enters no quality mean.
    pub gradable: bool,
    /// The weighted dimensions that were present, with their scores.
    pub dimensions: BTreeMap<String, f64>,
    /// The total weight of the present dimensions.
    pub weight_covered: f64,
}

impl RunScore {
    fn ungradable(q: Option<f64>) -> Self {
        RunScore {
            q,
            pass: false,
            gradable: false,
            dimensions: BTreeMap::new(),
            weight_covered: 0.0,
        }
    }
}

/// `RunQuality(weights, graderDims, status, passThreshold)`.
///
/// `grader_dims` has one entry per grader in grader order; `None` is a grader that
/// errored (its dimensions are absent). A `provider_error` or `timeout` run has no
/// `q_run` (D6). Otherwise each weighted dimension scores the mean over the graders that
/// emitted it, absent dimensions are skipped, and `q = Σ w·s / Σ w` over what is present
/// (renormalized). Covering no weight is ungradable. `pass = q >= pass_threshold`
/// (inclusive). Other statuses, including `parse_failure`, are graded normally.
pub fn run_quality(
    weights: &BTreeMap<String, f64>,
    grader_dims: &[Option<BTreeMap<String, f64>>],
    status: &str,
    pass_threshold: f64,
) -> RunScore {
    if status == status::PROVIDER_ERROR || status == status::TIMEOUT {
        return RunScore::ungradable(None);
    }
    let mut dimensions = BTreeMap::new();
    let mut weighted = 0.0;
    let mut covered = 0.0;
    for (dim, &w) in weights {
        let observed: Vec<f64> = grader_dims
            .iter()
            .flatten()
            .filter_map(|dims| dims.get(dim).copied())
            .collect();
        if observed.is_empty() {
            continue;
        }
        let s = observed.iter().sum::<f64>() / observed.len() as f64;
        dimensions.insert(dim.clone(), s);
        weighted += w * s;
        covered += w;
    }
    if covered <= 0.0 {
        return RunScore::ungradable(Some(0.0));
    }
    let q = weighted / covered;
    RunScore {
        q: Some(q),
        pass: q >= pass_threshold,
        gradable: true,
        dimensions,
        weight_covered: covered,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, f64)]) -> BTreeMap<String, f64> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect()
    }

    #[test]
    fn negative_weights_pass_through() {
        // Weights are not validated: -0.5 still counts.
        let score = run_quality(
            &map(&[("a", 1.0), ("b", -0.5)]),
            &[Some(map(&[("a", 1.0), ("b", 1.0)]))],
            status::OK,
            0.5,
        );
        assert_eq!(score.q, Some(1.0));
        assert_eq!(score.weight_covered, 0.5);
    }

    #[test]
    fn negative_coverage_is_ungradable() {
        let score = run_quality(
            &map(&[("a", -1.0)]),
            &[Some(map(&[("a", 1.0)]))],
            status::OK,
            0.0,
        );
        assert!(!score.gradable);
        assert_eq!(score.q, Some(0.0));
        assert!(score.dimensions.is_empty());
    }

    #[test]
    fn threshold_is_inclusive_and_strict_above() {
        let weights = map(&[("a", 1.0)]);
        let at = |s: f64| run_quality(&weights, &[Some(map(&[("a", s)]))], status::OK, 0.5).pass;
        assert!(at(0.5));
        assert!(!at(0.499_999));
        assert_eq!(status::PARSE_FAILURE, "parse_failure");
    }
}
