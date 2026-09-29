//! Decisions: recommendations, the classification cache, classifier budget counters, and
//! outcomes.
//!
//! Go sources: `parity/schema/queries/recommendations.sql`, `classification_cache.sql`,
//! and `outcomes.sql`. The classifier budget counters are new in Rust (spec §8.3, D4).

use chrono::{DateTime, NaiveDate, Utc};
use sqlx::PgExecutor;

use crate::{Decimal, Json, JsonRawValue};

/// A `recommendations` row. `request` never holds prompt text.
#[derive(Debug, Clone)]
pub struct Recommendation {
    pub id: String,
    pub tenant_id: String,
    pub api_key_id: String,
    pub prompt_hash: Option<String>,
    pub request: Json,
    pub classification: Json,
    pub candidates: Json,
    pub response: Json,
    pub matrix_version: String,
    pub created_at: DateTime<Utc>,
}

/// The values of `CreateRecommendation`.
#[derive(Debug, Clone, Copy)]
pub struct NewRecommendation<'a> {
    pub id: &'a str,
    pub tenant_id: &'a str,
    pub api_key_id: &'a str,
    pub prompt_hash: Option<&'a str>,
    pub request: &'a JsonRawValue,
    pub classification: &'a JsonRawValue,
    pub candidates: &'a JsonRawValue,
    pub response: &'a JsonRawValue,
    pub matrix_version: &'a str,
}

/// The filters of `ListRecommendationsPage`.
#[derive(Debug, Clone, Copy)]
pub struct RecommendationPageQuery<'a> {
    pub tenant_id: &'a str,
    /// Empty means every prompt hash.
    pub prompt_hash: &'a str,
    /// Empty means every capability. Matches `classification->>'capability'`.
    pub capability: &'a str,
    pub since: Option<DateTime<Utc>>,
    /// Empty is the first page.
    pub cursor_id: &'a str,
    pub page_limit: i32,
}

/// A `classification_cache` row.
#[derive(Debug, Clone)]
pub struct ClassificationCache {
    pub tenant_id: String,
    pub prompt_hash: String,
    /// `<classifier column>/<prompt version>`, plus `/<projection version>` when the
    /// request carries one (spec §8.3, D5).
    pub classifier_version: String,
    pub classification: Json,
    pub created_at: DateTime<Utc>,
}

/// An `outcomes` row.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub id: String,
    pub recommendation_id: String,
    pub model_used: String,
    pub outcome: String,
    pub escalated_to: Option<String>,
    pub details: Option<Json>,
    pub created_at: DateTime<Utc>,
    pub endpoint_used: String,
}

/// The filters of `ListOutcomesPage`.
#[derive(Debug, Clone, Copy)]
pub struct OutcomePageQuery<'a> {
    pub tenant_id: &'a str,
    /// Empty means every recommendation.
    pub recommendation_id: &'a str,
    /// Empty means every outcome.
    pub outcome: &'a str,
    pub since: Option<DateTime<Utc>>,
    /// Empty is the first page.
    pub cursor_id: &'a str,
    pub page_limit: i32,
}

// ---- recommendations.sql ----

/// `CreateRecommendation`.
pub async fn create_recommendation<'e>(
    db: impl PgExecutor<'e>,
    rec: &NewRecommendation<'_>,
) -> sqlx::Result<Recommendation> {
    sqlx::query_as!(
        Recommendation,
        r#"insert into recommendations (
             id, tenant_id, api_key_id, prompt_hash, request, classification, candidates,
             response, matrix_version
           )
           values ($1, $2, $3, $4, $5, $6, $7, $8, $9)
           returning id, tenant_id, api_key_id, prompt_hash, request as "request: _",
                     classification as "classification: _", candidates as "candidates: _",
                     response as "response: _", matrix_version, created_at"#,
        rec.id,
        rec.tenant_id,
        rec.api_key_id,
        rec.prompt_hash,
        rec.request as &JsonRawValue,
        rec.classification as &JsonRawValue,
        rec.candidates as &JsonRawValue,
        rec.response as &JsonRawValue,
        rec.matrix_version,
    )
    .fetch_one(db)
    .await
}

/// `GetRecommendation`.
pub async fn get_recommendation<'e>(
    db: impl PgExecutor<'e>,
    id: &str,
) -> sqlx::Result<Recommendation> {
    sqlx::query_as!(
        Recommendation,
        r#"select id, tenant_id, api_key_id, prompt_hash, request as "request: _",
                  classification as "classification: _", candidates as "candidates: _",
                  response as "response: _", matrix_version, created_at
           from recommendations
           where id = $1"#,
        id
    )
    .fetch_one(db)
    .await
}

/// `GetRecommendationForTenant`: another tenant's row is indistinguishable from a
/// missing one (both are [`sqlx::Error::RowNotFound`]).
pub async fn get_recommendation_for_tenant<'e>(
    db: impl PgExecutor<'e>,
    id: &str,
    tenant_id: &str,
) -> sqlx::Result<Recommendation> {
    sqlx::query_as!(
        Recommendation,
        r#"select id, tenant_id, api_key_id, prompt_hash, request as "request: _",
                  classification as "classification: _", candidates as "candidates: _",
                  response as "response: _", matrix_version, created_at
           from recommendations
           where id = $1 and tenant_id = $2"#,
        id,
        tenant_id
    )
    .fetch_one(db)
    .await
}

/// `RecommendationIDsForTenant`: which of `ids` belong to the tenant. No order.
pub async fn recommendation_ids_for_tenant<'e>(
    db: impl PgExecutor<'e>,
    tenant_id: &str,
    ids: &[String],
) -> sqlx::Result<Vec<String>> {
    sqlx::query_scalar!(
        "select id from recommendations
         where tenant_id = $1 and id = any($2::text[])",
        tenant_id,
        ids
    )
    .fetch_all(db)
    .await
}

/// `ListRecommendationsPage`: newest first by id, after the cursor.
pub async fn list_recommendations_page<'e>(
    db: impl PgExecutor<'e>,
    query: &RecommendationPageQuery<'_>,
) -> sqlx::Result<Vec<Recommendation>> {
    sqlx::query_as!(
        Recommendation,
        r#"select id, tenant_id, api_key_id, prompt_hash, request as "request: _",
                  classification as "classification: _", candidates as "candidates: _",
                  response as "response: _", matrix_version, created_at
           from recommendations
           where tenant_id = $1
             and ($2::text = '' or prompt_hash = $2)
             and ($3::text = '' or classification->>'capability' = $3)
             and ($4::timestamptz is null or created_at >= $4)
             and ($5::text = '' or id < $5)
           order by id desc
           limit $6"#,
        query.tenant_id,
        query.prompt_hash,
        query.capability,
        query.since,
        query.cursor_id,
        i64::from(query.page_limit),
    )
    .fetch_all(db)
    .await
}

/// `DeleteRecommendationsBefore`: hard-deletes rows created before the cutoff and
/// returns the count. Run it in the `history_prune` transaction after the captures and
/// outcomes that reference them.
pub async fn delete_recommendations_before<'e>(
    db: impl PgExecutor<'e>,
    cutoff: DateTime<Utc>,
) -> sqlx::Result<u64> {
    sqlx::query!("delete from recommendations where created_at < $1", cutoff)
        .execute(db)
        .await
        .map(|done| done.rows_affected())
}

// ---- classification_cache.sql ----

/// `GetClassificationCache`.
pub async fn get_classification_cache<'e>(
    db: impl PgExecutor<'e>,
    tenant_id: &str,
    prompt_hash: &str,
    classifier_version: &str,
) -> sqlx::Result<ClassificationCache> {
    sqlx::query_as!(
        ClassificationCache,
        r#"select tenant_id, prompt_hash, classifier_version,
                  classification as "classification: _", created_at
           from classification_cache
           where tenant_id = $1
             and prompt_hash = $2
             and classifier_version = $3"#,
        tenant_id,
        prompt_hash,
        classifier_version
    )
    .fetch_one(db)
    .await
}

/// `UpsertClassificationCache`: despite the name, the first write wins
/// (`on conflict do nothing`).
pub async fn upsert_classification_cache<'e>(
    db: impl PgExecutor<'e>,
    tenant_id: &str,
    prompt_hash: &str,
    classifier_version: &str,
    classification: &JsonRawValue,
) -> sqlx::Result<()> {
    sqlx::query!(
        "insert into classification_cache (tenant_id, prompt_hash, classifier_version, classification)
         values ($1, $2, $3, $4)
         on conflict (tenant_id, prompt_hash, classifier_version) do nothing",
        tenant_id,
        prompt_hash,
        classifier_version,
        classification as &JsonRawValue
    )
    .execute(db)
    .await
    .map(|_| ())
}

// ---- classifier budget counters (Rust only, D4) ----

/// Counts one classifier call against the tenant's quota for the UTC day. Returns the new
/// count, or `None` when the quota is used up, in which case nothing is counted and no
/// call may be made. Atomic across API replicas.
pub async fn count_classifier_call<'e>(
    db: impl PgExecutor<'e>,
    usage_date: NaiveDate,
    tenant_id: &str,
    quota: i32,
) -> sqlx::Result<Option<i32>> {
    sqlx::query_scalar!(
        "insert into classifier_usage (usage_date, tenant_id, calls)
         select $1, $2, 1 where $3::int > 0
         on conflict (usage_date, tenant_id) do update
           set calls = classifier_usage.calls + 1,
               updated_at = now()
           where classifier_usage.calls < $3
         returning calls",
        usage_date,
        tenant_id,
        quota
    )
    .fetch_optional(db)
    .await
}

/// Adds the provider cost of a classifier call to the tenant's row for the UTC day.
pub async fn add_classifier_spend<'e>(
    db: impl PgExecutor<'e>,
    usage_date: NaiveDate,
    tenant_id: &str,
    spend_usd: Decimal,
) -> sqlx::Result<()> {
    sqlx::query!(
        "insert into classifier_usage (usage_date, tenant_id, spend_usd)
         values ($1, $2, $3)
         on conflict (usage_date, tenant_id) do update
           set spend_usd = classifier_usage.spend_usd + excluded.spend_usd,
               updated_at = now()",
        usage_date,
        tenant_id,
        spend_usd
    )
    .execute(db)
    .await
    .map(|_| ())
}

/// The global classifier spend for the UTC day, across tenants. The spend cap compares
/// against it.
pub async fn classifier_spend_on<'e>(
    db: impl PgExecutor<'e>,
    usage_date: NaiveDate,
) -> sqlx::Result<Decimal> {
    sqlx::query_scalar!(
        r#"select coalesce(sum(spend_usd), 0) as "total!"
           from classifier_usage
           where usage_date = $1"#,
        usage_date
    )
    .fetch_one(db)
    .await
}

// ---- outcomes.sql ----

/// `CreateOutcome`: `escalated_to` stays NULL.
pub async fn create_outcome<'e>(
    db: impl PgExecutor<'e>,
    id: &str,
    recommendation_id: &str,
    model_used: &str,
    outcome: &str,
    endpoint_used: &str,
    details: Option<&JsonRawValue>,
) -> sqlx::Result<Outcome> {
    sqlx::query_as!(
        Outcome,
        r#"insert into outcomes (id, recommendation_id, model_used, outcome, endpoint_used, details)
           values ($1, $2, $3, $4, $5, $6)
           returning id, recommendation_id, model_used, outcome, escalated_to,
                     details as "details: _", created_at, endpoint_used"#,
        id,
        recommendation_id,
        model_used,
        outcome,
        endpoint_used,
        details as Option<&JsonRawValue>
    )
    .fetch_one(db)
    .await
}

/// `ListOutcomesPage`: the tenant's outcomes, scoped through the owning recommendation,
/// newest first by id, after the cursor.
pub async fn list_outcomes_page<'e>(
    db: impl PgExecutor<'e>,
    query: &OutcomePageQuery<'_>,
) -> sqlx::Result<Vec<Outcome>> {
    sqlx::query_as!(
        Outcome,
        r#"select o.id, o.recommendation_id, o.model_used, o.outcome, o.escalated_to,
                  o.details as "details: _", o.created_at, o.endpoint_used
           from outcomes o
           join recommendations r on r.id = o.recommendation_id
           where r.tenant_id = $1
             and ($2::text = '' or o.recommendation_id = $2)
             and ($3::text = '' or o.outcome = $3)
             and ($4::timestamptz is null or o.created_at >= $4)
             and ($5::text = '' or o.id < $5)
           order by o.id desc
           limit $6"#,
        query.tenant_id,
        query.recommendation_id,
        query.outcome,
        query.since,
        query.cursor_id,
        i64::from(query.page_limit),
    )
    .fetch_all(db)
    .await
}

/// `DeleteOutcomesForRecommendationsBefore`: deletes the outcomes of recommendations
/// created before the cutoff, whatever the outcome's own age.
pub async fn delete_outcomes_for_recommendations_before<'e>(
    db: impl PgExecutor<'e>,
    cutoff: DateTime<Utc>,
) -> sqlx::Result<u64> {
    sqlx::query!(
        "delete from outcomes o
         using recommendations r
         where o.recommendation_id = r.id
           and r.created_at < $1",
        cutoff
    )
    .execute(db)
    .await
    .map(|done| done.rows_affected())
}
