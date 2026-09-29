//! Prediction math (reference Part 3 §3 "internal/predictions", §2.8, and §2.11).
//!
//! Before each nightly batch, `prediction_lock` snapshots the pass probability that the
//! pass-rate EWMA implies for every scheduled cell. After the rebuild,
//! `calibration_score` compares the locked probabilities with the runs' pass outcomes.
//! Locking before the results exist makes hindsight impossible.

use std::collections::{BTreeMap, BTreeSet};

/// The `source` of an EWMA-implied prediction.
pub const SOURCE_EWMA: &str = "ewma";

/// A (model column, capability, difficulty tier) cell. The order compares the column
/// id bytewise, then the capability bytewise, then the tier numerically.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Cell {
    pub model_column_id: String,
    pub capability: String,
    pub difficulty_tier: i64,
}

/// A locked prediction for one cell.
#[derive(Debug, Clone, PartialEq)]
pub struct Prediction {
    pub cell: Cell,
    pub pass_prob: f64,
}

/// `Derive(scheduled, passRateEWMA)`: one prediction per distinct scheduled cell that
/// has a pass-rate EWMA, `pass_prob = clamp(ewma, 0, 1)`, sorted by cell.
pub fn derive(scheduled: &[Cell], pass_rate_ewma: &BTreeMap<Cell, f64>) -> Vec<Prediction> {
    scheduled
        .iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter_map(|cell| {
            pass_rate_ewma.get(cell).map(|&ewma| Prediction {
                cell: cell.clone(),
                pass_prob: ewma.clamp(0.0, 1.0),
            })
        })
        .collect()
}

/// A locked probability and the run's pass outcome.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pair {
    pub p: f64,
    pub o: bool,
}

fn outcome(pair: &Pair) -> f64 {
    if pair.o { 1.0 } else { 0.0 }
}

/// `Brier(pairs) = Σ (p - o)² / n` with `o` in {0, 1}; 0 for no pairs.
pub fn brier(pairs: &[Pair]) -> f64 {
    if pairs.is_empty() {
        return 0.0;
    }
    let sum: f64 = pairs
        .iter()
        .map(|pair| (pair.p - outcome(pair)).powi(2))
        .sum();
    sum / pairs.len() as f64
}

/// `Overconfidence(pairs) = (Σp - Σo) / n`, the mean probability minus the pass rate;
/// 0 for no pairs. Positive is overconfident.
pub fn overconfidence(pairs: &[Pair]) -> f64 {
    if pairs.is_empty() {
        return 0.0;
    }
    let predicted: f64 = pairs.iter().map(|pair| pair.p).sum();
    let observed: f64 = pairs.iter().map(outcome).sum();
    (predicted - observed) / pairs.len() as f64
}
