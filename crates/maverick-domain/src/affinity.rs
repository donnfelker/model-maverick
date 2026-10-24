//! Conversation affinity in the ranking (spec §7.6, D44).
//!
//! New in the Rust port. The Go engine ranks each request alone. This module wraps
//! [`ranking::rank`] and never changes it:
//! - Cost-aware off (the default): it ranks the survivors exactly as given and only
//!   reports whether the pick stayed on the stored column P.
//! - Cost-aware on: it ranks copies of the survivors with adjusted inputs. Each cost
//!   becomes the turn cost `C'` (step 1), and P's quality gets the incumbent bonus
//!   (steps 2 and 3). The partitions of §7.4 steps 5 and 6 are untouched (step 4).
//!
//! The caller's survivors are never changed, so the displayed `score` stays P's real
//! quality.
//!
//! [`served_column`] is the telemetry follow rule (spec §9.10): which chain entry served
//! a turn, when it was not the pick.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value as JsonValue;

use crate::policy::{Exclusion, ExclusionReason};
use crate::pricing::Pricing;
use crate::ranking::{self, RankFlags, RankParams, Ranking, ScoredSurvivor, Weights};

/// `MAVERICK_AFFINITY_INCUMBENT_BONUS` default, in quality points (the balanced band).
pub const INCUMBENT_BONUS: f64 = 1.5;

/// The prices that one survivor's turn cost reads, in USD per million tokens.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TurnPrices {
    pub input_per_mtok: Option<f64>,
    pub cache_read_per_mtok: Option<f64>,
    pub cache_write_per_mtok: Option<f64>,
}

impl TurnPrices {
    /// The turn prices of a column's snapshot. An input price of 0 counts as missing,
    /// because an empty snapshot parses to zero prices.
    pub fn from_pricing(pricing: &Pricing) -> Self {
        TurnPrices {
            input_per_mtok: (pricing.input_per_mtok > 0.0).then_some(pricing.input_per_mtok),
            cache_read_per_mtok: pricing.cache_read_per_mtok,
            cache_write_per_mtok: pricing.cache_write_per_mtok,
        }
    }
}

/// What the request and the stored row say about the conversation.
#[derive(Debug, Clone, PartialEq)]
pub struct AffinityContext {
    /// P: the column of the live row. `None` without a live row, and when
    /// `affinity.enabled` is false.
    pub previous_column_id: Option<String>,
    /// `now - last_used_at <= affinity.cache_warm_seconds`, computed by the caller.
    pub cache_warm: bool,
    /// `affinity.context_tokens`: the input tokens that this turn resends.
    pub context_tokens: Option<u64>,
    /// The effective setting: the request's `affinity.cost_aware`, else the tenant's.
    pub cost_aware: bool,
    /// `affinity.incumbent_bonus`, in quality points.
    pub incumbent_bonus: f64,
}

/// The decision status (spec §7.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AffinityStatus {
    /// No live row for the key.
    New,
    /// The pick is P.
    Kept,
    /// P is a survivor and the pick is another column.
    Switched,
    /// P did not survive §7.1 or §7.3.
    Broken,
}

/// Why the pick moved. A closed set; it serializes as the wire string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AffinityReason {
    /// Cost-aware off: the normal ranking picked another column.
    BestModel,
    /// P is under the availability floor and the pick is not.
    LowAvailability,
    /// P violates a request preference and the pick does not.
    Preference,
    /// The bonus was withheld because P does not clear the difficulty.
    Escalation,
    /// Another column won on quality or cost.
    Outranked,
    /// `broken`: P's exclusion reason.
    Excluded(ExclusionReason),
}

impl AffinityReason {
    /// The exact wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            AffinityReason::BestModel => "best_model",
            AffinityReason::LowAvailability => "low_availability",
            AffinityReason::Preference => "preference",
            AffinityReason::Escalation => "escalation",
            AffinityReason::Outranked => "outranked",
            AffinityReason::Excluded(reason) => reason.as_str(),
        }
    }
}

impl Serialize for AffinityReason {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// Why the incumbent bonus was not added to P.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BonusWithheld {
    /// P has `clears_difficulty: false` and another survivor clears it (step 3).
    Escalation,
}

/// Why the turn cost (step 1) did not apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnCostSkipped {
    CostAwareOff,
    NoContextTokens,
    MissingInputPrice,
}

/// The affinity decision and the numbers the trace block needs (spec §10.2).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AffinityDecision {
    pub previous_column_id: Option<String>,
    pub status: AffinityStatus,
    pub reason: Option<AffinityReason>,
    pub cache_warm: bool,
    pub context_tokens: Option<u64>,
    pub cost_aware: bool,
    /// The bonus added to P's quality, `0` when none was.
    pub bonus: f64,
    pub bonus_withheld: Option<BonusWithheld>,
    pub turn_cost_skipped: Option<TurnCostSkipped>,
    /// Each ranked survivor's `C'` when the turn cost applied. A survivor without a
    /// cost EWMA keeps no cost (`None`).
    pub ranking_costs: Option<BTreeMap<String, Option<f64>>>,
}

/// The chain and the affinity decision.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AffinityRanking {
    pub ranking: Ranking,
    pub decision: AffinityDecision,
}

/// The cost-aware adjustments: the inputs `rank` sees and the decision numbers.
struct Adjusted {
    survivors: Vec<ScoredSurvivor>,
    bonus: f64,
    bonus_withheld: Option<BonusWithheld>,
    turn_cost_skipped: Option<TurnCostSkipped>,
    ranking_costs: Option<BTreeMap<String, Option<f64>>>,
}

/// §7.4 with conversation affinity (spec §7.6).
///
/// `survivors`, `mode`, `weights`, and `params` are [`ranking::rank`]'s inputs.
/// `prices` holds each survivor's turn prices by column id; a survivor without an entry
/// has no input price. `exclusions` are the §7.1 exclusions, which name the reason when
/// P did not survive.
pub fn rank_with_affinity(
    survivors: &[ScoredSurvivor],
    prices: &BTreeMap<String, TurnPrices>,
    exclusions: &[Exclusion],
    mode: &str,
    weights: Option<Weights>,
    params: &RankParams,
    affinity: &AffinityContext,
) -> AffinityRanking {
    let previous = affinity.previous_column_id.as_deref();
    let adjusted = affinity
        .cost_aware
        .then(|| adjust(survivors, prices, previous, affinity));
    let ranking = match &adjusted {
        Some(adjusted) => ranking::rank(&adjusted.survivors, mode, weights, params),
        None => ranking::rank(survivors, mode, weights, params),
    };

    let (status, reason) = match previous {
        None => (AffinityStatus::New, None),
        Some(id) => match ranking.ranked.iter().find(|entry| entry.column_id == id) {
            None => (
                AffinityStatus::Broken,
                Some(AffinityReason::Excluded(exclusion_reason(
                    id,
                    &ranking.exclusions,
                    exclusions,
                ))),
            ),
            Some(pinned) => {
                let pick = &ranking.ranked[0];
                if pick.column_id == id {
                    (AffinityStatus::Kept, None)
                } else {
                    let reason = switch_reason(&pinned.flags, &pick.flags, adjusted.as_ref());
                    (AffinityStatus::Switched, Some(reason))
                }
            }
        },
    };

    let decision = AffinityDecision {
        previous_column_id: affinity.previous_column_id.clone(),
        status,
        reason,
        cache_warm: previous.is_some() && affinity.cache_warm,
        context_tokens: affinity.context_tokens,
        cost_aware: affinity.cost_aware,
        bonus: adjusted.as_ref().map_or(0.0, |a| a.bonus),
        bonus_withheld: adjusted.as_ref().and_then(|a| a.bonus_withheld),
        turn_cost_skipped: adjusted
            .as_ref()
            .map_or(Some(TurnCostSkipped::CostAwareOff), |a| a.turn_cost_skipped),
        ranking_costs: adjusted.and_then(|a| a.ranking_costs),
    };
    AffinityRanking { ranking, decision }
}

/// Steps 1 to 3 on copies of the survivors.
fn adjust(
    survivors: &[ScoredSurvivor],
    prices: &BTreeMap<String, TurnPrices>,
    previous: Option<&str>,
    affinity: &AffinityContext,
) -> Adjusted {
    // The survivors that `rank` keeps after its `no_data` step.
    let any_data = survivors.iter().any(|survivor| survivor.has_data);
    let ranked: Vec<&ScoredSurvivor> = survivors
        .iter()
        .filter(|survivor| !any_data || survivor.has_data)
        .collect();
    let pinned = previous.and_then(|id| ranked.iter().copied().find(|s| s.column_id == id));

    // Step 3: a conversation that gets harder moves to a column that clears the tier.
    let escalation = pinned.is_some_and(|pinned| {
        !pinned.clears_difficulty
            && ranked
                .iter()
                .any(|s| s.column_id != pinned.column_id && s.clears_difficulty)
    });
    // Step 2: the bonus needs P's quality.
    let bonus = match pinned {
        Some(pinned) if !escalation && pinned.quality.is_some() => affinity.incumbent_bonus,
        _ => 0.0,
    };

    // Step 1: every ranked survivor needs an input price, or no one gets a turn cost.
    let rates: Option<Vec<f64>> = match affinity.context_tokens {
        None => None,
        Some(_) => ranked
            .iter()
            .map(|survivor| rate(survivor, prices, previous, affinity.cache_warm))
            .collect(),
    };
    let turn_cost_skipped = match (affinity.context_tokens, &rates) {
        (None, _) => Some(TurnCostSkipped::NoContextTokens),
        (Some(_), None) => Some(TurnCostSkipped::MissingInputPrice),
        (Some(_), Some(_)) => None,
    };
    let mut turn_costs: BTreeMap<String, Option<f64>> = BTreeMap::new();
    if let (Some(tokens), Some(rates)) = (affinity.context_tokens, &rates) {
        for (survivor, rate) in ranked.iter().zip(rates) {
            let cost = survivor
                .cost_per_run
                .map(|cost| cost + tokens as f64 * rate / 1e6);
            turn_costs.insert(survivor.column_id.clone(), cost);
        }
    }

    let adjusted = survivors
        .iter()
        .map(|survivor| {
            let mut survivor = survivor.clone();
            if let Some(cost) = turn_costs.get(&survivor.column_id) {
                survivor.cost_per_run = *cost;
            }
            if previous == Some(survivor.column_id.as_str()) {
                survivor.quality = survivor.quality.map(|quality| quality + bonus);
            }
            survivor
        })
        .collect();
    Adjusted {
        survivors: adjusted,
        bonus,
        bonus_withheld: escalation.then_some(BonusWithheld::Escalation),
        turn_cost_skipped,
        ranking_costs: rates.map(|_| turn_costs),
    }
}

/// A survivor's context price: P's cache-read price while its cache is warm, else the
/// cache-write price, else the input price. `None` when it has no input price.
fn rate(
    survivor: &ScoredSurvivor,
    prices: &BTreeMap<String, TurnPrices>,
    previous: Option<&str>,
    cache_warm: bool,
) -> Option<f64> {
    let prices = prices.get(&survivor.column_id)?;
    let input = prices.input_per_mtok?;
    let warm_read = (cache_warm && previous == Some(survivor.column_id.as_str()))
        .then_some(prices.cache_read_per_mtok)
        .flatten();
    Some(warm_read.or(prices.cache_write_per_mtok).unwrap_or(input))
}

/// The first reason that holds, in the spec's order.
fn switch_reason(
    pinned: &RankFlags,
    pick: &RankFlags,
    adjusted: Option<&Adjusted>,
) -> AffinityReason {
    let Some(adjusted) = adjusted else {
        return AffinityReason::BestModel;
    };
    if pinned.low_availability && !pick.low_availability {
        AffinityReason::LowAvailability
    } else if !pinned.prefs_satisfied && pick.prefs_satisfied {
        AffinityReason::Preference
    } else if adjusted.bonus_withheld.is_some() {
        AffinityReason::Escalation
    } else {
        AffinityReason::Outranked
    }
}

/// P's exclusion: `no_data` from the ranking, else its §7.1 reason, else `not_in_pool`
/// (P is no longer in the request's pool).
fn exclusion_reason(id: &str, no_data: &[Exclusion], policy: &[Exclusion]) -> ExclusionReason {
    no_data
        .iter()
        .chain(policy)
        .find(|exclusion| exclusion.column_id == id)
        .map_or(ExclusionReason::NotInPool, |exclusion| exclusion.reason)
}

/// One entry of a stored response chain, as a telemetry item names it (spec §9.10). Other
/// members of the entry are ignored.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ServedEntry {
    pub column_id: String,
    pub model: String,
    pub endpoint: String,
    #[serde(default)]
    pub execution_aliases: BTreeMap<String, JsonValue>,
}

/// The column of the chain entry that served a turn, when that entry is not the pick
/// (spec §9.10, D44). An entry matches when `model_used` and `endpoint_used` are its
/// `model` and `endpoint`, or when `endpoint_used` names one of its execution aliases
/// and `model_used` is that alias's value (D15). The first match decides: when it is
/// index 0, or nothing matches, the row stays where it is (`None`).
pub fn served_column<'a>(
    chain: &'a [ServedEntry],
    model_used: &str,
    endpoint_used: &str,
) -> Option<&'a str> {
    let matches = |entry: &ServedEntry| {
        (entry.model == model_used && entry.endpoint == endpoint_used)
            || entry
                .execution_aliases
                .get(endpoint_used)
                .is_some_and(|alias| alias.as_str() == Some(model_used))
    };
    let first = chain.iter().position(matches)?;
    (first > 0).then(|| chain[first].column_id.as_str())
}
