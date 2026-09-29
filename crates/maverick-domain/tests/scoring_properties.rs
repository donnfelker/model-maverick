//! The Go scoring property tests (reference Part 3 §6, "Property tests"). Go's PCG seeds
//! do not reproduce outside Go, so these are properties over proptest's generators.

use std::collections::BTreeMap;

use maverick_domain::scoring::{
    ALPHA, LAMBDA_AMB, LAMBDA_VAR, Stream, ambiguity_penalty, run_quality, shared_dimension_spread,
    status, variance_penalty,
};
use proptest::prelude::*;

/// One grader: `None` errored; otherwise each dimension present with probability 2/3.
fn grader(dims: usize) -> impl Strategy<Value = Option<BTreeMap<String, f64>>> {
    let present = prop::collection::vec((0u8..3, 0.0f64..=1.0), dims).prop_map(|slots| {
        slots
            .into_iter()
            .enumerate()
            .filter(|(_, (roll, _))| *roll < 2)
            .map(|(i, (_, score))| (format!("d{i}"), score))
            .collect::<BTreeMap<_, _>>()
    });
    (0u8..5, present).prop_map(|(roll, dims)| (roll > 0).then_some(dims))
}

type Weights = BTreeMap<String, f64>;
type GraderDims = Vec<Option<BTreeMap<String, f64>>>;

fn run_case() -> impl Strategy<Value = (Weights, GraderDims)> {
    (1usize..=5).prop_flat_map(|dims| {
        (
            prop::collection::vec(0.01f64..=1.01, dims).prop_map(|ws| {
                ws.into_iter()
                    .enumerate()
                    .map(|(i, w)| (format!("d{i}"), w))
                    .collect()
            }),
            prop::collection::vec(grader(dims), 1..=3),
        )
    })
}

fn population_sigma(values: &[f64]) -> f64 {
    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    (values.iter().map(|v| (v - mean) * (v - mean)).sum::<f64>() / n).sqrt()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]

    /// parity: scoring.TestRunQualityBoundsProperty
    #[test]
    fn run_quality_bounds((weights, graders) in run_case()) {
        let score = run_quality(&weights, &graders, status::OK, 0.7);
        let q = score.q.expect("an ok run has q_run");
        if score.gradable {
            prop_assert!((0.0..=1.0).contains(&q), "q {q}");
        } else {
            prop_assert_eq!(q, 0.0);
        }
    }

    /// parity: scoring.TestPenaltyProperties
    #[test]
    fn penalty_properties(
        q in prop::collection::vec(0.0f64..=1.0, 1..=10),
        spread in 0.0f64..=0.5,
    ) {
        let v = variance_penalty(&q, LAMBDA_VAR);
        prop_assert!(v.sigma >= 0.0);
        prop_assert!(v.score >= 0.0 && v.score <= v.mu + 1e-15, "{v:?}");
        let penalized = ambiguity_penalty(v.score, spread, LAMBDA_AMB);
        prop_assert!((0.0..=v.score).contains(&penalized), "{penalized} vs {v:?}");
    }

    /// parity: scoring.TestSharedDimensionSpreadBoundsProperty
    #[test]
    fn shared_dimension_spread_bounds(values in prop::collection::vec(0.0f64..=1.0, 2..=4)) {
        let graders: Vec<Option<BTreeMap<String, f64>>> = values
            .iter()
            .map(|v| Some(BTreeMap::from([("dim".to_owned(), *v)])))
            .collect();
        let spread = shared_dimension_spread(&graders).expect("two or more observations");
        prop_assert!((0.0..=0.5).contains(&spread), "spread {spread}");
    }

    /// parity: scoring.TestVariancePenaltySigmaMatchesDirectFormula
    #[test]
    fn variance_sigma_matches_direct_formula(q in prop::collection::vec(0.0f64..=1.0, 1..=12)) {
        let v = variance_penalty(&q, LAMBDA_VAR);
        prop_assert!((v.sigma - population_sigma(&q)).abs() <= 1e-12);
        prop_assert_eq!(v.n, q.len());
    }

    /// parity: scoring.TestEWMABoundsProperty
    #[test]
    fn ewma_bounds(
        alpha in 0.001f64..0.999,
        observations in prop::collection::vec(0.0f64..=1.0, 50),
    ) {
        let mut stream = Stream::new(alpha);
        for x in observations {
            stream.fold(x);
            prop_assert!((0.0..=1.0).contains(&stream.ewma()), "ewma {}", stream.ewma());
            prop_assert!(stream.variance() >= 0.0);
        }
    }

    /// parity: scoring.TestResumeMatchesDirectFoldProperty
    #[test]
    fn resume_matches_direct_fold(
        alpha in 0.05f64..1.0,
        observations in prop::collection::vec(0.0f64..=1.0, 1..=20),
    ) {
        let mut direct = Stream::new(alpha);
        let mut series = Vec::new();
        for x in &observations {
            direct.fold(*x);
            series.push(direct.ewma());
        }
        let resumed = Stream::resume(alpha, &series);
        prop_assert_eq!(resumed.n(), direct.n());
        prop_assert!((resumed.ewma() - direct.ewma()).abs() <= 1e-9);
        prop_assert!((resumed.variance() - direct.variance()).abs() <= 1e-9);
    }
}

#[test]
fn defaults_used_by_the_properties() {
    assert_eq!((ALPHA, LAMBDA_VAR, LAMBDA_AMB), (0.3, 0.5, 0.25));
}
