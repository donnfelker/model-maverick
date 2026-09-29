//! Store tests for capture windows, debug captures, and the access log (real Postgres).

use std::sync::LazyLock;

use chrono::{DateTime, TimeZone, Utc};
use maverick_store::capture::{
    self, CapturePageQuery, DeletedCapture, NewDebugCapture, TenantCapturePageQuery,
};
use maverick_store::decisions::{self, NewRecommendation};
use maverick_store::tenancy;
use maverick_store::{JsonRawValue, MIGRATOR, is_not_found, sqlstate};
use maverick_testdb::Schema;
use sqlx::PgPool;

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

fn code(result: sqlx::Result<impl Sized>) -> String {
    match result {
        Ok(_) => panic!("expected a database error"),
        Err(err) => sqlstate(&err).unwrap_or_else(|| panic!("not a database error: {err}")),
    }
}

fn at(d: u32, h: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, d, h, 0, 0).unwrap()
}

/// Seeds tenants `ten_1`/`ten_2`, keys `key_1`/`key_2`, and recommendations `rec_1`
/// (ten_1, prompt hash h1) and `rec_2` (ten_2).
async fn seed(pool: &PgPool) {
    let classification =
        JsonRawValue::from_string(r#"{"capability":"summarization"}"#.to_owned()).unwrap();
    let empty = JsonRawValue::from_string("{}".to_owned()).unwrap();
    for (tenant, key, rec, hash) in [
        ("ten_1", "key_1", "rec_1", Some("h1")),
        ("ten_2", "key_2", "rec_2", None),
    ] {
        tenancy::create_tenant(pool, tenant, "Acme", &[1; 32])
            .await
            .expect("tenant");
        tenancy::create_api_key(pool, key, tenant, key.as_bytes(), "abcd", "")
            .await
            .expect("key");
        decisions::create_recommendation(
            pool,
            &NewRecommendation {
                id: rec,
                tenant_id: tenant,
                api_key_id: key,
                prompt_hash: hash,
                request: &empty,
                classification: &classification,
                candidates: &empty,
                response: &empty,
                matrix_version: "2026-07-01",
            },
        )
        .await
        .expect("recommendation");
    }
}

fn new_capture<'a>(
    id: &'a str,
    tenant: &'a str,
    rec: &'a str,
    window: Option<&'a str>,
    expires_at: DateTime<Utc>,
) -> NewDebugCapture<'a> {
    NewDebugCapture {
        id,
        tenant_id: tenant,
        window_id: window,
        recommendation_id: rec,
        ciphertext: b"\x01\x02sealed",
        nonce: &[9; 12],
        capture_key_version: 2,
        expires_at,
    }
}

#[tokio::test]
async fn windows_are_active_on_a_half_open_interval_and_deleted_per_tenant() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed(&pool).await;

    let window = capture::create_capture_window(
        &pool,
        "cw_1",
        "ten_1",
        Some("key_1"),
        Some(0.5),
        at(1, 0),
        at(1, 12),
    )
    .await
    .expect("window");
    assert_eq!(window.confidence_below, Some(0.5));
    let open =
        capture::create_capture_window(&pool, "cw_2", "ten_1", None, None, at(1, 6), at(2, 0))
            .await
            .expect("window");
    assert_eq!((open.api_key_id, open.confidence_below), (None, None));
    capture::create_capture_window(&pool, "cw_3", "ten_2", None, None, at(1, 0), at(3, 0))
        .await
        .expect("other tenant");

    let active = |time| {
        let pool = pool.clone();
        async move {
            let mut ids: Vec<String> = capture::list_active_capture_windows(&pool, "ten_1", time)
                .await
                .expect("active")
                .into_iter()
                .map(|w| w.id)
                .collect();
            ids.sort(); // The query has no ORDER BY, as in Go.
            ids
        }
    };
    assert_eq!(active(at(1, 0)).await, ["cw_1"], "starts_at is inclusive");
    assert_eq!(active(at(1, 6)).await, ["cw_1", "cw_2"]);
    assert_eq!(active(at(1, 12)).await, ["cw_2"], "ends_at is exclusive");
    assert!(active(at(2, 0)).await.is_empty());

    assert_eq!(
        capture::delete_capture_window_for_tenant(&pool, "cw_3", "ten_1")
            .await
            .expect("other tenant"),
        0
    );
    assert_eq!(
        capture::delete_capture_window_for_tenant(&pool, "cw_9", "ten_1")
            .await
            .expect("unknown"),
        0
    );
    assert_eq!(
        capture::delete_capture_window_for_tenant(&pool, "cw_1", "ten_1")
            .await
            .expect("own"),
        1
    );
    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn captures_keep_their_key_version_and_survive_their_window() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed(&pool).await;
    capture::create_capture_window(&pool, "cw_1", "ten_1", None, None, at(1, 0), at(2, 0))
        .await
        .expect("window");

    let created = capture::create_debug_capture(
        &pool,
        &new_capture("cap_2", "ten_1", "rec_1", Some("cw_1"), at(9, 0)),
    )
    .await
    .expect("capture");
    assert_eq!(created.capture_key_version, 2);
    assert_eq!(created.ciphertext, b"\x01\x02sealed");
    assert_eq!(created.window_id.as_deref(), Some("cw_1"));
    capture::create_debug_capture(
        &pool,
        &new_capture("cap_1", "ten_1", "rec_1", None, at(9, 0)),
    )
    .await
    .expect("capture");
    capture::create_debug_capture(
        &pool,
        &new_capture("cap_3", "ten_2", "rec_2", None, at(9, 0)),
    )
    .await
    .expect("capture");

    assert_eq!(
        capture::get_debug_capture_for_tenant(&pool, "cap_2", "ten_1")
            .await
            .expect("own"),
        created
    );
    assert!(is_not_found(
        &capture::get_debug_capture_for_tenant(&pool, "cap_3", "ten_1")
            .await
            .unwrap_err()
    ));
    assert_eq!(
        capture::get_debug_capture_by_id(&pool, "cap_3")
            .await
            .expect("operator read")
            .tenant_id,
        "ten_2"
    );
    let listed: Vec<String> = capture::list_debug_captures_by_tenant(&pool, "ten_1")
        .await
        .expect("list")
        .into_iter()
        .map(|c| c.id)
        .collect();
    assert_eq!(listed, ["cap_1", "cap_2"]);

    // Ending the window early deletes it; its captures live on with no window (0004).
    capture::delete_capture_window_for_tenant(&pool, "cw_1", "ten_1")
        .await
        .expect("delete window");
    let orphan = capture::get_debug_capture_by_id(&pool, "cap_2")
        .await
        .expect("still there");
    assert_eq!(orphan.window_id, None);
    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn capture_pages_never_widen_the_tenant_scope() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed(&pool).await;
    capture::create_capture_window(&pool, "cw_1", "ten_1", None, None, at(1, 0), at(2, 0))
        .await
        .expect("window");
    for (id, tenant, rec, window) in [
        ("cap_1", "ten_1", "rec_1", Some("cw_1")),
        ("cap_2", "ten_2", "rec_2", None),
        ("cap_3", "ten_1", "rec_1", None),
        ("cap_4", "ten_1", "rec_1", Some("cw_1")),
    ] {
        capture::create_debug_capture(&pool, &new_capture(id, tenant, rec, window, at(9, 0)))
            .await
            .expect("capture");
    }

    let base = CapturePageQuery {
        window_id: "",
        tenant_id: "",
        cursor_id: "",
        page_limit: 10,
    };
    let operator = |q| {
        let pool = pool.clone();
        async move {
            capture::list_debug_captures_page(&pool, &q)
                .await
                .expect("page")
                .into_iter()
                .map(|c| c.id)
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(operator(base).await, ["cap_4", "cap_3", "cap_2", "cap_1"]);
    assert_eq!(
        operator(CapturePageQuery {
            tenant_id: "ten_2",
            ..base
        })
        .await,
        ["cap_2"]
    );
    assert_eq!(
        operator(CapturePageQuery {
            window_id: "cw_1",
            ..base
        })
        .await,
        ["cap_4", "cap_1"]
    );
    assert_eq!(
        operator(CapturePageQuery {
            cursor_id: "cap_3",
            page_limit: 1,
            ..base
        })
        .await,
        ["cap_2"]
    );
    let first = capture::list_debug_captures_page(
        &pool,
        &CapturePageQuery {
            page_limit: 1,
            ..base
        },
    )
    .await
    .expect("page");
    assert_eq!(first[0].prompt_hash.as_deref(), Some("h1"));
    assert_eq!(
        first[0].classification.get(),
        r#"{"capability": "summarization"}"#
    );

    let tenant = TenantCapturePageQuery {
        tenant_id: "ten_1",
        window_id: "",
        cursor_id: "",
        page_limit: 10,
    };
    let own: Vec<String> = capture::list_debug_captures_page_for_tenant(&pool, &tenant)
        .await
        .expect("tenant page")
        .into_iter()
        .map(|c| c.id)
        .collect();
    assert_eq!(own, ["cap_4", "cap_3", "cap_1"]);
    assert!(
        capture::list_debug_captures_page_for_tenant(
            &pool,
            &TenantCapturePageQuery {
                tenant_id: "ten_9",
                ..tenant
            }
        )
        .await
        .expect("unknown tenant")
        .is_empty(),
        "an unknown tenant sees nothing; the scope is unconditional"
    );
    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn expiry_and_prune_return_the_deleted_identities() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed(&pool).await;
    capture::create_capture_window(&pool, "cw_old", "ten_1", None, None, at(1, 0), at(2, 0))
        .await
        .expect("window");
    capture::create_capture_window(&pool, "cw_new", "ten_1", None, None, at(1, 0), at(5, 0))
        .await
        .expect("window");
    capture::create_debug_capture(
        &pool,
        &new_capture("cap_1", "ten_1", "rec_1", None, at(3, 0)),
    )
    .await
    .expect("capture");
    capture::create_debug_capture(
        &pool,
        &new_capture("cap_2", "ten_2", "rec_2", None, at(4, 0)),
    )
    .await
    .expect("capture");
    capture::create_debug_capture(
        &pool,
        &new_capture("cap_3", "ten_1", "rec_1", None, at(8, 0)),
    )
    .await
    .expect("capture");

    let mut expired = capture::delete_expired_debug_captures(&pool, at(4, 0))
        .await
        .expect("expire");
    expired.sort();
    assert_eq!(
        expired,
        [
            DeletedCapture {
                id: "cap_1".into(),
                tenant_id: "ten_1".into()
            },
            DeletedCapture {
                id: "cap_2".into(),
                tenant_id: "ten_2".into()
            },
        ],
        "expires_at <= cutoff"
    );
    assert_eq!(
        capture::delete_expired_capture_windows(&pool, at(2, 0))
            .await
            .expect("windows"),
        1,
        "ends_at <= cutoff"
    );

    // history_prune: captures of old recommendations go first.
    sqlx::query("UPDATE recommendations SET created_at = $1 WHERE id = 'rec_1'")
        .bind(at(1, 0))
        .execute(&pool)
        .await
        .expect("backdate");
    let pruned = capture::delete_captures_for_recommendations_before(&pool, at(2, 0))
        .await
        .expect("prune");
    assert_eq!(
        pruned,
        [DeletedCapture {
            id: "cap_3".into(),
            tenant_id: "ten_1".into()
        }]
    );
    assert!(
        capture::list_debug_captures_by_tenant(&pool, "ten_1")
            .await
            .expect("list")
            .is_empty()
    );
    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn the_access_log_outlives_the_capture() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed(&pool).await;
    capture::create_debug_capture(
        &pool,
        &new_capture("cap_1", "ten_1", "rec_1", None, at(3, 0)),
    )
    .await
    .expect("capture");

    assert!(
        !capture::capture_was_deleted(&pool, "cap_1", "ten_1")
            .await
            .expect("check")
    );
    assert!(is_not_found(
        &capture::find_capture_delete_log(&pool, "cap_1")
            .await
            .unwrap_err()
    ));
    capture::insert_capture_access_log(&pool, "cal_2", "cap_1", "ten_1", "key_1", "read")
        .await
        .expect("read");
    capture::insert_capture_access_log(&pool, "cal_1", "cap_1", "ten_1", "key_1", "replay")
        .await
        .expect("replay");
    assert_eq!(
        code(
            capture::insert_capture_access_log(&pool, "cal_x", "cap_1", "ten_1", "key_1", "export")
                .await
        ),
        "23514"
    );

    capture::delete_expired_debug_captures(&pool, at(3, 0))
        .await
        .expect("expire");
    let log = capture::insert_capture_access_log(
        &pool,
        "cal_3",
        "cap_1",
        "ten_1",
        "capture_expiry",
        "delete",
    )
    .await
    .expect("no FK: the log row outlives the capture");
    assert_eq!(log.action, "delete");

    let actions: Vec<String> = capture::list_capture_access_log_by_capture(&pool, "cap_1", "ten_1")
        .await
        .expect("list")
        .into_iter()
        .map(|l| format!("{}:{}", l.id, l.action))
        .collect();
    assert_eq!(actions, ["cal_1:replay", "cal_2:read", "cal_3:delete"]);
    assert!(
        capture::list_capture_access_log_by_capture(&pool, "cap_1", "ten_2")
            .await
            .expect("other tenant")
            .is_empty()
    );
    assert!(
        capture::capture_was_deleted(&pool, "cap_1", "ten_1")
            .await
            .expect("check")
    );
    assert!(
        !capture::capture_was_deleted(&pool, "cap_1", "ten_2")
            .await
            .expect("check")
    );
    let found = capture::find_capture_delete_log(&pool, "cap_1")
        .await
        .expect("delete row");
    assert_eq!(
        (found.tenant_id.as_str(), found.actor.as_str()),
        ("ten_1", "capture_expiry")
    );
    pool.close().await;
    db.close().await;
}
