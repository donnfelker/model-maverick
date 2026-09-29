//! The Go `internal/store` DB tests (reference Part 1 §12), one Rust test each.
//!
//! Known gap (parity/README): these given/when/then cases were written from the Go test
//! code and a manual run, not from their original harness. Where the Rust engine differs
//! by design, the test names the D-row.

use std::str::FromStr;
use std::sync::LazyLock;

use chrono::{Duration, Utc};
use maverick_store::catalog::{self, ModelColumnUpdate, NewModelColumn};
use maverick_store::corpus::{self, TaskUpsert};
use maverick_store::health::{self, GRAPHILE_WORKER_SCHEMA};
use maverick_store::measurement::{self, NewRun};
use maverick_store::tenancy;
use maverick_store::{
    Decimal, JsonRawValue, MIGRATOR, batches, is_not_found, numeric_to_f64, sqlstate,
};
use maverick_testdb::Schema;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use test_support::step;
use ulid::Ulid;

async fn migrate_job_queue(url: String) -> Result<(), maverick_testdb::BoxError> {
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await?;
    graphile_worker::WorkerUtils::new(pool.clone(), GRAPHILE_WORKER_SCHEMA)
        .migrate()
        .await?;
    pool.close().await;
    Ok(())
}

// parity: store.TestMain (maverick-testdb boots Postgres 16 once per binary and migrates)
static SCHEMA: LazyLock<Schema> = LazyLock::new(|| {
    Schema::sqlx(&MIGRATOR).then(Schema::custom("graphile_worker 0.13", migrate_job_queue))
});

fn json(text: &str) -> Box<JsonRawValue> {
    JsonRawValue::from_string(text.to_owned()).expect("valid JSON")
}

fn ulid() -> String {
    Ulid::generate().to_string()
}

/// Seed helper: tenant `Acme` with 32 random HMAC bytes.
async fn seed_tenant(pool: &PgPool) -> String {
    let id = format!("ten_{}", ulid());
    let key: Vec<u8> = Sha256::digest(id.as_bytes()).to_vec();
    tenancy::create_tenant(pool, &id, "Acme", &key)
        .await
        .expect("tenant");
    id
}

/// Seed helper: an `anthropic`/`direct` column `claude-test-m_<ulid>`, status
/// `cold_start`, settings and pricing `{}`.
async fn seed_column(pool: &PgPool) -> String {
    let id = format!("col_{}", ulid());
    let model = format!("claude-test-m_{}", ulid());
    let empty = json("{}");
    catalog::create_model_column(
        pool,
        &NewModelColumn {
            id: &id,
            provider: "anthropic",
            model_id: &model,
            endpoint: "direct",
            settings: &empty,
            status: "cold_start",
            pricing: &empty,
            quality_source_column_id: None,
            cold_start_until: None,
            deprecation_date: None,
            execution_aliases: None,
            retention_mode: None,
            retention_verified_at: None,
            retention_valid_until: None,
            retention_source: None,
            execution_alias_retention: None,
            param_constraints: None,
        },
    )
    .await
    .expect("column");
    id
}

/// Seed helper: corpus version `cv_<ulid>` and one task in it.
async fn seed_task(pool: &PgPool) -> (String, String) {
    let version = format!("cv_{}", ulid());
    corpus::upsert_corpus_version(pool, &version, "", 1, false)
        .await
        .expect("version");
    let id = format!("task_{}", ulid().to_lowercase());
    let tags = vec!["legal".to_owned()];
    let (knobs, attachments) = (json("{}"), json("[]"));
    let (graders, weights) = (
        json(r#"[{"type":"json_schema"}]"#),
        json(r#"{"schema_validity":1.0}"#),
    );
    corpus::upsert_task(
        pool,
        &TaskUpsert {
            id: &id,
            capability: "structured_extraction",
            domain_tags: &tags,
            difficulty_tier: 3,
            difficulty_knobs: &knobs,
            prompt: "Extract the parties from the contract.",
            attachments: &attachments,
            temperature: 0.0,
            max_tokens: 1024,
            grader_specs: &graders,
            dimension_weights: &weights,
            pass_threshold: 0.7,
            smoke_member: false,
            corpus_version: &version,
            active: true,
        },
    )
    .await
    .expect("task");
    (version, id)
}

/// Seed helper: one `ok` run of a fresh task and column in a fresh `bat_` batch.
async fn seed_run(pool: &PgPool) -> measurement::Run {
    let (_, task) = seed_task(pool).await;
    let column = seed_column(pool).await;
    let (id, batch) = (format!("run_{}", ulid()), format!("bat_{}", ulid()));
    measurement::insert_run(pool, &new_run(&id, &batch, &task, &column))
        .await
        .expect("run")
}

fn new_run<'a>(id: &'a str, batch: &'a str, task: &'a str, column: &'a str) -> NewRun<'a> {
    NewRun {
        id,
        batch_id: batch,
        task_id: task,
        model_column_id: column,
        repeat_index: 0,
        output: Some(r#"{"parties": ["A", "B"]}"#),
        tokens_in: Some(812),
        tokens_out: Some(64),
        latency_ms: Some(1450),
        cost: Some(Decimal::from_str("0.012345").unwrap()),
        temperature: 0.0,
        status: "ok",
        error: None,
        matrix_version: "2026-07-01",
    }
}

async fn regclass(pool: &PgPool, name: &str) -> Option<String> {
    sqlx::query_scalar("SELECT to_regclass($1)::text")
        .bind(name)
        .fetch_one(pool)
        .await
        .expect("to_regclass")
}

// parity: store.TestMigrateRunsGooseThenRiverIdempotently
// D21/D22: sqlx replaces goose and graphile_worker replaces River.
#[tokio::test]
async fn migrate_runs_the_engine_then_the_job_queue_idempotently() {
    let _logs = test_support::init_test_tracing();
    let Some(db) = maverick_testdb::database(&Schema::empty()).await else {
        return;
    };
    let pool = db.pool().await;
    for pass in 1..=2 {
        step!("migrate, pass {pass}");
        MIGRATOR.run(&pool).await.expect("engine migrations");
        migrate_job_queue(db.url().to_owned())
            .await
            .expect("job queue migrations");
    }
    step!("check the tables");
    for table in [
        "public._sqlx_migrations",
        "public.tenants",
        "public.runs",
        "public.batches",
        "graphile_worker._private_jobs",
        "graphile_worker._private_workers",
    ] {
        assert!(regclass(&pool, table).await.is_some(), "{table} exists");
    }
    assert_eq!(regclass(&pool, "public.goose_db_version").await, None);
    assert_eq!(regclass(&pool, "public.river_job").await, None);
    pool.close().await;
    db.close().await;
}

// parity: store.TestReadJobHealth
// D35: graphile_worker workers replace River queues (no `paused`), and last batch times
// come from `batches.completed_at`.
#[tokio::test]
async fn read_job_health() {
    let _logs = test_support::init_test_tracing();
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;

    step!("empty database");
    assert!(
        health::list_worker_heartbeats(&pool, GRAPHILE_WORKER_SCHEMA)
            .await
            .expect("workers")
            .is_empty()
    );
    assert!(
        batches::list_last_completed_batches(&pool)
            .await
            .expect("batches")
            .is_empty()
    );

    step!("seed workers and batches");
    for worker in ["graphile_worker_default", "graphile_worker_stale"] {
        sqlx::query("SELECT graphile_worker.worker_heartbeat($1)")
            .bind(worker)
            .execute(&pool)
            .await
            .expect("heartbeat");
    }
    sqlx::query(
        "UPDATE graphile_worker._private_workers SET last_heartbeat_at = now() - interval '10 minutes'
         WHERE id = 'graphile_worker_stale'",
    )
    .execute(&pool)
    .await
    .expect("stale worker");
    sqlx::raw_sql(
        "INSERT INTO batches (id, kind, batch_window, status, completed_at) VALUES
           ('bat_1', 'nightly_smoke', '2026-09-27', 'completed', now() - interval '2 hours'),
           ('bat_2', 'nightly_smoke', '2026-09-28', 'completed', now() - interval '1 hour'),
           ('bat_3', 'weekly_sweep', '2026-W39', 'completed', now() - interval '3 days');
         INSERT INTO batches (id, kind, batch_window, status, lease_owner)
           VALUES ('bat_4', 'latency_probes', '2026-09-28T10', 'running', 'w');",
    )
    .execute(&pool)
    .await
    .expect("batches");

    step!("read");
    let workers = health::list_worker_heartbeats(&pool, GRAPHILE_WORKER_SCHEMA)
        .await
        .expect("workers");
    assert_eq!(workers.len(), 2);
    assert!(workers[0].age_seconds <= 30.0, "{workers:?}");
    assert!(workers[1].age_seconds >= 300.0, "{workers:?}");
    let last = batches::list_last_completed_batches(&pool)
        .await
        .expect("batches");
    let kinds: Vec<&str> = last.iter().map(|(kind, _)| kind.as_str()).collect();
    assert_eq!(
        kinds,
        ["nightly_smoke", "weekly_sweep"],
        "no latency_probes"
    );
    let since = Utc::now() - last[0].1;
    assert!(
        since > Duration::zero() && since <= Duration::minutes(90),
        "{since}"
    );
    pool.close().await;
    db.close().await;
}

// parity: store.TestTenantsRoundTrip
#[tokio::test]
async fn tenants_round_trip() {
    let _logs = test_support::init_test_tracing();
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    step!("create");
    let id = seed_tenant(&pool).await;
    let created = tenancy::get_tenant(&pool, &id).await.expect("get");
    assert_eq!(
        (created.name.as_str(), created.status.as_str()),
        ("Acme", "active")
    );
    step!("update");
    let updated = tenancy::update_tenant(&pool, &id, "Acme Corp", "suspended")
        .await
        .expect("update");
    assert_eq!(
        (updated.name.as_str(), updated.status.as_str()),
        ("Acme Corp", "suspended")
    );
    assert!(
        tenancy::list_tenants(&pool)
            .await
            .expect("list")
            .iter()
            .any(|t| t.id == id)
    );
    step!("delete");
    tenancy::delete_tenant(&pool, &id).await.expect("delete");
    assert!(is_not_found(
        &tenancy::get_tenant(&pool, &id).await.unwrap_err()
    ));
    pool.close().await;
    db.close().await;
}

// parity: store.TestAPIKeysRoundTrip
#[tokio::test]
async fn api_keys_round_trip() {
    let _logs = test_support::init_test_tracing();
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let tenant = seed_tenant(&pool).await;
    let id = format!("key_{}", ulid());
    let hash = Sha256::digest(ulid().as_bytes()).to_vec();
    step!("create");
    let key = tenancy::create_api_key(&pool, &id, &tenant, &hash, "f00d", "rails-app")
        .await
        .expect("create");
    assert_eq!((key.status.as_str(), key.revoked_at), ("active", None));
    assert_eq!(
        tenancy::get_api_key_by_hash(&pool, &hash)
            .await
            .expect("by hash")
            .id,
        id
    );
    let listed = tenancy::list_api_keys_by_tenant(&pool, &tenant)
        .await
        .expect("list");
    assert_eq!(
        listed.iter().map(|k| k.id.as_str()).collect::<Vec<_>>(),
        [id.as_str()]
    );
    step!("revoke");
    let revoked = tenancy::revoke_api_key(&pool, &id).await.expect("revoke");
    assert_eq!(revoked.status, "revoked");
    assert!(revoked.revoked_at.is_some());
    step!("delete");
    tenancy::delete_api_key(&pool, &id).await.expect("delete");
    assert!(is_not_found(
        &tenancy::get_api_key(&pool, &id).await.unwrap_err()
    ));
    pool.close().await;
    db.close().await;
}

fn update_of(column: &catalog::ModelColumn) -> ModelColumnUpdate<'_> {
    ModelColumnUpdate {
        settings: &column.settings,
        status: &column.status,
        pricing: &column.pricing,
        quality_source_column_id: column.quality_source_column_id.as_deref(),
        cold_start_until: column.cold_start_until,
        deprecation_date: column.deprecation_date,
        execution_aliases: None,
        retention_mode: &column.retention_mode,
        retention_verified_at: column.retention_verified_at,
        retention_valid_until: column.retention_valid_until,
        retention_source: column.retention_source.as_deref(),
        execution_alias_retention: None,
        param_constraints: None,
    }
}

// parity: store.TestModelColumnsRoundTrip
#[tokio::test]
async fn model_columns_round_trip() {
    let _logs = test_support::init_test_tracing();
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let id = format!("col_{}", ulid());
    let model = format!("claude-test-m_{}", ulid());
    let settings = json(r#"{"thinking": true}"#);
    let pricing = json(r#"{"input_per_mtok": 3.0, "output_per_mtok": 15.0}"#);
    let soon = Utc::now().date_naive() + Duration::days(5);
    step!("create");
    let mut new = NewModelColumn {
        id: &id,
        provider: "anthropic",
        model_id: &model,
        endpoint: "direct",
        settings: &settings,
        status: "cold_start",
        pricing: &pricing,
        quality_source_column_id: None,
        cold_start_until: Some(soon),
        deprecation_date: None,
        execution_aliases: None,
        retention_mode: None,
        retention_verified_at: None,
        retention_valid_until: None,
        retention_source: None,
        execution_alias_retention: None,
        param_constraints: None,
    };
    catalog::create_model_column(&pool, &new)
        .await
        .expect("create");
    let got = catalog::get_model_column(&pool, &id).await.expect("get");
    assert_eq!(
        (got.provider.as_str(), got.endpoint.as_str()),
        ("anthropic", "direct")
    );
    assert_eq!(got.cold_start_until, Some(soon));

    step!("update: status active, new pricing, cold_start_until NULL");
    let new_pricing = json(r#"{"input_per_mtok": 2.5, "output_per_mtok": 12.0}"#);
    let mut update = update_of(&got);
    update.status = "active";
    update.pricing = &new_pricing;
    update.cold_start_until = None;
    let updated = catalog::update_model_column(&pool, &id, &update)
        .await
        .expect("update");
    assert_eq!(updated.status, "active");
    assert_eq!(
        updated.cold_start_until, None,
        "a NULL param clears the column"
    );
    assert!(
        catalog::list_model_columns(&pool)
            .await
            .expect("list")
            .iter()
            .any(|c| c.id == id)
    );

    step!("delete");
    catalog::delete_model_column(&pool, &id)
        .await
        .expect("delete");
    assert!(is_not_found(
        &catalog::get_model_column(&pool, &id).await.unwrap_err()
    ));

    // The identity index hashes the normalized settings text, so key order and spacing
    // do not make a new column (reference Part 1 §2.3).
    step!("identity collision");
    let a = json(r#"{"a":1,"b":2}"#);
    let b = json(r#"{"b": 2, "a": 1}"#);
    new.settings = &a;
    catalog::create_model_column(&pool, &new)
        .await
        .expect("create a");
    let other = format!("col_{}", ulid());
    new.id = &other;
    new.settings = &b;
    let err = catalog::create_model_column(&pool, &new).await.unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("23505"), "{err}");
    pool.close().await;
    db.close().await;
}

// parity: store.TestModelColumnsExecutionAliasesRoundTrip
#[tokio::test]
async fn model_columns_execution_aliases_round_trip() {
    let _logs = test_support::init_test_tracing();
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    step!("1. create without aliases");
    let plain = seed_column(&pool).await;
    assert_eq!(
        catalog::get_model_column(&pool, &plain)
            .await
            .expect("get")
            .execution_aliases
            .get(),
        "{}"
    );

    step!("2. create an openrouter column with aliases");
    let id = format!("col_{}", ulid());
    let model = format!("claude-test-m_{}", ulid());
    let empty = json("{}");
    let aliases = json(r#"{"openrouter": "anthropic/claude-sonnet-4-6"}"#);
    catalog::create_model_column(
        &pool,
        &NewModelColumn {
            id: &id,
            provider: "anthropic",
            model_id: &model,
            endpoint: "openrouter",
            settings: &empty,
            status: "cold_start",
            pricing: &empty,
            quality_source_column_id: None,
            cold_start_until: None,
            deprecation_date: None,
            execution_aliases: Some(&aliases),
            retention_mode: None,
            retention_verified_at: None,
            retention_valid_until: None,
            retention_source: None,
            execution_alias_retention: None,
            param_constraints: None,
        },
    )
    .await
    .expect("create");
    let got = catalog::get_model_column(&pool, &id).await.expect("get");
    assert_eq!(got.endpoint, "openrouter", "endpoint has no CHECK");
    let decoded: serde_json::Value = serde_json::from_str(got.execution_aliases.get()).unwrap();
    assert_eq!(
        decoded,
        serde_json::json!({"openrouter": "anthropic/claude-sonnet-4-6"})
    );

    step!("3. replace the map whole, unknown keys kept");
    let replaced =
        json(r#"{"openrouter": "anthropic/claude-opus-4-8", "future_key": "ignored-but-kept"}"#);
    let mut update = update_of(&got);
    update.execution_aliases = Some(&replaced);
    let step3 = catalog::update_model_column(&pool, &id, &update)
        .await
        .expect("update");
    let decoded: serde_json::Value = serde_json::from_str(step3.execution_aliases.get()).unwrap();
    assert_eq!(
        decoded,
        serde_json::json!({"openrouter": "anthropic/claude-opus-4-8", "future_key": "ignored-but-kept"})
    );

    step!("4. NULL aliases keep the stored map");
    let mut update = update_of(&step3);
    update.status = "active";
    let step4 = catalog::update_model_column(&pool, &id, &update)
        .await
        .expect("update");
    assert_eq!(step4.execution_aliases.get(), step3.execution_aliases.get());
    pool.close().await;
    db.close().await;
}

// parity: store.TestTasksUpsertRoundTrip
#[tokio::test]
async fn tasks_upsert_round_trip() {
    let _logs = test_support::init_test_tracing();
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let (version, id) = seed_task(&pool).await;
    let before = corpus::get_task(&pool, &id).await.expect("seeded");
    step!("upsert the same id");
    let tags = vec!["legal".to_owned(), "fintech".to_owned()];
    let (knobs, attachments) = (json("{}"), json("[]"));
    let (graders, weights) = (
        json(r#"[{"type":"json_schema"}]"#),
        json(r#"{"schema_validity":1.0}"#),
    );
    let after = corpus::upsert_task(
        &pool,
        &TaskUpsert {
            id: &id,
            capability: "structured_extraction",
            domain_tags: &tags,
            difficulty_tier: 4,
            difficulty_knobs: &knobs,
            prompt: "Extract the parties and effective date.",
            attachments: &attachments,
            temperature: 0.0,
            max_tokens: 1024,
            grader_specs: &graders,
            dimension_weights: &weights,
            pass_threshold: 0.7,
            smoke_member: true,
            corpus_version: &version,
            active: true,
        },
    )
    .await
    .expect("upsert");
    assert_eq!(
        (
            after.difficulty_tier,
            after.smoke_member,
            after.domain_tags.len()
        ),
        (4, true, 2)
    );
    assert_eq!(after.created_at, before.created_at);
    assert_eq!(
        corpus::get_task(&pool, &id).await.expect("get").prompt,
        "Extract the parties and effective date."
    );
    let listed = corpus::list_tasks_by_corpus_version(&pool, &version)
        .await
        .expect("list");
    assert_eq!(
        listed.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
        [id.as_str()]
    );
    pool.close().await;
    db.close().await;
}

// parity: store.TestRunsInsertRoundTrip
#[tokio::test]
async fn runs_insert_round_trip() {
    let _logs = test_support::init_test_tracing();
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let run = seed_run(&pool).await;
    let got = measurement::get_run(&pool, &run.id).await.expect("get");
    assert_eq!(got.status, "ok");
    assert_eq!(got.output.as_deref(), Some(r#"{"parties": ["A", "B"]}"#));
    assert_eq!(got.latency_ms, Some(1450));
    let cost = numeric_to_f64(got.cost.expect("cost"));
    assert!((cost - 0.012345).abs() < 1e-9, "{cost}");
    let listed = measurement::list_runs_by_batch(&pool, &run.batch_id)
        .await
        .expect("list");
    assert_eq!(
        listed.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        [run.id.as_str()]
    );
    pool.close().await;
    db.close().await;
}

// parity: store.TestRunsUniqueIndexOnBatchTaskColumnRepeat
#[tokio::test]
async fn runs_unique_index_on_batch_task_column_repeat() {
    let _logs = test_support::init_test_tracing();
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let run = seed_run(&pool).await;
    let other = format!("run_{}", ulid());
    let err = measurement::insert_run(
        &pool,
        &new_run(&other, &run.batch_id, &run.task_id, &run.model_column_id),
    )
    .await
    .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("23505"), "{err}");
    pool.close().await;
    db.close().await;
}

// parity: store.TestScoresRoundTrip
#[tokio::test]
async fn scores_round_trip() {
    let _logs = test_support::init_test_tracing();
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let run = seed_run(&pool).await;
    let detail = json(r#"{"rule_hits": 2}"#);
    let score =
        measurement::insert_score(&pool, &run.id, 0, "schema_validity", 0.85, Some(&detail))
            .await
            .expect("insert");
    assert_eq!(score.value, 0.85_f32);
    let listed = measurement::list_scores_by_run(&pool, &run.id)
        .await
        .expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].grader_idx, 0);
    pool.close().await;
    db.close().await;
}
