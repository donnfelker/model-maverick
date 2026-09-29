//! Store tests for alerts (real Postgres).

use std::sync::LazyLock;

use chrono::{DateTime, TimeZone, Utc};
use maverick_store::alerts::{self, Alert};
use maverick_store::{JsonRawValue, MIGRATOR, sqlstate};
use maverick_testdb::Schema;
use sqlx::PgPool;

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

fn at(d: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, d, 0, 0, 0).unwrap()
}

async fn alert(
    pool: &PgPool,
    id: &str,
    kind: &str,
    subject: &str,
    created_at: DateTime<Utc>,
) -> Alert {
    let subject = JsonRawValue::from_string(subject.to_owned()).expect("subject");
    let row = alerts::insert_alert(pool, id, kind, "warning", &subject, "message")
        .await
        .expect("alert");
    sqlx::query("UPDATE alerts SET created_at = $2 WHERE id = $1")
        .bind(id)
        .bind(created_at)
        .execute(pool)
        .await
        .expect("backdate");
    row
}

fn ids(rows: Vec<Alert>) -> Vec<String> {
    rows.into_iter().map(|a| a.id).collect()
}

#[tokio::test]
async fn alerts_are_listed_oldest_first_by_kind_subject_and_feed() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let cell = r#"{"model_column_id":"col_a","capability":"summarization","difficulty_tier":3,"as_of":"2026-09-01"}"#;

    let first = alert(&pool, "alr_2", "regression", cell, at(2)).await;
    assert_eq!(first.acked_at, None);
    assert_eq!(
        first.subject.get(),
        r#"{"as_of": "2026-09-01", "capability": "summarization", "difficulty_tier": 3, "model_column_id": "col_a"}"#
    );
    alert(&pool, "alr_1", "regression", cell, at(2)).await;
    alert(
        &pool,
        "alr_3",
        "regression",
        r#"{"model_column_id":"col_a","capability":"summarization","difficulty_tier":4}"#,
        at(3),
    )
    .await;
    alert(
        &pool,
        "alr_4",
        "regression",
        r#"{"model_column_id":"col_b"}"#,
        at(1),
    )
    .await;
    alert(
        &pool,
        "alr_5",
        "job_failure",
        r#"{"job":"nightly_smoke"}"#,
        at(4),
    )
    .await;
    alert(
        &pool,
        "alr_6",
        "regression",
        r#"{"model_column_id":"col_a","difficulty_tier":"x"}"#,
        at(5),
    )
    .await;

    let bad = JsonRawValue::from_string("{}".to_owned()).unwrap();
    let err = alerts::insert_alert(&pool, "alr_x", "regression", "fatal", &bad, "m")
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("23514"), "{err}");

    // Same created_at: ties break by id.
    assert_eq!(
        ids(alerts::list_alerts_by_kind(&pool, "regression")
            .await
            .expect("by kind")),
        ["alr_4", "alr_1", "alr_2", "alr_3", "alr_6"]
    );
    assert_eq!(
        ids(alerts::list_regression_alerts_for_column(&pool, "col_a")
            .await
            .expect("column")),
        ["alr_1", "alr_2", "alr_3", "alr_6"]
    );
    // difficulty_tier compares as text, so a malformed subject cannot fail the scan.
    assert_eq!(
        ids(
            alerts::list_regression_alerts_for_cell(&pool, "col_a", "summarization", "3")
                .await
                .expect("cell")
        ),
        ["alr_1", "alr_2"]
    );
    assert_eq!(
        ids(alerts::list_alerts_feed(&pool, "", None)
            .await
            .expect("feed")),
        ["alr_4", "alr_1", "alr_2", "alr_3", "alr_5", "alr_6"]
    );
    assert_eq!(
        ids(alerts::list_alerts_feed(&pool, "job_failure", None)
            .await
            .expect("feed")),
        ["alr_5"]
    );
    assert_eq!(
        ids(alerts::list_alerts_feed(&pool, "", Some(at(3)))
            .await
            .expect("feed")),
        ["alr_3", "alr_5", "alr_6"]
    );
    pool.close().await;
    db.close().await;
}
