//! DIVERGENCE: D51. The `model_discovery` job (spec §4.6, §11) on real Postgres, with
//! scripted model lists from the fake provider, so no test calls a real provider.

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use chrono::{TimeZone, Utc};
use maverick_jobs::discovery::{
    self, DiscoveryConfig, DiscoveryOutcome, DiscoveryOutput, NewEntry, ProviderReport,
    ProviderStatus,
};
use maverick_jobs::{BatchLeaseConfig, BatchLeaseManager, DayArgs, JobsError};
use maverick_providers::ProviderErrorKind;
use maverick_providers::fake::FakeModelLister;
use maverick_providers::models::{ListedModel, ModelListFuture, ModelLister};
use maverick_store::alerts::{Alert, list_alerts_by_kind};
use maverick_store::catalog::{NewModelColumn, create_model_column};
use maverick_store::{JsonRawValue, MIGRATOR};
use maverick_testdb::Schema;
use serde_json::{Value, json};
use sqlx::PgPool;

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

async fn column(
    pool: &PgPool,
    id: &str,
    provider: &str,
    model_id: &str,
    endpoint: &str,
    status: &str,
) {
    let empty = JsonRawValue::from_string("{}".to_owned()).expect("json");
    create_model_column(
        pool,
        &NewModelColumn {
            id,
            provider,
            model_id,
            endpoint,
            settings: &empty,
            status,
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

fn listed(provider: &str, model_id: &str, display_name: Option<&str>) -> ListedModel {
    ListedModel {
        provider: provider.to_owned(),
        model_id: model_id.to_owned(),
        display_name: display_name.map(str::to_owned),
        created_at: None,
    }
}

fn config(listers: Vec<Arc<dyn ModelLister>>) -> DiscoveryConfig {
    DiscoveryConfig {
        lease: BatchLeaseManager::new(BatchLeaseConfig::default()),
        notifier: None,
        listers,
    }
}

fn day(day: &str) -> DayArgs {
    DayArgs {
        day: day.to_owned(),
    }
}

fn subjects(alerts: &[Alert]) -> Vec<Value> {
    alerts
        .iter()
        .map(|alert| serde_json::from_str(alert.subject.get()).expect("subject"))
        .collect()
}

#[tokio::test]
async fn discovery_raises_new_and_withdrawn_once_and_leases_the_day() {
    let _ = parity_support::divergence!("D51");
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = &db.pool().await;
    column(
        pool,
        "col_haiku",
        "anthropic",
        "claude-haiku-4-5",
        "direct",
        "active",
    )
    .await;
    column(pool, "col_grok3", "xai", "grok-3", "direct", "active").await;
    column(pool, "col_gpt4", "openai", "gpt-4", "direct", "active").await;
    let created = Utc
        .with_ymd_and_hms(2026, 9, 30, 0, 0, 0)
        .single()
        .expect("time");
    let listers: Vec<Arc<dyn ModelLister>> = vec![
        Arc::new(FakeModelLister::new(
            "anthropic",
            vec![
                listed(
                    "anthropic",
                    "claude-haiku-4-5-20251001",
                    Some("Claude Haiku 4.5"),
                ),
                ListedModel {
                    created_at: Some(created),
                    ..listed(
                        "anthropic",
                        "claude-sonnet-5-5-20260930",
                        Some("Claude Sonnet 5.5"),
                    )
                },
            ],
        )),
        Arc::new(FakeModelLister::new(
            "xai",
            vec![listed("xai", "grok-4-0709", None)],
        )),
        // A failed provider withdraws nothing: col_gpt4 stays quiet.
        Arc::new(FakeModelLister::failing(
            "openai",
            ProviderErrorKind::ProviderError,
        )),
    ];
    let config = config(listers);

    let first = discovery::model_discovery(pool, &day("2031-01-02"), &config, "worker-a")
        .await
        .expect("first run");

    assert_eq!(
        first,
        DiscoveryOutcome::Discovered(DiscoveryOutput {
            new: 2,
            withdrawn: 1,
            new_alerts: 2,
            withdrawn_alerts: 1,
        })
    );
    let new = list_alerts_by_kind(pool, "new_model_available")
        .await
        .expect("alerts");
    assert_eq!(
        subjects(&new),
        vec![
            json!({"provider": "anthropic", "base_model_id": "claude-sonnet-5-5", "model_id": "claude-sonnet-5-5-20260930"}),
            json!({"provider": "xai", "base_model_id": "grok-4", "model_id": "grok-4-0709"}),
        ]
    );
    assert!(new.iter().all(|alert| alert.severity == "info"));
    assert_eq!(
        new[0].message,
        "provider anthropic lists model \"claude-sonnet-5-5-20260930\" that no catalog column covers; enroll it with the models refresh or the column API (spec 4.6)"
    );
    let withdrawn = list_alerts_by_kind(pool, "model_withdrawn")
        .await
        .expect("alerts");
    assert_eq!(
        subjects(&withdrawn),
        vec![json!({"column_id": "col_grok3", "provider": "xai", "model_id": "grok-3"})]
    );
    assert_eq!(withdrawn[0].severity, "warning");
    assert_eq!(
        withdrawn[0].message,
        "provider xai no longer lists model \"grok-3\" of column col_grok3; deprecate the column when it is gone (spec 4.6)"
    );

    // D10: the same day again is a no-op.
    let again = discovery::model_discovery(pool, &day("2031-01-02"), &config, "worker-a")
        .await
        .expect("same day");
    assert_eq!(again, DiscoveryOutcome::AlreadyFinished);

    // The next day finds the same models but writes no second alert.
    let next = discovery::model_discovery(pool, &day("2031-01-03"), &config, "worker-a")
        .await
        .expect("next day");
    assert_eq!(
        next,
        DiscoveryOutcome::Discovered(DiscoveryOutput {
            new: 2,
            withdrawn: 1,
            new_alerts: 0,
            withdrawn_alerts: 0,
        })
    );
    assert_eq!(
        list_alerts_by_kind(pool, "new_model_available")
            .await
            .expect("alerts")
            .len(),
        2
    );
    assert_eq!(
        list_alerts_by_kind(pool, "model_withdrawn")
            .await
            .expect("alerts")
            .len(),
        1
    );
    db.close().await;
}

#[tokio::test]
async fn discover_reports_every_provider_in_order_with_details() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = &db.pool().await;
    let created = Utc
        .with_ymd_and_hms(2025, 7, 9, 0, 0, 0)
        .single()
        .expect("time");
    let listers: Vec<Arc<dyn ModelLister>> = vec![
        Arc::new(FakeModelLister::failing(
            "openai",
            ProviderErrorKind::RateLimited,
        )),
        Arc::new(FakeModelLister::new(
            "xai",
            vec![ListedModel {
                created_at: Some(created),
                ..listed("xai", "grok-4-0709", Some("Grok 4"))
            }],
        )),
        // A lister for a provider discovery does not report on is ignored.
        Arc::new(FakeModelLister::new(
            "mistral",
            vec![listed("mistral", "m-1", None)],
        )),
    ];

    let run = discovery::discover(pool, &listers).await.expect("discover");

    assert_eq!(
        run.providers,
        vec![
            ProviderReport {
                provider: "anthropic".to_owned(),
                status: ProviderStatus::SkippedNoKey,
                listed: None,
                error: None,
            },
            ProviderReport {
                provider: "google".to_owned(),
                status: ProviderStatus::SkippedNoKey,
                listed: None,
                error: None,
            },
            ProviderReport {
                provider: "openai".to_owned(),
                status: ProviderStatus::Failed,
                listed: None,
                error: Some("rate_limited (status 429)".to_owned()),
            },
            ProviderReport {
                provider: "xai".to_owned(),
                status: ProviderStatus::Ok,
                listed: Some(1),
                error: None,
            },
        ]
    );
    // An empty catalog makes every kept model new; the provider's details come along.
    assert_eq!(
        run.new,
        vec![NewEntry {
            provider: "xai".to_owned(),
            model_id: "grok-4-0709".to_owned(),
            base_model_id: "grok-4".to_owned(),
            display_name: Some("Grok 4".to_owned()),
            created_at: Some(created),
        }]
    );
    assert!(run.withdrawn.is_empty());
    assert_eq!(
        serde_json::to_value(&run.providers[2]).expect("encodes"),
        json!({"provider": "openai", "status": "failed", "error": "rate_limited (status 429)"})
    );
    assert_eq!(
        serde_json::to_value(&run.providers[3]).expect("encodes"),
        json!({"provider": "xai", "status": "ok", "listed": 1})
    );
    db.close().await;
}

/// A lister that never answers in time.
struct Slow;

impl ModelLister for Slow {
    fn provider(&self) -> &str {
        "anthropic"
    }

    fn list_models<'a>(&'a self) -> ModelListFuture<'a> {
        Box::pin(async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            Ok(Vec::new())
        })
    }
}

#[tokio::test]
async fn a_slow_provider_is_failed_and_the_others_still_run() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = &db.pool().await;
    column(
        pool,
        "col_opus",
        "anthropic",
        "claude-opus-4-1",
        "direct",
        "active",
    )
    .await;
    let listers: Vec<Arc<dyn ModelLister>> = vec![
        Arc::new(Slow),
        Arc::new(FakeModelLister::new(
            "google",
            vec![listed("google", "gemini-2.5-pro", None)],
        )),
    ];

    let run = discovery::discover_with_timeout(pool, &listers, Duration::from_millis(50))
        .await
        .expect("discover");

    assert_eq!(run.providers[0].status, ProviderStatus::Failed);
    assert_eq!(run.providers[0].error.as_deref(), Some("timeout"));
    assert_eq!(run.providers[1].status, ProviderStatus::Ok);
    // The timed-out provider withdraws nothing.
    assert!(run.withdrawn.is_empty());
    assert_eq!(run.new.len(), 1);
    db.close().await;
}

#[tokio::test]
async fn a_bad_day_is_rejected_before_any_call() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = &db.pool().await;
    let error = discovery::model_discovery(pool, &day("2031-13-01"), &config(Vec::new()), "w")
        .await
        .expect_err("bad day");
    assert!(
        matches!(
            error,
            discovery::DiscoveryError::Args(JobsError::InvalidDay { .. })
        ),
        "{error}"
    );
    db.close().await;
}
