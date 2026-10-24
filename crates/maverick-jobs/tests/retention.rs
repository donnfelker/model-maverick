//! capture_expiry, affinity_prune (D44), and history_prune on real Postgres (reference
//! Part 3 §6 retention cases, Part 4 §4.7; spec §7.6).

use std::sync::LazyLock;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use maverick_jobs::retention::{
    self, CAPTURE_EXPIRY_ACTOR, HISTORY_PRUNE_ACTOR, RetentionDeps, RetentionError,
};
use maverick_jobs::{DayArgs, HourArgs, JobClient, JobClientConfig, JobKind, JobsError};
use maverick_store::affinity::{self, AffinityRow};
use maverick_store::capture::{self, NewDebugCapture};
use maverick_store::decisions::{self, NewRecommendation};
use maverick_store::{JsonRawValue, MIGRATOR, tenancy};
use maverick_testdb::Schema;
use sqlx::PgPool;

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

const RETENTION: Duration = Duration::from_secs(72 * 60 * 60);

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 28, 10, 0, 0).unwrap()
}

fn hours(n: i64) -> chrono::Duration {
    chrono::Duration::hours(n)
}

async fn seed_tenant(pool: &PgPool) {
    tenancy::create_tenant(pool, "ten_1", "Acme", &[1; 32])
        .await
        .expect("tenant");
    tenancy::create_api_key(pool, "key_1", "ten_1", b"hash", "abcd", "")
        .await
        .expect("key");
}

async fn recommendation(pool: &PgPool, id: &str, created_at: DateTime<Utc>) {
    let empty = JsonRawValue::from_string("{}".to_owned()).unwrap();
    decisions::create_recommendation(
        pool,
        &NewRecommendation {
            id,
            tenant_id: "ten_1",
            api_key_id: "key_1",
            prompt_hash: Some("h"),
            request: &empty,
            classification: &empty,
            candidates: &empty,
            response: &empty,
            matrix_version: "2026-07-01",
            affinity: None,
        },
    )
    .await
    .expect("recommendation");
    sqlx::query("UPDATE recommendations SET created_at = $2 WHERE id = $1")
        .bind(id)
        .bind(created_at)
        .execute(pool)
        .await
        .expect("backdate");
}

async fn debug_capture(
    pool: &PgPool,
    id: &str,
    rec: &str,
    window: Option<&str>,
    expires_at: DateTime<Utc>,
    key_version: i32,
) {
    capture::create_debug_capture(
        pool,
        &NewDebugCapture {
            id,
            tenant_id: "ten_1",
            window_id: window,
            recommendation_id: rec,
            ciphertext: b"sealed",
            nonce: &[0; 12],
            capture_key_version: key_version,
            expires_at,
        },
    )
    .await
    .expect("capture");
}

async fn log_actions(pool: &PgPool, capture_id: &str) -> Vec<String> {
    capture::list_capture_access_log_by_capture(pool, capture_id, "ten_1")
        .await
        .expect("log")
        .into_iter()
        .map(|l| format!("{} by {}", l.action, l.actor))
        .collect()
}

fn deps(pool: &PgPool) -> RetentionDeps<'_> {
    RetentionDeps {
        pool,
        notifier: None,
        capture_retention: RETENTION,
        history_retention_days: 365,
    }
}

fn hour_args() -> HourArgs {
    HourArgs {
        hour: "2026-09-28T10".to_owned(),
    }
}

// parity: measurement-core/jobs.TestCaptureExpiryDeletesExpiredAndLogsDelete
#[tokio::test]
async fn capture_expiry_deletes_expired_captures_and_logs_the_delete() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed_tenant(&pool).await;
    recommendation(&pool, "rec_1", now()).await;
    debug_capture(
        &pool,
        "cap_expired",
        "rec_1",
        None,
        now() - chrono::Duration::minutes(1),
        1,
    )
    .await;
    debug_capture(&pool, "cap_live", "rec_1", None, now() + hours(1), 1).await;
    capture::insert_capture_access_log(&pool, "cal_0", "cap_expired", "ten_1", "key_1", "replay")
        .await
        .expect("replay log");

    let out = retention::run_capture_expiry(&deps(&pool), &hour_args(), now())
        .await
        .expect("capture_expiry");
    assert!(out.deleted_captures >= 1, "{out:?}");
    let remaining: Vec<String> = capture::list_debug_captures_by_tenant(&pool, "ten_1")
        .await
        .unwrap()
        .into_iter()
        .map(|c| c.id)
        .collect();
    assert_eq!(remaining, ["cap_live"]);
    assert_eq!(
        log_actions(&pool, "cap_expired").await,
        [
            "replay by key_1",
            &format!("delete by {CAPTURE_EXPIRY_ACTOR}")
        ]
    );
    assert!(log_actions(&pool, "cap_live").await.is_empty());
    pool.close().await;
    db.close().await;
}

// parity: measurement-core/jobs.TestCaptureExpirySweepsSpentWindows
#[tokio::test]
async fn capture_expiry_sweeps_spent_windows() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed_tenant(&pool).await;
    let retention = hours(72);
    for (id, starts, ends) in [
        (
            "cw_spent",
            now() - retention - hours(10),
            now() - retention - hours(1),
        ),
        ("cw_recent", now() - hours(5), now() - hours(2)),
        ("cw_active", now() - hours(1), now() + hours(1)),
    ] {
        capture::create_capture_window(&pool, id, "ten_1", None, None, starts, ends)
            .await
            .expect("window");
    }
    let out = retention::run_capture_expiry(&deps(&pool), &hour_args(), now())
        .await
        .expect("capture_expiry");
    assert_eq!(out.deleted_windows, 1);
    let remaining: Vec<String> = sqlx::query_scalar("SELECT id FROM capture_windows ORDER BY id")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(remaining, ["cw_active", "cw_recent"]);
    let active: Vec<String> = capture::list_active_capture_windows(&pool, "ten_1", now())
        .await
        .unwrap()
        .into_iter()
        .map(|w| w.id)
        .collect();
    assert_eq!(active, ["cw_active"]);
    pool.close().await;
    db.close().await;
}

// parity: measurement-core/jobs.TestCaptureExpiryIdempotent
#[tokio::test]
async fn capture_expiry_is_idempotent() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed_tenant(&pool).await;
    recommendation(&pool, "rec_1", now()).await;
    debug_capture(&pool, "cap_1", "rec_1", None, now() - hours(1), 1).await;
    let first = retention::run_capture_expiry(&deps(&pool), &hour_args(), now())
        .await
        .expect("first");
    assert_eq!(first.deleted_captures, 1);
    let second = retention::run_capture_expiry(&deps(&pool), &hour_args(), now())
        .await
        .expect("second");
    assert_eq!((second.deleted_captures, second.deleted_windows), (0, 0));
    let logs: i64 = sqlx::query_scalar("SELECT count(*) FROM capture_access_log")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(logs, 1, "the second run adds no log rows");
    pool.close().await;
    db.close().await;
}

// parity: measurement-core/jobs.TestHistoryPruneDeletesAgedRowsKeepsRecent
#[tokio::test]
async fn history_prune_deletes_aged_rows_and_keeps_recent_ones() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed_tenant(&pool).await;
    recommendation(&pool, "rec_old", now() - chrono::Duration::days(370)).await;
    recommendation(&pool, "rec_recent", now() - chrono::Duration::days(360)).await;
    for (id, rec) in [("out_old", "rec_old"), ("out_recent", "rec_recent")] {
        decisions::create_outcome(&pool, id, rec, "col_a", "success", "direct", None)
            .await
            .expect("outcome");
    }
    // A straggler capture that has not expired yet still goes with its recommendation.
    debug_capture(
        &pool,
        "cap_straggler",
        "rec_old",
        None,
        now() + hours(48),
        1,
    )
    .await;
    capture::insert_capture_access_log(&pool, "cal_0", "cap_straggler", "ten_1", "key_1", "replay")
        .await
        .expect("replay log");

    let out = retention::run_history_prune(
        &deps(&pool),
        &DayArgs {
            day: "2026-09-28".to_owned(),
        },
        now(),
    )
    .await
    .expect("history_prune");
    assert_eq!(out.cutoff, "2025-09-28T10:00:00Z");
    assert!(
        out.deleted_recommendations >= 1 && out.deleted_outcomes >= 1 && out.deleted_captures >= 1,
        "{out:?}"
    );
    let recs: Vec<String> = sqlx::query_scalar("SELECT id FROM recommendations ORDER BY id")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(recs, ["rec_recent"]);
    let outcomes: Vec<String> = sqlx::query_scalar("SELECT id FROM outcomes ORDER BY id")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(outcomes, ["out_recent"]);
    assert_eq!(
        log_actions(&pool, "cap_straggler").await,
        [
            "replay by key_1",
            &format!("delete by {HISTORY_PRUNE_ACTOR}")
        ]
    );
    pool.close().await;
    db.close().await;
}

// parity: measurement-core/jobs.TestHistoryPruneIdempotent
#[tokio::test]
async fn history_prune_is_idempotent() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed_tenant(&pool).await;
    recommendation(&pool, "rec_old", now() - chrono::Duration::days(400)).await;
    let args = DayArgs {
        day: "2026-09-28".to_owned(),
    };
    retention::run_history_prune(&deps(&pool), &args, now())
        .await
        .expect("first");
    let second = retention::run_history_prune(&deps(&pool), &args, now())
        .await
        .expect("second");
    assert_eq!(
        (
            second.deleted_recommendations,
            second.deleted_outcomes,
            second.deleted_captures
        ),
        (0, 0, 0)
    );
    pool.close().await;
    db.close().await;
}

// parity: measurement-core/jobs.TestCaptureExpiryWorkRejectsBadHour
#[tokio::test]
async fn capture_expiry_rejects_a_bad_hour() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let err = retention::run_capture_expiry(
        &deps(&pool),
        &HourArgs {
            hour: "not-an-hour".to_owned(),
        },
        now(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            err,
            RetentionError::InvalidArgument(JobsError::InvalidHour { .. })
        ),
        "{err:?}"
    );
    assert!(
        err.to_string()
            .starts_with(r#"jobs: capture_expiry: invalid hour "not-an-hour": "#),
        "{err}"
    );
    pool.close().await;
    db.close().await;
}

// parity: measurement-core/jobs.TestRetentionJobArgsUniquePerWindow
#[test]
fn retention_jobs_are_unique_per_window() {
    assert_eq!(JobKind::CaptureExpiry.as_str(), "capture_expiry");
    assert_eq!(JobKind::HistoryPrune.as_str(), "history_prune");
    let a = JobKind::CaptureExpiry.job_key("2026-09-28T10");
    assert_eq!(
        a,
        JobKind::CaptureExpiry.job_key("2026-09-28T10"),
        "same window, same key"
    );
    assert_ne!(a, JobKind::CaptureExpiry.job_key("2026-09-28T11"));
    assert_ne!(
        JobKind::HistoryPrune.job_key("2026-09-28"),
        JobKind::HistoryPrune.job_key("2026-09-29")
    );
}

fn valid_client_config() -> JobClientConfig {
    let vars = [
        ("MAVERICK_ROLE", "origin".to_owned()),
        ("MAVERICK_DATABASE_URL", "postgres://u:p@h/db".to_owned()),
        ("MAVERICK_INTERNAL_TOKENS", "ops:s".to_owned()),
        ("MAVERICK_CAPTURE_MASTER_KEY", "ab".repeat(32)),
    ];
    let config = maverick_config::Config::load_with(|name| {
        vars.iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.clone())
    })
    .expect("config");
    JobClientConfig::from(&config)
}

// parity: measurement-core/jobs.TestNewClientRejectsBadCaptureExpiryCron
#[test]
fn the_job_client_rejects_a_bad_capture_expiry_cron() {
    let mut config = valid_client_config();
    config.cron_capture_expiry = "99 99 * * *".to_owned();
    let err = JobClient::new(config).expect_err("rejected");
    assert!(
        matches!(
            err,
            JobsError::InvalidCron {
                env_var: "MAVERICK_CRON_CAPTURE_EXPIRY",
                ..
            }
        ),
        "{err:?}"
    );
}

// parity: measurement-core/jobs.TestNewClientRejectsBadHistoryPruneCron
#[test]
fn the_job_client_rejects_a_bad_history_prune_cron() {
    let mut config = valid_client_config();
    config.cron_history_prune = "99 99 * * *".to_owned();
    let err = JobClient::new(config).expect_err("rejected");
    assert!(
        matches!(
            err,
            JobsError::InvalidCron {
                env_var: "MAVERICK_CRON_HISTORY_PRUNE",
                ..
            }
        ),
        "{err:?}"
    );
}

/// A failed run rolls back and writes a `job_failure` warning in its own statement.
#[tokio::test]
async fn a_failed_run_rolls_back_and_writes_a_job_failure_alert() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed_tenant(&pool).await;
    recommendation(&pool, "rec_1", now()).await;
    debug_capture(&pool, "cap_expired", "rec_1", None, now() - hours(1), 1).await;
    // Break the log insert that runs after the capture delete.
    sqlx::query("ALTER TABLE capture_access_log RENAME TO capture_access_log_gone")
        .execute(&pool)
        .await
        .unwrap();

    let err = retention::run_capture_expiry(&deps(&pool), &hour_args(), now())
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("jobs: capture_expiry: logging capture deletes: "),
        "{err}"
    );
    assert_eq!(
        capture::list_debug_captures_by_tenant(&pool, "ten_1")
            .await
            .unwrap()
            .len(),
        1,
        "the delete rolled back"
    );
    let alerts = maverick_store::alerts::list_alerts_by_kind(&pool, "job_failure")
        .await
        .unwrap();
    assert_eq!(alerts.len(), 1);
    assert_eq!(alerts[0].severity, "warning");
    assert_eq!(alerts[0].message, format!("capture_expiry failed: {err}"));
    let subject: serde_json::Value = serde_json::from_str(alerts[0].subject.get()).unwrap();
    assert_eq!(subject["job"], "capture_expiry");
    assert_eq!(subject["hour"], "2026-09-28T10");
    assert_eq!(subject["error"], err.to_string());
    pool.close().await;
    db.close().await;
}

/// D18: a retired master key may be dropped once `max_window_minutes + retention_hours`
/// have passed since rotation. capture_expiry is what makes that safe: by then no
/// capture sealed with the old key is left.
#[tokio::test]
async fn no_capture_sealed_with_a_retired_key_outlives_the_key_drop() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed_tenant(&pool).await;
    recommendation(&pool, "rec_1", now()).await;
    let (max_window_minutes, retention_hours) = (24 * 60_i64, 72_i64);
    let rotated_at = now();
    // The latest capture the old key can seal: taken at rotation, in a window that opened
    // just before and runs for the longest allowed time.
    let window_ends =
        rotated_at - chrono::Duration::minutes(1) + chrono::Duration::minutes(max_window_minutes);
    capture::create_capture_window(
        &pool,
        "cw_1",
        "ten_1",
        None,
        None,
        rotated_at - chrono::Duration::minutes(1),
        window_ends,
    )
    .await
    .unwrap();
    let expires_at = window_ends + hours(retention_hours);
    debug_capture(&pool, "cap_old_key", "rec_1", Some("cw_1"), expires_at, 1).await;
    debug_capture(
        &pool,
        "cap_new_key",
        "rec_1",
        None,
        expires_at + hours(24),
        2,
    )
    .await;

    let drop_at =
        rotated_at + chrono::Duration::minutes(max_window_minutes) + hours(retention_hours);
    assert!(maverick_domain::capture::previous_key_expired(
        rotated_at,
        drop_at,
        max_window_minutes,
        retention_hours
    ));
    assert!(!maverick_domain::capture::previous_key_expired(
        rotated_at,
        drop_at - chrono::Duration::seconds(1),
        max_window_minutes,
        retention_hours
    ));

    let deps = RetentionDeps {
        capture_retention: Duration::from_secs(retention_hours as u64 * 3600),
        ..deps(&pool)
    };
    retention::run_capture_expiry(
        &deps,
        &HourArgs {
            hour: drop_at.format("%Y-%m-%dT%H").to_string(),
        },
        drop_at,
    )
    .await
    .expect("capture_expiry");
    let old_key: i64 =
        sqlx::query_scalar("SELECT count(*) FROM debug_captures WHERE capture_key_version = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        old_key, 0,
        "no capture sealed with the retired key is left at drop time"
    );
    let new_key: i64 =
        sqlx::query_scalar("SELECT count(*) FROM debug_captures WHERE capture_key_version = 2")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(new_key, 1);
    pool.close().await;
    db.close().await;
}

/// DIVERGENCE: D44. `affinity_prune` deletes only the rows whose `expires_at` has passed,
/// and a rerun deletes nothing.
#[tokio::test]
async fn affinity_prune_deletes_only_expired_rows() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed_tenant(&pool).await;
    sqlx::query(
        "INSERT INTO model_columns (id, provider, model_id, endpoint)
         VALUES ('col_1', 'anthropic', 'm', 'direct')",
    )
    .execute(&pool)
    .await
    .expect("column");
    for (hash, expires_at) in [
        ("h_expired", now() - hours(1)),
        ("h_live", now() + hours(1)),
    ] {
        affinity::upsert_affinity(
            &pool,
            &AffinityRow {
                tenant_id: "ten_1".to_owned(),
                affinity_hash: hash.to_owned(),
                column_id: "col_1".to_owned(),
                recommendation_id: "rec_1".to_owned(),
                last_used_at: expires_at - hours(24),
                expires_at,
            },
        )
        .await
        .expect("row");
    }

    let first = retention::run_affinity_prune(&deps(&pool), &hour_args(), now())
        .await
        .expect("prune");
    assert_eq!(first.deleted_rows, 1);
    let second = retention::run_affinity_prune(&deps(&pool), &hour_args(), now())
        .await
        .expect("rerun");
    assert_eq!(second.deleted_rows, 0);
    let left: Vec<String> =
        sqlx::query_scalar("SELECT affinity_hash FROM conversation_affinity ORDER BY 1")
            .fetch_all(&pool)
            .await
            .expect("rows");
    assert_eq!(left, ["h_live"]);

    let bad = HourArgs {
        hour: "2026-09-28".to_owned(),
    };
    assert!(matches!(
        retention::run_affinity_prune(&deps(&pool), &bad, now()).await,
        Err(RetentionError::InvalidArgument(_))
    ));
    db.close().await;
}

/// DIVERGENCE: D44. A bad `MAVERICK_CRON_AFFINITY_PRUNE` is refused like the others.
#[test]
fn the_job_client_rejects_a_bad_affinity_prune_cron() {
    let mut config = valid_client_config();
    config.cron_affinity_prune = "99 99 * * *".to_owned();
    let err = JobClient::new(config).expect_err("rejected");
    assert!(
        matches!(
            err,
            JobsError::InvalidCron {
                env_var: "MAVERICK_CRON_AFFINITY_PRUNE",
                ..
            }
        ),
        "{err:?}"
    );
}
