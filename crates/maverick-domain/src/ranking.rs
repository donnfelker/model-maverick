//! Ranking (spec §7.4; reference Part 4 §2.1 to §2.3): orders the policy survivors into
//! the pick (index 0) and the pool-constrained fallback chain.
//!
//! Pure: the caller scores each survivor from the cell's latest aggregates first
//! (`ScoredSurvivor`). Two Rust-only rules apply on top of Go's `Rank`:
//! - D8: a column whose trailing 7-night availability is below the floor ranks after
//!   every column at or above it and carries `low_availability: true`.
//! - D9: each entry carries its column's `settings` and effective `retention_mode`.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::policy::{Exclusion, ExclusionReason, RetentionMode};

/// `MAVERICK_RANKING_BAND_QUALITY` default, in quality points.
pub const BAND_QUALITY: f64 = 0.0;
/// `MAVERICK_RANKING_BAND_BALANCED` default (also the band of unknown modes).
pub const BAND_BALANCED: f64 = 1.5;
/// `MAVERICK_RANKING_BAND_COST` default.
pub const BAND_COST: f64 = 5.5;
/// `MAVERICK_RANKING_NOISE_BAND` default: the tie window in quality points.
pub const NOISE_BAND: f64 = 0.75;
/// `MAVERICK_RANKING_AVAILABILITY_FLOOR` default (D8).
pub const AVAILABILITY_FLOOR: f64 = 0.9;
/// The detail of a `no_data` exclusion.
pub const NO_DATA_DETAIL: &str = "no aggregate row for the workload cell";

/// The ranking knobs (config, spec §14.1).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RankParams {
    pub band_quality: f64,
    pub band_balanced: f64,
    pub band_cost: f64,
    pub noise_band: f64,
    /// D8. Go had no availability input.
    #[serde(default = "default_availability_floor")]
    pub availability_floor: f64,
}

fn default_availability_floor() -> f64 {
    AVAILABILITY_FLOOR
}

impl Default for RankParams {
    fn default() -> Self {
        RankParams {
            band_quality: BAND_QUALITY,
            band_balanced: BAND_BALANCED,
            band_cost: BAND_COST,
            noise_band: NOISE_BAND,
            availability_floor: AVAILABILITY_FLOOR,
        }
    }
}

impl RankParams {
    /// The leader band of a mode: `quality`, `cost`, and anything else as `balanced`.
    pub fn band(&self, mode: &str) -> f64 {
        match mode {
            "quality" => self.band_quality,
            "cost" => self.band_cost,
            _ => self.band_balanced,
        }
    }
}

/// Value weights over normalized quality, cost, and latency.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Weights {
    pub quality: f64,
    pub cost: f64,
    pub latency: f64,
}

/// `DefaultWeights(mode)`: the weights of a tenant with no `tenant_weights` row for the
/// capability. Any mode other than `quality` or `cost` (including empty) is `balanced`.
pub fn default_weights(mode: &str) -> Weights {
    match mode {
        "quality" => Weights {
            quality: 0.8,
            cost: 0.1,
            latency: 0.1,
        },
        "cost" => Weights {
            quality: 0.25,
            cost: 0.6,
            latency: 0.15,
        },
        _ => Weights {
            quality: 0.5,
            cost: 0.35,
            latency: 0.15,
        },
    }
}

/// `displayScore`: the absolute quality (x100) shown on an entry, to one decimal,
/// rounding the float64 product half away from zero. Ranking normalization is never
/// shown.
pub fn display_score(quality: f64) -> f64 {
    (quality * 10.0).round() / 10.0
}

/// `HighStakes(tags)`: any tag is exactly `medical`, `legal`, or `financial`
/// (case-sensitive).
pub fn high_stakes<S: AsRef<str>>(tags: &[S]) -> bool {
    tags.iter()
        .any(|tag| matches!(tag.as_ref(), "medical" | "legal" | "financial"))
}

/// One policy survivor with its score inputs for the workload cell (spec §7.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoredSurvivor {
    pub column_id: String,
    /// Quality EWMA x100, from the quality source column.
    #[serde(default)]
    pub quality: Option<f64>,
    /// `cost_per_run_usd` EWMA of the column itself.
    #[serde(default)]
    pub cost_per_run: Option<f64>,
    /// `latency_p95_ms` EWMA of the column itself.
    #[serde(default)]
    pub latency_p95: Option<f64>,
    /// D8: mean availability over the trailing 7 nights. `None` (unmeasured) is never
    /// below the floor.
    #[serde(default)]
    pub availability: Option<f64>,
    /// The provider, for same-family tie-breaks.
    pub family: String,
    /// Any aggregate row exists for the cell.
    pub has_data: bool,
    pub measured: bool,
    pub clears_difficulty: bool,
    pub prefs_satisfied: bool,
    /// D9: the column's `settings`, carried to its entry.
    #[serde(default = "empty_object")]
    pub settings: JsonValue,
    /// D9: the column's effective retention mode, carried to its entry.
    #[serde(default)]
    pub retention_mode: RetentionMode,
}

fn empty_object() -> JsonValue {
    JsonValue::Object(serde_json::Map::new())
}

/// The flags of a ranked entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RankFlags {
    /// Band membership only: after the tie pass a leader can sit below a non-leader.
    pub leader: bool,
    pub measured: bool,
    pub clears_difficulty: bool,
    pub prefs_satisfied: bool,
    /// D8.
    pub low_availability: bool,
}

/// One entry of the chain, in rank order.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RankedEntry {
    pub column_id: String,
    /// 0-based position across every partition.
    pub rank: usize,
    /// The normalized value used to order leaders.
    pub v: f64,
    pub flags: RankFlags,
    /// D9.
    pub settings: JsonValue,
    /// D9.
    pub retention_mode: RetentionMode,
}

/// The ranked chain and the `no_data` exclusions.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Ranking {
    pub ranked: Vec<RankedEntry>,
    pub exclusions: Vec<Exclusion>,
}

/// A kept survivor during ranking.
struct Candidate<'a> {
    survivor: &'a ScoredSurvivor,
    measured: bool,
    v: f64,
    low_availability: bool,
}

impl Candidate<'_> {
    fn quality(&self) -> Option<f64> {
        self.survivor.quality
    }

    fn cost(&self) -> Option<f64> {
        self.survivor.cost_per_run
    }
}

/// `Rank(survivors, mode, weights?, params)`.
///
/// 1. `no_data`: when some survivor has data, drop the ones without it; when none has
///    data, keep them all as unmeasured (G1).
/// 2. Weights: the tenant's, else [`default_weights`].
/// 3. Min-max normalize quality, cost (inverted), and latency (inverted) over the kept
///    set and compute `V`.
/// 4. Partition by availability (D8), then by preferences; order each partition by the
///    mode's quality band, `V`, the quality-ordered tail, and the noise tie pass.
pub fn rank(
    survivors: &[ScoredSurvivor],
    mode: &str,
    weights: Option<Weights>,
    params: &RankParams,
) -> Ranking {
    let any_data = survivors.iter().any(|survivor| survivor.has_data);
    let mut exclusions = Vec::new();
    let mut kept = Vec::with_capacity(survivors.len());
    for survivor in survivors {
        if !any_data {
            kept.push(candidate(survivor, false, params));
        } else if survivor.has_data {
            kept.push(candidate(survivor, survivor.measured, params));
        } else {
            exclusions.push(Exclusion {
                column_id: survivor.column_id.clone(),
                reason: ExclusionReason::NoData,
                detail: NO_DATA_DETAIL.to_owned(),
            });
        }
    }

    let weights = weights.unwrap_or_else(|| default_weights(mode));
    let qualities: Vec<Option<f64>> = kept.iter().map(Candidate::quality).collect();
    let costs: Vec<Option<f64>> = kept.iter().map(Candidate::cost).collect();
    let latencies: Vec<Option<f64>> = kept.iter().map(|c| c.survivor.latency_p95).collect();
    let nq = normalize(&qualities, false);
    let nc = normalize(&costs, true);
    let nl = normalize(&latencies, true);
    for (i, candidate) in kept.iter_mut().enumerate() {
        // `w_q*nq + w_c*nc + w_l*nl`, with both additions fused exactly as Go's arm64
        // backend fused them when the parity fixtures were recorded:
        // `fma(w_l, nl, fma(w_c, nc, w_q*nq))`. Plain arithmetic differs in the last
        // bit on some inputs. `mul_add` is correctly rounded on every platform.
        candidate.v = weights
            .latency
            .mul_add(nl[i], weights.cost.mul_add(nc[i], weights.quality * nq[i]));
    }

    let band = params.band(mode);
    let mut order = Vec::with_capacity(kept.len());
    for low_availability in [false, true] {
        for prefs_satisfied in [true, false] {
            let members: Vec<usize> = (0..kept.len())
                .filter(|&i| {
                    kept[i].low_availability == low_availability
                        && kept[i].survivor.prefs_satisfied == prefs_satisfied
                })
                .collect();
            order.extend(order_partition(&members, &kept, band, params.noise_band));
        }
    }

    let ranked = order
        .into_iter()
        .enumerate()
        .map(|(rank, (i, leader))| {
            let candidate = &kept[i];
            let survivor = candidate.survivor;
            RankedEntry {
                column_id: survivor.column_id.clone(),
                rank,
                v: candidate.v,
                flags: RankFlags {
                    leader,
                    measured: candidate.measured,
                    clears_difficulty: survivor.clears_difficulty,
                    prefs_satisfied: survivor.prefs_satisfied,
                    low_availability: candidate.low_availability,
                },
                settings: survivor.settings.clone(),
                retention_mode: survivor.retention_mode,
            }
        })
        .collect();
    Ranking { ranked, exclusions }
}

fn candidate<'a>(
    survivor: &'a ScoredSurvivor,
    measured: bool,
    params: &RankParams,
) -> Candidate<'a> {
    Candidate {
        survivor,
        measured,
        v: 0.0,
        low_availability: survivor
            .availability
            .is_some_and(|availability| availability < params.availability_floor),
    }
}

/// Min-max over the candidates that have the value: `1.0` for a degenerate span, else
/// `(x - min) / (max - min)`; inverted dimensions use `1 - m`. A candidate without the
/// value scores `0`. QUIRK: in a degenerate span quality reads 1.0 but cost and latency
/// read 0.0.
fn normalize(values: &[Option<f64>], invert: bool) -> Vec<f64> {
    let present = || values.iter().flatten().copied();
    let min = present().fold(f64::INFINITY, f64::min);
    let max = present().fold(f64::NEG_INFINITY, f64::max);
    values
        .iter()
        .map(|value| match value {
            None => 0.0,
            Some(x) => {
                let m = if max <= min {
                    1.0
                } else {
                    (x - min) / (max - min)
                };
                if invert { 1.0 - m } else { m }
            }
        })
        .collect()
}

/// Orders one partition: leaders (within `band` of the partition's best quality, or
/// everyone when no member has quality) by `V` descending, then the rest by quality
/// descending with unmeasured quality last, then the noise tie pass. Returns
/// `(candidate index, leader)` pairs.
fn order_partition(
    members: &[usize],
    kept: &[Candidate<'_>],
    band: f64,
    noise_band: f64,
) -> Vec<(usize, bool)> {
    let max_quality = members
        .iter()
        .filter_map(|&i| kept[i].quality())
        .reduce(f64::max);
    let is_leader = |i: usize| match max_quality {
        None => true,
        Some(max) => kept[i]
            .quality()
            .is_some_and(|quality| quality >= max - band),
    };
    let (mut leaders, mut tail): (Vec<usize>, Vec<usize>) =
        members.iter().partition(|&&i| is_leader(i));
    leaders.sort_by(|&a, &b| kept[b].v.partial_cmp(&kept[a].v).unwrap_or(Ordering::Equal));
    tail.sort_by(|&a, &b| match (kept[a].quality(), kept[b].quality()) {
        (Some(qa), Some(qb)) => qb.partial_cmp(&qa).unwrap_or(Ordering::Equal),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    });
    let leader_count = leaders.len();
    let seq: Vec<(usize, bool)> = leaders
        .into_iter()
        .chain(tail)
        .enumerate()
        .map(|(position, i)| (i, position < leader_count))
        .collect();
    noise_pass(seq, kept, noise_band)
}

/// The noise tie pass: repeatedly move the tie replacement for the current head to the
/// output. QUIRK: a displaced head is not re-queued; it stays first in the remainder.
fn noise_pass(
    mut remaining: Vec<(usize, bool)>,
    kept: &[Candidate<'_>],
    noise_band: f64,
) -> Vec<(usize, bool)> {
    let mut out = Vec::with_capacity(remaining.len());
    while !remaining.is_empty() {
        let at = tie_replacement(&remaining, kept, noise_band);
        out.push(remaining.remove(at));
    }
    out
}

/// `tieReplacement(rem)`: inside the contiguous run within `noise_band` quality of the
/// head, the cheapest candidate cheaper than the head replaces it, except that a
/// cheaper same-family sibling beats a cheaper cross-family one. `0` keeps the head.
fn tie_replacement(remaining: &[(usize, bool)], kept: &[Candidate<'_>], noise_band: f64) -> usize {
    let head = &kept[remaining[0].0];
    let (Some(head_quality), Some(head_cost)) = (head.quality(), head.cost()) else {
        return 0;
    };
    let head_family = &head.survivor.family;
    let mut cheapest: Option<(usize, f64)> = None;
    let mut same_family: Option<(usize, f64)> = None;
    for (at, &(i, _)) in remaining.iter().enumerate().skip(1) {
        let other = &kept[i];
        let Some(quality) = other.quality() else {
            break;
        };
        if (quality - head_quality).abs() > noise_band {
            break;
        }
        let Some(cost) = other.cost() else {
            continue;
        };
        if cost >= head_cost {
            continue;
        }
        if cheapest.is_none_or(|(_, best)| cost < best) {
            cheapest = Some((at, cost));
        }
        if other.survivor.family == *head_family && same_family.is_none_or(|(_, best)| cost < best)
        {
            same_family = Some((at, cost));
        }
    }
    // Go: no cheaper candidate keeps the head; a cross-family cheapest yields to a
    // same-family sibling; otherwise the cheapest wins. When the cheapest is already
    // same-family it is also the cheapest sibling (both keep the first on ties), so the
    // rule reduces to "sibling, else cheapest, else the head".
    same_family.or(cheapest).map_or(0, |(at, _)| at)
}
