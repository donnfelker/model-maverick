//! `Evaluate(cfg, catalog, cellAggregates, request)`: spec §7.1 steps 1 to 4 as a
//! pure function (reference Part 4 §1.3), plus the D1 zero-retention filter.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde_json::json;

use super::{
    Alert, AlertSubject, CatalogColumn, CellAggregate, CellAggregates, ColumnStatus, Constraint,
    Decision, ExclusionReason, PolicyError, PolicyErrorCode, PolicyRequest, Pool, RetentionMode,
    Survivor, TenantConfig, Violation,
};
use crate::gofmt::{fmt_v_f64, quote};

/// Resolves the request's pool (also used by `/v1/policies`): the first pool with the
/// requested name, or the first default pool when the name is empty.
pub fn resolve_pool<'a>(cfg: &'a TenantConfig, name: &str) -> Result<&'a Pool, PolicyError> {
    if name.is_empty() {
        return cfg
            .pools
            .iter()
            .find(|pool| pool.is_default)
            .ok_or_else(|| {
                error(
                    PolicyErrorCode::UnknownPool,
                    "tenant has no default pool".to_owned(),
                    json!({"pool": ""}),
                )
            });
    }
    cfg.pools
        .iter()
        .find(|pool| pool.name == name)
        .ok_or_else(|| {
            error(
                PolicyErrorCode::UnknownPool,
                format!("pool {} is not configured for this tenant", quote(name)),
                json!({"pool": name}),
            )
        })
}

/// Turns the request's pool into ordered survivors and records every exclusion.
///
/// The order is fixed: restrict to the pool, then the allowlist, then the standing
/// constraints (zero retention first, then cost and latency), then the request
/// preferences. `now` decides whether retention evidence has expired.
///
/// Zero retention (D1) fails closed: when it empties the candidates the request is
/// refused with `no_compliant_columns`, which carries the critical alert to raise.
/// Cost and latency may be relaxed by the G1 drift fallback, and only over
/// zero-compliant columns. A pool whose columns are all retired yields a decision with
/// no survivors and an alert, not an error (the caller fails that request).
pub fn evaluate(
    cfg: &TenantConfig,
    catalog: &[CatalogColumn],
    aggregates: &CellAggregates,
    request: &PolicyRequest,
    now: DateTime<Utc>,
) -> Result<Decision, PolicyError> {
    let pool = resolve_pool(cfg, &request.pool)?;
    let mut decision = Decision {
        pool: pool.name.clone(),
        survivors: Vec::new(),
        exclusions: Vec::new(),
        constraints_met: true,
        drift_alert: None,
    };

    // Step 1: restrict to the pool. All `not_in_pool` rows come first (catalog order),
    // then `status_retired` rows (pool order). A pool id missing from the catalog is
    // skipped silently.
    let members: HashSet<&str> = pool.model_column_ids.iter().map(String::as_str).collect();
    for column in catalog {
        if !members.contains(column.id.as_str()) {
            decision.exclude(
                &column.id,
                ExclusionReason::NotInPool,
                format!("column is not in pool {}", quote(&pool.name)),
            );
        }
    }
    let by_id: HashMap<&str, &CatalogColumn> = catalog
        .iter()
        .map(|column| (column.id.as_str(), column))
        .collect();
    let mut candidates = Vec::new();
    for id in &pool.model_column_ids {
        let Some(&column) = by_id.get(id.as_str()) else {
            continue;
        };
        if column.status == ColumnStatus::Retired {
            decision.exclude(
                id,
                ExclusionReason::StatusRetired,
                "column status is retired".to_owned(),
            );
        } else {
            candidates.push(column);
        }
    }
    if candidates.is_empty() {
        decision.drift_alert = Some(Alert::pool_would_be_empty(
            subject(cfg, pool, request, None, None),
            format!(
                "pool {} has no usable columns: every column is retired",
                quote(&pool.name)
            ),
        ));
        return Ok(decision);
    }

    // Step 2: the allowlist (G3). `None` or an empty list is no filter.
    if let Some(entries) = request
        .allowed_models
        .as_deref()
        .filter(|entries| !entries.is_empty())
    {
        candidates = apply_allowlist(entries, catalog, pool, &by_id, candidates, &mut decision)?;
    }

    // Step 3a: zero retention (D1). Never relaxed for G1.
    if cfg.constraints.zero_retention {
        let mut compliant = Vec::with_capacity(candidates.len());
        for column in candidates {
            let mode = column.effective_retention_mode(now);
            if mode == RetentionMode::Zero {
                compliant.push(column);
            } else {
                decision.exclude(
                    &column.id,
                    ExclusionReason::StandingZeroRetention,
                    format!("column retention_mode is {}, not zero", mode.as_str()),
                );
            }
        }
        if compliant.is_empty() {
            return Err(no_compliant_columns(cfg, pool, request));
        }
        candidates = compliant;
    }

    // Step 3b: standing cost and latency, with the G1 drift fallback.
    let standing = &cfg.constraints;
    let checked: Vec<(&CatalogColumn, Vec<Violation>)> = candidates
        .into_iter()
        .map(|column| {
            let found = violations(
                standing.max_cost_per_run,
                standing.max_latency_ms,
                aggregates.get(&column.id),
            );
            (column, found)
        })
        .collect();
    let mut survivors = Vec::with_capacity(checked.len());
    if checked.iter().any(|(_, found)| found.is_empty()) {
        for (column, found) in checked {
            match found.first() {
                None => survivors.push(survivor(column, Vec::new())),
                Some(first) => exclude_standing(&mut decision, column, first),
            }
        }
    } else {
        let excesses: Vec<f64> = checked.iter().map(|(_, found)| excess(found)).collect();
        let best = excesses.iter().copied().fold(f64::INFINITY, f64::min);
        let mut retained = Vec::new();
        for ((column, found), e) in checked.into_iter().zip(excesses) {
            // Exact float equality (QUIRK): differently rounded equal excesses do not tie.
            if e > best {
                exclude_standing(&mut decision, column, &found[0]);
            } else {
                retained.push(column.id.clone());
                survivors.push(survivor(column, found));
            }
        }
        decision.constraints_met = false;
        let message = format!(
            "standing constraints emptied pool {} for {} tier {} at request time; \
             retained least-violating columns [{}]",
            quote(&pool.name),
            request.capability,
            request.difficulty_tier,
            retained.join(" "),
        );
        decision.drift_alert = Some(Alert::pool_would_be_empty(
            subject(cfg, pool, request, Some(retained), None),
            message,
        ));
    }

    // Step 4: request preferences. A stable partition: satisfiers keep their order,
    // violators follow in order with their preference violations appended.
    let preferences = &request.constraints;
    let mut satisfying = Vec::with_capacity(survivors.len());
    let mut violating = Vec::new();
    for mut entry in survivors {
        let found = violations(
            preferences.max_cost_per_run,
            preferences.max_latency_ms,
            aggregates.get(&entry.column_id),
        );
        if found.is_empty() {
            satisfying.push(entry);
        } else {
            entry.prefs_satisfied = false;
            entry.violations.extend(found);
            violating.push(entry);
        }
    }
    satisfying.extend(violating);
    decision.survivors = satisfying;
    Ok(decision)
}

/// Step 2. Entries are checked in request order and the first bad entry wins, so an
/// unknown entry after a valid one still fails.
fn apply_allowlist<'c>(
    entries: &[String],
    catalog: &[CatalogColumn],
    pool: &Pool,
    by_id: &HashMap<&str, &'c CatalogColumn>,
    candidates: Vec<&'c CatalogColumn>,
    decision: &mut Decision,
) -> Result<Vec<&'c CatalogColumn>, PolicyError> {
    let mut marked = vec![false; candidates.len()];
    for entry in entries {
        if !catalog.iter().any(|column| column.matches_entry(entry)) {
            return Err(error(
                PolicyErrorCode::UnknownModel,
                format!(
                    "allowed_models entry {} is not in the model catalog",
                    quote(entry)
                ),
                json!({"entry": entry}),
            ));
        }
        let mut matched = false;
        for (mark, column) in marked.iter_mut().zip(&candidates) {
            if column.matches_entry(entry) {
                *mark = true;
                matched = true;
            }
        }
        if !matched {
            // QUIRK: the retired wording needs a retired match inside the pool; a
            // retired catalog column outside it gets the plain message.
            let retired_in_pool = pool
                .model_column_ids
                .iter()
                .filter_map(|id| by_id.get(id.as_str()))
                .any(|column| {
                    column.status == ColumnStatus::Retired && column.matches_entry(entry)
                });
            let message = if retired_in_pool {
                format!(
                    "allowed_models entry {} matches only retired columns in pool {}",
                    quote(entry),
                    quote(&pool.name)
                )
            } else {
                format!(
                    "allowed_models entry {} is not in pool {}",
                    quote(entry),
                    quote(&pool.name)
                )
            };
            return Err(error(
                PolicyErrorCode::ModelNotInPool,
                message,
                json!({"entry": entry, "pool": pool.name}),
            ));
        }
    }
    let mut kept = Vec::with_capacity(candidates.len());
    for (column, mark) in candidates.into_iter().zip(marked) {
        if mark {
            kept.push(column);
        } else {
            decision.exclude(
                &column.id,
                ExclusionReason::NotInAllowlist,
                "column matches no allowed_models entry".to_owned(),
            );
        }
    }
    Ok(kept)
}

/// `violations(maxCost, maxLatency, agg)`: cost first, then latency. A limit binds only
/// when set and when the column has that aggregate; a value equal to the limit passes.
fn violations(
    max_cost_per_run: Option<f64>,
    max_latency_ms: Option<i32>,
    aggregate: Option<&CellAggregate>,
) -> Vec<Violation> {
    let mut found = Vec::new();
    let cost = aggregate.and_then(|agg| agg.cost_per_run_usd);
    if let (Some(limit), Some(estimated)) = (max_cost_per_run, cost) {
        if estimated > limit {
            found.push(Violation {
                constraint: Constraint::MaxCostPerRun,
                limit,
                estimated,
            });
        }
    }
    let latency = aggregate.and_then(|agg| agg.latency_p95_ms);
    if let (Some(limit), Some(estimated)) = (max_latency_ms, latency) {
        let limit = f64::from(limit);
        if estimated > limit {
            found.push(Violation {
                constraint: Constraint::MaxLatencyMs,
                limit,
                estimated,
            });
        }
    }
    found
}

/// The drift fallback's excess: the sum of each violation's relative overshoot, or the
/// raw overshoot for a non-positive limit (writes validate limits `> 0`).
fn excess(found: &[Violation]) -> f64 {
    found.iter().fold(0.0, |sum, violation| {
        let over = violation.estimated - violation.limit;
        if violation.limit > 0.0 {
            sum + over / violation.limit
        } else {
            sum + over
        }
    })
}

/// A standing violator is excluded with the reason and detail of its first violation.
fn exclude_standing(decision: &mut Decision, column: &CatalogColumn, first: &Violation) {
    decision.exclude(
        &column.id,
        first.constraint.standing_reason(),
        format!(
            "{} EWMA {} exceeds {} {}",
            first.constraint.dimension(),
            fmt_v_f64(first.estimated),
            first.constraint.as_str(),
            fmt_v_f64(first.limit),
        ),
    );
}

fn survivor(column: &CatalogColumn, violations: Vec<Violation>) -> Survivor {
    Survivor {
        column_id: column.id.clone(),
        prefs_satisfied: true,
        violations,
    }
}

fn subject(
    cfg: &TenantConfig,
    pool: &Pool,
    request: &PolicyRequest,
    retained: Option<Vec<String>>,
    constraint: Option<&'static str>,
) -> AlertSubject {
    AlertSubject {
        tenant_id: cfg.tenant_id.clone(),
        pool: pool.name.clone(),
        capability: request.capability.clone(),
        difficulty_tier: request.difficulty_tier,
        retained,
        constraint,
    }
}

/// D1: `409 no_compliant_columns`, with the critical alert naming tenant and pool.
fn no_compliant_columns(cfg: &TenantConfig, pool: &Pool, request: &PolicyRequest) -> PolicyError {
    let alert = Alert::pool_would_be_empty(
        subject(cfg, pool, request, None, Some("zero_retention")),
        format!(
            "zero retention emptied pool {} for {} tier {} at request time; \
             no column holds valid zero-retention evidence",
            quote(&pool.name),
            request.capability,
            request.difficulty_tier,
        ),
    );
    PolicyError {
        code: PolicyErrorCode::NoCompliantColumns,
        message: format!(
            "no column in pool {} holds valid zero-retention evidence",
            quote(&pool.name)
        ),
        details: json!({"pool": pool.name}),
        alert: Some(Box::new(alert)),
    }
}

fn error(code: PolicyErrorCode, message: String, details: serde_json::Value) -> PolicyError {
    PolicyError {
        code,
        message,
        details,
        alert: None,
    }
}
