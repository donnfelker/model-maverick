//! Corpus persistence and activation on real Postgres (reference Part 2 §13.3–13.4).
//!
//! The corpus is the synthetic fixture under `parity/fixtures/measurement-io/testdata`,
//! never the private corpus (G7).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use maverick_domain::tasks::{self, Corpus};
use maverick_store::MIGRATOR;
use maverick_store::corpus::{self, CorpusError};
use maverick_testdb::Schema;
use sqlx::PgPool;

// parity: measurement-io/tasks.TestMain (maverick-testdb: one Postgres 16 per binary)
static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

/// The four-task valid corpus the Go store tests load.
fn valid_corpus() -> Corpus {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../parity/fixtures/measurement-io/testdata/internal/tasks/testdata/valid");
    let corpus = tasks::load(&dir).expect("load the fixture corpus");
    assert_eq!(corpus.tasks.len(), 4);
    assert!(corpus.load_violations.is_empty());
    corpus
}

async fn version_row(pool: &PgPool, version: &str) -> Option<corpus::CorpusVersion> {
    match corpus::get_corpus_version(pool, version).await {
        Ok(row) => Some(row),
        Err(err) if maverick_store::is_not_found(&err) => None,
        Err(err) => panic!("get version: {err}"),
    }
}

async fn active_among(pool: &PgPool, versions: &[&str]) -> i64 {
    let versions: Vec<String> = versions.iter().map(|v| (*v).to_owned()).collect();
    sqlx::query_scalar("SELECT count(*) FROM corpus_versions WHERE active AND version = ANY($1)")
        .bind(versions)
        .fetch_one(pool)
        .await
        .expect("count active")
}

#[tokio::test]
// parity: measurement-io/tasks.TestStoreRoundTrip
async fn store_round_trips_the_corpus() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let loaded = valid_corpus();
    corpus::store_corpus(&pool, "v-roundtrip", "mini corpus", &loaded)
        .await
        .expect("store");

    let version = version_row(&pool, "v-roundtrip")
        .await
        .expect("version row");
    assert!(!version.active);
    assert_eq!(version.task_count, 4);
    assert_eq!(version.description, "mini corpus");
    let stored = corpus::list_tasks_by_corpus_version(&pool, "v-roundtrip")
        .await
        .expect("tasks");
    assert_eq!(stored.len(), 4);

    let task = corpus::get_task(&pool, "task_sx_017")
        .await
        .expect("task_sx_017");
    let source = loaded
        .tasks
        .iter()
        .find(|t| t.id == "task_sx_017")
        .expect("in corpus");
    assert_eq!(task.capability, "structured_extraction");
    assert_eq!(task.difficulty_tier, 3);
    assert_eq!(task.prompt, source.prompt);
    assert_eq!(task.max_tokens, 2000);
    assert_eq!(task.pass_threshold, 0.7_f32, "stored as float32");
    assert!(!task.smoke_member);
    assert!(task.active);
    assert_eq!(task.domain_tags, ["legal"]);

    let weights: serde_json::Value =
        serde_json::from_str(task.dimension_weights.get()).expect("weights JSON");
    assert_eq!(weights["field_accuracy"], serde_json::json!(0.4));
    let graders: Vec<serde_json::Value> =
        serde_json::from_str(task.grader_specs.get()).expect("graders JSON");
    assert_eq!(graders.len(), 2);
    assert_eq!(graders[0]["type"], "json_schema");
    let attachments: Vec<serde_json::Value> =
        serde_json::from_str(task.attachments.get()).expect("attachments JSON");
    assert_eq!(attachments[0]["name"], "agreement.txt");
    let knobs: serde_json::Value =
        serde_json::from_str(task.difficulty_knobs.get()).expect("knobs JSON");
    assert_eq!(knobs["field_count"], serde_json::json!(3));
    pool.close().await;
    db.close().await;
}

#[tokio::test]
// parity: measurement-io/tasks.TestStoreReloadDeactivatesRemovedTasks
async fn a_reload_deactivates_removed_tasks() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let mut loaded = valid_corpus();
    corpus::store_corpus(&pool, "v-reload", "", &loaded)
        .await
        .expect("store");
    loaded.tasks.retain(|t| t.id != "task_fq_001");
    corpus::store_corpus(&pool, "v-reload", "", &loaded)
        .await
        .expect("reload");

    assert!(
        !corpus::get_task(&pool, "task_fq_001")
            .await
            .expect("kept row")
            .active
    );
    assert!(
        corpus::get_task(&pool, "task_sx_017")
            .await
            .expect("task")
            .active
    );
    assert_eq!(
        version_row(&pool, "v-reload")
            .await
            .expect("version")
            .task_count,
        3
    );
    pool.close().await;
    db.close().await;
}

#[tokio::test]
// parity: measurement-io/tasks.TestStoreReloadKeepsActiveFlag
async fn a_reload_keeps_the_active_flag() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let loaded = valid_corpus();
    corpus::store_corpus(&pool, "v-keep", "first", &loaded)
        .await
        .expect("store");
    corpus::activate_corpus(&pool, "v-keep")
        .await
        .expect("activate");
    corpus::store_corpus(&pool, "v-keep", "second", &loaded)
        .await
        .expect("reload");
    let version = version_row(&pool, "v-keep").await.expect("version");
    assert!(version.active);
    assert_eq!(version.description, "second");
    pool.close().await;
    db.close().await;
}

#[tokio::test]
// parity: measurement-io/tasks.TestActivateUnknownVersion
async fn activating_an_unknown_version_fails() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let err = corpus::activate_corpus(&pool, "v-does-not-exist")
        .await
        .unwrap_err();
    assert!(matches!(err, CorpusError::UnknownVersion { .. }), "{err:?}");
    assert_eq!(
        err.to_string(),
        r#"tasks: activate "v-does-not-exist": unknown corpus version"#
    );
    pool.close().await;
    db.close().await;
}

#[tokio::test]
// parity: measurement-io/tasks.TestActivateFlipsExactlyOneVersion
async fn activation_flips_exactly_one_version() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let loaded = valid_corpus();
    for version in ["v-a", "v-b"] {
        corpus::store_corpus(&pool, version, "", &loaded)
            .await
            .expect("store");
    }
    corpus::activate_corpus(&pool, "v-a")
        .await
        .expect("activate a");
    corpus::activate_corpus(&pool, "v-b")
        .await
        .expect("activate b");
    let active: Vec<String> = corpus::list_corpus_versions(&pool)
        .await
        .expect("list")
        .into_iter()
        .filter(|v| v.active)
        .map(|v| v.version)
        .collect();
    assert_eq!(active, ["v-b"]);
    pool.close().await;
    db.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
// parity: measurement-io/tasks.TestActivateAtomicity
async fn readers_never_see_other_than_one_active_version() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let versions = ["v-atomic-1", "v-atomic-2"];
    let loaded = valid_corpus();
    for version in versions {
        corpus::store_corpus(&pool, version, "", &loaded)
            .await
            .expect("store");
    }
    corpus::activate_corpus(&pool, versions[0])
        .await
        .expect("activate");

    let done = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicUsize::new(0));
    let mut readers = Vec::new();
    for _ in 0..6 {
        let (pool, done, reads) = (pool.clone(), done.clone(), reads.clone());
        readers.push(tokio::spawn(async move {
            let mut bad = Vec::new();
            while !done.load(Ordering::Relaxed) {
                let count = active_among(&pool, &versions).await;
                reads.fetch_add(1, Ordering::Relaxed);
                if count != 1 {
                    bad.push(count);
                }
            }
            bad
        }));
    }
    let mut writers = Vec::new();
    for version in versions {
        let pool = pool.clone();
        writers.push(tokio::spawn(async move {
            for _ in 0..25 {
                corpus::activate_corpus(&pool, version)
                    .await
                    .expect("activate");
            }
        }));
    }
    for writer in writers {
        writer.await.expect("writer");
    }
    done.store(true, Ordering::Relaxed);
    for reader in readers {
        assert_eq!(
            reader.await.expect("reader"),
            Vec::<i64>::new(),
            "a reader saw a count other than 1"
        );
    }
    assert!(reads.load(Ordering::Relaxed) > 0);
    assert_eq!(active_among(&pool, &versions).await, 1);
    pool.close().await;
    db.close().await;
}

#[tokio::test]
// parity: measurement-io/tasks.TestStoreRejectsEmptyCorpus
async fn store_rejects_an_empty_corpus_and_an_empty_version() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let err = corpus::store_corpus(&pool, "v-empty", "", &Corpus::default())
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "tasks: store v-empty: corpus has no tasks");
    let err = corpus::store_corpus(&pool, "", "", &valid_corpus())
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "tasks: store: version is required");
    assert!(version_row(&pool, "v-empty").await.is_none());
    pool.close().await;
    db.close().await;
}

#[tokio::test]
// parity: measurement-io/tasks.TestStoreIsTransactional
async fn a_failing_task_rolls_back_the_whole_store() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let mut loaded = valid_corpus();
    let mut bad = loaded.tasks[0].clone();
    bad.id = "task_txn_bad".to_owned();
    bad.difficulty_tier = Some(0); // violates the tasks.difficulty_tier CHECK
    loaded.tasks.push(bad);

    let err = corpus::store_corpus(&pool, "v-txn", "", &loaded)
        .await
        .unwrap_err();
    let CorpusError::Task { id, .. } = &err else {
        panic!("a task error: {err:?}");
    };
    assert_eq!(id, "task_txn_bad");
    assert!(
        err.to_string()
            .starts_with("tasks: store v-txn: task task_txn_bad: "),
        "{err}"
    );
    assert!(
        version_row(&pool, "v-txn").await.is_none(),
        "the version row rolled back"
    );
    assert!(
        corpus::list_tasks_by_corpus_version(&pool, "v-txn")
            .await
            .expect("tasks")
            .is_empty()
    );
    pool.close().await;
    db.close().await;
}
