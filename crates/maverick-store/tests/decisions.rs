//! Store tests for recommendations, the classification cache, classifier budget
//! counters, and outcomes (real Postgres).

use std::str::FromStr;
use std::sync::LazyLock;

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use maverick_store::decisions::{
    self, NewRecommendation, OutcomePageQuery, RecommendationPageQuery,
};
use maverick_store::tenancy;
use maverick_store::{Decimal, Json, JsonRawValue, MIGRATOR, is_not_found, sqlstate};
use maverick_testdb::Schema;
use sqlx::PgPool;

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

fn at(d: u32, h: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, d, h, 0, 0).unwrap()
}

/// Seeds tenants `ten_1`/`ten_2` with keys `key_1`/`key_2`.
async fn seed_tenants(pool: &PgPool) {
    for (tenant, key) in [("ten_1", "key_1"), ("ten_2", "key_2")] {
        tenancy::create_tenant(pool, tenant, "Acme", &[1; 32])
            .await
            .expect("tenant");
        tenancy::create_api_key(pool, key, tenant, key.as_bytes(), "abcd", "")
            .await
            .expect("key");
    }
}

/// Inserts a recommendation and backdates it to `created_at`.
async fn recommendation(
    pool: &PgPool,
    id: &str,
    tenant: &str,
    prompt_hash: Option<&str>,
    capability: &str,
    created_at: DateTime<Utc>,
) {
    let key = if tenant == "ten_1" { "key_1" } else { "key_2" };
    let classification = json(&format!(
        r#"{{"capability":"{capability}","difficulty":3}}"#
    ));
    let request = json(r#"{"workload":{"capability":"x"}}"#);
    let empty = json("[]");
    let response = json(r#"{"chain":[]}"#);
    decisions::create_recommendation(
        pool,
        &NewRecommendation {
            id,
            tenant_id: tenant,
            api_key_id: key,
            prompt_hash,
            request: &request,
            classification: &classification,
            candidates: &empty,
            response: &response,
            matrix_version: "2026-07-01",
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

fn ids<T>(rows: Vec<T>, id: impl Fn(&T) -> &str) -> Vec<String> {
    rows.iter().map(|row| id(row).to_owned()).collect()
}

#[tokio::test]
async fn recommendations_are_tenant_scoped_and_paged_newest_first() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed_tenants(&pool).await;
    recommendation(
        &pool,
        "rec_01",
        "ten_1",
        Some("h1"),
        "summarization",
        at(1, 0),
    )
    .await;
    recommendation(&pool, "rec_02", "ten_1", None, "classification", at(2, 0)).await;
    recommendation(
        &pool,
        "rec_03",
        "ten_2",
        Some("h1"),
        "summarization",
        at(3, 0),
    )
    .await;
    recommendation(
        &pool,
        "rec_04",
        "ten_1",
        Some("h1"),
        "summarization",
        at(4, 0),
    )
    .await;

    let rec = decisions::get_recommendation(&pool, "rec_01")
        .await
        .expect("get");
    assert_eq!(rec.prompt_hash.as_deref(), Some("h1"));
    assert_eq!(
        rec.classification.get(),
        r#"{"capability": "summarization", "difficulty": 3}"#
    );
    assert!(is_not_found(
        &decisions::get_recommendation_for_tenant(&pool, "rec_03", "ten_1")
            .await
            .unwrap_err()
    ));
    assert_eq!(
        decisions::get_recommendation_for_tenant(&pool, "rec_03", "ten_2")
            .await
            .expect("own")
            .tenant_id,
        "ten_2"
    );

    let mut owned = decisions::recommendation_ids_for_tenant(
        &pool,
        "ten_1",
        &[
            "rec_03".to_owned(),
            "rec_04".to_owned(),
            "rec_99".to_owned(),
            "rec_01".to_owned(),
        ],
    )
    .await
    .expect("ids");
    owned.sort();
    assert_eq!(
        owned,
        ["rec_01", "rec_04"],
        "no existence oracle for other tenants"
    );

    let base = RecommendationPageQuery {
        tenant_id: "ten_1",
        prompt_hash: "",
        capability: "",
        since: None,
        cursor_id: "",
        page_limit: 2,
    };
    let db_ref = &pool;
    let page = |q| async move {
        ids(
            decisions::list_recommendations_page(db_ref, &q)
                .await
                .expect("page"),
            |r| &r.id,
        )
    };
    assert_eq!(page(base).await, ["rec_04", "rec_02"]);
    assert_eq!(
        page(RecommendationPageQuery {
            cursor_id: "rec_02",
            ..base
        })
        .await,
        ["rec_01"]
    );
    assert_eq!(
        page(RecommendationPageQuery {
            prompt_hash: "h1",
            page_limit: 10,
            ..base
        })
        .await,
        ["rec_04", "rec_01"]
    );
    assert_eq!(
        page(RecommendationPageQuery {
            capability: "classification",
            page_limit: 10,
            ..base
        })
        .await,
        ["rec_02"]
    );
    assert_eq!(
        page(RecommendationPageQuery {
            since: Some(at(2, 0)),
            page_limit: 10,
            ..base
        })
        .await,
        ["rec_04", "rec_02"]
    );
    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn the_classification_cache_keeps_the_first_write() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed_tenants(&pool).await;
    let version = "col_haiku/v3/wp1";
    assert!(is_not_found(
        &decisions::get_classification_cache(&pool, "ten_1", "h1", version)
            .await
            .unwrap_err()
    ));
    let first = json(r#"{"capability":"summarization"}"#);
    let second = json(r#"{"capability":"classification"}"#);
    decisions::upsert_classification_cache(&pool, "ten_1", "h1", version, &first)
        .await
        .expect("insert");
    decisions::upsert_classification_cache(&pool, "ten_1", "h1", version, &second)
        .await
        .expect("conflict is not an error");
    let cached = decisions::get_classification_cache(&pool, "ten_1", "h1", version)
        .await
        .expect("get");
    assert_eq!(
        cached.classification.get(),
        r#"{"capability": "summarization"}"#
    );

    // The projection version is part of classifier_version, so it is a separate entry.
    decisions::upsert_classification_cache(&pool, "ten_1", "h1", "col_haiku/v3/wp2", &second)
        .await
        .expect("new version");
    assert_eq!(
        decisions::get_classification_cache(&pool, "ten_1", "h1", "col_haiku/v3/wp2")
            .await
            .expect("get")
            .classification
            .get(),
        r#"{"capability": "classification"}"#
    );
    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn the_classifier_quota_and_spend_are_counted_per_utc_day() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed_tenants(&pool).await;
    let day = NaiveDate::from_ymd_opt(2026, 9, 28).unwrap();
    let next = day.succ_opt().unwrap();

    let mut counts = Vec::new();
    for _ in 0..3 {
        counts.push(
            decisions::count_classifier_call(&pool, day, "ten_1", 2)
                .await
                .expect("count"),
        );
    }
    assert_eq!(counts, [Some(1), Some(2), None]);
    assert_eq!(
        decisions::count_classifier_call(&pool, next, "ten_1", 2)
            .await
            .expect("new day"),
        Some(1)
    );
    assert_eq!(
        decisions::count_classifier_call(&pool, day, "ten_2", 0)
            .await
            .expect("zero quota"),
        None,
        "a zero quota never counts a call"
    );

    let cost = |text: &str| Decimal::from_str(text).unwrap();
    decisions::add_classifier_spend(&pool, day, "ten_1", cost("0.001250"))
        .await
        .expect("spend");
    decisions::add_classifier_spend(&pool, day, "ten_1", cost("0.000750"))
        .await
        .expect("spend");
    decisions::add_classifier_spend(&pool, day, "ten_2", cost("0.010000"))
        .await
        .expect("spend without a counted call");
    decisions::add_classifier_spend(&pool, next, "ten_2", cost("5"))
        .await
        .expect("other day");
    assert_eq!(
        decisions::classifier_spend_on(&pool, day)
            .await
            .expect("sum"),
        cost("0.012")
    );
    assert_eq!(
        decisions::classifier_spend_on(&pool, day.pred_opt().unwrap())
            .await
            .expect("empty day"),
        Decimal::ZERO
    );
    // The spend-only row for ten_2 counts calls from zero.
    assert_eq!(
        decisions::count_classifier_call(&pool, day, "ten_2", 1)
            .await
            .expect("count"),
        Some(1)
    );
    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn outcomes_are_scoped_by_the_owning_recommendation() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed_tenants(&pool).await;
    recommendation(&pool, "rec_1", "ten_1", None, "summarization", at(1, 0)).await;
    recommendation(&pool, "rec_2", "ten_1", None, "summarization", at(2, 0)).await;
    recommendation(&pool, "rec_3", "ten_2", None, "summarization", at(3, 0)).await;

    let details = json(r#"{"settings_applied":true,"attempts":2}"#);
    let outcome = decisions::create_outcome(
        &pool,
        "out_1",
        "rec_1",
        "col_a",
        "completed_unchecked",
        "openrouter",
        Some(&details),
    )
    .await
    .expect("outcome");
    assert_eq!(outcome.escalated_to, None);
    assert_eq!(outcome.endpoint_used, "openrouter");
    assert_eq!(
        outcome.details.as_ref().map(|d| d.get()),
        Some(r#"{"attempts": 2, "settings_applied": true}"#)
    );
    for (id, rec, result) in [
        ("out_2", "rec_2", "success"),
        ("out_3", "rec_1", "provider_error"),
        ("out_4", "rec_3", "success"),
    ] {
        decisions::create_outcome(&pool, id, rec, "col_a", result, "direct", None)
            .await
            .expect("outcome");
    }
    assert_eq!(
        code(
            decisions::create_outcome(&pool, "out_5", "rec_1", "col_a", "done", "direct", None)
                .await
        ),
        "23514"
    );
    assert_eq!(
        code(
            decisions::create_outcome(&pool, "out_6", "rec_x", "col_a", "success", "direct", None)
                .await
        ),
        "23503"
    );
    sqlx::query("UPDATE outcomes SET created_at = $1 WHERE id = 'out_1'")
        .bind(at(1, 0))
        .execute(&pool)
        .await
        .expect("backdate");

    let base = OutcomePageQuery {
        tenant_id: "ten_1",
        recommendation_id: "",
        outcome: "",
        since: None,
        cursor_id: "",
        page_limit: 10,
    };
    let db_ref = &pool;
    let page = |q| async move {
        ids(
            decisions::list_outcomes_page(db_ref, &q)
                .await
                .expect("page"),
            |o| &o.id,
        )
    };
    assert_eq!(page(base).await, ["out_3", "out_2", "out_1"]);
    assert_eq!(
        page(OutcomePageQuery {
            page_limit: 1,
            cursor_id: "out_3",
            ..base
        })
        .await,
        ["out_2"]
    );
    assert_eq!(
        page(OutcomePageQuery {
            recommendation_id: "rec_1",
            ..base
        })
        .await,
        ["out_3", "out_1"]
    );
    assert_eq!(
        page(OutcomePageQuery {
            outcome: "success",
            ..base
        })
        .await,
        ["out_2"]
    );
    assert_eq!(
        page(OutcomePageQuery {
            since: Some(at(2, 0)),
            ..base
        })
        .await,
        ["out_3", "out_2"]
    );
    assert!(
        page(OutcomePageQuery {
            recommendation_id: "rec_3",
            ..base
        })
        .await
        .is_empty(),
        "another tenant's recommendation gives an empty page"
    );
    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn history_prune_deletes_outcomes_then_recommendations() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    seed_tenants(&pool).await;
    recommendation(&pool, "rec_old", "ten_1", None, "summarization", at(1, 0)).await;
    recommendation(&pool, "rec_new", "ten_1", None, "summarization", at(20, 0)).await;
    decisions::create_outcome(
        &pool, "out_old", "rec_old", "col_a", "success", "direct", None,
    )
    .await
    .expect("outcome");
    decisions::create_outcome(
        &pool, "out_new", "rec_new", "col_a", "success", "direct", None,
    )
    .await
    .expect("outcome");
    let cutoff = at(10, 0);

    assert_eq!(
        code(decisions::delete_recommendations_before(&pool, cutoff).await),
        "23503",
        "outcomes reference the recommendation"
    );
    let mut tx = pool.begin().await.expect("begin");
    assert_eq!(
        decisions::delete_outcomes_for_recommendations_before(&mut *tx, cutoff)
            .await
            .expect("outcomes"),
        1
    );
    assert_eq!(
        decisions::delete_recommendations_before(&mut *tx, cutoff)
            .await
            .expect("recommendations"),
        1
    );
    tx.commit().await.expect("commit");
    assert!(is_not_found(
        &decisions::get_recommendation(&pool, "rec_old")
            .await
            .unwrap_err()
    ));
    decisions::get_recommendation(&pool, "rec_new")
        .await
        .expect("kept");
    pool.close().await;
    db.close().await;
}
