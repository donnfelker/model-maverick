//! Store tests for runs, scores, aggregates, predictions, and calibration (real Postgres).

use std::str::FromStr;
use std::sync::LazyLock;

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use maverick_store::catalog::{self, NewModelColumn};
use maverick_store::corpus::{self, TaskUpsert};
use maverick_store::measurement::{
    self, AggregateCursor, AggregateHistoryQuery, AggregateUpsert, CalibrationPageQuery,
    CellDimension, NewRun, ParseFailureRate, Prediction, RunTriple,
};
use maverick_store::{Decimal, JsonRawValue, MIGRATOR, is_not_found, sqlstate};
use maverick_testdb::Schema;
use sqlx::PgPool;

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

fn code(result: sqlx::Result<impl Sized>) -> String {
    match result {
        Ok(_) => panic!("expected a database error"),
        Err(err) => sqlstate(&err).unwrap_or_else(|| panic!("not a database error: {err}")),
    }
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|s| (*s).to_owned()).collect()
}

fn date(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).expect("valid date")
}

fn at(y: i32, m: u32, d: u32, h: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap()
}

/// Seeds corpus version `v1`, tasks `task_a`/`task_b`, and columns `col_a`/`col_b`.
async fn seed(pool: &PgPool) {
    corpus::upsert_corpus_version(pool, "v1", "", 2, true)
        .await
        .expect("version");
    let empty = JsonRawValue::from_string("{}".to_owned()).unwrap();
    let list = JsonRawValue::from_string("[]".to_owned()).unwrap();
    let graders = JsonRawValue::from_string(r#"[{"type":"json_schema"}]"#.to_owned()).unwrap();
    let weights = JsonRawValue::from_string(r#"{"schema_validity":1.0}"#.to_owned()).unwrap();
    let tags = strings(&["legal"]);
    for id in ["task_a", "task_b"] {
        corpus::upsert_task(
            pool,
            &TaskUpsert {
                id,
                capability: "structured_extraction",
                domain_tags: &tags,
                difficulty_tier: 3,
                difficulty_knobs: &empty,
                prompt: "Extract the parties from the contract.",
                attachments: &list,
                temperature: 0.0,
                max_tokens: 1024,
                grader_specs: &graders,
                dimension_weights: &weights,
                pass_threshold: 0.7,
                smoke_member: false,
                corpus_version: "v1",
                active: true,
            },
        )
        .await
        .expect("task");
    }
    for (id, model) in [("col_a", "m-a"), ("col_b", "m-b")] {
        catalog::create_model_column(
            pool,
            &NewModelColumn {
                id,
                provider: "anthropic",
                model_id: model,
                endpoint: "direct",
                settings: &empty,
                status: "active",
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
    }
}

fn run<'a>(id: &'a str, batch: &'a str, task: &'a str, column: &'a str, repeat: i16) -> NewRun<'a> {
    NewRun {
        id,
        batch_id: batch,
        task_id: task,
        model_column_id: column,
        repeat_index: repeat,
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

async fn set_ran_at(pool: &PgPool, id: &str, ran_at: DateTime<Utc>) {
    sqlx::query("UPDATE runs SET ran_at = $2 WHERE id = $1")
        .bind(id)
        .bind(ran_at)
        .execute(pool)
        .await
        .expect("set ran_at");
}

#[tokio::test]
async fn runs_insert_list_and_duplicates() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed(&pool).await;

    let inserted = measurement::insert_run(&pool, &run("run_1", "bat_1", "task_b", "col_a", 0))
        .await
        .expect("insert");
    assert_eq!(inserted.cost, Some(Decimal::from_str("0.012345").unwrap()));
    assert_eq!(inserted.tokens_in, Some(812));
    let mut failed = run("run_2", "bat_1", "task_a", "col_b", 1);
    failed.output = None;
    failed.tokens_in = None;
    failed.tokens_out = None;
    failed.latency_ms = None;
    failed.cost = None;
    failed.status = "provider_error";
    failed.error = Some("503 from provider");
    let failed = measurement::insert_run(&pool, &failed)
        .await
        .expect("insert");
    assert_eq!((failed.output, failed.cost), (None, None));
    measurement::insert_run(&pool, &run("run_3", "bat_1", "task_a", "col_b", 0))
        .await
        .expect("insert");
    measurement::insert_run(&pool, &run("run_4", "bat_2", "task_a", "col_a", 0))
        .await
        .expect("other batch");

    assert_eq!(
        code(measurement::insert_run(&pool, &run("run_5", "bat_1", "task_b", "col_a", 0)).await),
        "23505",
        "one run per (batch, task, column, repeat)"
    );
    let mut bad = run("run_6", "bat_1", "task_b", "col_a", 2);
    bad.status = "crashed";
    assert_eq!(code(measurement::insert_run(&pool, &bad).await), "23514");

    // The resume insert tolerates the duplicate and writes nothing.
    assert!(
        measurement::insert_run_if_absent(&pool, &run("run_7", "bat_1", "task_b", "col_a", 0))
            .await
            .expect("duplicate")
            .is_none()
    );
    assert!(is_not_found(
        &measurement::get_run(&pool, "run_7").await.unwrap_err()
    ));
    let fresh =
        measurement::insert_run_if_absent(&pool, &run("run_8", "bat_1", "task_b", "col_a", 1))
            .await
            .expect("new triple");
    assert_eq!(fresh.map(|r| r.id).as_deref(), Some("run_8"));

    let ids: Vec<String> = measurement::list_runs_by_batch(&pool, "bat_1")
        .await
        .expect("list")
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(
        ids,
        ["run_3", "run_2", "run_1", "run_8"],
        "task, column, repeat"
    );
    let triples = measurement::list_run_triples_by_batch(&pool, "bat_1")
        .await
        .expect("triples");
    assert_eq!(
        triples,
        [
            RunTriple {
                task_id: "task_a".into(),
                model_column_id: "col_b".into(),
                repeat_index: 0
            },
            RunTriple {
                task_id: "task_a".into(),
                model_column_id: "col_b".into(),
                repeat_index: 1
            },
            RunTriple {
                task_id: "task_b".into(),
                model_column_id: "col_a".into(),
                repeat_index: 0
            },
            RunTriple {
                task_id: "task_b".into(),
                model_column_id: "col_a".into(),
                repeat_index: 1
            },
        ]
    );
    assert_eq!(
        measurement::get_run(&pool, "run_2")
            .await
            .expect("get")
            .error
            .as_deref(),
        Some("503 from provider")
    );
    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn run_cost_and_parse_failure_windows_are_half_open() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed(&pool).await;

    let zero = measurement::sum_run_cost_between(&pool, at(2026, 9, 1, 0), at(2026, 9, 2, 0))
        .await
        .expect("empty sum");
    assert_eq!(zero, Decimal::ZERO);

    let mut parse = run("run_2", "bat_1", "task_a", "col_a", 0);
    parse.status = "parse_failure";
    let mut no_cost = run("run_3", "bat_1", "task_b", "col_a", 0);
    no_cost.cost = None;
    for (new_run, ran_at) in [
        (
            run("run_1", "bat_1", "task_a", "col_b", 0),
            at(2026, 9, 1, 0),
        ),
        (parse, at(2026, 9, 1, 12)),
        (no_cost, at(2026, 9, 1, 23)),
        (
            run("run_4", "bat_2", "task_b", "col_b", 0),
            at(2026, 9, 2, 0),
        ),
    ] {
        measurement::insert_run(&pool, &new_run)
            .await
            .expect("insert");
        set_ran_at(&pool, new_run.id, ran_at).await;
    }

    // [start, end): run_1 is in, run_4 (at end) is out, run_3 has a NULL cost.
    let total = measurement::sum_run_cost_between(&pool, at(2026, 9, 1, 0), at(2026, 9, 2, 0))
        .await
        .expect("sum");
    assert_eq!(total, Decimal::from_str("0.024690").unwrap());

    let rate = |column: &str, total: i64, parse_failures: i64| ParseFailureRate {
        model_column_id: column.to_owned(),
        total,
        parse_failures,
    };
    assert_eq!(
        measurement::list_batch_parse_failure_rates(&pool, "bat_1")
            .await
            .expect("batch rates"),
        [rate("col_a", 2, 1), rate("col_b", 1, 0)]
    );
    assert_eq!(
        measurement::list_parse_failure_rates_between(&pool, at(2026, 9, 1, 6), at(2026, 9, 3, 0))
            .await
            .expect("window rates"),
        [rate("col_a", 2, 1), rate("col_b", 1, 0)],
        "run_1 is before the window; run_4 is inside"
    );
    assert!(
        measurement::list_batch_parse_failure_rates(&pool, "bat_none")
            .await
            .expect("empty")
            .is_empty()
    );
    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn scores_are_listed_by_run_and_by_batch() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed(&pool).await;
    measurement::insert_run(&pool, &run("run_b", "bat_1", "task_a", "col_a", 0))
        .await
        .expect("run");
    measurement::insert_run(&pool, &run("run_a", "bat_1", "task_b", "col_a", 0))
        .await
        .expect("run");
    measurement::insert_run(&pool, &run("run_c", "bat_2", "task_a", "col_b", 0))
        .await
        .expect("run");

    let detail = JsonRawValue::from_string(r#"{"hits":["r1"],"judge":"ok"}"#.to_owned()).unwrap();
    let score = measurement::insert_score(&pool, "run_b", 1, "rule_compliance", 0.7, Some(&detail))
        .await
        .expect("score");
    assert_eq!(score.value, 0.7_f32);
    assert_eq!(
        score.detail.as_ref().map(|d| d.get()),
        Some(r#"{"hits": ["r1"], "judge": "ok"}"#)
    );
    measurement::insert_score(&pool, "run_b", 0, "schema_validity", 1.0, None)
        .await
        .expect("score");
    measurement::insert_score(&pool, "run_b", 1, "coverage", 0.5, None)
        .await
        .expect("score");
    measurement::insert_score(&pool, "run_a", 0, "schema_validity", 0.0, None)
        .await
        .expect("score");
    measurement::insert_score(&pool, "run_c", 0, "schema_validity", 1.0, None)
        .await
        .expect("other batch");

    assert_eq!(
        code(measurement::insert_score(&pool, "run_a", 1, "coverage", 1.5, None).await),
        "23514",
        "value is in [0, 1]"
    );
    assert_eq!(
        code(measurement::insert_score(&pool, "run_b", 0, "schema_validity", 1.0, None).await),
        "23505"
    );

    let keys = |scores: Vec<measurement::Score>| {
        scores
            .into_iter()
            .map(|s| format!("{}/{}/{}", s.run_id, s.grader_idx, s.dimension))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        keys(
            measurement::list_scores_by_run(&pool, "run_b")
                .await
                .expect("by run")
        ),
        [
            "run_b/0/schema_validity",
            "run_b/1/coverage",
            "run_b/1/rule_compliance"
        ]
    );
    assert_eq!(
        keys(
            measurement::list_scores_by_batch(&pool, "bat_1")
                .await
                .expect("by batch")
        ),
        [
            "run_a/0/schema_validity",
            "run_b/0/schema_validity",
            "run_b/1/coverage",
            "run_b/1/rule_compliance"
        ]
    );
    pool.close().await;
    db.close().await;
}

fn aggregate<'a>(
    column: &'a str,
    capability: &'a str,
    tier: i16,
    dimension: &'a str,
    ewma: f32,
    measured: bool,
    as_of: NaiveDate,
) -> AggregateUpsert<'a> {
    AggregateUpsert {
        model_column_id: column,
        capability,
        difficulty_tier: tier,
        dimension,
        ewma,
        variance: 0.01,
        n: 3,
        measured,
        matrix_version: "2026-07-01",
        as_of,
    }
}

fn aggregate_keys(rows: &[measurement::Aggregate]) -> Vec<String> {
    rows.iter()
        .map(|a| {
            format!(
                "{}/{}/{}/{}@{}={}",
                a.model_column_id, a.capability, a.difficulty_tier, a.dimension, a.as_of, a.ewma
            )
        })
        .collect()
}

/// Seeds a small aggregate history across two columns, two capabilities, two tiers,
/// two dimensions, and three nights.
async fn seed_aggregates(pool: &PgPool) {
    let rows = [
        aggregate(
            "col_a",
            "summarization",
            3,
            "quality",
            0.5,
            false,
            date(2026, 9, 1),
        ),
        aggregate(
            "col_a",
            "summarization",
            3,
            "quality",
            0.6,
            false,
            date(2026, 9, 2),
        ),
        aggregate(
            "col_a",
            "summarization",
            3,
            "quality",
            0.7,
            true,
            date(2026, 9, 3),
        ),
        aggregate(
            "col_a",
            "summarization",
            3,
            "pass_rate",
            0.8,
            true,
            date(2026, 9, 3),
        ),
        aggregate(
            "col_a",
            "summarization",
            2,
            "quality",
            0.9,
            false,
            date(2026, 9, 2),
        ),
        aggregate(
            "col_a",
            "classification",
            3,
            "quality",
            0.4,
            false,
            date(2026, 9, 3),
        ),
        aggregate(
            "col_b",
            "summarization",
            3,
            "quality",
            0.3,
            false,
            date(2026, 9, 1),
        ),
    ];
    for row in &rows {
        measurement::upsert_aggregate(pool, row)
            .await
            .expect("aggregate");
    }
}

#[tokio::test]
async fn aggregates_upsert_series_latest_and_distinct_on() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed(&pool).await;
    seed_aggregates(&pool).await;

    // A conflict on the key and as_of replaces the values.
    let mut replace = aggregate(
        "col_a",
        "summarization",
        3,
        "quality",
        0.75,
        true,
        date(2026, 9, 3),
    );
    replace.n = 4;
    let replaced = measurement::upsert_aggregate(&pool, &replace)
        .await
        .expect("replace");
    assert_eq!((replaced.ewma, replaced.n), (0.75_f32, 4));
    assert_eq!(f64::from(replaced.variance), f64::from(0.01_f32));

    let cell = CellDimension {
        model_column_id: "col_a",
        capability: "summarization",
        difficulty_tier: 3,
        dimension: "quality",
    };
    assert_eq!(
        aggregate_keys(
            &measurement::list_aggregate_series_before(&pool, &cell, date(2026, 9, 3))
                .await
                .expect("series")
        ),
        [
            "col_a/summarization/3/quality@2026-09-01=0.5",
            "col_a/summarization/3/quality@2026-09-02=0.6"
        ]
    );
    let latest = measurement::get_latest_aggregate(&pool, &cell)
        .await
        .expect("latest");
    assert_eq!(latest.as_of, date(2026, 9, 3));
    let missing = CellDimension {
        dimension: "latency_p50_ms",
        ..cell
    };
    assert!(is_not_found(
        &measurement::get_latest_aggregate(&pool, &missing)
            .await
            .unwrap_err()
    ));

    assert_eq!(
        aggregate_keys(
            &measurement::list_latest_aggregates_by_columns(
                &pool,
                &strings(&["col_b", "col_a"]),
                &strings(&["quality"])
            )
            .await
            .expect("by columns")
        ),
        [
            "col_a/classification/3/quality@2026-09-03=0.4",
            "col_a/summarization/2/quality@2026-09-02=0.9",
            "col_a/summarization/3/quality@2026-09-03=0.75",
            "col_b/summarization/3/quality@2026-09-01=0.3"
        ]
    );
    assert!(
        measurement::list_latest_aggregates_by_columns(&pool, &strings(&["col_a"]), &[])
            .await
            .expect("no dimensions")
            .is_empty()
    );
    assert_eq!(
        aggregate_keys(
            &measurement::list_latest_aggregates_by_dimension(&pool, "pass_rate")
                .await
                .expect("by dimension")
        ),
        ["col_a/summarization/3/pass_rate@2026-09-03=0.8"]
    );
    assert_eq!(
        aggregate_keys(
            &measurement::list_latest_cell_aggregates(
                &pool,
                &strings(&["col_a", "col_b"]),
                "summarization",
                &strings(&["quality", "pass_rate"])
            )
            .await
            .expect("cells")
        ),
        [
            "col_a/summarization/2/quality@2026-09-02=0.9",
            "col_a/summarization/3/pass_rate@2026-09-03=0.8",
            "col_a/summarization/3/quality@2026-09-03=0.75",
            "col_b/summarization/3/quality@2026-09-01=0.3"
        ]
    );
    assert_eq!(
        aggregate_keys(
            &measurement::list_aggregates_at_as_of(&pool, "quality", date(2026, 9, 3))
                .await
                .expect("at as_of")
        ),
        [
            "col_a/classification/3/quality@2026-09-03=0.4",
            "col_a/summarization/3/quality@2026-09-03=0.75"
        ]
    );
    assert_eq!(
        measurement::list_measured_column_ids(&pool)
            .await
            .expect("measured"),
        ["col_a"]
    );
    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn aggregate_history_pages_follow_the_keyset_cursor() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed(&pool).await;
    seed_aggregates(&pool).await;
    let base = AggregateHistoryQuery {
        model_column_id: "col_a",
        dimension: "quality",
        capability: "",
        difficulty: None,
        since: None,
        cursor: None,
        page_limit: 2,
    };

    let first = measurement::list_aggregate_history_page(&pool, &base)
        .await
        .expect("page 1");
    assert_eq!(
        aggregate_keys(&first),
        [
            "col_a/summarization/3/quality@2026-09-01=0.5",
            "col_a/summarization/2/quality@2026-09-02=0.9"
        ]
    );
    let last = first.last().expect("row");
    let cursor = AggregateCursor {
        as_of: last.as_of,
        capability: last.capability.clone(),
        difficulty_tier: last.difficulty_tier,
    };
    let second = measurement::list_aggregate_history_page(
        &pool,
        &AggregateHistoryQuery {
            cursor: Some(&cursor),
            ..base
        },
    )
    .await
    .expect("page 2");
    assert_eq!(
        aggregate_keys(&second),
        [
            "col_a/summarization/3/quality@2026-09-02=0.6",
            "col_a/classification/3/quality@2026-09-03=0.4"
        ]
    );

    let filtered = measurement::list_aggregate_history_page(
        &pool,
        &AggregateHistoryQuery {
            capability: "summarization",
            difficulty: Some(3),
            since: Some(date(2026, 9, 2)),
            page_limit: 10,
            ..base
        },
    )
    .await
    .expect("filtered");
    assert_eq!(
        aggregate_keys(&filtered),
        [
            "col_a/summarization/3/quality@2026-09-02=0.6",
            "col_a/summarization/3/quality@2026-09-03=0.7"
        ]
    );
    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn the_first_prediction_lock_wins() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed(&pool).await;
    let lock = |column: &str, capability: &str, tier: i16, p: f32| Prediction {
        batch_id: "2026-09-28".to_owned(),
        model_column_id: column.to_owned(),
        capability: capability.to_owned(),
        difficulty_tier: tier,
        predicted_pass_prob: p,
        source: "ewma".to_owned(),
        matrix_version: "2026-07-01".to_owned(),
        locked_at: at(2026, 9, 28, 2),
    };

    assert_eq!(
        measurement::insert_prediction_lock(&pool, &lock("col_b", "summarization", 3, 0.6))
            .await
            .expect("lock"),
        1
    );
    assert_eq!(
        measurement::insert_prediction_lock(&pool, &lock("col_b", "summarization", 3, 0.99))
            .await
            .expect("hindsight"),
        0
    );
    measurement::insert_prediction_lock(&pool, &lock("col_a", "summarization", 3, 0.7))
        .await
        .expect("lock");
    measurement::insert_prediction_lock(&pool, &lock("col_a", "classification", 4, 0.2))
        .await
        .expect("lock");

    let listed = measurement::list_predictions_by_night(&pool, "2026-09-28")
        .await
        .expect("list");
    assert_eq!(
        listed,
        [
            lock("col_a", "classification", 4, 0.2),
            lock("col_a", "summarization", 3, 0.7),
            lock("col_b", "summarization", 3, 0.6),
        ]
    );
    assert_eq!(
        measurement::count_predictions_by_night(&pool, "2026-09-28")
            .await
            .expect("count"),
        3
    );
    assert_eq!(
        measurement::count_predictions_by_night(&pool, "2026-09-29")
            .await
            .expect("count"),
        0
    );
    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn calibration_upserts_and_pages_newest_batch_first() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;

    let first = measurement::upsert_calibration(&pool, "bat_01", "summarization", 0.2, 0.05, 10)
        .await
        .expect("insert");
    let retried = measurement::upsert_calibration(&pool, "bat_01", "summarization", 0.25, -0.1, 12)
        .await
        .expect("retry");
    assert_eq!(
        (retried.brier_score, retried.overconfidence, retried.n),
        (0.25, -0.1, 12)
    );
    assert!(retried.scored_at >= first.scored_at);
    for (batch, capability) in [
        ("bat_01", "classification"),
        ("bat_02", "summarization"),
        ("bat_02", "classification"),
        ("bat_03", "extraction"),
    ] {
        measurement::upsert_calibration(&pool, batch, capability, 0.1, 0.0, 5)
            .await
            .expect("insert");
    }
    let keys = |rows: Vec<measurement::Calibration>| {
        rows.into_iter()
            .map(|c| format!("{}/{}", c.batch_id, c.capability))
            .collect::<Vec<_>>()
    };
    let base = CalibrationPageQuery {
        capability: "",
        since: None,
        cursor_batch: "",
        cursor_capability: "",
        page_limit: 3,
    };
    assert_eq!(
        keys(
            measurement::list_calibration_page(&pool, &base)
                .await
                .expect("page 1")
        ),
        [
            "bat_03/extraction",
            "bat_02/classification",
            "bat_02/summarization"
        ]
    );
    assert_eq!(
        keys(
            measurement::list_calibration_page(
                &pool,
                &CalibrationPageQuery {
                    cursor_batch: "bat_02",
                    cursor_capability: "classification",
                    ..base
                }
            )
            .await
            .expect("page 2")
        ),
        [
            "bat_02/summarization",
            "bat_01/classification",
            "bat_01/summarization"
        ]
    );
    assert_eq!(
        keys(
            measurement::list_calibration_page(
                &pool,
                &CalibrationPageQuery {
                    capability: "summarization",
                    page_limit: 10,
                    ..base
                }
            )
            .await
            .expect("by capability")
        ),
        ["bat_02/summarization", "bat_01/summarization"]
    );
    sqlx::query("UPDATE calibration SET scored_at = $1 WHERE batch_id = 'bat_01'")
        .bind(at(2026, 1, 1, 0))
        .execute(&pool)
        .await
        .expect("age bat_01");
    assert_eq!(
        keys(
            measurement::list_calibration_page(
                &pool,
                &CalibrationPageQuery {
                    since: Some(at(2026, 6, 1, 0)),
                    page_limit: 10,
                    ..base
                }
            )
            .await
            .expect("since")
        ),
        [
            "bat_03/extraction",
            "bat_02/classification",
            "bat_02/summarization"
        ]
    );
    pool.close().await;
    db.close().await;
}
