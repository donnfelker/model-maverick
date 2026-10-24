//! Active public bundle metadata (spec §14.4, D49).

use chrono::{DateTime, Days, NaiveDate, Utc};
use sqlx::PgExecutor;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveBundle {
    pub matrix_version: String,
    pub published_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

/// D49: an explicit projection of global catalog, aggregate, calibration, and public
/// check definitions. The caller uses one repeatable-read snapshot. No tenant, corpus,
/// run, prediction, grader, or decision table is queried here.
pub async fn public_matrix(
    conn: &mut sqlx::PgConnection,
    matrix_version: &str,
    night: NaiveDate,
) -> sqlx::Result<maverick_bundle::PublicMatrix> {
    use maverick_bundle::{Aggregate, Calibration, CatalogColumn, PublicMatrix};
    let catalog = crate::catalog::list_model_columns(&mut *conn)
        .await?
        .into_iter()
        .map(|c| {
            let json = |raw: &crate::JsonRawValue| {
                serde_json::from_str(raw.get()).map_err(|e| sqlx::Error::Decode(Box::new(e)))
            };
            Ok(CatalogColumn {
                id: c.id,
                provider: c.provider,
                model_id: c.model_id,
                endpoint: c.endpoint,
                settings: json(&c.settings)?,
                status: c.status,
                pricing: json(&c.pricing)?,
                quality_source_column_id: c.quality_source_column_id,
                cold_start_until: c.cold_start_until,
                deprecation_date: c.deprecation_date,
                execution_aliases: json(&c.execution_aliases)?,
                retention_mode: c.retention_mode,
                retention_verified_at: c.retention_verified_at,
                retention_valid_until: c.retention_valid_until,
                retention_source: c.retention_source,
                execution_alias_retention: json(&c.execution_alias_retention)?,
                param_constraints: json(&c.param_constraints)?,
            })
        })
        .collect::<sqlx::Result<Vec<_>>>()?;
    let first = night
        .checked_sub_days(Days::new(29))
        .unwrap_or(NaiveDate::MIN);
    let aggregates = sqlx::query_as!(Aggregate,
        "select model_column_id, capability, difficulty_tier, dimension, ewma, variance, n, measured, matrix_version, as_of
         from aggregates where matrix_version = $1 and as_of >= $2 and as_of <= $3
         order by model_column_id, capability, difficulty_tier, dimension, as_of",
        matrix_version, first, night).fetch_all(&mut *conn).await?;
    let calibration = sqlx::query_as!(
        Calibration,
        "select batch_id, capability, brier_score, overconfidence, n, scored_at
         from calibration order by batch_id, capability"
    )
    .fetch_all(&mut *conn)
    .await?;
    let checks = maverick_domain::checks::CHECK_DEFINITIONS
        .iter()
        .map(|check| {
            let document =
                serde_json::from_str(&check.document()).expect("static public check JSON");
            (check.capability.to_owned(), vec![document])
        })
        .collect();
    Ok(PublicMatrix {
        catalog,
        aggregates,
        calibration,
        checks,
    })
}

/// Hold the export lock and one repeatable-read snapshot through filesystem publication.
/// A blocking publisher owns the transaction, so cancellation cannot release the lock
/// while that publisher still writes the index.
pub async fn export_snapshot(
    pool: &sqlx::PgPool,
    matrix_version: &str,
    night: NaiveDate,
) -> sqlx::Result<(
    sqlx::Transaction<'static, sqlx::Postgres>,
    maverick_bundle::PublicMatrix,
)> {
    let mut tx = pool
        .begin_with("BEGIN ISOLATION LEVEL REPEATABLE READ")
        .await?;
    sqlx::query("select pg_advisory_xact_lock(5570194218848331)")
        .execute(&mut *tx)
        .await?;
    let matrix = public_matrix(&mut tx, matrix_version, night).await?;
    Ok((tx, matrix))
}

/// No active bundle is a valid cold start on a replica.
pub async fn active_bundle<'e>(db: impl PgExecutor<'e>) -> sqlx::Result<Option<ActiveBundle>> {
    sqlx::query_as!(
        ActiveBundle,
        "select matrix_version, published_at, created_at from matrix_imports where active"
    )
    .fetch_optional(db)
    .await
}
