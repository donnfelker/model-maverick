//! The model catalog: `model_columns`.
//!
//! Go source: `parity/schema/queries/model_columns.sql`. Rust adds the retention fields
//! (spec §4.5, D1, D15) and `param_constraints` (D29).

use chrono::{DateTime, NaiveDate, Utc};
use sqlx::PgExecutor;

use crate::{Json, JsonRawValue};

/// A `model_columns` row.
#[derive(Debug, Clone)]
pub struct ModelColumn {
    pub id: String,
    pub provider: String,
    pub model_id: String,
    pub endpoint: String,
    pub settings: Json,
    pub status: String,
    pub pricing: Json,
    pub quality_source_column_id: Option<String>,
    pub cold_start_until: Option<NaiveDate>,
    pub deprecation_date: Option<NaiveDate>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub execution_aliases: Json,
    pub retention_mode: String,
    pub retention_verified_at: Option<DateTime<Utc>>,
    pub retention_valid_until: Option<DateTime<Utc>>,
    pub retention_source: Option<String>,
    pub execution_alias_retention: Json,
    pub param_constraints: Json,
}

/// The values of `CreateModelColumn`.
#[derive(Debug, Clone, Copy)]
pub struct NewModelColumn<'a> {
    pub id: &'a str,
    pub provider: &'a str,
    pub model_id: &'a str,
    pub endpoint: &'a str,
    pub settings: &'a JsonRawValue,
    pub status: &'a str,
    pub pricing: &'a JsonRawValue,
    pub quality_source_column_id: Option<&'a str>,
    pub cold_start_until: Option<NaiveDate>,
    pub deprecation_date: Option<NaiveDate>,
    /// `None` stores `{}`, the column default.
    pub execution_aliases: Option<&'a JsonRawValue>,
    /// `None` stores `unknown`, the column default.
    pub retention_mode: Option<&'a str>,
    pub retention_verified_at: Option<DateTime<Utc>>,
    pub retention_valid_until: Option<DateTime<Utc>>,
    pub retention_source: Option<&'a str>,
    /// `None` stores `{}`, the column default.
    pub execution_alias_retention: Option<&'a JsonRawValue>,
    /// `None` stores `{}`, the column default.
    pub param_constraints: Option<&'a JsonRawValue>,
}

/// The values of `UpdateModelColumn`. Every field is written as given, so a `None` on a
/// nullable column clears it. The three jsonb maps are the exception: `None` keeps the
/// stored map, and a value replaces it whole.
#[derive(Debug, Clone, Copy)]
pub struct ModelColumnUpdate<'a> {
    pub settings: &'a JsonRawValue,
    pub status: &'a str,
    pub pricing: &'a JsonRawValue,
    pub quality_source_column_id: Option<&'a str>,
    pub cold_start_until: Option<NaiveDate>,
    pub deprecation_date: Option<NaiveDate>,
    pub execution_aliases: Option<&'a JsonRawValue>,
    pub retention_mode: &'a str,
    pub retention_verified_at: Option<DateTime<Utc>>,
    pub retention_valid_until: Option<DateTime<Utc>>,
    pub retention_source: Option<&'a str>,
    pub execution_alias_retention: Option<&'a JsonRawValue>,
    pub param_constraints: Option<&'a JsonRawValue>,
}

/// `CreateModelColumn`.
pub async fn create_model_column<'e>(
    db: impl PgExecutor<'e>,
    column: &NewModelColumn<'_>,
) -> sqlx::Result<ModelColumn> {
    sqlx::query_as!(
        ModelColumn,
        r#"insert into model_columns (
             id, provider, model_id, endpoint, settings, status, pricing,
             quality_source_column_id, cold_start_until, deprecation_date, execution_aliases,
             retention_mode, retention_verified_at, retention_valid_until, retention_source,
             execution_alias_retention, param_constraints
           )
           values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, coalesce($11::jsonb, '{}'),
                   coalesce($12, 'unknown'), $13, $14, $15,
                   coalesce($16::jsonb, '{}'), coalesce($17::jsonb, '{}'))
           returning id, provider, model_id, endpoint, settings as "settings: _", status,
                     pricing as "pricing: _", quality_source_column_id, cold_start_until,
                     deprecation_date, created_at, updated_at,
                     execution_aliases as "execution_aliases: _", retention_mode,
                     retention_verified_at, retention_valid_until, retention_source,
                     execution_alias_retention as "execution_alias_retention: _",
                     param_constraints as "param_constraints: _""#,
        column.id,
        column.provider,
        column.model_id,
        column.endpoint,
        column.settings as &JsonRawValue,
        column.status,
        column.pricing as &JsonRawValue,
        column.quality_source_column_id,
        column.cold_start_until,
        column.deprecation_date,
        column.execution_aliases as Option<&JsonRawValue>,
        column.retention_mode,
        column.retention_verified_at,
        column.retention_valid_until,
        column.retention_source,
        column.execution_alias_retention as Option<&JsonRawValue>,
        column.param_constraints as Option<&JsonRawValue>,
    )
    .fetch_one(db)
    .await
}

/// `GetModelColumn`.
pub async fn get_model_column<'e>(db: impl PgExecutor<'e>, id: &str) -> sqlx::Result<ModelColumn> {
    sqlx::query_as!(
        ModelColumn,
        r#"select id, provider, model_id, endpoint, settings as "settings: _", status,
                  pricing as "pricing: _", quality_source_column_id, cold_start_until,
                  deprecation_date, created_at, updated_at,
                  execution_aliases as "execution_aliases: _", retention_mode,
                  retention_verified_at, retention_valid_until, retention_source,
                  execution_alias_retention as "execution_alias_retention: _",
                  param_constraints as "param_constraints: _"
           from model_columns
           where id = $1"#,
        id
    )
    .fetch_one(db)
    .await
}

/// `ListModelColumns`: ordered by id.
pub async fn list_model_columns<'e>(db: impl PgExecutor<'e>) -> sqlx::Result<Vec<ModelColumn>> {
    sqlx::query_as!(
        ModelColumn,
        r#"select id, provider, model_id, endpoint, settings as "settings: _", status,
                  pricing as "pricing: _", quality_source_column_id, cold_start_until,
                  deprecation_date, created_at, updated_at,
                  execution_aliases as "execution_aliases: _", retention_mode,
                  retention_verified_at, retention_valid_until, retention_source,
                  execution_alias_retention as "execution_alias_retention: _",
                  param_constraints as "param_constraints: _"
           from model_columns
           order by id"#
    )
    .fetch_all(db)
    .await
}

/// `UpdateModelColumn`: also sets `updated_at = now()`. See [`ModelColumnUpdate`].
pub async fn update_model_column<'e>(
    db: impl PgExecutor<'e>,
    id: &str,
    update: &ModelColumnUpdate<'_>,
) -> sqlx::Result<ModelColumn> {
    sqlx::query_as!(
        ModelColumn,
        r#"update model_columns
           set settings = $2,
               status = $3,
               pricing = $4,
               quality_source_column_id = $5,
               cold_start_until = $6,
               deprecation_date = $7,
               execution_aliases = coalesce($8, execution_aliases),
               retention_mode = $9,
               retention_verified_at = $10,
               retention_valid_until = $11,
               retention_source = $12,
               execution_alias_retention = coalesce($13, execution_alias_retention),
               param_constraints = coalesce($14, param_constraints),
               updated_at = now()
           where id = $1
           returning id, provider, model_id, endpoint, settings as "settings: _", status,
                     pricing as "pricing: _", quality_source_column_id, cold_start_until,
                     deprecation_date, created_at, updated_at,
                     execution_aliases as "execution_aliases: _", retention_mode,
                     retention_verified_at, retention_valid_until, retention_source,
                     execution_alias_retention as "execution_alias_retention: _",
                     param_constraints as "param_constraints: _""#,
        id,
        update.settings as &JsonRawValue,
        update.status,
        update.pricing as &JsonRawValue,
        update.quality_source_column_id,
        update.cold_start_until,
        update.deprecation_date,
        update.execution_aliases as Option<&JsonRawValue>,
        update.retention_mode,
        update.retention_verified_at,
        update.retention_valid_until,
        update.retention_source,
        update.execution_alias_retention as Option<&JsonRawValue>,
        update.param_constraints as Option<&JsonRawValue>,
    )
    .fetch_one(db)
    .await
}

/// `DeleteModelColumn`.
pub async fn delete_model_column<'e>(db: impl PgExecutor<'e>, id: &str) -> sqlx::Result<()> {
    sqlx::query!("delete from model_columns where id = $1", id)
        .execute(db)
        .await
        .map(|_| ())
}

/// `ListModelColumnsByStatuses`: ordered by id.
pub async fn list_model_columns_by_statuses<'e>(
    db: impl PgExecutor<'e>,
    statuses: &[String],
) -> sqlx::Result<Vec<ModelColumn>> {
    sqlx::query_as!(
        ModelColumn,
        r#"select id, provider, model_id, endpoint, settings as "settings: _", status,
                  pricing as "pricing: _", quality_source_column_id, cold_start_until,
                  deprecation_date, created_at, updated_at,
                  execution_aliases as "execution_aliases: _", retention_mode,
                  retention_verified_at, retention_valid_until, retention_source,
                  execution_alias_retention as "execution_alias_retention: _",
                  param_constraints as "param_constraints: _"
           from model_columns
           where status = any($1::text[])
           order by id"#,
        statuses
    )
    .fetch_all(db)
    .await
}

/// `ListModelColumnsByIDs`: ordered by id. Unknown ids are absent.
pub async fn list_model_columns_by_ids<'e>(
    db: impl PgExecutor<'e>,
    ids: &[String],
) -> sqlx::Result<Vec<ModelColumn>> {
    sqlx::query_as!(
        ModelColumn,
        r#"select id, provider, model_id, endpoint, settings as "settings: _", status,
                  pricing as "pricing: _", quality_source_column_id, cold_start_until,
                  deprecation_date, created_at, updated_at,
                  execution_aliases as "execution_aliases: _", retention_mode,
                  retention_verified_at, retention_valid_until, retention_source,
                  execution_alias_retention as "execution_alias_retention: _",
                  param_constraints as "param_constraints: _"
           from model_columns
           where id = any($1::text[])
           order by id"#,
        ids
    )
    .fetch_all(db)
    .await
}

/// Which catalog field a `catalog_changes` line reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogChange {
    Status,
    Pricing,
    ExecutionAliases,
    /// Per-model parameter constraints (D29).
    ParamConstraints,
}

impl CatalogChange {
    pub fn as_str(self) -> &'static str {
        match self {
            CatalogChange::Status => "status",
            CatalogChange::Pricing => "pricing",
            CatalogChange::ExecutionAliases => "execution_aliases",
            CatalogChange::ParamConstraints => "param_constraints",
        }
    }
}

/// The INFO `catalog_changes` line (reference Part 5 §10.3): `change`, `column_id`,
/// `old`, `new`. Pricing, aliases, and parameter constraints are passed as their JSON
/// text; status as the name.
pub fn log_change(change: CatalogChange, column_id: &str, old: &str, new: &str) {
    tracing::info!(
        change = change.as_str(),
        column_id,
        old,
        new,
        "catalog_changes"
    );
}
