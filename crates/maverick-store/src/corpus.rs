//! Corpus versions and tasks.
//!
//! Go sources: `parity/schema/queries/corpus_versions.sql` and `tasks.sql`.

use chrono::{DateTime, Utc};
use sqlx::PgExecutor;

use crate::{Json, JsonRawValue};

/// The advisory lock key that serializes corpus activation: `0x6d76636f7270` ("mvcorp").
pub const CORPUS_ACTIVATION_LOCK_KEY: i64 = 0x6d76_636f_7270;

/// A `corpus_versions` row.
#[derive(Debug, Clone)]
pub struct CorpusVersion {
    pub version: String,
    pub description: String,
    pub task_count: i32,
    pub active: bool,
    pub created_at: DateTime<Utc>,
}

/// A `tasks` row. `temperature` and `pass_threshold` are `real`, so they are `f32`.
#[derive(Debug, Clone)]
pub struct Task {
    pub id: String,
    pub capability: String,
    pub domain_tags: Vec<String>,
    pub difficulty_tier: i16,
    pub difficulty_knobs: Json,
    pub prompt: String,
    pub attachments: Json,
    pub temperature: f32,
    pub max_tokens: i32,
    pub grader_specs: Json,
    pub dimension_weights: Json,
    pub pass_threshold: f32,
    pub smoke_member: bool,
    pub corpus_version: String,
    pub active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// The values of `UpsertTask`.
#[derive(Debug, Clone, Copy)]
pub struct TaskUpsert<'a> {
    pub id: &'a str,
    pub capability: &'a str,
    pub domain_tags: &'a [String],
    pub difficulty_tier: i16,
    pub difficulty_knobs: &'a JsonRawValue,
    pub prompt: &'a str,
    pub attachments: &'a JsonRawValue,
    pub temperature: f32,
    pub max_tokens: i32,
    pub grader_specs: &'a JsonRawValue,
    pub dimension_weights: &'a JsonRawValue,
    pub pass_threshold: f32,
    pub smoke_member: bool,
    pub corpus_version: &'a str,
    pub active: bool,
}

// ---- corpus_versions.sql ----

/// `UpsertCorpusVersion`: a conflict updates description and task count only. `active`
/// and `created_at` keep their stored values.
pub async fn upsert_corpus_version<'e>(
    db: impl PgExecutor<'e>,
    version: &str,
    description: &str,
    task_count: i32,
    active: bool,
) -> sqlx::Result<CorpusVersion> {
    sqlx::query_as!(
        CorpusVersion,
        "insert into corpus_versions (version, description, task_count, active)
         values ($1, $2, $3, $4)
         on conflict (version) do update set
           description = excluded.description,
           task_count = excluded.task_count
         returning version, description, task_count, active, created_at",
        version,
        description,
        task_count,
        active
    )
    .fetch_one(db)
    .await
}

/// `GetCorpusVersion`.
pub async fn get_corpus_version<'e>(
    db: impl PgExecutor<'e>,
    version: &str,
) -> sqlx::Result<CorpusVersion> {
    sqlx::query_as!(
        CorpusVersion,
        "select version, description, task_count, active, created_at
         from corpus_versions
         where version = $1",
        version
    )
    .fetch_one(db)
    .await
}

/// `GetActiveCorpusVersion`: the active row. No active row is
/// [`sqlx::Error::RowNotFound`].
pub async fn get_active_corpus_version<'e>(db: impl PgExecutor<'e>) -> sqlx::Result<CorpusVersion> {
    sqlx::query_as!(
        CorpusVersion,
        "select version, description, task_count, active, created_at
         from corpus_versions
         where active
         limit 1"
    )
    .fetch_one(db)
    .await
}

/// `ListCorpusVersions`: ordered by creation time, then version.
pub async fn list_corpus_versions<'e>(db: impl PgExecutor<'e>) -> sqlx::Result<Vec<CorpusVersion>> {
    sqlx::query_as!(
        CorpusVersion,
        "select version, description, task_count, active, created_at
         from corpus_versions
         order by created_at, version"
    )
    .fetch_all(db)
    .await
}

/// `AcquireCorpusActivationLock`: a transaction-scoped advisory lock. Call it inside a
/// transaction with [`CORPUS_ACTIVATION_LOCK_KEY`].
pub async fn acquire_corpus_activation_lock<'e>(
    db: impl PgExecutor<'e>,
    key: i64,
) -> sqlx::Result<()> {
    sqlx::query!("select pg_advisory_xact_lock($1)", key)
        .execute(db)
        .await
        .map(|_| ())
}

/// `ActivateCorpusVersion`: makes `version` the only active row. Returns the rows
/// touched; 0 means the version is unknown and nothing changed.
pub async fn activate_corpus_version<'e>(
    db: impl PgExecutor<'e>,
    version: &str,
) -> sqlx::Result<u64> {
    sqlx::query!(
        "update corpus_versions
         set active = (corpus_versions.version = $1)
         where (corpus_versions.active or corpus_versions.version = $1)
           and exists (select 1 from corpus_versions cv where cv.version = $1)",
        version
    )
    .execute(db)
    .await
    .map(|done| done.rows_affected())
}

// ---- tasks.sql ----

/// `UpsertTask`: a conflict on id replaces every other column and sets
/// `updated_at = now()`. `created_at` keeps its stored value.
pub async fn upsert_task<'e>(db: impl PgExecutor<'e>, task: &TaskUpsert<'_>) -> sqlx::Result<Task> {
    sqlx::query_as!(
        Task,
        r#"insert into tasks (
             id, capability, domain_tags, difficulty_tier, difficulty_knobs, prompt,
             attachments, temperature, max_tokens, grader_specs, dimension_weights,
             pass_threshold, smoke_member, corpus_version, active
           )
           values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
           on conflict (id) do update set
             capability = excluded.capability,
             domain_tags = excluded.domain_tags,
             difficulty_tier = excluded.difficulty_tier,
             difficulty_knobs = excluded.difficulty_knobs,
             prompt = excluded.prompt,
             attachments = excluded.attachments,
             temperature = excluded.temperature,
             max_tokens = excluded.max_tokens,
             grader_specs = excluded.grader_specs,
             dimension_weights = excluded.dimension_weights,
             pass_threshold = excluded.pass_threshold,
             smoke_member = excluded.smoke_member,
             corpus_version = excluded.corpus_version,
             active = excluded.active,
             updated_at = now()
           returning id, capability, domain_tags, difficulty_tier,
                     difficulty_knobs as "difficulty_knobs: _", prompt,
                     attachments as "attachments: _", temperature, max_tokens,
                     grader_specs as "grader_specs: _",
                     dimension_weights as "dimension_weights: _", pass_threshold,
                     smoke_member, corpus_version, active, created_at, updated_at"#,
        task.id,
        task.capability,
        task.domain_tags,
        task.difficulty_tier,
        task.difficulty_knobs as &JsonRawValue,
        task.prompt,
        task.attachments as &JsonRawValue,
        task.temperature,
        task.max_tokens,
        task.grader_specs as &JsonRawValue,
        task.dimension_weights as &JsonRawValue,
        task.pass_threshold,
        task.smoke_member,
        task.corpus_version,
        task.active,
    )
    .fetch_one(db)
    .await
}

/// `DeactivateCorpusTasksNotIn`: deactivates the version's tasks whose id is not in
/// `task_ids`, and counts already inactive rows too.
///
/// QUIRK: with an empty list, `not (id = any('{}'))` is true for every row, so every task
/// of the version is deactivated. The Go caller never passes an empty list.
pub async fn deactivate_corpus_tasks_not_in<'e>(
    db: impl PgExecutor<'e>,
    corpus_version: &str,
    task_ids: &[String],
) -> sqlx::Result<u64> {
    sqlx::query!(
        "update tasks
         set active = false, updated_at = now()
         where corpus_version = $1
           and not (id = any($2::text[]))",
        corpus_version,
        task_ids
    )
    .execute(db)
    .await
    .map(|done| done.rows_affected())
}

/// `GetTask`.
pub async fn get_task<'e>(db: impl PgExecutor<'e>, id: &str) -> sqlx::Result<Task> {
    sqlx::query_as!(
        Task,
        r#"select id, capability, domain_tags, difficulty_tier,
                  difficulty_knobs as "difficulty_knobs: _", prompt,
                  attachments as "attachments: _", temperature, max_tokens,
                  grader_specs as "grader_specs: _",
                  dimension_weights as "dimension_weights: _", pass_threshold,
                  smoke_member, corpus_version, active, created_at, updated_at
           from tasks
           where id = $1"#,
        id
    )
    .fetch_one(db)
    .await
}

/// `ListTasksByCorpusVersion`: ordered by id.
pub async fn list_tasks_by_corpus_version<'e>(
    db: impl PgExecutor<'e>,
    corpus_version: &str,
) -> sqlx::Result<Vec<Task>> {
    sqlx::query_as!(
        Task,
        r#"select id, capability, domain_tags, difficulty_tier,
                  difficulty_knobs as "difficulty_knobs: _", prompt,
                  attachments as "attachments: _", temperature, max_tokens,
                  grader_specs as "grader_specs: _",
                  dimension_weights as "dimension_weights: _", pass_threshold,
                  smoke_member, corpus_version, active, created_at, updated_at
           from tasks
           where corpus_version = $1
           order by id"#,
        corpus_version
    )
    .fetch_all(db)
    .await
}

/// `ListActiveTasks`: ordered by id.
pub async fn list_active_tasks<'e>(db: impl PgExecutor<'e>) -> sqlx::Result<Vec<Task>> {
    sqlx::query_as!(
        Task,
        r#"select id, capability, domain_tags, difficulty_tier,
                  difficulty_knobs as "difficulty_knobs: _", prompt,
                  attachments as "attachments: _", temperature, max_tokens,
                  grader_specs as "grader_specs: _",
                  dimension_weights as "dimension_weights: _", pass_threshold,
                  smoke_member, corpus_version, active, created_at, updated_at
           from tasks
           where active
           order by id"#
    )
    .fetch_all(db)
    .await
}

/// `ListActiveTasksByCorpusVersion`: ordered by id.
pub async fn list_active_tasks_by_corpus_version<'e>(
    db: impl PgExecutor<'e>,
    corpus_version: &str,
) -> sqlx::Result<Vec<Task>> {
    sqlx::query_as!(
        Task,
        r#"select id, capability, domain_tags, difficulty_tier,
                  difficulty_knobs as "difficulty_knobs: _", prompt,
                  attachments as "attachments: _", temperature, max_tokens,
                  grader_specs as "grader_specs: _",
                  dimension_weights as "dimension_weights: _", pass_threshold,
                  smoke_member, corpus_version, active, created_at, updated_at
           from tasks
           where active and corpus_version = $1
           order by id"#,
        corpus_version
    )
    .fetch_all(db)
    .await
}
