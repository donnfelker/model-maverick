//! Batch persistence on real Postgres (reference Part 2 §5.7), through a [`RunSink`] that
//! calls the store's `InsertRun`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::str::FromStr;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use maverick_providers::fake::{FakeProvider, FixtureSpec};
use maverick_runner::{
    Batch, BatchColumn, BatchTask, Cancellation, Clients, RunRow, RunSink, Runner, RunnerConfig,
    SinkFuture,
};
use maverick_store::catalog::{self, NewModelColumn};
use maverick_store::corpus::{self, TaskUpsert};
use maverick_store::measurement::{self, NewRun};
use maverick_store::{Decimal, JsonRawValue, MIGRATOR};
use maverick_testdb::Schema;
use serde_json::Value;
use sqlx::PgPool;

/// The shared Postgres harness: `maverick-testdb` boots one Postgres 16 per test binary
/// and migrates it once.
///
/// parity: runner.TestMain
static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

/// `InsertRun`, one call per row.
struct StoreSink(PgPool);

impl RunSink for StoreSink {
    fn insert_run<'a>(&'a self, row: &'a RunRow) -> SinkFuture<'a> {
        Box::pin(async move {
            let cost = row.cost.as_deref().map(Decimal::from_str).transpose()?;
            measurement::insert_run(
                &self.0,
                &NewRun {
                    id: &row.id,
                    batch_id: &row.batch_id,
                    task_id: &row.task_id,
                    model_column_id: &row.model_column_id,
                    repeat_index: row.repeat_index,
                    output: row.output.as_deref(),
                    tokens_in: row.tokens_in,
                    tokens_out: row.tokens_out,
                    latency_ms: row.latency_ms,
                    cost,
                    temperature: row.temperature,
                    status: row.status,
                    error: row.error.as_deref(),
                    matrix_version: &row.matrix_version,
                },
            )
            .await?;
            Ok(())
        })
    }
}

fn json(text: &str) -> Box<JsonRawValue> {
    JsonRawValue::from_string(text.to_owned()).expect("json")
}

const VERSION: &str = "2026-07-01";

async fn seed_task(pool: &PgPool, id: &str, prompt: &str) {
    let tags: Vec<String> = Vec::new();
    corpus::upsert_task(
        pool,
        &TaskUpsert {
            id,
            capability: "structured_extraction",
            domain_tags: &tags,
            difficulty_tier: 1,
            difficulty_knobs: &json("{}"),
            prompt,
            attachments: &json("[]"),
            temperature: 0.0,
            max_tokens: 256,
            grader_specs: &json(r#"[{"type":"golden_answer"}]"#),
            dimension_weights: &json(r#"{"answer_correctness":1.0}"#),
            pass_threshold: 0.7,
            smoke_member: false,
            corpus_version: VERSION,
            active: true,
        },
    )
    .await
    .expect("task");
}

async fn seed_column(pool: &PgPool, column: &BatchColumn) {
    catalog::create_model_column(
        pool,
        &NewModelColumn {
            id: &column.id,
            provider: &column.provider,
            model_id: &column.model_id,
            endpoint: "direct",
            settings: &json(&column.settings_json),
            status: &column.status,
            pricing: &json(&column.pricing_json),
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
}

fn batch_task(id: &str, prompt: &str) -> BatchTask {
    BatchTask {
        id: id.to_owned(),
        prompt: prompt.to_owned(),
        attachments_json: "[]".to_owned(),
        temperature: 0.0,
        max_tokens: 256,
    }
}

fn to_f64(cost: Option<Decimal>) -> f64 {
    cost.expect("cost").to_string().parse().expect("f64")
}

/// parity: runner.TestBatchPersistsTwoTasksTwoColumnsThreeRepeats
#[tokio::test]
async fn persists_two_tasks_two_columns_three_repeats() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let doc = parity_support::fixture("measurement-io", "runner.json").expect("fixture");
    let case = &doc["batch_integration"];

    corpus::upsert_corpus_version(&pool, VERSION, "", 2, true)
        .await
        .expect("version");
    let prompts = [
        ("task_one", "one", "Runner integration prompt one."),
        ("task_two", "two", "Runner integration prompt two."),
    ];
    for (id, _, prompt) in prompts {
        seed_task(&pool, id, prompt).await;
    }
    let columns: Vec<BatchColumn> = case["columns"]
        .as_array()
        .expect("columns")
        .iter()
        .zip(["col_a", "col_b"])
        .map(|(c, id)| BatchColumn {
            id: id.to_owned(),
            provider: c["provider"].as_str().expect("provider").to_owned(),
            model_id: c["model_id"].as_str().expect("model").to_owned(),
            status: "active".to_owned(),
            settings_json: "{}".to_owned(),
            pricing_json: c["pricing_json"].as_str().expect("pricing").to_owned(),
        })
        .collect();
    for column in &columns {
        seed_column(&pool, column).await;
    }
    let specs: Vec<FixtureSpec> =
        serde_json::from_value(case["fake_fixtures"].clone()).expect("fixtures");
    let fake = Arc::new(FakeProvider::from_fixtures(&specs).expect("fake"));
    let clients = Clients::new()
        .with("anthropic", Arc::clone(&fake) as _)
        .with("openai", fake);
    let config = RunnerConfig {
        repeats: 3,
        timeout: Duration::from_secs(30),
        limits: HashMap::new(),
    };
    let runner = Runner::new(clients, Arc::new(StoreSink(pool.clone())), config);
    let summary = runner
        .run(
            &Batch {
                id: String::new(),
                matrix_version: VERSION.to_owned(),
                repeats: 0,
                tasks: prompts
                    .iter()
                    .map(|(id, _, prompt)| batch_task(id, prompt))
                    .collect(),
                columns,
                done: HashSet::new(),
            },
            &Cancellation::new(),
        )
        .await;
    assert_eq!(summary.error(), None);
    assert!(summary.batch_id.starts_with("bat_"));

    let rows = measurement::list_runs_by_batch(&pool, &summary.batch_id)
        .await
        .expect("rows");
    let expect = &case["expect"];
    assert_eq!(rows.len(), 12);
    assert_eq!(summary.total, 12);
    assert_eq!(summary.by_status.get("ok"), Some(&12));
    let per_pair: BTreeMap<String, f64> =
        serde_json::from_value(expect["per_pair_cost"].clone()).expect("costs");
    let mut repeats: BTreeMap<(String, String), Vec<i16>> = BTreeMap::new();
    let mut total_cost = 0.0;
    for row in &rows {
        assert_eq!(row.status, "ok");
        assert_eq!(row.batch_id, summary.batch_id);
        assert_eq!(row.matrix_version, VERSION);
        assert!(row.latency_ms.expect("latency") >= 5);
        let short = if row.task_id == "task_one" {
            "one"
        } else {
            "two"
        };
        let side = if row.model_column_id == "col_a" {
            "a"
        } else {
            "b"
        };
        let cost = to_f64(row.cost);
        assert_eq!(
            cost,
            per_pair[&format!("{short}\u{d7}{side}")],
            "{short} {side}"
        );
        total_cost += cost;
        repeats
            .entry((row.task_id.clone(), row.model_column_id.clone()))
            .or_default()
            .push(row.repeat_index);
    }
    assert_eq!(repeats.len(), 4);
    for indexes in repeats.values_mut() {
        indexes.sort_unstable();
        assert_eq!(
            serde_json::json!(indexes),
            expect["repeat_indexes_per_pair"]
        );
    }
    parity_support::assert_f64_within(
        summary.cost_usd,
        total_cost,
        expect["summary_cost_equals_sum_of_rows_within"]
            .as_f64()
            .expect("tolerance"),
    );
    db.close().await;
}

/// parity: runner.TestBatchPersistsErrorStatuses
#[tokio::test]
async fn persists_error_statuses() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    corpus::upsert_corpus_version(&pool, VERSION, "", 2, true)
        .await
        .expect("version");
    seed_task(&pool, "task_rate", "rate limited prompt").await;
    seed_task(&pool, "task_timeout", "timeout prompt").await;
    let column = BatchColumn {
        id: "col_err".to_owned(),
        provider: "anthropic".to_owned(),
        model_id: "claude-errors".to_owned(),
        status: "active".to_owned(),
        settings_json: "{}".to_owned(),
        pricing_json: r#"{"input_per_mtok": 3.0, "output_per_mtok": 15.0}"#.to_owned(),
    };
    seed_column(&pool, &column).await;
    let specs: Vec<FixtureSpec> = serde_json::from_value(Value::from(vec![
        serde_json::json!({"model": "claude-errors", "prompt": "rate limited prompt", "error": "rate_limited"}),
        serde_json::json!({"model": "claude-errors", "prompt": "timeout prompt", "error": "timeout"}),
    ]))
    .expect("fixtures");
    let fake = Arc::new(FakeProvider::from_fixtures(&specs).expect("fake"));
    let runner = Runner::new(
        Clients::new().with("anthropic", fake),
        Arc::new(StoreSink(pool.clone())),
        RunnerConfig::default(),
    );
    let summary = runner
        .run(
            &Batch {
                id: String::new(),
                matrix_version: VERSION.to_owned(),
                repeats: 1,
                tasks: vec![
                    batch_task("task_rate", "rate limited prompt"),
                    batch_task("task_timeout", "timeout prompt"),
                ],
                columns: vec![column],
                done: HashSet::new(),
            },
            &Cancellation::new(),
        )
        .await;
    assert_eq!(summary.error(), None);
    let rows = measurement::list_runs_by_batch(&pool, &summary.batch_id)
        .await
        .expect("rows");
    assert_eq!(rows.len(), 2);
    for row in rows {
        let want = if row.task_id == "task_rate" {
            "provider_error"
        } else {
            "timeout"
        };
        assert_eq!(row.status, want, "{}", row.task_id);
        assert_eq!(row.output, None);
        assert_eq!(row.cost, None);
        assert!(!row.error.unwrap_or_default().is_empty());
    }
    db.close().await;
}
