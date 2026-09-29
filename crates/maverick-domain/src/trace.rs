//! The decision-trace candidates document (reference part 4 §4.3; spec §10.2; G6).
//!
//! Every `recommendations` row stores the candidates the decision considered, as one JSON
//! array: the survivors in rank order, then the exclusions (policy order, then `no_data`).
//! A decision is auditable from this stored data alone.
//!
//! Each candidate is written in the Go struct's exact field order:
//! `column_id, provider, model, endpoint, status, rank, quality, pass_rate,
//! cost_per_run_usd, latency_p95_ms, v, leader, measured, clears_difficulty,
//! prefs_satisfied, exclusion_reason, detail`. The first five fields are always written.
//! Every later field is omitted when unset (Go `omitempty`): an unset number or boolean is
//! `None`, and an empty `exclusion_reason` or `detail` is unset. A set value is written even
//! when it is zero or `false` (Go pointers). Numbers use Go's JSON number format (`88`, not
//! `88.0`).
//!
//! D8 adds `availability` and `low_availability` on survivors. They are written after every
//! Go field, so a Go-shaped trace is a field-by-field prefix of a Rust one.

use crate::gojson::{self, JsonValue};
use crate::policy::Exclusion;
use crate::ranking::{self, RankedEntry};

/// A candidate's status in the trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CandidateStatus {
    /// Survived policy and was ranked.
    #[default]
    Survivor,
    /// Excluded by policy or by ranking (`no_data`).
    Excluded,
}

impl CandidateStatus {
    /// The stored value: `survivor` or `excluded`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Survivor => "survivor",
            Self::Excluded => "excluded",
        }
    }
}

/// Where a column runs. Empty strings are written as empty strings.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ColumnInfo {
    /// The provider, such as `anthropic`.
    pub provider: String,
    /// The model id.
    pub model: String,
    /// The endpoint, such as `direct`.
    pub endpoint: String,
}

/// The measurements a survivor's trace records.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SurvivorMetrics {
    /// Quality EWMA x100 from the quality source. The trace shows its display score
    /// ([`ranking::display_score`], rounded to 0.1).
    pub quality: Option<f64>,
    /// Pass-rate EWMA at the request difficulty, from the quality source.
    pub pass_rate: Option<f64>,
    /// Cost per run, raw.
    pub cost_per_run_usd: Option<f64>,
    /// p95 latency, raw.
    pub latency_p95_ms: Option<f64>,
    /// D8: mean availability over the trailing 7 nights.
    pub availability: Option<f64>,
}

/// One candidate in the trace. Build survivors with [`TraceCandidate::survivor`] and
/// exclusions with [`TraceCandidate::exclusion`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TraceCandidate {
    /// The column id.
    pub column_id: String,
    /// The provider.
    pub provider: String,
    /// The model id.
    pub model: String,
    /// The endpoint.
    pub endpoint: String,
    /// Survivor or excluded.
    pub status: CandidateStatus,
    /// 0-based rank across the chain (survivors).
    pub rank: Option<usize>,
    /// The display quality score (survivors; omitted when the cell has none).
    pub quality: Option<f64>,
    /// The pass rate (survivors; omitted when absent).
    pub pass_rate: Option<f64>,
    /// The cost per run (survivors; omitted when absent).
    pub cost_per_run_usd: Option<f64>,
    /// The p95 latency (survivors; omitted when absent).
    pub latency_p95_ms: Option<f64>,
    /// The normalized ranking value (survivors).
    pub v: Option<f64>,
    /// Leader flag (survivors; written even when false).
    pub leader: Option<bool>,
    /// Measured flag (survivors; written even when false).
    pub measured: Option<bool>,
    /// Clears the request difficulty (survivors; written even when false).
    pub clears_difficulty: Option<bool>,
    /// Satisfies the request preferences (survivors; written even when false).
    pub prefs_satisfied: Option<bool>,
    /// The exclusion reason (exclusions; omitted when empty).
    pub exclusion_reason: String,
    /// The exclusion detail (exclusions; omitted when empty).
    pub detail: String,
    /// D8: mean availability over the trailing 7 nights (survivors).
    pub availability: Option<f64>,
    /// D8: below the availability floor (survivors).
    pub low_availability: Option<bool>,
}

impl TraceCandidate {
    /// A survivor from its ranked entry, its column, and its measurements.
    #[must_use]
    pub fn survivor(ranked: &RankedEntry, column: &ColumnInfo, metrics: &SurvivorMetrics) -> Self {
        Self {
            column_id: ranked.column_id.clone(),
            provider: column.provider.clone(),
            model: column.model.clone(),
            endpoint: column.endpoint.clone(),
            status: CandidateStatus::Survivor,
            rank: Some(ranked.rank),
            quality: metrics.quality.map(ranking::display_score),
            pass_rate: metrics.pass_rate,
            cost_per_run_usd: metrics.cost_per_run_usd,
            latency_p95_ms: metrics.latency_p95_ms,
            v: Some(ranked.v),
            leader: Some(ranked.flags.leader),
            measured: Some(ranked.flags.measured),
            clears_difficulty: Some(ranked.flags.clears_difficulty),
            prefs_satisfied: Some(ranked.flags.prefs_satisfied),
            exclusion_reason: String::new(),
            detail: String::new(),
            availability: metrics.availability,
            low_availability: Some(ranked.flags.low_availability),
        }
    }

    /// An exclusion from the policy or ranking exclusion and its column.
    #[must_use]
    pub fn exclusion(exclusion: &Exclusion, column: &ColumnInfo) -> Self {
        Self {
            column_id: exclusion.column_id.clone(),
            provider: column.provider.clone(),
            model: column.model.clone(),
            endpoint: column.endpoint.clone(),
            status: CandidateStatus::Excluded,
            exclusion_reason: exclusion.reason.as_str().to_owned(),
            detail: exclusion.detail.clone(),
            ..Self::default()
        }
    }

    /// The candidate as an ordered JSON object. Fails on a non-finite number, as Go's
    /// `json.Marshal` does.
    pub fn to_json(&self) -> Result<JsonValue, String> {
        let mut members = vec![
            ("column_id", JsonValue::String(self.column_id.clone())),
            ("provider", JsonValue::String(self.provider.clone())),
            ("model", JsonValue::String(self.model.clone())),
            ("endpoint", JsonValue::String(self.endpoint.clone())),
            ("status", JsonValue::String(self.status.as_str().to_owned())),
        ];
        if let Some(rank) = self.rank {
            members.push(("rank", JsonValue::Number(rank.to_string())));
        }
        let numbers = [
            ("quality", self.quality),
            ("pass_rate", self.pass_rate),
            ("cost_per_run_usd", self.cost_per_run_usd),
            ("latency_p95_ms", self.latency_p95_ms),
            ("v", self.v),
        ];
        for (name, value) in numbers {
            if let Some(value) = value {
                members.push((name, number(name, value)?));
            }
        }
        let flags = [
            ("leader", self.leader),
            ("measured", self.measured),
            ("clears_difficulty", self.clears_difficulty),
            ("prefs_satisfied", self.prefs_satisfied),
        ];
        for (name, value) in flags {
            if let Some(value) = value {
                members.push((name, JsonValue::Bool(value)));
            }
        }
        for (name, value) in [
            ("exclusion_reason", &self.exclusion_reason),
            ("detail", &self.detail),
        ] {
            if !value.is_empty() {
                members.push((name, JsonValue::String(value.clone())));
            }
        }
        if let Some(availability) = self.availability {
            members.push(("availability", number("availability", availability)?));
        }
        if let Some(low) = self.low_availability {
            members.push(("low_availability", JsonValue::Bool(low)));
        }
        Ok(JsonValue::Object(
            members
                .into_iter()
                .map(|(name, value)| (name.to_owned(), value))
                .collect(),
        ))
    }
}

/// A Go JSON number, or the Go marshal error for a non-finite value.
fn number(name: &str, value: f64) -> Result<JsonValue, String> {
    gojson::encode_f64(value)
        .map(JsonValue::Number)
        .map_err(|error| format!("trace candidate {name}: {error}"))
}

/// The stored candidates array: survivors in rank order (stable), then the exclusions in
/// the order given (policy order, then `no_data`).
#[must_use]
pub fn candidates(
    survivors: impl IntoIterator<Item = TraceCandidate>,
    exclusions: impl IntoIterator<Item = TraceCandidate>,
) -> Vec<TraceCandidate> {
    let mut ordered: Vec<TraceCandidate> = survivors.into_iter().collect();
    ordered.sort_by_key(|candidate| candidate.rank);
    ordered.extend(exclusions);
    ordered
}

/// The exact JSON text of the candidates column.
pub fn candidates_json(candidates: &[TraceCandidate]) -> Result<String, String> {
    let items = candidates
        .iter()
        .map(TraceCandidate::to_json)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(gojson::marshal(&JsonValue::Array(items)))
}
