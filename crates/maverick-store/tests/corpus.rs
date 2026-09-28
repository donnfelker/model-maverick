//! Store tests for corpus versions and tasks (real Postgres).

use std::sync::LazyLock;

use maverick_store::corpus::{self, CORPUS_ACTIVATION_LOCK_KEY, Task, TaskUpsert};
use maverick_store::{Json, JsonRawValue, MIGRATOR, is_not_found, sqlstate};
use maverick_testdb::Schema;

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

fn json(text: &str) -> Json {
    JsonRawValue::from_string(text.to_owned()).expect("valid JSON")
}

fn code(result: sqlx::Result<impl Sized>) -> String {
    match result {
        Ok(_) => panic!("expected a database error"),
        Err(err) => sqlstate(&err).unwrap_or_else(|| panic!("not a database error: {err}")),
    }
}

fn active_versions(versions: &[corpus::CorpusVersion]) -> Vec<&str> {
    versions
        .iter()
        .filter(|v| v.active)
        .map(|v| v.version.as_str())
        .collect()
}

#[tokio::test]
async fn corpus_versions_upsert_keeps_active_and_created_at() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;

    let first = corpus::upsert_corpus_version(&pool, "2026-07-01", "first", 10, true)
        .await
        .expect("insert");
    assert!(first.active);
    let again = corpus::upsert_corpus_version(&pool, "2026-07-01", "renamed", 12, false)
        .await
        .expect("conflict");
    assert_eq!(
        (again.description.as_str(), again.task_count),
        ("renamed", 12)
    );
    assert!(again.active, "a conflict keeps the stored active flag");
    assert_eq!(again.created_at, first.created_at);

    let read = corpus::get_corpus_version(&pool, "2026-07-01")
        .await
        .expect("get");
    assert_eq!(read.description, "renamed");
    assert!(is_not_found(
        &corpus::get_corpus_version(&pool, "1999-01-01")
            .await
            .unwrap_err()
    ));
    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn activation_leaves_exactly_one_active_version() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;

    assert!(is_not_found(
        &corpus::get_active_corpus_version(&pool).await.unwrap_err()
    ));
    for version in ["2026-08-01", "2026-07-01"] {
        corpus::upsert_corpus_version(&pool, version, "", 0, false)
            .await
            .expect("insert");
    }
    // Ordered by created_at, then version: insertion order here.
    let listed: Vec<String> = corpus::list_corpus_versions(&pool)
        .await
        .expect("list")
        .into_iter()
        .map(|v| v.version)
        .collect();
    assert_eq!(listed, ["2026-08-01", "2026-07-01"]);

    assert_eq!(
        corpus::activate_corpus_version(&pool, "1999-01-01")
            .await
            .expect("unknown"),
        0,
        "an unknown version touches nothing"
    );
    assert_eq!(
        corpus::activate_corpus_version(&pool, "2026-07-01")
            .await
            .expect("activate"),
        1
    );
    assert_eq!(
        corpus::activate_corpus_version(&pool, "2026-08-01")
            .await
            .expect("switch"),
        2,
        "the old active row and the new one"
    );
    let versions = corpus::list_corpus_versions(&pool).await.expect("list");
    assert_eq!(active_versions(&versions), ["2026-08-01"]);
    assert_eq!(
        corpus::get_active_corpus_version(&pool)
            .await
            .expect("active")
            .version,
        "2026-08-01"
    );
    assert_eq!(
        corpus::activate_corpus_version(&pool, "1999-01-01")
            .await
            .expect("unknown"),
        0
    );
    let versions = corpus::list_corpus_versions(&pool).await.expect("list");
    assert_eq!(
        active_versions(&versions),
        ["2026-08-01"],
        "an unknown version leaves the active one"
    );
    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn the_activation_lock_is_held_until_the_transaction_ends() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let mut holder = pool.begin().await.expect("begin");
    corpus::acquire_corpus_activation_lock(&mut *holder, CORPUS_ACTIVATION_LOCK_KEY)
        .await
        .expect("lock");

    let try_lock = "SELECT pg_try_advisory_xact_lock($1)";
    let mut other = pool.begin().await.expect("begin");
    let got: bool = sqlx::query_scalar(try_lock)
        .bind(CORPUS_ACTIVATION_LOCK_KEY)
        .fetch_one(&mut *other)
        .await
        .expect("try lock");
    assert!(!got, "a second transaction cannot take the lock");
    other.rollback().await.expect("rollback");

    holder.commit().await.expect("commit");
    let mut other = pool.begin().await.expect("begin");
    let got: bool = sqlx::query_scalar(try_lock)
        .bind(CORPUS_ACTIVATION_LOCK_KEY)
        .fetch_one(&mut *other)
        .await
        .expect("try lock");
    assert!(got, "commit released the lock");
    other.rollback().await.expect("rollback");
    assert_eq!(CORPUS_ACTIVATION_LOCK_KEY, 120_355_241_816_688);
    pool.close().await;
    db.close().await;
}

struct TaskJson {
    knobs: Json,
    attachments: Json,
    graders: Json,
    weights: Json,
}

impl TaskJson {
    fn new() -> Self {
        Self {
            knobs: json("{}"),
            attachments: json("[]"),
            graders: json(r#"[{"type":"json_schema"}]"#),
            weights: json(r#"{"schema_validity":1.0}"#),
        }
    }

    fn task<'a>(&'a self, id: &'a str, version: &'a str, tags: &'a [String]) -> TaskUpsert<'a> {
        TaskUpsert {
            id,
            capability: "structured_extraction",
            domain_tags: tags,
            difficulty_tier: 3,
            difficulty_knobs: &self.knobs,
            prompt: "Extract the parties from the contract.",
            attachments: &self.attachments,
            temperature: 0.0,
            max_tokens: 1024,
            grader_specs: &self.graders,
            dimension_weights: &self.weights,
            pass_threshold: 0.7,
            smoke_member: false,
            corpus_version: version,
            active: true,
        }
    }
}

fn task_ids(tasks: Vec<Task>) -> Vec<String> {
    tasks.into_iter().map(|t| t.id).collect()
}

#[tokio::test]
async fn upsert_task_replaces_every_column_and_keeps_created_at() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    corpus::upsert_corpus_version(&pool, "v1", "", 1, false)
        .await
        .expect("version");
    corpus::upsert_corpus_version(&pool, "v2", "", 1, false)
        .await
        .expect("version");
    let data = TaskJson::new();
    let tags = vec!["legal".to_owned()];

    let first = corpus::upsert_task(&pool, &data.task("task_sx_017", "v1", &tags))
        .await
        .expect("insert");
    assert_eq!(first.domain_tags, ["legal"]);
    assert_eq!(first.pass_threshold, 0.7_f32);
    assert_eq!(f64::from(first.pass_threshold), 0.699_999_988_079_071);
    assert_eq!(first.grader_specs.get(), r#"[{"type": "json_schema"}]"#);
    assert_eq!(first.dimension_weights.get(), r#"{"schema_validity": 1.0}"#);

    let no_tags: Vec<String> = Vec::new();
    let mut changed = data.task("task_sx_017", "v2", &no_tags);
    changed.difficulty_tier = 5;
    changed.temperature = 0.3;
    changed.smoke_member = true;
    changed.active = false;
    let second = corpus::upsert_task(&pool, &changed)
        .await
        .expect("conflict");
    assert_eq!(second.corpus_version, "v2");
    assert_eq!(second.difficulty_tier, 5);
    assert_eq!(second.temperature, 0.3_f32);
    assert!(second.domain_tags.is_empty());
    assert!(second.smoke_member && !second.active);
    assert_eq!(second.created_at, first.created_at);
    assert!(second.updated_at >= first.updated_at);

    let mut bad = data.task("task_bad", "v1", &tags);
    bad.difficulty_tier = 6;
    assert_eq!(code(corpus::upsert_task(&pool, &bad).await), "23514");
    assert_eq!(
        code(corpus::upsert_task(&pool, &data.task("task_x", "v9", &tags)).await),
        "23503",
        "corpus_version must exist"
    );
    assert!(is_not_found(
        &corpus::get_task(&pool, "task_missing").await.unwrap_err()
    ));
    assert_eq!(
        corpus::get_task(&pool, "task_sx_017")
            .await
            .expect("get")
            .difficulty_tier,
        5
    );
    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn task_lists_and_deactivation() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    for version in ["v1", "v2"] {
        corpus::upsert_corpus_version(&pool, version, "", 0, false)
            .await
            .expect("version");
    }
    let data = TaskJson::new();
    let tags: Vec<String> = Vec::new();
    for (id, version, active) in [
        ("task_c", "v1", true),
        ("task_a", "v1", true),
        ("task_b", "v1", false),
        ("task_d", "v2", true),
    ] {
        let mut task = data.task(id, version, &tags);
        task.active = active;
        corpus::upsert_task(&pool, &task).await.expect("task");
    }

    assert_eq!(
        task_ids(
            corpus::list_tasks_by_corpus_version(&pool, "v1")
                .await
                .expect("list")
        ),
        ["task_a", "task_b", "task_c"]
    );
    assert_eq!(
        task_ids(corpus::list_active_tasks(&pool).await.expect("list")),
        ["task_a", "task_c", "task_d"]
    );
    assert_eq!(
        task_ids(
            corpus::list_active_tasks_by_corpus_version(&pool, "v1")
                .await
                .expect("list")
        ),
        ["task_a", "task_c"]
    );

    // Keep only task_a in v1. task_b was already inactive and is counted too.
    let keep = vec!["task_a".to_owned(), "task_d".to_owned()];
    assert_eq!(
        corpus::deactivate_corpus_tasks_not_in(&pool, "v1", &keep)
            .await
            .expect("deactivate"),
        2
    );
    assert_eq!(
        task_ids(corpus::list_active_tasks(&pool).await.expect("list")),
        ["task_a", "task_d"],
        "v2 is untouched"
    );
    assert_eq!(
        corpus::deactivate_corpus_tasks_not_in(&pool, "v9", &keep)
            .await
            .expect("unknown version"),
        0
    );
    pool.close().await;
    db.close().await;
}
