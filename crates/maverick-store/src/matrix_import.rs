//! D49: one transaction installs an authenticated public matrix. No corpus is needed.
use maverick_bundle::VerifiedBundle;
use maverick_domain::pricing::{is_local, parse_pricing};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImportOutcome {
    Imported,
    AlreadyActive,
}

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("matrix import requires an authenticated bundle")]
    Unauthenticated,
    #[error("matrix bundle contains inconsistent aggregate versions")]
    AggregateVersion,
    #[error("matrix bundle is older than the active bundle; use --allow-older for rollback")]
    Older,
    #[error("stored local pricing is invalid")]
    Pricing,
    #[error("matrix import database: {0}")]
    Database(#[from] sqlx::Error),
}

/// The caller must authenticate with `maverick_bundle::verify` before opening this
/// transaction. The manifest hash identifies re-imports; an active import is a no-op.
/// Older, previously imported bundles can be reactivated only with `allow_older`.
pub async fn import(
    pool: &PgPool,
    bundle: &VerifiedBundle,
    allow_older: bool,
) -> Result<ImportOutcome, ImportError> {
    if bundle.signing_kids.is_empty() {
        return Err(ImportError::Unauthenticated);
    }
    let manifest = &bundle.manifest;
    if bundle
        .matrix
        .aggregates
        .iter()
        .any(|row| row.matrix_version != manifest.matrix_version)
    {
        return Err(ImportError::AggregateVersion);
    }
    let hash = Sha256::digest(&bundle.manifest_bytes).to_vec();
    let mut tx = pool.begin().await?;
    // Read committed plus a transaction lock makes a waiter see the preceding import.
    sqlx::query("select pg_advisory_xact_lock(5570194218848332)")
        .execute(&mut *tx)
        .await?;
    let active =
        sqlx::query!("select manifest_sha256, published_at from matrix_imports where active")
            .fetch_optional(&mut *tx)
            .await?;
    if let Some(active) = active {
        if active.manifest_sha256 == hash {
            return Ok(ImportOutcome::AlreadyActive);
        }
        if manifest.published_at < active.published_at && !allow_older {
            return Err(ImportError::Older);
        }
    }
    for column in &bundle.matrix.catalog {
        // Keep stored pricing in the upsert and return it while holding the row lock.
        // This protects a concurrent local PUT, including a racing first insert.
        // Set quality-source references in a second pass, after every new id exists.
        let stored: crate::Json=sqlx::query_scalar!(r#"insert into model_columns (
            id,provider,model_id,endpoint,settings,status,pricing,cold_start_until,deprecation_date,
            execution_aliases,retention_mode,retention_verified_at,retention_valid_until,retention_source,
            execution_alias_retention,param_constraints)
            values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16)
            on conflict (id) do update set provider=excluded.provider, model_id=excluded.model_id,
              endpoint=excluded.endpoint, settings=excluded.settings, status=excluded.status,
              cold_start_until=excluded.cold_start_until, deprecation_date=excluded.deprecation_date,
              execution_aliases=excluded.execution_aliases,retention_mode=excluded.retention_mode,
              retention_verified_at=excluded.retention_verified_at,retention_valid_until=excluded.retention_valid_until,
              retention_source=excluded.retention_source,execution_alias_retention=excluded.execution_alias_retention,
              param_constraints=excluded.param_constraints,updated_at=now()
            returning pricing as "pricing: _""#,
            column.id,column.provider,column.model_id,column.endpoint,column.settings,column.status,column.pricing,
            column.cold_start_until,column.deprecation_date,column.execution_aliases,column.retention_mode,
            column.retention_verified_at,column.retention_valid_until,column.retention_source,
            column.execution_alias_retention,column.param_constraints).fetch_one(&mut *tx).await?;
        if !is_local(&parse_pricing(stored.get()).map_err(|_| ImportError::Pricing)?) {
            sqlx::query!(
                "update model_columns set pricing=$2 where id=$1",
                column.id,
                column.pricing
            )
            .execute(&mut *tx)
            .await?;
        }
    }
    for column in &bundle.matrix.catalog {
        sqlx::query!(
            "update model_columns set quality_source_column_id=$2 where id=$1",
            column.id,
            column.quality_source_column_id
        )
        .execute(&mut *tx)
        .await?;
    }
    for row in &bundle.matrix.aggregates {
        // Spec §14.4: an existing key stays as it is, including local latency rows.
        sqlx::query!(
            "insert into aggregates (model_column_id,capability,difficulty_tier,dimension,
            ewma,variance,n,measured,matrix_version,as_of) values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)
            on conflict (model_column_id,capability,difficulty_tier,dimension,as_of) do nothing",
            row.model_column_id,
            row.capability,
            row.difficulty_tier,
            row.dimension,
            row.ewma,
            row.variance,
            row.n,
            row.measured,
            manifest.matrix_version,
            row.as_of
        )
        .execute(&mut *tx)
        .await?;
    }
    for row in &bundle.matrix.calibration {
        sqlx::query!(
            "insert into calibration (batch_id,capability,brier_score,overconfidence,n,scored_at)
            values ($1,$2,$3,$4,$5,$6) on conflict (batch_id,capability) do nothing",
            row.batch_id,
            row.capability,
            row.brier_score,
            row.overconfidence,
            row.n,
            row.scored_at
        )
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query!("insert into matrix_imports (manifest_sha256,format,matrix_version,published_at,origin_release,signing_kids)
        values ($1,$2,$3,$4,$5,$6) on conflict (manifest_sha256) do nothing",hash,manifest.format,manifest.matrix_version,
        manifest.published_at,manifest.origin_release,&bundle.signing_kids).execute(&mut *tx).await?;
    sqlx::query!("update matrix_imports set active=false where active")
        .execute(&mut *tx)
        .await?;
    sqlx::query!(
        "update matrix_imports set active=true where manifest_sha256=$1",
        hash
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(ImportOutcome::Imported)
}
