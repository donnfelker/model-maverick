//! The scoring persistence glue (reference Part 3 §1.8): `FoldObservations`, the only
//! writer of `aggregates`, and `SaveScores`.

use std::collections::BTreeMap;

use chrono::NaiveDate;
use maverick_domain::scoring::fold_observation;
use maverick_store::JsonRawValue;
use maverick_store::measurement::{self, AggregateUpsert, CellDimension};
use sqlx::PgConnection;

/// A (model column, capability, difficulty tier) cell.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CellKey {
    pub model_column_id: String,
    pub capability: String,
    pub difficulty_tier: i16,
}

/// The EWMA weight and the observation count at which a cell is measured.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FoldParams {
    pub alpha: f64,
    pub min_nights: u64,
}

/// `FoldObservations(cell, observations, as_of, matrix_version, α, min_nights)`: for
/// each dimension in bytewise order, resume the stream from the stored (float32)
/// EWMAs of the nights before `as_of`, fold the observation, and upsert the `as_of`
/// row. Re-folding a night replaces its row, so `n` never double counts; later rows are
/// not recomputed (QUIRK Q4). Returns the number of rows written.
pub async fn fold_observations(
    conn: &mut PgConnection,
    cell: &CellKey,
    observations: &BTreeMap<&str, f64>,
    as_of: NaiveDate,
    matrix_version: &str,
    params: &FoldParams,
) -> sqlx::Result<usize> {
    for (&dimension, &observation) in observations {
        let key = CellDimension {
            model_column_id: &cell.model_column_id,
            capability: &cell.capability,
            difficulty_tier: cell.difficulty_tier,
            dimension,
        };
        let series = measurement::list_aggregate_series_before(&mut *conn, &key, as_of).await?;
        let ewmas: Vec<f64> = series.iter().map(|row| f64::from(row.ewma)).collect();
        let stored = fold_observation(params.alpha, &ewmas, observation, params.min_nights);
        // The stored values are already float32-rounded, so the casts are exact.
        #[allow(clippy::cast_possible_truncation)]
        let (ewma, variance) = (stored.ewma as f32, stored.variance as f32);
        measurement::upsert_aggregate(
            &mut *conn,
            &AggregateUpsert {
                model_column_id: &cell.model_column_id,
                capability: &cell.capability,
                difficulty_tier: cell.difficulty_tier,
                dimension,
                ewma,
                variance,
                n: i32::try_from(stored.n).unwrap_or(i32::MAX),
                measured: stored.measured,
                matrix_version,
                as_of,
            },
        )
        .await?;
    }
    Ok(observations.len())
}

/// `SaveScores(runID, graderIdx, dims, detail)`: one `scores` row per dimension in
/// sorted order, `value = f32(clamp(score, 0, 1))`, every row sharing `detail`.
pub async fn save_scores(
    conn: &mut PgConnection,
    run_id: &str,
    grader_idx: i16,
    dims: &BTreeMap<String, f64>,
    detail: Option<&JsonRawValue>,
) -> Result<(), String> {
    for (dimension, &score) in dims {
        #[allow(clippy::cast_possible_truncation)]
        let value = score.clamp(0.0, 1.0) as f32;
        measurement::insert_score(&mut *conn, run_id, grader_idx, dimension, value, detail)
            .await
            .map_err(|e| {
                format!(
                    "scoring: save scores: run {run_id} grader {grader_idx} dimension {dimension}: {e}"
                )
            })?;
    }
    Ok(())
}
