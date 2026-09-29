//! Recommendation entries (reference Part 4 §2.6; spec §8.1, §8.5): one per ranked
//! column, in rank order, with the displayed score and a human-readable reason.
//!
//! Reasons explain; flags are contractual. The reason strings are not contractual per
//! the spec, but parity requires Go's exact wording. D8 adds `low_availability` and D9
//! adds `settings` and `retention_mode` to every entry.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::policy::RetentionMode;
use crate::ranking::display_score;

/// The reason of a pick that does not clear the requested difficulty. The dash is
/// U+2014 with one space on each side.
pub const BELOW_CEILING: &str = "below difficulty ceiling \u{2014} best available";
/// The reason of the pick in `cost` mode.
pub const CHEAPEST_IN_POOL: &str = "cheapest in pool";
/// The reason of the first entry from another provider family than the pick.
pub const CROSS_FAMILY: &str = "cross-family alternative";
/// The reason of an entry whose quality is above the pick's.
pub const ESCALATION: &str = "escalation target";
/// The reason of every other entry.
pub const FALLBACK: &str = "fallback candidate";

/// The catalog fields an entry shows for its column.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntryColumn {
    pub id: String,
    pub model_id: String,
    pub provider: String,
    pub endpoint: String,
    /// The stored `execution_aliases` jsonb, verbatim (`{}` when empty).
    #[serde(default = "empty_object")]
    pub execution_aliases: JsonValue,
}

/// One ranked column as the entry builder reads it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RankedColumn {
    pub column_id: String,
    /// The provider, compared with the pick's for the cross-family slot.
    pub family: String,
    /// Quality EWMA x100; `None` when unmeasured.
    #[serde(default)]
    pub quality: Option<f64>,
    pub clears_difficulty: bool,
    /// D8.
    #[serde(default)]
    pub low_availability: bool,
    /// D9: the column's settings.
    #[serde(default = "empty_object")]
    pub settings: JsonValue,
    /// D9: the column's effective retention mode.
    #[serde(default)]
    pub retention_mode: RetentionMode,
}

fn empty_object() -> JsonValue {
    JsonValue::Object(serde_json::Map::new())
}

/// One entry of the `recommendations` array, in the response field order.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecommendationEntry {
    pub provider: String,
    pub model: String,
    pub endpoint: String,
    pub column_id: String,
    pub execution_aliases: JsonValue,
    /// D9.
    pub settings: JsonValue,
    /// D9.
    pub retention_mode: RetentionMode,
    /// The displayed quality, one decimal; `None` when unmeasured.
    pub score: Option<f64>,
    pub clears_difficulty: bool,
    /// D8.
    pub low_availability: bool,
    pub reason: String,
}

/// Builds the entries for the ranked columns (pick first). `columns` supplies each
/// column's catalog fields; `mode`, `capability`, and `difficulty` shape the pick's
/// reason.
///
/// Reason, first matching rule: the pick is `below difficulty ceiling`, `cheapest in
/// pool` (cost mode), or `cheapest column clearing tier <d> <capability> ...` (every
/// other mode, QUIRK: even when chosen for quality). After the pick, the first entry
/// from another family is the `cross-family alternative` (QUIRK: only once); an entry
/// whose quality beats the pick's is an `escalation target`; the rest are `fallback
/// candidate`s.
pub fn recommendations(
    columns: &[EntryColumn],
    ranked: &[RankedColumn],
    mode: &str,
    capability: &str,
    difficulty: i32,
) -> Vec<RecommendationEntry> {
    let by_id: HashMap<&str, &EntryColumn> = columns
        .iter()
        .map(|column| (column.id.as_str(), column))
        .collect();
    let Some(pick) = ranked.first() else {
        return Vec::new();
    };
    let mut cross_family_used = false;
    let mut entries = Vec::with_capacity(ranked.len());
    for (index, entry) in ranked.iter().enumerate() {
        let reason = if index == 0 {
            pick_reason(entry, mode, capability, difficulty)
        } else if !cross_family_used && entry.family != pick.family {
            cross_family_used = true;
            CROSS_FAMILY.to_owned()
        } else if matches!(
            (entry.quality, pick.quality),
            (Some(quality), Some(pick_quality)) if quality > pick_quality
        ) {
            ESCALATION.to_owned()
        } else {
            FALLBACK.to_owned()
        };
        let column = by_id.get(entry.column_id.as_str());
        entries.push(RecommendationEntry {
            provider: column.map_or_else(String::new, |c| c.provider.clone()),
            model: column.map_or_else(String::new, |c| c.model_id.clone()),
            endpoint: column.map_or_else(String::new, |c| c.endpoint.clone()),
            column_id: entry.column_id.clone(),
            execution_aliases: column.map_or_else(empty_object, |c| c.execution_aliases.clone()),
            settings: entry.settings.clone(),
            retention_mode: entry.retention_mode,
            score: entry.quality.map(display_score),
            clears_difficulty: entry.clears_difficulty,
            low_availability: entry.low_availability,
            reason,
        });
    }
    entries
}

fn pick_reason(pick: &RankedColumn, mode: &str, capability: &str, difficulty: i32) -> String {
    if !pick.clears_difficulty {
        BELOW_CEILING.to_owned()
    } else if mode == "cost" {
        CHEAPEST_IN_POOL.to_owned()
    } else {
        format!(
            "cheapest column clearing tier {difficulty} {capability} at your quality bar in \
             this pool"
        )
    }
}
