//! Difficulty ceilings, the `clears` flag, and the banded policy table behind
//! `GET /v1/policies` (spec §6.6, §7.5, §9.3; reference Part 4 §2.4-§2.5).
//!
//! These functions are pure `f64` math with a `1e-9` tolerance. They never widen their
//! inputs. In production the callers read `real` columns (`aggregates.ewma`,
//! `cost_per_run_usd` EWMA, `quality_bar`) as `f64::from(value as f32)` and pass the widened
//! values in, so a stored pass rate of `0.85` reaches [`ceiling`] as `0.8500000238418579`
//! and clears the widened default bar. The goldens and the 4000 monotonicity cases record
//! the exact `f64` values Go's `Ceiling` received, so replaying them needs no widening here.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The default ceiling buffer beta (`MAVERICK_CEILING_BUFFER`).
pub const DEFAULT_CEILING_BUFFER: f64 = 0.05;

/// The three difficulty bands and their tiers (reference §2.5).
pub const BANDS: [(&str, &[u8]); 3] = [("easy", &[1, 2]), ("medium", &[3]), ("hard", &[4, 5])];

/// A pass-rate cell for one difficulty tier.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PassRate {
    /// Difficulty tier, 1 to 5.
    pub tier: u8,
    /// The tier's EWMA pass rate (already f32-widened by the caller in production).
    pub ewma: f64,
    /// Whether the cell has enough observations to be measured.
    pub measured: bool,
}

/// A pass-rate cell tagged with its column and capability, for [`ceilings`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cell {
    /// The model column id.
    pub column_id: String,
    /// The capability the cell measures.
    pub capability: String,
    /// Difficulty tier, 1 to 5.
    pub tier: u8,
    /// The tier's EWMA pass rate.
    pub ewma: f64,
    /// Whether the cell is measured.
    pub measured: bool,
}

/// A candidate column for [`resolve_bands`], with its precomputed ceiling and per-tier cost
/// and pass-rate cells.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BandCandidate {
    /// The model column id.
    pub column_id: String,
    /// The column's ceiling at the capability (from [`ceiling`]).
    pub ceiling: u8,
    /// `cost_per_run_usd` EWMA by tier.
    #[serde(default)]
    pub cost_by_tier: BTreeMap<u8, f64>,
    /// Pass-rate EWMA by tier.
    #[serde(default)]
    pub pass_by_tier: BTreeMap<u8, f64>,
}

/// The band pick returned by [`resolve_bands`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BandPick {
    /// The band name (`easy`, `medium`, `hard`).
    pub band: String,
    /// The band's tiers.
    pub band_tiers: Vec<u8>,
    /// The chosen column id, or `""` when there were no candidates.
    pub column_id: String,
    /// The chosen column's ceiling.
    pub ceiling: u8,
    /// Whether the ceiling clears the band's top tier.
    pub clears_band: bool,
    /// The chosen column's pass rate at the band's top tier, if present.
    pub pass_rate: Option<f64>,
}

/// The largest tier `T` such that every tier `1..=T` has a measured cell whose EWMA clears
/// `bar + buffer` (with a `1e-9` tolerance, so exactly `bar + buffer` clears). A missing tier,
/// an unmeasured cell, or a rate below the threshold stops the contiguous scan. Returns 0 when
/// tier 1 does not clear.
#[must_use]
pub fn ceiling(pass_rates: &[PassRate], bar: f64, buffer: f64) -> u8 {
    let threshold = bar + buffer - 1e-9;
    let mut result = 0;
    for tier in 1..=5u8 {
        let Some(cell) = pass_rates.iter().find(|cell| cell.tier == tier) else {
            break;
        };
        if !cell.measured || cell.ewma < threshold {
            break;
        }
        result = tier;
    }
    result
}

/// Whether a column with the given ceiling clears a difficulty.
#[must_use]
pub fn clears(ceiling: u8, difficulty: u8) -> bool {
    ceiling >= difficulty
}

/// Group cells by (column, capability) and compute each group's [`ceiling`] at that
/// capability's bar. A capability with no bar in `bar_by_capability` uses
/// [`DEFAULT_QUALITY_BAR`](crate::scoring::DEFAULT_QUALITY_BAR). Groups keep every column and
/// capability that has at least one cell, even when the ceiling is 0.
#[must_use]
pub fn ceilings(
    cells: &[Cell],
    bar_by_capability: &BTreeMap<String, f64>,
    buffer: f64,
) -> BTreeMap<String, BTreeMap<String, u8>> {
    let mut groups: BTreeMap<(String, String), Vec<PassRate>> = BTreeMap::new();
    for cell in cells {
        groups
            .entry((cell.column_id.clone(), cell.capability.clone()))
            .or_default()
            .push(PassRate {
                tier: cell.tier,
                ewma: cell.ewma,
                measured: cell.measured,
            });
    }

    let mut out: BTreeMap<String, BTreeMap<String, u8>> = BTreeMap::new();
    for ((column_id, capability), rates) in groups {
        let bar = bar_by_capability
            .get(&capability)
            .copied()
            .unwrap_or(crate::scoring::DEFAULT_QUALITY_BAR);
        let value = ceiling(&rates, bar, buffer);
        out.entry(column_id).or_default().insert(capability, value);
    }
    out
}

/// Pick, for each band, the best candidate in pool order (reference §2.5). "Best" clears the
/// band's top tier if any does; otherwise the highest ceiling; otherwise the cheapest known
/// band cost, keeping the incumbent (pool order) on a tie or when both costs are unknown. With
/// no candidates every band picks the empty column.
#[must_use]
pub fn resolve_bands(candidates: &[BandCandidate]) -> Vec<BandPick> {
    BANDS
        .iter()
        .map(|(name, tiers)| {
            let top = *tiers.last().expect("a band has at least one tier");
            let Some(first) = candidates.first() else {
                return BandPick {
                    band: (*name).to_string(),
                    band_tiers: tiers.to_vec(),
                    column_id: String::new(),
                    ceiling: 0,
                    clears_band: false,
                    pass_rate: None,
                };
            };
            let mut best = first;
            for candidate in &candidates[1..] {
                if better(candidate, best, top, tiers) {
                    best = candidate;
                }
            }
            BandPick {
                band: (*name).to_string(),
                band_tiers: tiers.to_vec(),
                column_id: best.column_id.clone(),
                ceiling: best.ceiling,
                clears_band: best.ceiling >= top,
                pass_rate: best.pass_by_tier.get(&top).copied(),
            }
        })
        .collect()
}

/// Whether candidate `a` beats the incumbent `b` for a band (reference §2.5 `better`).
fn better(a: &BandCandidate, b: &BandCandidate, top: u8, tiers: &[u8]) -> bool {
    let a_clears = a.ceiling >= top;
    let b_clears = b.ceiling >= top;
    if a_clears != b_clears {
        return a_clears;
    }
    if !a_clears {
        match a.ceiling.cmp(&b.ceiling) {
            Ordering::Greater => return true,
            Ordering::Less => return false,
            Ordering::Equal => {}
        }
    }
    match (band_cost(a, tiers), band_cost(b, tiers)) {
        (Some(_), None) => true,
        (None, Some(_)) => false,
        (Some(a_cost), Some(b_cost)) => a_cost < b_cost,
        (None, None) => false,
    }
}

/// The mean of a candidate's costs over the band tiers that have one, or `None` when none do.
fn band_cost(candidate: &BandCandidate, tiers: &[u8]) -> Option<f64> {
    let costs: Vec<f64> = tiers
        .iter()
        .filter_map(|tier| candidate.cost_by_tier.get(tier).copied())
        .collect();
    if costs.is_empty() {
        None
    } else {
        Some(costs.iter().sum::<f64>() / costs.len() as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rate(tier: u8, ewma: f64) -> PassRate {
        PassRate {
            tier,
            ewma,
            measured: true,
        }
    }

    #[test]
    fn exact_bar_plus_buffer_clears_via_tolerance() {
        let rates: Vec<PassRate> = (1..=5).map(|tier| rate(tier, 0.85)).collect();
        assert_eq!(ceiling(&rates, 0.8, 0.05), 5);
    }

    #[test]
    fn tier_one_below_bar_is_zero() {
        assert_eq!(ceiling(&[rate(1, 0.80), rate(2, 0.99)], 0.8, 0.05), 0);
    }

    #[test]
    fn a_gap_stops_the_scan() {
        // Tier 2 is missing, so tier 3 cannot lift the ceiling above 1.
        assert_eq!(ceiling(&[rate(1, 0.99), rate(3, 0.99)], 0.8, 0.05), 1);
    }

    #[test]
    fn an_unmeasured_cell_stops_the_scan() {
        let rates = [
            rate(1, 0.99),
            PassRate {
                tier: 2,
                ewma: 0.99,
                measured: false,
            },
        ];
        assert_eq!(ceiling(&rates, 0.8, 0.05), 1);
    }

    #[test]
    fn clears_is_ge() {
        assert!(clears(3, 3));
        assert!(clears(5, 3));
        assert!(!clears(3, 4));
        assert!(!clears(0, 1));
    }

    #[test]
    fn ewma_exactly_at_the_threshold_clears_but_one_ulp_below_does_not() {
        let (bar, buffer) = (0.8, 0.05);
        let threshold = bar + buffer - 1e-9;
        assert_eq!(
            ceiling(&[rate(1, threshold)], bar, buffer),
            1,
            "exactly bar + buffer - 1e-9 clears"
        );
        let below = f64::from_bits(threshold.to_bits() - 1);
        assert_eq!(
            ceiling(&[rate(1, below)], bar, buffer),
            0,
            "one ULP below the threshold does not clear"
        );
    }

    #[test]
    fn ceilings_defaults_the_bar_for_an_unlisted_capability() {
        let cells = [Cell {
            column_id: "col".to_string(),
            capability: "unlisted".to_string(),
            tier: 1,
            ewma: 0.99,
            measured: true,
        }];
        let out = ceilings(&cells, &BTreeMap::new(), 0.05);
        // 0.99 clears the default bar (0.80) + buffer, so the ceiling is 1.
        assert_eq!(out["col"]["unlisted"], 1);
    }

    #[test]
    fn ceiling_is_monotone_non_increasing_in_the_bar() {
        let rates: Vec<PassRate> = (1..=5).map(|tier| rate(tier, 0.9)).collect();
        let low = ceiling(&rates, 0.5, 0.05);
        let high = ceiling(&rates, 0.95, 0.05);
        assert!(high <= low, "a higher bar never raises the ceiling");
    }

    #[test]
    fn empty_candidates_pick_the_empty_column_in_every_band() {
        let picks = resolve_bands(&[]);
        assert_eq!(picks.len(), 3);
        for pick in picks {
            assert_eq!(pick.column_id, "");
            assert_eq!(pick.ceiling, 0);
            assert!(!pick.clears_band);
            assert_eq!(pick.pass_rate, None);
        }
    }

    #[test]
    fn band_cost_is_a_mean_not_a_sum() {
        // Both clear the easy band (top tier 2), so cost decides. "few" has one costed tier
        // (mean 0.01); "many" has two cheaper tiers (mean 0.006). By the mean "many" wins; by
        // a raw sum "few" would (0.01 < 0.012). "few" is the pool-order incumbent.
        let few = BandCandidate {
            column_id: "few".to_string(),
            ceiling: 2,
            cost_by_tier: BTreeMap::from([(1, 0.01)]),
            pass_by_tier: BTreeMap::new(),
        };
        let many = BandCandidate {
            column_id: "many".to_string(),
            ceiling: 2,
            cost_by_tier: BTreeMap::from([(1, 0.006), (2, 0.006)]),
            pass_by_tier: BTreeMap::new(),
        };
        let picks = resolve_bands(&[few, many]);
        let easy = picks
            .iter()
            .find(|pick| pick.band == "easy")
            .expect("easy band");
        assert_eq!(
            easy.column_id, "many",
            "band cost is the mean over covered tiers, not the sum"
        );
    }

    #[test]
    fn fallback_prefers_the_higher_ceiling_when_nothing_clears() {
        // The hard band's top tier is 5; neither ceiling clears it, so the higher ceiling
        // wins, independent of pool order.
        let low = BandCandidate {
            column_id: "low".to_string(),
            ceiling: 2,
            cost_by_tier: BTreeMap::new(),
            pass_by_tier: BTreeMap::new(),
        };
        let high = BandCandidate {
            column_id: "high".to_string(),
            ceiling: 3,
            cost_by_tier: BTreeMap::new(),
            pass_by_tier: BTreeMap::new(),
        };
        for order in [
            vec![low.clone(), high.clone()],
            vec![high.clone(), low.clone()],
        ] {
            let picks = resolve_bands(&order);
            let hard = picks
                .iter()
                .find(|pick| pick.band == "hard")
                .expect("hard band");
            assert_eq!(
                hard.column_id, "high",
                "higher ceiling wins when nothing clears"
            );
        }
    }
}
