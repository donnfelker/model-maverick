//! Corpus versions and tasks.
//!
//! Go sources: `parity/schema/queries/corpus_versions.sql` and `tasks.sql`, plus the
//! `Store` and `Activate` transactions of `internal/tasks` ([`store_corpus`],
//! [`activate_corpus`]; reference Part 1 §3, Part 2 §13.3).

use chrono::{DateTime, Utc};
use maverick_domain::tasks::Corpus;
use sqlx::{PgExecutor, PgPool};

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

// ---- internal/tasks Store and Activate ----

/// An error from [`store_corpus`] or [`activate_corpus`]. The texts are the Go ones.
#[derive(Debug, thiserror::Error)]
pub enum CorpusError {
    #[error("tasks: store: version is required")]
    VersionRequired,
    #[error("tasks: store {version}: corpus has no tasks")]
    NoTasks { version: String },
    #[error("tasks: store {version}: task {id}: {message}")]
    Task {
        version: String,
        id: String,
        message: String,
    },
    #[error("tasks: store {version}: deactivating removed tasks: {source}")]
    Deactivate {
        version: String,
        source: sqlx::Error,
    },
    #[error("tasks: store {version}: {source}")]
    Store {
        version: String,
        source: sqlx::Error,
    },
    /// `ErrUnknownVersion` in Go.
    #[error("tasks: activate \"{version}\": unknown corpus version")]
    UnknownVersion { version: String },
    #[error("tasks: activate {version}: {source}")]
    Activate {
        version: String,
        source: sqlx::Error,
    },
}

/// Persists a loaded corpus under `version` in one transaction (Go `tasks.Store`).
///
/// The version row is upserted inactive (a reload keeps its active flag), every task is
/// upserted in load order and marked active, and the version's tasks that are not in
/// the corpus are deactivated. Any error rolls everything back. Validate the corpus
/// first; this function does not.
///
/// QUIRK Q-T3: the task id is the primary key, so a task id that exists under another
/// version moves to this one.
pub async fn store_corpus(
    pool: &PgPool,
    version: &str,
    description: &str,
    corpus: &Corpus,
) -> Result<(), CorpusError> {
    if version.is_empty() {
        return Err(CorpusError::VersionRequired);
    }
    if corpus.tasks.is_empty() {
        return Err(CorpusError::NoTasks {
            version: version.to_owned(),
        });
    }
    let store_err = |source| CorpusError::Store {
        version: version.to_owned(),
        source,
    };
    let task_count = i32::try_from(corpus.tasks.len()).unwrap_or(i32::MAX);

    let mut tx = pool.begin().await.map_err(store_err)?;
    upsert_corpus_version(&mut *tx, version, description, task_count, false)
        .await
        .map_err(store_err)?;
    let mut ids = Vec::with_capacity(corpus.tasks.len());
    for task in &corpus.tasks {
        let task_err = |message: String| CorpusError::Task {
            version: version.to_owned(),
            id: task.id.clone(),
            message,
        };
        let raw = |text: String| JsonRawValue::from_string(text).map_err(|err| err.to_string());
        let knobs = task
            .difficulty_knobs_json()
            .and_then(raw)
            .map_err(task_err)?;
        let weights = task
            .dimension_weights_json()
            .and_then(raw)
            .map_err(task_err)?;
        let attachments = raw(task.attachments_json()).map_err(task_err)?;
        let graders = raw(task.grader_specs_json()).map_err(task_err)?;
        let row = TaskUpsert {
            id: &task.id,
            capability: &task.capability,
            domain_tags: &task.domain_tags,
            // Go converted int to int16 and int32 with wraparound; validation rejects
            // such values before a store.
            difficulty_tier: task.tier_or_zero() as i16,
            difficulty_knobs: &knobs,
            prompt: &task.prompt,
            attachments: &attachments,
            temperature: task.temperature,
            max_tokens: task.max_tokens.unwrap_or(0) as i32,
            grader_specs: &graders,
            dimension_weights: &weights,
            pass_threshold: task.pass_threshold as f32,
            smoke_member: task.smoke_member,
            corpus_version: version,
            active: true,
        };
        upsert_task(&mut *tx, &row)
            .await
            .map_err(|err| task_err(err.to_string()))?;
        ids.push(task.id.clone());
    }
    deactivate_corpus_tasks_not_in(&mut *tx, version, &ids)
        .await
        .map_err(|source| CorpusError::Deactivate {
            version: version.to_owned(),
            source,
        })?;
    tx.commit().await.map_err(store_err)
}

/// Makes `version` the only active corpus version in one transaction (Go
/// `tasks.Activate`). The advisory lock serializes concurrent activations, and readers
/// see exactly one active version at every moment.
pub async fn activate_corpus(pool: &PgPool, version: &str) -> Result<(), CorpusError> {
    let activate_err = |source| CorpusError::Activate {
        version: version.to_owned(),
        source,
    };
    let mut tx = pool.begin().await.map_err(activate_err)?;
    acquire_corpus_activation_lock(&mut *tx, CORPUS_ACTIVATION_LOCK_KEY)
        .await
        .map_err(activate_err)?;
    let touched = activate_corpus_version(&mut *tx, version)
        .await
        .map_err(activate_err)?;
    if touched == 0 {
        tx.rollback().await.map_err(activate_err)?;
        return Err(CorpusError::UnknownVersion {
            version: version.to_owned(),
        });
    }
    tx.commit().await.map_err(activate_err)
}
