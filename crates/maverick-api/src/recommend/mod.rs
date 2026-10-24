//! `POST /v1/recommend` on the declared path, and `GET /v1/checks/{ref...}` (spec §9.2;
//! reference Part 5 §6.5, §6.6; Part 4 §2.6).
//!
//! The declared path: validate, load the tenant's pools, weights, constraints, and the
//! catalog, read the workload cell's aggregates, apply policy (pool, allowlist, standing
//! constraints including D1, preferences), rank, persist one `recommendations` trace
//! row, and answer. No provider call. Every successful call writes exactly one trace
//! row; a failed call writes none. [`recommend`] is the one service that both the HTTP
//! handler and MCP call.
//!
//! The prompt path classifies first ([`crate::classify::Resolver`]: cache, classifier
//! call, D2 zero-retention refusal, D3 deadline, D4 budget), then continues as the declared
//! path with the classified workload. The response adds `prompt_hash` (and, D4,
//! `classification_quota_exceeded: true` when the budget stopped the classifier). The
//! trace row stores the prompt hash and the request without the prompt. After the trace
//! row, the capture hook may seal the prompt into a debug capture.
//!
//! The Rust response adds D8, D9, D15, D29, and D39 to the Go shape: each entry carries
//! `execution_alias_retention`, `settings`, `param_constraints`, and `retention_mode`
//! after `execution_aliases` and `low_availability` after `clears_difficulty`, and
//! `zero_retention` and then `telemetry` follow `measured` (spec §9.2, §9.10).

mod prompt;
mod request;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Extension, Path, Request, State};
use axum::http::{Method, Response, StatusCode};
use chrono::{DateTime, Duration, Utc};
use maverick_alerts::{AlertKind, AlertNotifier, Severity};
use maverick_config::Config;
use maverick_domain::affinity::{
    self as conversation, AffinityContext, AffinityDecision, AffinityStatus, BonusWithheld,
    TurnCostSkipped, TurnPrices,
};
use maverick_domain::capture::affinity_hash;
use maverick_domain::gofmt::quote;
use maverick_domain::gojson::{self, JsonValue};
use maverick_domain::policy::{
    self, CatalogColumn, CellAggregate, CellAggregates, ColumnStatus, PolicyError, PolicyErrorCode,
    PolicyRequest, RequestConstraints, RetentionMode, StandingConstraints, TenantConfig,
};
use maverick_domain::pricing::parse_pricing;
use maverick_domain::ranking::{self, RankParams, ScoredSurvivor, Weights};
use maverick_domain::scoring::observe::dimension;
use maverick_domain::trace::{self, ColumnInfo, SurvivorMetrics, TraceCandidate};
use maverick_domain::{ceilings, checks, entries, ids};
use maverick_store::affinity::{self as affinity_store, AffinityRow};
use maverick_store::catalog::{self, ModelColumn};
use maverick_store::decisions::{self, NewRecommendation};
use maverick_store::measurement::{self, Aggregate};
use maverick_store::{JsonRawValue, numeric_to_f64, tenancy};
use sqlx::PgPool;

use crate::capture::{CaptureRequest, Capturer};
use crate::classify::{ResolveInput, Resolver, ResolverSettings};
use crate::observe::RequestLog;
use crate::{ApiError, ApiState, ErrorCode, Identity, internal_error, not_found, read_body};

use request::Form;

/// The difficulty of a declared workload that names none.
const DEFAULT_DIFFICULTY: i64 = 3;
/// The quality bar of a tenant without a `tenant_weights` row for the capability: the Go
/// float32 `0.80` widened to `0.800000011920929` (reference Part 4 §2.4).
const DEFAULT_QUALITY_BAR: f32 = 0.80;
/// D8: availability is averaged over this many trailing nights, today included.
const AVAILABILITY_NIGHTS: i64 = 7;

/// The conversation affinity knobs (spec §7.6, §14.1, D44).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AffinitySettings {
    /// `MAVERICK_AFFINITY_ENABLED`. False validates `affinity` but reads and writes no
    /// row and answers `new` with the normal ranking.
    pub enabled: bool,
    /// `MAVERICK_AFFINITY_TTL`: a row's lifetime after its last use.
    pub ttl: Duration,
    /// `MAVERICK_AFFINITY_CACHE_WARM_SECONDS`.
    pub cache_warm: Duration,
    /// `MAVERICK_AFFINITY_INCUMBENT_BONUS`, in quality points.
    pub incumbent_bonus: f64,
}

impl Default for AffinitySettings {
    fn default() -> Self {
        Self {
            enabled: true,
            ttl: Duration::hours(24),
            cache_warm: Duration::seconds(300),
            incumbent_bonus: conversation::INCUMBENT_BONUS,
        }
    }
}

/// The recommend knobs: ranking bands, the ceiling buffer, where alerts go, the prompt
/// path's classifier resolver, the debug-capture hook, and conversation affinity.
#[derive(Clone)]
pub struct RecommendConfig {
    pub role: maverick_config::Role,
    pub rank: RankParams,
    pub ceiling_buffer: f64,
    /// Notified after a `pool_would_be_empty` alert row is written. `None` writes the row
    /// only.
    pub alerts: Option<Arc<dyn AlertNotifier>>,
    /// The prompt path's classifier. Without classifier clients (see
    /// [`Resolver::connect`]) every cache miss answers the fallback classification.
    pub classifier: Arc<Resolver>,
    /// The debug-capture hook. `None` never captures.
    pub capture: Option<Arc<Capturer>>,
    /// D44.
    pub affinity: AffinitySettings,
}

impl Default for RecommendConfig {
    fn default() -> Self {
        Self {
            role: maverick_config::Role::Origin,
            rank: RankParams::default(),
            ceiling_buffer: ceilings::DEFAULT_CEILING_BUFFER,
            alerts: None,
            classifier: Arc::new(Resolver::default()),
            capture: None,
            affinity: AffinitySettings::default(),
        }
    }
}

impl fmt::Debug for RecommendConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecommendConfig")
            .field("role", &self.role)
            .field("rank", &self.rank)
            .field("ceiling_buffer", &self.ceiling_buffer)
            .field("alerts", &self.alerts.is_some())
            .field("classifier", &self.classifier)
            .field("capture", &self.capture)
            .field("affinity", &self.affinity)
            .finish()
    }
}

impl RecommendConfig {
    /// The knobs from the configuration (spec §14.1). Alerts start without a notifier and
    /// the classifier without clients; serve connects them.
    pub fn from_config(config: &Config) -> Self {
        Self {
            role: config.role,
            rank: RankParams {
                band_quality: config.ranking_band_quality,
                band_balanced: config.ranking_band_balanced,
                band_cost: config.ranking_band_cost,
                noise_band: config.ranking_noise_band,
                availability_floor: config.ranking_availability_floor,
            },
            ceiling_buffer: config.ceiling_buffer,
            alerts: None,
            classifier: Arc::new(Resolver::new(ResolverSettings::from_config(config))),
            capture: Some(Arc::new(Capturer::from_config(config))),
            affinity: AffinitySettings {
                enabled: config.affinity_enabled,
                ttl: Duration::from_std(config.affinity_ttl).unwrap_or(Duration::hours(24)),
                cache_warm: Duration::seconds(i64::from(config.affinity_cache_warm_seconds)),
                incumbent_bonus: config.affinity_incumbent_bonus,
            },
        }
    }
}

/// A recommendation that was made and persisted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recommended {
    /// The `rec_<ULID>` id of the trace row.
    pub id: String,
    /// The response body: compact JSON and one `\n`.
    pub body: String,
}

/// Why a recommendation failed.
#[derive(Debug)]
pub enum RecommendError {
    /// A request the API refuses, with its envelope.
    Api(ApiError),
    /// Anything else: logged, answered `500 internal_error`.
    Internal(String),
}

impl From<ApiError> for RecommendError {
    fn from(error: ApiError) -> Self {
        Self::Api(error)
    }
}

impl From<sqlx::Error> for RecommendError {
    fn from(error: sqlx::Error) -> Self {
        Self::Internal(error.to_string())
    }
}

/// `POST /v1/recommend`.
pub(crate) async fn recommend_handler(
    State(api): State<ApiState>,
    Extension(identity): Extension<Identity>,
    log: Option<Extension<RequestLog>>,
    request: Request,
) -> Response<Body> {
    if request.method() != Method::POST {
        return not_found();
    }
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let body = match read_body(request.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let Some(pool) = api.pool() else {
        return internal_error();
    };
    match recommend(pool, api.recommend(), &identity, &body).await {
        Ok(recommended) => {
            if let Some(Extension(log)) = log {
                log.set_recommendation_id(&recommended.id);
            }
            crate::response_with_json(StatusCode::OK, recommended.body.into_bytes())
        }
        Err(RecommendError::Api(error)) => error.into_response(),
        Err(RecommendError::Internal(error)) => {
            tracing::error!(
                method = %method,
                path = %path,
                error = %error,
                "recommend handler failure"
            );
            internal_error()
        }
    }
}

/// `GET /v1/checks/{ref...}`: the check spec document, or 404 for an unknown ref.
pub(crate) async fn checks_handler(Path(path): Path<String>, request: Request) -> Response<Body> {
    if request.method() != Method::GET {
        return not_found();
    }
    let reference = format!("checks/{path}");
    match checks::lookup_check(&reference) {
        Some(definition) => {
            let mut body = definition.document();
            body.push('\n');
            crate::response_with_json(StatusCode::OK, body.into_bytes())
        }
        None => ApiError::new(
            StatusCode::NOT_FOUND,
            ErrorCode::NotFound,
            format!("check spec {} is not registered", quote(&reference)),
        )
        .detail("ref", reference)
        .into_response(),
    }
}

/// Makes and persists one recommendation for `identity` from a request body, and records
/// the recommend metrics (spec §14.3).
pub async fn recommend(
    pool: &PgPool,
    config: &RecommendConfig,
    identity: &Identity,
    body: &[u8],
) -> Result<Recommended, RecommendError> {
    let started = std::time::Instant::now();
    let result = recommend_uninstrumented(pool, config, identity, body).await;
    let succeeded = result.as_ref().ok().map(|recommended| {
        #[derive(serde::Deserialize)]
        struct Labels {
            #[serde(default)]
            measured: bool,
            #[serde(default)]
            classification: Method,
        }
        #[derive(Default, serde::Deserialize)]
        struct Method {
            #[serde(default)]
            method: String,
        }
        let labels: Option<Labels> = serde_json::from_str(&recommended.body).ok();
        labels.map_or((false, "other"), |l| {
            (
                l.measured,
                crate::metrics::method_label(&l.classification.method),
            )
        })
    });
    crate::metrics::recommend_finished(started.elapsed(), succeeded);
    result
}

async fn recommend_uninstrumented(
    pool: &PgPool,
    config: &RecommendConfig,
    identity: &Identity,
    body: &[u8],
) -> Result<Recommended, RecommendError> {
    let request = request::decode(body)?;
    let form = request::validate(&request)?;
    // D44: `affinity` is validated even when the setting turns it off.
    let affinity = request::affinity(&request)?;
    let affinity_on = affinity.is_some() && config.affinity.enabled;
    // D47: no mode (or an empty one) ranks in quality mode, as `maverick/auto` does (D45).
    // Go defaulted to balanced.
    let mode = if request.mode.is_empty() {
        "quality"
    } else {
        request.mode.as_str()
    };
    let now = Utc::now();
    let tenant = identity.tenant_id.as_str();

    // Step 3: the tenant's configuration and the whole catalog. The prompt path needs the
    // zero-retention constraint (D2) and the catalog before it classifies. The pools,
    // weights, constraints, and active corpus version come in one round trip, concurrent
    // with the catalog read (e3x).
    // The tenant row (its prompt HMAC key) comes in the same round trip when the prompt
    // path or the affinity hash needs it (D44).
    let needs_tenant_row = matches!(form, Form::Prompt { .. }) || affinity_on;
    let (context, columns, tenant_row) = tokio::try_join!(
        tenancy::get_tenant_context(pool, tenant),
        catalog::list_model_columns(pool),
        async {
            if needs_tenant_row {
                tenancy::get_tenant(pool, tenant).await.map(Some)
            } else {
                Ok(None)
            }
        },
    )?;
    let pools = context.pools;
    let weights = context.weights;
    let active_corpus_version = if config.role == maverick_config::Role::Replica {
        Some(crate::active_matrix_version(pool, config.role).await?)
    } else {
        context.active_corpus_version
    };
    let (standing, telemetry_opt_out, tenant_cost_aware) = match context.constraints {
        Some(row) => (
            StandingConstraints {
                max_cost_per_run: row.max_cost_per_run.map(numeric_to_f64),
                max_latency_ms: row.max_latency_ms,
                zero_retention: row.zero_retention,
            },
            row.telemetry_opt_out,
            row.affinity_cost_aware,
        ),
        None => (StandingConstraints::default(), false, false),
    };
    // D44: the conversation's row is keyed by the HMAC of its key, never the key.
    let conversation_hash = match (&affinity, &tenant_row) {
        (Some(affinity), Some(row)) if affinity_on => {
            Some(affinity_hash(&row.prompt_hmac_key, &affinity.key))
        }
        _ => None,
    };
    let zero_retention = standing.zero_retention;

    // Step 2: the workload, declared or classified.
    let (workload, prompt_path) = match form {
        Form::Declared {
            capability,
            difficulty,
            domain_tags,
        } => {
            let declared = Declared {
                defaulted: difficulty.is_none(),
                difficulty: difficulty.unwrap_or(DEFAULT_DIFFICULTY),
                capability,
                domain_tags,
            };
            (declared.resolved(), None)
        }
        Form::Prompt {
            prompt,
            projection_version,
        } => {
            let Some(row) = &tenant_row else {
                return Err(RecommendError::Internal(
                    "the prompt path has no tenant row".to_owned(),
                ));
            };
            let resolution = config
                .classifier
                .resolve(
                    pool,
                    &ResolveInput {
                        tenant_id: tenant,
                        prompt_hmac_key: &row.prompt_hmac_key,
                        quota_override: row.classification_quota_override,
                        zero_retention,
                        prompt: &prompt,
                        projection_version: projection_version.as_deref(),
                        columns: &columns,
                        now,
                    },
                )
                .await
                .map_err(|refused| {
                    ApiError::new(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        ErrorCode::ZeroRetentionRequiresDeclared,
                        refused.to_string(),
                    )
                    .detail("field", "workload.prompt")
                })?;
            let workload = Resolved {
                capability: resolution.classification.capability.clone(),
                difficulty: resolution.classification.difficulty,
                classification: prompt::classification_block(&resolution),
            };
            (workload, Some((prompt, resolution)))
        }
    };
    let difficulty = i32::try_from(workload.difficulty)
        .ok()
        .filter(|d| (1..=5).contains(d))
        .ok_or_else(|| {
            RecommendError::Internal(format!(
                "difficulty {} is out of range",
                workload.difficulty
            ))
        })?;
    let tier = i16::try_from(difficulty).expect("1..=5");
    let cfg = TenantConfig {
        tenant_id: tenant.to_owned(),
        pools: pools
            .into_iter()
            .map(|p| policy::Pool {
                name: p.name,
                model_column_ids: p.model_column_ids,
                is_default: p.is_default,
            })
            .collect(),
        constraints: standing,
    };
    let by_id: HashMap<&str, &ModelColumn> = columns.iter().map(|c| (c.id.as_str(), c)).collect();
    let resolved = policy::resolve_pool(&cfg, &request.pool).map_err(policy_error)?;

    // Step 4: the cell aggregates of the pool's columns and their quality sources.
    let pool_columns: Vec<&ModelColumn> = resolved
        .model_column_ids
        .iter()
        .filter_map(|id| by_id.get(id.as_str()).copied())
        .collect();
    let mut wanted: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for column in &pool_columns {
        for id in [column.id.as_str(), quality_source(column)] {
            if seen.insert(id) {
                wanted.push(id.to_owned());
            }
        }
    }
    let dimensions = [
        dimension::QUALITY,
        dimension::PASS_RATE,
        dimension::COST_PER_RUN_USD,
        dimension::LATENCY_P95_MS,
    ]
    .map(str::to_owned);
    // D8's availability is per column, so it is read for the whole pool here, concurrently
    // with the cells, and looked up for the survivors below (e3x).
    let pool_ids: Vec<String> = pool_columns.iter().map(|c| c.id.clone()).collect();
    let since = (now - Duration::days(AVAILABILITY_NIGHTS - 1)).date_naive();
    // D44: the conversation's row is read at the same time.
    let (rows, availability_rows, stored_affinity) = tokio::try_join!(
        measurement::list_latest_cell_aggregates(pool, &wanted, &workload.capability, &dimensions),
        measurement::list_trailing_availability(pool, &pool_ids, &workload.capability, tier, since),
        async {
            match &conversation_hash {
                Some(hash) => affinity_store::get_affinity(pool, tenant, hash, now).await,
                None => Ok(None),
            }
        },
    )?;
    let cells = Cells::new(&rows);
    let aggregates: CellAggregates = pool_columns
        .iter()
        .map(|column| {
            (
                column.id.clone(),
                CellAggregate {
                    cost_per_run_usd: cells.ewma(&column.id, tier, dimension::COST_PER_RUN_USD),
                    latency_p95_ms: cells.ewma(&column.id, tier, dimension::LATENCY_P95_MS),
                },
            )
        })
        .collect();

    // Step 5: policy.
    let catalog: Vec<CatalogColumn> = columns.iter().map(catalog_column).collect();
    let policy_request = PolicyRequest {
        pool: request.pool.clone(),
        allowed_models: request.allowed_models.clone(),
        capability: workload.capability.clone(),
        difficulty_tier: difficulty,
        constraints: RequestConstraints {
            max_cost_per_run: request.constraints.max_cost_per_run,
            max_latency_ms: request.constraints.max_latency_ms,
        },
    };
    let decision = match policy::evaluate(&cfg, &catalog, &aggregates, &policy_request, now) {
        Ok(decision) => decision,
        Err(error) => {
            // D1: a pool emptied by zero retention raises its alert, then answers 409.
            if let Some(alert) = &error.alert {
                write_alert(pool, config, alert).await?;
            }
            return Err(policy_error(error).into());
        }
    };
    // Step 6: the G1 drift fallback's alert.
    if let Some(alert) = &decision.drift_alert {
        write_alert(pool, config, alert).await?;
    }
    // Step 7.
    if decision.survivors.is_empty() {
        return Err(RecommendError::Internal(format!(
            "pool {} has no usable columns for {} tier {}",
            quote(&decision.pool),
            workload.capability,
            difficulty
        )));
    }

    // Step 8: ceilings, score inputs, and the ranking.
    let bar = f64::from(
        weights
            .iter()
            .find(|w| w.capability == workload.capability)
            .map_or(DEFAULT_QUALITY_BAR, |w| w.quality_bar),
    );
    let tenant_weights = weights
        .iter()
        .find(|w| w.capability == workload.capability)
        .map(|w| Weights {
            quality: f64::from(w.w_quality),
            cost: f64::from(w.w_cost),
            latency: f64::from(w.w_latency),
        });
    let availability: HashMap<String, f64> = availability_rows
        .into_iter()
        .map(|row| (row.model_column_id, row.availability))
        .collect();

    let mut scored = Vec::with_capacity(decision.survivors.len());
    let mut metrics = HashMap::with_capacity(decision.survivors.len());
    for survivor in &decision.survivors {
        let column = by_id
            .get(survivor.column_id.as_str())
            .copied()
            .ok_or_else(|| {
                RecommendError::Internal(format!(
                    "survivor {} is not in the catalog",
                    survivor.column_id
                ))
            })?;
        let source = quality_source(column);
        let quality = cells.get(source, tier, dimension::QUALITY);
        let pass_rate = cells.ewma(source, tier, dimension::PASS_RATE);
        let cost = cells.ewma(&column.id, tier, dimension::COST_PER_RUN_USD);
        let latency = cells.ewma(&column.id, tier, dimension::LATENCY_P95_MS);
        let ceiling = ceilings::ceiling(&cells.pass_rates(source), bar, config.ceiling_buffer);
        let quality_x100 = quality.map(|cell| f64::from(cell.ewma) * 100.0);
        let column_availability = availability.get(&column.id).copied();
        scored.push(ScoredSurvivor {
            column_id: column.id.clone(),
            quality: quality_x100,
            cost_per_run: cost,
            latency_p95: latency,
            availability: column_availability,
            family: column.provider.clone(),
            has_data: quality.is_some()
                || pass_rate.is_some()
                || cost.is_some()
                || latency.is_some(),
            measured: quality.is_some_and(|cell| cell.measured),
            clears_difficulty: ceilings::clears(
                ceiling,
                u8::try_from(difficulty).unwrap_or(u8::MAX),
            ),
            prefs_satisfied: survivor.prefs_satisfied,
            settings: serde_json::Value::Null,
            retention_mode: effective_retention(column, now),
        });
        metrics.insert(
            column.id.clone(),
            SurvivorMetrics {
                quality: quality_x100,
                pass_rate,
                cost_per_run_usd: cost,
                latency_p95_ms: latency,
                availability: column_availability,
            },
        );
    }
    // D44: with `affinity`, the ranking goes through the affinity wrapper; without it, the
    // ranking is exactly the Go one.
    let (ranking, affinity_outcome) = match &affinity {
        None => (
            ranking::rank(&scored, mode, tenant_weights, &config.rank),
            None,
        ),
        Some(requested) => {
            let context = AffinityContext {
                previous_column_id: stored_affinity.as_ref().map(|row| row.column_id.clone()),
                cache_warm: stored_affinity
                    .as_ref()
                    .is_some_and(|row| now - row.last_used_at <= config.affinity.cache_warm),
                context_tokens: requested.context_tokens,
                cost_aware: affinity_on && requested.cost_aware.unwrap_or(tenant_cost_aware),
                incumbent_bonus: config.affinity.incumbent_bonus,
            };
            let prices: BTreeMap<String, TurnPrices> = scored
                .iter()
                .filter_map(|survivor| {
                    let column = by_id.get(survivor.column_id.as_str())?;
                    let pricing = parse_pricing(column.pricing.get()).ok()?;
                    Some((
                        survivor.column_id.clone(),
                        TurnPrices::from_pricing(&pricing),
                    ))
                })
                .collect();
            let out = conversation::rank_with_affinity(
                &scored,
                &prices,
                &decision.exclusions,
                mode,
                tenant_weights,
                &config.rank,
                &context,
            );
            let source = if requested.cost_aware.is_some() {
                "request"
            } else {
                "tenant"
            };
            (out.ranking, Some((out.decision, prices, source)))
        }
    };
    let Some(pick) = ranking.ranked.first() else {
        return Err(RecommendError::Internal(
            "ranking kept no candidate".to_owned(),
        ));
    };

    // Step 9 and 10: the response.
    let matrix_version = active_corpus_version.unwrap_or_default();
    let scored_by_id: HashMap<&str, &ScoredSurvivor> =
        scored.iter().map(|s| (s.column_id.as_str(), s)).collect();
    let ranked_columns: Vec<entries::RankedColumn> = ranking
        .ranked
        .iter()
        .map(|entry| {
            let survivor = scored_by_id[entry.column_id.as_str()];
            entries::RankedColumn {
                column_id: entry.column_id.clone(),
                family: survivor.family.clone(),
                quality: survivor.quality,
                clears_difficulty: entry.flags.clears_difficulty,
                low_availability: entry.flags.low_availability,
                settings: serde_json::Value::Null,
                retention_mode: survivor.retention_mode,
            }
        })
        .collect();
    let entry_columns: Vec<entries::EntryColumn> = pool_columns
        .iter()
        .map(|column| entries::EntryColumn {
            id: column.id.clone(),
            model_id: column.model_id.clone(),
            provider: column.provider.clone(),
            endpoint: column.endpoint.clone(),
            execution_aliases: serde_json::Value::Null,
        })
        .collect();
    let mut recommendations = entries::recommendations(
        &entry_columns,
        &ranked_columns,
        mode,
        &workload.capability,
        difficulty,
    );
    // D44: a pick that cost-aware affinity kept says so.
    if let (Some((decision, prices, _)), Some(first)) =
        (&affinity_outcome, recommendations.first_mut())
    {
        if decision.status == AffinityStatus::Kept && decision.cost_aware {
            first.reason = kept_reason(decision, prices.get(&first.column_id));
        }
    }
    let violations = decision
        .survivors
        .iter()
        .find(|s| s.column_id == pick.column_id)
        .map(|s| s.violations.clone())
        .unwrap_or_default();

    let id = ids::new(ids::prefix::RECOMMENDATION);
    let classification = workload.classification.clone();
    let prompt_hash = prompt_path
        .as_ref()
        .map(|(_, resolution)| resolution.prompt_hash.clone());
    let mut items = Vec::with_capacity(recommendations.len());
    for entry in &recommendations {
        let column = by_id[entry.column_id.as_str()];
        items.push(entry_json(entry, column, now)?);
    }
    let mut violation_items = Vec::with_capacity(violations.len());
    for violation in &violations {
        violation_items.push(object(vec![
            ("constraint", string(violation.constraint.as_str())),
            ("limit", number(violation.limit)?),
            ("estimated", number(violation.estimated)?),
        ]));
    }
    let check = checks::check_spec_for(&workload.capability);
    let mut members = vec![
        ("classification", classification.clone()),
        ("recommendations", JsonValue::Array(items)),
        ("pool", string(&decision.pool)),
        ("mode", string(mode)),
        (
            "check_spec",
            object(vec![
                ("type", string(check.kind)),
                ("ref", string(check.reference)),
            ]),
        ),
        (
            "constraints_met",
            JsonValue::Bool(decision.constraints_met && pick.flags.prefs_satisfied),
        ),
        ("violations", JsonValue::Array(violation_items)),
    ];
    if let Some(hash) = &prompt_hash {
        members.push(("prompt_hash", string(hash)));
    }
    members.extend([
        ("recommendation_id", string(&id)),
        ("matrix_version", string(&matrix_version)),
        ("measured", JsonValue::Bool(pick.flags.measured)),
        // D9.
        ("zero_retention", JsonValue::Bool(zero_retention)),
        // D39: false when the tenant opted out of telemetry.
        ("telemetry", JsonValue::Bool(!telemetry_opt_out)),
    ]);
    // D44: only when the request sent `affinity`, before the quota flag.
    if let Some((decision, _, _)) = &affinity_outcome {
        members.push(("affinity", affinity_response(decision)));
    }
    // D4: only when the budget stopped the classifier.
    if prompt_path
        .as_ref()
        .is_some_and(|(_, resolution)| resolution.quota_exceeded)
    {
        members.push(("classification_quota_exceeded", JsonValue::Bool(true)));
    }
    let response = object(members);
    let response_text = gojson::marshal(&response);

    // Step 11: the trace row. Survivors in rank order, then the policy exclusions, then
    // the ranking's `no_data` exclusions.
    let info = |column_id: &str| {
        by_id
            .get(column_id)
            .map(|c| ColumnInfo {
                provider: c.provider.clone(),
                model: c.model_id.clone(),
                endpoint: c.endpoint.clone(),
            })
            .unwrap_or_default()
    };
    let candidates = trace::candidates(
        ranking.ranked.iter().map(|entry| {
            TraceCandidate::survivor(
                entry,
                &info(&entry.column_id),
                &metrics[entry.column_id.as_str()],
            )
        }),
        decision
            .exclusions
            .iter()
            .chain(&ranking.exclusions)
            .map(|exclusion| TraceCandidate::exclusion(exclusion, &info(&exclusion.column_id))),
    );
    let candidates = trace::candidates_json(&candidates).map_err(RecommendError::Internal)?;
    let body_text = String::from_utf8_lossy(body);
    let stored_request = match (&prompt_path, &affinity) {
        // The affinity key is never stored either (D44).
        (_, Some(_)) => raw(
            prompt::exclude_affinity_key(&body_text, prompt_path.is_some())
                .map_err(RecommendError::Internal)?,
        )?,
        // The prompt is never stored (spec §10).
        (Some(_), None) => {
            raw(prompt::exclude_prompt(&body_text).map_err(RecommendError::Internal)?)?
        }
        (None, None) => raw(body_text.trim_matches([' ', '\t', '\n', '\r']).to_owned())?,
    };
    let affinity_block = match &affinity_outcome {
        Some((decision, _, source)) => Some(raw(gojson::marshal(&affinity_trace(
            conversation_hash.as_deref(),
            decision,
            source,
        )?))?),
        None => None,
    };
    let classification_raw = raw(gojson::marshal(&classification))?;
    let candidates_raw = raw(candidates)?;
    let response_raw = raw(response_text.clone())?;
    let new_recommendation = NewRecommendation {
        id: &id,
        tenant_id: tenant,
        api_key_id: &identity.api_key_id,
        prompt_hash: prompt_hash.as_deref(),
        request: &stored_request,
        classification: &classification_raw,
        candidates: &candidates_raw,
        response: &response_raw,
        matrix_version: &matrix_version,
        affinity: affinity_block.as_deref(),
    };
    match &conversation_hash {
        // D44: the row commits with the trace row, in one statement.
        Some(hash) => {
            decisions::create_recommendation_with_affinity(
                pool,
                &new_recommendation,
                &AffinityRow {
                    tenant_id: tenant.to_owned(),
                    affinity_hash: hash.clone(),
                    column_id: pick.column_id.clone(),
                    recommendation_id: id.clone(),
                    last_used_at: now,
                    expires_at: now + config.affinity.ttl,
                },
            )
            .await?;
        }
        None => {
            decisions::create_recommendation(pool, &new_recommendation).await?;
        }
    }
    if let Some((decision, _, _)) = &affinity_outcome {
        crate::metrics::affinity_decision(status_name(decision.status), decision.cost_aware);
    }

    // The capture hook runs after the trace row and never fails the request.
    if let (Some((prompt, resolution)), Some(capturer)) = (&prompt_path, &config.capture) {
        capturer
            .maybe(
                pool,
                &CaptureRequest {
                    tenant_id: tenant,
                    api_key_id: &identity.api_key_id,
                    zero_retention,
                    recommendation_id: &id,
                    prompt,
                    confidence: resolution.classification.confidence,
                    now,
                },
            )
            .await;
    }

    let mut body = response_text;
    body.push('\n');
    Ok(Recommended { id, body })
}

/// The workload the rest of the pipeline uses, with its response `classification` block.
struct Resolved {
    capability: String,
    difficulty: i64,
    classification: JsonValue,
}

/// A resolved declared workload.
struct Declared {
    capability: String,
    difficulty: i64,
    /// True when the request named no difficulty.
    defaulted: bool,
    domain_tags: Vec<String>,
}

impl Declared {
    fn resolved(&self) -> Resolved {
        Resolved {
            capability: self.capability.clone(),
            difficulty: self.difficulty,
            classification: self.classification(),
        }
    }

    /// The classification block: confidence 1, method `declared`, and
    /// `difficulty_source: "default"` only when the difficulty was defaulted.
    fn classification(&self) -> JsonValue {
        let mut members = vec![
            ("capability", string(&self.capability)),
            ("difficulty", JsonValue::Number(self.difficulty.to_string())),
            (
                "domain_tags",
                JsonValue::Array(self.domain_tags.iter().map(|t| string(t)).collect()),
            ),
            (
                "high_stakes",
                JsonValue::Bool(ranking::high_stakes(&self.domain_tags)),
            ),
            ("confidence", JsonValue::Number("1".to_owned())),
            ("method", string("declared")),
        ];
        if self.defaulted {
            members.push(("difficulty_source", string("default")));
        }
        object(members)
    }
}

/// The newest aggregate rows of the workload's capability, by (column, tier, dimension).
struct Cells<'a>(HashMap<(&'a str, i16, &'a str), &'a Aggregate>);

impl<'a> Cells<'a> {
    fn new(rows: &'a [Aggregate]) -> Self {
        Self(
            rows.iter()
                .map(|row| {
                    (
                        (
                            row.model_column_id.as_str(),
                            row.difficulty_tier,
                            row.dimension.as_str(),
                        ),
                        row,
                    )
                })
                .collect(),
        )
    }

    fn get(&self, column: &str, tier: i16, dimension: &str) -> Option<&'a Aggregate> {
        self.0.get(&(column, tier, dimension)).copied()
    }

    /// The cell's EWMA widened from `real`.
    fn ewma(&self, column: &str, tier: i16, dimension: &str) -> Option<f64> {
        self.get(column, tier, dimension)
            .map(|cell| f64::from(cell.ewma))
    }

    /// The column's pass-rate cells, tiers 1 to 5, for its ceiling.
    fn pass_rates(&self, column: &str) -> Vec<ceilings::PassRate> {
        (1..=5u8)
            .filter_map(|tier| {
                self.get(column, i16::from(tier), dimension::PASS_RATE)
                    .map(|cell| ceilings::PassRate {
                        tier,
                        ewma: f64::from(cell.ewma),
                        measured: cell.measured,
                    })
            })
            .collect()
    }
}

/// Where a column's quality and pass-rate cells come from (spec §4.2).
fn quality_source(column: &ModelColumn) -> &str {
    column
        .quality_source_column_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .unwrap_or(&column.id)
}

fn catalog_column(column: &ModelColumn) -> CatalogColumn {
    CatalogColumn {
        id: column.id.clone(),
        model_id: column.model_id.clone(),
        status: match column.status.as_str() {
            "cold_start" => ColumnStatus::ColdStart,
            "active" => ColumnStatus::Active,
            "deprecated" => ColumnStatus::Deprecated,
            // The CHECK constraint allows nothing else; an unknown status is not runnable.
            _ => ColumnStatus::Retired,
        },
        retention_mode: retention_mode(&column.retention_mode),
        retention_valid_until: column.retention_valid_until,
    }
}

/// A stored retention mode; anything unrecognized reads as `unknown` (fail closed).
fn retention_mode(stored: &str) -> RetentionMode {
    match stored {
        "standard" => RetentionMode::Standard,
        "zero" => RetentionMode::Zero,
        _ => RetentionMode::Unknown,
    }
}

pub(crate) fn effective_retention(column: &ModelColumn, now: DateTime<Utc>) -> RetentionMode {
    retention_mode(&column.retention_mode).effective(column.retention_valid_until, now)
}

/// The API error for a policy refusal.
pub(crate) fn policy_error(error: PolicyError) -> ApiError {
    let code = match error.code {
        PolicyErrorCode::UnknownPool => ErrorCode::UnknownPool,
        PolicyErrorCode::UnknownModel => ErrorCode::UnknownModel,
        PolicyErrorCode::ModelNotInPool => ErrorCode::ModelNotInPool,
        PolicyErrorCode::NoCompliantColumns => ErrorCode::NoCompliantColumns,
    };
    let status =
        StatusCode::from_u16(error.code.http_status()).unwrap_or(StatusCode::UNPROCESSABLE_ENTITY);
    let mut api = ApiError::new(status, code, error.message);
    if let serde_json::Value::Object(details) = error.details {
        api.details = details.into_iter().collect::<BTreeMap<_, _>>();
    }
    api
}

/// Writes a request-time `pool_would_be_empty` alert; a failed write fails the request.
async fn write_alert(
    pool: &PgPool,
    config: &RecommendConfig,
    alert: &policy::Alert,
) -> Result<(), RecommendError> {
    let severity = match alert.severity {
        "info" => Severity::Info,
        "warning" => Severity::Warning,
        _ => Severity::Critical,
    };
    maverick_alerts::insert_alert(
        pool,
        config.alerts.as_deref(),
        AlertKind::PoolWouldBeEmpty,
        severity,
        &alert.subject,
        &alert.message,
    )
    .await
    .map(|_| ())
    .map_err(|error| RecommendError::Internal(error.to_string()))
}

/// One `recommendations` entry on the wire. `execution_aliases`, `settings`, and
/// `param_constraints` are the stored jsonb text, compacted, with its key order and
/// number spelling.
fn entry_json(
    entry: &entries::RecommendationEntry,
    column: &ModelColumn,
    now: DateTime<Utc>,
) -> Result<JsonValue, RecommendError> {
    let score = match entry.score {
        Some(score) => number(score)?,
        None => JsonValue::Null,
    };
    Ok(object(vec![
        ("provider", string(&entry.provider)),
        ("model", string(&entry.model)),
        ("endpoint", string(&entry.endpoint)),
        ("column_id", string(&entry.column_id)),
        ("execution_aliases", jsonb(column.execution_aliases.get())?),
        // D15: each alias's effective retention mode (openrouter spec §2.3).
        (
            "execution_alias_retention",
            crate::models::alias_retention(column, now).map_err(RecommendError::Internal)?,
        ),
        // D9.
        ("settings", jsonb(column.settings.get())?),
        // D29: the model's parameter constraints, verbatim; `{}` means none.
        ("param_constraints", jsonb(column.param_constraints.get())?),
        ("retention_mode", string(entry.retention_mode.as_str())),
        ("score", score),
        (
            "clears_difficulty",
            JsonValue::Bool(entry.clears_difficulty),
        ),
        // D8.
        ("low_availability", JsonValue::Bool(entry.low_availability)),
        ("reason", string(&entry.reason)),
    ]))
}

/// The wire name of an affinity status (spec §7.6).
fn status_name(status: AffinityStatus) -> &'static str {
    match status {
        AffinityStatus::New => "new",
        AffinityStatus::Kept => "kept",
        AffinityStatus::Switched => "switched",
        AffinityStatus::Broken => "broken",
    }
}

fn optional_text(value: Option<&str>) -> JsonValue {
    value.map_or(JsonValue::Null, string)
}

/// The response's `affinity` block (spec §9.2, D44).
fn affinity_response(decision: &AffinityDecision) -> JsonValue {
    object(vec![
        ("status", string(status_name(decision.status))),
        (
            "previous_column_id",
            optional_text(decision.previous_column_id.as_deref()),
        ),
        (
            "reason",
            optional_text(decision.reason.map(|reason| reason.as_str())),
        ),
        ("cache_warm", JsonValue::Bool(decision.cache_warm)),
        ("cost_aware", JsonValue::Bool(decision.cost_aware)),
    ])
}

/// The trace's `affinity` block, in the spec §10.2 key order.
fn affinity_trace(
    hash: Option<&str>,
    decision: &AffinityDecision,
    source: &str,
) -> Result<JsonValue, RecommendError> {
    let ranking_costs = match &decision.ranking_costs {
        None => JsonValue::Null,
        Some(costs) => JsonValue::Object(
            costs
                .iter()
                .map(|(column_id, cost)| {
                    let cost = match cost {
                        Some(cost) => number(*cost)?,
                        None => JsonValue::Null,
                    };
                    Ok((column_id.clone(), cost))
                })
                .collect::<Result<Vec<_>, RecommendError>>()?,
        ),
    };
    let skipped = decision.turn_cost_skipped.map(|skipped| match skipped {
        TurnCostSkipped::CostAwareOff => "cost_aware_off",
        TurnCostSkipped::NoContextTokens => "no_context_tokens",
        TurnCostSkipped::MissingInputPrice => "missing_input_price",
    });
    let withheld = decision.bonus_withheld.map(|withheld| match withheld {
        BonusWithheld::Escalation => "escalation",
    });
    Ok(object(vec![
        ("affinity_hash", optional_text(hash)),
        (
            "previous_column_id",
            optional_text(decision.previous_column_id.as_deref()),
        ),
        ("status", string(status_name(decision.status))),
        (
            "reason",
            optional_text(decision.reason.map(|reason| reason.as_str())),
        ),
        ("cache_warm", JsonValue::Bool(decision.cache_warm)),
        (
            "context_tokens",
            decision.context_tokens.map_or(JsonValue::Null, |tokens| {
                JsonValue::Number(tokens.to_string())
            }),
        ),
        ("cost_aware", JsonValue::Bool(decision.cost_aware)),
        ("cost_aware_source", string(source)),
        ("bonus", number(decision.bonus)?),
        ("bonus_withheld", optional_text(withheld)),
        ("turn_cost_skipped", optional_text(skipped)),
        ("ranking_costs", ranking_costs),
    ]))
}

/// The reason text of a pick that cost-aware affinity kept. With a warm cache and a
/// turn cost, it names the saving: the cold price of the context minus the cache-read
/// price. Informative, not contractual (spec §7.6).
fn kept_reason(decision: &AffinityDecision, prices: Option<&TurnPrices>) -> String {
    let mut reason = "kept for conversation continuity".to_owned();
    let saving = match (decision.context_tokens, prices) {
        (Some(tokens), Some(prices)) if decision.cache_warm && decision.ranking_costs.is_some() => {
            let read = prices.cache_read_per_mtok;
            let cold = prices.cache_write_per_mtok.or(prices.input_per_mtok);
            read.zip(cold)
                .map(|(read, cold)| tokens as f64 * (cold - read) / 1e6)
        }
        _ => None,
    };
    if let Some(saving) = saving.filter(|saving| *saving >= 0.005) {
        reason.push_str(&format!(
            "; warm cache saves an estimated ${saving:.2} this turn"
        ));
    }
    reason
}

/// A stored jsonb value; SQL `null` or a JSON `null` is `{}`.
fn jsonb(text: &str) -> Result<JsonValue, RecommendError> {
    match gojson::unmarshal(text) {
        Ok(JsonValue::Null) => Ok(JsonValue::Object(Vec::new())),
        Ok(value) => Ok(value),
        Err(error) => Err(RecommendError::Internal(format!(
            "stored jsonb does not parse: {error}"
        ))),
    }
}

fn raw(text: String) -> Result<Box<JsonRawValue>, RecommendError> {
    JsonRawValue::from_string(text)
        .map_err(|error| RecommendError::Internal(format!("trace JSON: {error}")))
}

fn object(members: Vec<(&str, JsonValue)>) -> JsonValue {
    JsonValue::Object(
        members
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    )
}

fn string(value: &str) -> JsonValue {
    JsonValue::String(value.to_owned())
}

/// A Go `float64` on the wire; NaN and infinities fail as Go's encoder does.
fn number(value: f64) -> Result<JsonValue, RecommendError> {
    gojson::encode_f64(value)
        .map(JsonValue::Number)
        .map_err(RecommendError::Internal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_classification_block() {
        let declared = Declared {
            capability: "factual_qa".to_owned(),
            difficulty: 3,
            defaulted: true,
            domain_tags: Vec::new(),
        };
        assert_eq!(
            gojson::marshal(&declared.classification()),
            r#"{"capability":"factual_qa","difficulty":3,"domain_tags":[],"high_stakes":false,"confidence":1,"method":"declared","difficulty_source":"default"}"#
        );
        let declared = Declared {
            capability: "structured_extraction".to_owned(),
            difficulty: 3,
            defaulted: false,
            domain_tags: vec!["legal".to_owned()],
        };
        assert_eq!(
            gojson::marshal(&declared.classification()),
            r#"{"capability":"structured_extraction","difficulty":3,"domain_tags":["legal"],"high_stakes":true,"confidence":1,"method":"declared"}"#
        );
    }

    #[test]
    fn jsonb_keeps_order_and_number_spelling() {
        let value = jsonb(r#"{"vertex": "a", "openrouter": "b", "n": 3.0}"#).unwrap();
        assert_eq!(
            gojson::marshal(&value),
            r#"{"vertex":"a","openrouter":"b","n":3.0}"#
        );
        assert_eq!(gojson::marshal(&jsonb("null").unwrap()), "{}");
    }

    #[test]
    fn default_quality_bar_is_the_widened_float32() {
        assert_eq!(f64::from(DEFAULT_QUALITY_BAR), 0.800_000_011_920_929);
    }

    #[test]
    fn unknown_stored_values_fail_closed() {
        assert_eq!(retention_mode("zero"), RetentionMode::Zero);
        assert_eq!(retention_mode("standard"), RetentionMode::Standard);
        assert_eq!(retention_mode("bogus"), RetentionMode::Unknown);
    }
}
