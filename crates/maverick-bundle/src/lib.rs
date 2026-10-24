//! Public matrix bundles (spec §14.4, D49). Only explicit global DTOs cross this boundary.

use std::collections::BTreeMap;

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

pub const FORMAT: &str = "mmbundle1";
pub const PAYLOAD_FILES: [&str; 4] = [
    "catalog.json",
    "aggregates.jsonl",
    "calibration.json",
    "checks.json",
];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaRange {
    pub min: i64,
    pub max: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format: String,
    pub matrix_version: String,
    pub published_at: DateTime<Utc>,
    pub origin_release: String,
    pub schema: SchemaRange,
    pub files: BTreeMap<String, String>,
}

/// Global catalog fields only. Store rows are never serialized directly.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogColumn {
    pub id: String,
    pub provider: String,
    pub model_id: String,
    pub endpoint: String,
    pub settings: serde_json::Value,
    pub status: String,
    pub pricing: serde_json::Value,
    pub quality_source_column_id: Option<String>,
    pub cold_start_until: Option<NaiveDate>,
    pub deprecation_date: Option<NaiveDate>,
    pub execution_aliases: serde_json::Value,
    pub retention_mode: String,
    pub retention_verified_at: Option<DateTime<Utc>>,
    pub retention_valid_until: Option<DateTime<Utc>>,
    pub retention_source: Option<String>,
    pub execution_alias_retention: serde_json::Value,
    pub param_constraints: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Aggregate {
    pub model_column_id: String,
    pub capability: String,
    pub difficulty_tier: i16,
    pub dimension: String,
    pub ewma: f32,
    pub variance: f32,
    pub n: i32,
    pub measured: bool,
    pub matrix_version: String,
    pub as_of: NaiveDate,
}

/// Calibration reports expose capability-level statistics, never prediction/task ids.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Calibration {
    pub batch_id: String,
    pub capability: String,
    pub brier_score: f32,
    pub overconfidence: f32,
    pub n: i32,
    pub scored_at: DateTime<Utc>,
}

mod archive;
mod publish;

pub use archive::{BundleError, SigningKeys, VerifiedBundle, build, inspect, sha256, verify};
pub use publish::{IndexEntry, filename, publish};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicMatrix {
    pub catalog: Vec<CatalogColumn>,
    pub aggregates: Vec<Aggregate>,
    pub calibration: Vec<Calibration>,
    pub checks: BTreeMap<String, Vec<serde_json::Value>>,
}
