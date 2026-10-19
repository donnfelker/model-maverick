//! Background re-resolution of the classifier columns and the judge pool (D41).
//!
//! Go resolved both once at boot and stayed degraded until a restart. Here a column or a
//! pool that did not resolve is retried every [`Rewatch::retry`]. Once it resolves, the
//! engine uses it without a restart and logs INFO. A resolved one is refreshed every
//! [`Rewatch::refresh`], so a catalog change (a column added, retired, or re-pointed)
//! does not leave a stale resolution in place. While unresolved, the current fallback
//! holds: prompt-path requests get `fallback_default` (G1), zero-retention tenants still
//! fail closed, and judge_rubric gradings fail.
//!
//! No classifier calls means the failure-rate alert never fires, so an unresolved
//! classifier raises its own `classifier_degraded` alert (critical, subject `reason:
//! "unresolved"`) once it has been unresolved for [`Rewatch::alert_after`]. Resolution
//! after that alert writes an `info` alert of the same kind (`reason: "resolved"`).

use std::collections::HashMap;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use chrono::Utc;
use maverick_alerts::{AlertKind, AlertNotifier, Severity, write_alert};
use maverick_api::classifier_evidence;
use maverick_api::classify::{Resolver, ResolverSettings};
use maverick_domain::gojson::JsonValue;
use maverick_domain::graders::judge_rubric::{
    FamilyResolver, JudgeClient, PoolColumn, pool_from_config,
};
use maverick_domain::graders::{
    BoxError, BoxFuture, GradeInput, GradeResult, Grader, JudgeRubric, SpecValidator,
};
use maverick_providers::ProviderClient;
use maverick_store::catalog::ModelColumn;
use serde_json::json;
use sqlx::PgPool;
use tokio::time::Instant;

/// How often the re-resolution runs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Rewatch {
    /// While a column or the pool is unresolved.
    pub retry: Duration,
    /// After it resolved.
    pub refresh: Duration,
    /// How long the classifier may stay unresolved before `classifier_degraded` fires.
    pub alert_after: Duration,
}

impl Rewatch {
    pub(crate) const DEFAULT: Rewatch = Rewatch {
        retry: Duration::from_secs(30),
        refresh: Duration::from_secs(300),
        alert_after: Duration::from_secs(300),
    };
}

/// The classifier columns, resolved again until they all resolve, then refreshed.
pub(crate) async fn watch_classifier(
    resolver: Arc<Resolver>,
    pool: PgPool,
    clients: HashMap<String, Arc<dyn ProviderClient>>,
    notifier: Arc<dyn AlertNotifier>,
    timing: Rewatch,
) {
    let settings = resolver.settings().clone();
    let mut complete = resolver.connection().complete();
    let mut unresolved_since = (!complete).then(Instant::now);
    let mut alerted = false;
    // Spec §8.3: the zero-retention classifier column is validated at boot.
    match maverick_store::catalog::list_model_columns(&pool).await {
        Ok(columns) => check_evidence(&pool, notifier.as_ref(), &settings, &columns).await,
        Err(error) => {
            tracing::warn!(error = %error, "classifier evidence check: reading the catalog failed");
        }
    }
    loop {
        tokio::time::sleep(if complete {
            timing.refresh
        } else {
            timing.retry
        })
        .await;
        match maverick_store::catalog::list_model_columns(&pool).await {
            Ok(columns) => {
                let now = resolver.reconnect(&columns, &clients).complete();
                if now && !complete {
                    let seconds = unresolved_since.map_or(0, |since| since.elapsed().as_secs());
                    tracing::info!(
                        column = %settings.column,
                        unresolved_seconds = seconds,
                        "classifier column resolved; prompt-path requests use the classifier"
                    );
                    if alerted {
                        alert(
                            &pool,
                            notifier.as_ref(),
                            Severity::Info,
                            &settings.column,
                            settings.zero_retention_column.as_deref(),
                            "resolved",
                            seconds,
                            format!(
                                "classifier column resolved after {seconds}s; prompt-path \
                                 requests use the classifier again"
                            ),
                        )
                        .await;
                    }
                    unresolved_since = None;
                    alerted = false;
                } else if !now && complete {
                    tracing::error!(
                        column = %settings.column,
                        "classifier column unavailable; prompt-path requests will receive \
                         the fallback classification"
                    );
                    unresolved_since = Some(Instant::now());
                }
                complete = now;
                check_evidence(&pool, notifier.as_ref(), &settings, &columns).await;
            }
            Err(error) => {
                tracing::warn!(error = %error, "classifier re-resolution: reading the catalog failed");
            }
        }
        if let Some(since) = unresolved_since
            && !alerted
            && since.elapsed() >= timing.alert_after
        {
            let seconds = since.elapsed().as_secs();
            alert(
                &pool,
                notifier.as_ref(),
                Severity::Critical,
                &settings.column,
                settings.zero_retention_column.as_deref(),
                "unresolved",
                seconds,
                format!(
                    "classifier column unresolved for {seconds}s; prompt-path requests \
                     receive the fallback classification (D41)"
                ),
            )
            .await;
            alerted = true;
        }
    }
}

/// The zero-retention classifier column's evidence, covered like a pool member's (spec
/// §8.3): its loss and its return each write one `classifier_degraded` alert. This also
/// catches evidence that expires with no catalog write.
async fn check_evidence(
    pool: &PgPool,
    notifier: &dyn AlertNotifier,
    settings: &ResolverSettings,
    columns: &[ModelColumn],
) {
    if let Err(error) =
        classifier_evidence::alert_on_change(pool, Some(notifier), settings, columns, Utc::now())
            .await
    {
        tracing::error!(error = %error, "checking the zero-retention classifier column's evidence");
    }
}

#[allow(clippy::too_many_arguments)]
async fn alert(
    pool: &PgPool,
    notifier: &dyn AlertNotifier,
    severity: Severity,
    column: &str,
    zero_retention_column: Option<&str>,
    reason: &str,
    seconds: u64,
    message: String,
) {
    let subject = json!({
        "classifier_column": column,
        "reason": reason,
        "unresolved_seconds": seconds,
        "zero_retention_column": zero_retention_column,
    });
    match write_alert(
        pool,
        AlertKind::ClassifierDegraded,
        severity,
        &subject,
        &message,
    )
    .await
    {
        Ok(row) => notifier.notify(&row),
        Err(error) => tracing::error!(error = %error, "writing classifier_degraded alert"),
    }
}

/// The judge_rubric grader, swappable after startup: the validation-only instance until
/// the judge pool resolves.
pub(crate) struct JudgeSlot {
    judge: RwLock<Arc<JudgeRubric>>,
    resolved: RwLock<bool>,
}

impl JudgeSlot {
    pub(crate) fn new(judge: Option<JudgeRubric>) -> Self {
        let resolved = judge.is_some();
        JudgeSlot {
            judge: RwLock::new(Arc::new(judge.unwrap_or_else(JudgeRubric::validator))),
            resolved: RwLock::new(resolved),
        }
    }

    fn current(&self) -> Arc<JudgeRubric> {
        Arc::clone(&self.judge.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// True when the pool resolved and the slot grades.
    pub(crate) fn resolved(&self) -> bool {
        *self.resolved.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn set(&self, judge: Option<JudgeRubric>) {
        *self
            .resolved
            .write()
            .unwrap_or_else(PoisonError::into_inner) = judge.is_some();
        *self.judge.write().unwrap_or_else(PoisonError::into_inner) =
            Arc::new(judge.unwrap_or_else(JudgeRubric::validator));
    }
}

impl SpecValidator for JudgeSlot {
    fn kind(&self) -> &str {
        "judge_rubric"
    }

    fn validate_spec(&self, spec: &JsonValue) -> Result<(), String> {
        self.current().validate_spec(spec)
    }
}

impl Grader for JudgeSlot {
    fn grade<'a>(
        &'a self,
        spec: &'a JsonValue,
        input: GradeInput<'a>,
    ) -> BoxFuture<'a, Result<GradeResult, BoxError>> {
        let judge = self.current();
        Box::pin(async move { judge.grade(spec, input).await })
    }
}

/// The judge pool from the config refs, the catalog, and the judge clients.
pub(crate) async fn resolve_judge(
    refs: &[String],
    pool: &PgPool,
    judges: &HashMap<String, Arc<dyn JudgeClient>>,
    families: Arc<dyn FamilyResolver>,
) -> Result<JudgeRubric, String> {
    let columns = maverick_store::catalog::list_model_columns(pool)
        .await
        .map_err(|error| error.to_string())?;
    let catalog: Vec<PoolColumn<'_>> = columns
        .iter()
        .map(|column| PoolColumn {
            model_id: &column.model_id,
            endpoint: &column.endpoint,
            provider: &column.provider,
        })
        .collect();
    let refs: Vec<&str> = refs.iter().map(String::as_str).collect();
    let members = pool_from_config(&refs, &catalog, judges)?;
    JudgeRubric::new(members, Some(families))
}

/// The judge pool, resolved again until it resolves, then refreshed.
pub(crate) async fn watch_judge(
    slot: Arc<JudgeSlot>,
    refs: Vec<String>,
    pool: PgPool,
    judges: HashMap<String, Arc<dyn JudgeClient>>,
    families: Arc<dyn FamilyResolver>,
    timing: Rewatch,
) {
    loop {
        let was = slot.resolved();
        tokio::time::sleep(if was { timing.refresh } else { timing.retry }).await;
        match resolve_judge(&refs, &pool, &judges, Arc::clone(&families)).await {
            Ok(judge) => {
                slot.set(Some(judge));
                if !was {
                    tracing::info!(pool = ?refs, "judge pool resolved; judge_rubric gradings resume");
                }
            }
            Err(error) => {
                slot.set(None);
                if was {
                    tracing::error!(
                        pool = ?refs,
                        error = %error,
                        "judge pool unavailable; judge_rubric gradings will fail until it resolves"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::LazyLock;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use maverick_api::classifier_evidence::{LOST, RESTORED};
    use maverick_api::classify::{Method, ResolveInput};
    use maverick_domain::graders::judge_rubric::JudgeRequest;
    use maverick_providers::{CompletionRequest, CompletionResponse, ProviderFuture};
    use maverick_store::alerts::Alert;
    use maverick_store::catalog::{self, NewModelColumn};
    use maverick_store::{JsonRawValue, tenancy};
    use maverick_testdb::Schema;

    use super::*;

    static SCHEMA: LazyLock<Schema> = LazyLock::new(|| {
        Schema::custom("engine + graphile_worker", |url| async move {
            maverick_store::migrate::migrate(&url).await?;
            Ok(())
        })
    });

    const CLASSIFIER: &str = "claude-haiku-4-5/direct";
    const VALID: &str = r#"{"capability":"structured_extraction","difficulty":3,"domain_tags":[],"confidence":0.91}"#;

    fn fast() -> Rewatch {
        Rewatch {
            retry: Duration::from_millis(50),
            refresh: Duration::from_secs(60),
            alert_after: Duration::from_millis(150),
        }
    }

    struct Answering;

    impl ProviderClient for Answering {
        fn complete<'a>(&'a self, _request: CompletionRequest) -> ProviderFuture<'a> {
            Box::pin(async move {
                Ok(CompletionResponse {
                    text: VALID.to_owned(),
                    tokens_in: 100,
                    tokens_out: 20,
                })
            })
        }

        fn name(&self) -> &str {
            "anthropic"
        }
    }

    struct Judging;

    impl JudgeClient for Judging {
        fn complete<'a>(
            &'a self,
            _request: JudgeRequest,
        ) -> BoxFuture<'a, Result<String, BoxError>> {
            Box::pin(async move { Ok(r#"{"scores": {}}"#.to_owned()) })
        }
    }

    struct NoFamilies;

    impl FamilyResolver for NoFamilies {
        fn family<'a>(&'a self, _column_id: &'a str) -> BoxFuture<'a, Result<String, BoxError>> {
            Box::pin(async move { Ok("anthropic".to_owned()) })
        }
    }

    #[derive(Default)]
    struct Counter(AtomicUsize);

    impl AlertNotifier for Counter {
        fn notify(&self, _alert: &Alert) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    async fn column(pool: &PgPool, id: &str, provider: &str, model: &str) {
        let empty = JsonRawValue::from_string("{}".to_owned()).expect("json");
        catalog::create_model_column(
            pool,
            &NewModelColumn {
                id,
                provider,
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

    /// `(severity, reason)` of every classifier_degraded alert, oldest first.
    async fn degraded(pool: &PgPool) -> Vec<(String, String)> {
        sqlx::query_as::<_, (String, String)>(
            "select severity, subject->>'reason' from alerts
             where kind = 'classifier_degraded' order by created_at, id",
        )
        .fetch_all(pool)
        .await
        .expect("alerts")
    }

    async fn until<F, Fut>(what: &str, mut check: F)
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let started = Instant::now();
        while !check().await {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "timed out: {what}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// D41: a classifier column missing at boot is resolved later without a restart;
    /// the unresolved stretch raises `classifier_degraded`, and the resolution clears it.
    #[tokio::test]
    async fn a_classifier_column_added_after_boot_is_used_without_a_restart() {
        let Some(db) = maverick_testdb::database(&SCHEMA).await else {
            return;
        };
        let pool = db.pool().await;
        tenancy::create_tenant(&pool, "ten_rewatch", "Acme", &[0; 32])
            .await
            .expect("tenant");
        let clients: HashMap<String, Arc<dyn ProviderClient>> = HashMap::from([(
            "anthropic".to_owned(),
            Arc::new(Answering) as Arc<dyn ProviderClient>,
        )]);
        let settings = ResolverSettings {
            column: CLASSIFIER.to_owned(),
            ..ResolverSettings::default()
        };
        // Boot: the catalog has no classifier column.
        let resolver = Arc::new(Resolver::new(settings).connect(&[], &clients));
        let input = |prompt: &'static str| ResolveInput {
            tenant_id: "ten_rewatch",
            prompt_hmac_key: &[0; 32],
            quota_override: None,
            zero_retention: false,
            prompt,
            projection_version: None,
            columns: &[],
            now: Utc::now(),
        };
        let before = resolver
            .resolve(&pool, &input("before"))
            .await
            .expect("resolve");
        assert_eq!(before.method, Method::FallbackDefault);

        let counter = Arc::new(Counter::default());
        let watcher = tokio::spawn(watch_classifier(
            Arc::clone(&resolver),
            pool.clone(),
            clients,
            Arc::clone(&counter) as Arc<dyn AlertNotifier>,
            fast(),
        ));
        until("the unresolved alert", || async {
            degraded(&pool).await == [("critical".to_owned(), "unresolved".to_owned())]
        })
        .await;

        column(&pool, "col_classifier", "anthropic", "claude-haiku-4-5").await;
        until("the resolution", || async {
            resolver.connection().complete()
        })
        .await;
        until("the resolved alert", || async {
            degraded(&pool).await.len() == 2
        })
        .await;
        assert_eq!(
            degraded(&pool).await,
            [
                ("critical".to_owned(), "unresolved".to_owned()),
                ("info".to_owned(), "resolved".to_owned()),
            ]
        );
        assert_eq!(counter.0.load(Ordering::SeqCst), 2);
        let after = resolver
            .resolve(&pool, &input("after"))
            .await
            .expect("resolve");
        assert_eq!(after.method, Method::Classified);
        watcher.abort();
        pool.close().await;
        db.close().await;
    }

    /// Spec §8.3: the zero-retention classifier column is validated at boot, like a pool
    /// member. A column with no zero evidence alerts at once, and evidence that arrives
    /// with no write through the API is noticed on the next refresh (D2).
    #[tokio::test]
    async fn the_zero_retention_columns_evidence_is_checked_at_boot_and_on_refresh() {
        let Some(db) = maverick_testdb::database(&SCHEMA).await else {
            return;
        };
        let pool = db.pool().await;
        column(&pool, "col_classifier", "anthropic", "claude-haiku-4-5").await;
        column(&pool, "col_zr", "anthropic", "zr-classifier").await;
        let clients: HashMap<String, Arc<dyn ProviderClient>> = HashMap::from([(
            "anthropic".to_owned(),
            Arc::new(Answering) as Arc<dyn ProviderClient>,
        )]);
        let settings = ResolverSettings {
            column: CLASSIFIER.to_owned(),
            zero_retention_column: Some("zr-classifier/direct".to_owned()),
            ..ResolverSettings::default()
        };
        let columns = catalog::list_model_columns(&pool).await.expect("columns");
        let resolver = Arc::new(Resolver::new(settings).connect(&columns, &clients));
        assert!(resolver.connection().complete(), "both columns resolve");

        let counter = Arc::new(Counter::default());
        let watcher = tokio::spawn(watch_classifier(
            Arc::clone(&resolver),
            pool.clone(),
            clients,
            Arc::clone(&counter) as Arc<dyn AlertNotifier>,
            Rewatch {
                refresh: Duration::from_millis(50),
                ..fast()
            },
        ));
        until("the boot check", || async {
            degraded(&pool).await == [("critical".to_owned(), LOST.to_owned())]
        })
        .await;

        sqlx::query(
            "update model_columns set retention_mode = 'zero',
                 retention_verified_at = now() - interval '10 days',
                 retention_valid_until = now() + interval '60 days'
             where id = 'col_zr'",
        )
        .execute(&pool)
        .await
        .expect("evidence");
        until("the restored alert", || async {
            degraded(&pool).await.len() == 2
        })
        .await;
        assert_eq!(
            degraded(&pool).await,
            [
                ("critical".to_owned(), LOST.to_owned()),
                ("info".to_owned(), RESTORED.to_owned()),
            ]
        );
        assert_eq!(counter.0.load(Ordering::SeqCst), 2);
        watcher.abort();
        pool.close().await;
        db.close().await;
    }

    /// D41: a judge pool missing at boot resolves later and the slot starts grading.
    #[tokio::test]
    async fn a_judge_pool_added_after_boot_is_used_without_a_restart() {
        let Some(db) = maverick_testdb::database(&SCHEMA).await else {
            return;
        };
        let pool = db.pool().await;
        let judges: HashMap<String, Arc<dyn JudgeClient>> = HashMap::from([
            (
                "google".to_owned(),
                Arc::new(Judging) as Arc<dyn JudgeClient>,
            ),
            (
                "openai".to_owned(),
                Arc::new(Judging) as Arc<dyn JudgeClient>,
            ),
        ]);
        let refs = vec![
            "gemini-3.1-pro/direct".to_owned(),
            "gpt-5.5/direct".to_owned(),
        ];
        let families: Arc<dyn FamilyResolver> = Arc::new(NoFamilies);
        let boot = resolve_judge(&refs, &pool, &judges, Arc::clone(&families)).await;
        assert!(boot.is_err(), "the catalog has no judge columns yet");
        let slot = Arc::new(JudgeSlot::new(boot.ok()));
        assert!(!slot.resolved());
        let watcher = tokio::spawn(watch_judge(
            Arc::clone(&slot),
            refs,
            pool.clone(),
            judges,
            families,
            fast(),
        ));
        column(&pool, "col_gemini", "google", "gemini-3.1-pro").await;
        column(&pool, "col_gpt", "openai", "gpt-5.5").await;
        until("the judge pool", || async { slot.resolved() }).await;
        assert_eq!(slot.kind(), "judge_rubric");
        watcher.abort();
        pool.close().await;
        db.close().await;
    }
}
