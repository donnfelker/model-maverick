//! `GET /v1/policies?capability=&pool=`: the banded policy table (spec §9.3; reference Part 5
//! §6.4, Part 4 §2.5).
//!
//! Per capability, per band (`easy` 1-2, `medium` 3, `hard` 4-5), the pool column that
//! resolves the band: the first that clears its top tier, else the highest ceiling, else
//! the cheapest known band cost, ties keeping pool order (G1: a band always names a column
//! when the pool has one). Candidates are the pool's columns in pool order that exist in the
//! catalog and are not retired; no allowlist, standing constraint, or preference applies.
//! Ceilings use the pass-rate cells of the column's quality source at the tenant's bar for
//! the capability; band costs use the column's own cost cells.
//!
//! Order: unknown query parameter; capability; pool; then the table. QUIRK: `quality_bar`
//! is the tenant's bar only when one capability is asked for and has a weights row; for
//! the full table it is always the default 0.8, though each capability's ceilings still use
//! its own bar.

use std::collections::{BTreeMap, HashMap};

use axum::body::Body;
use axum::extract::{Extension, Request, State};
use axum::http::{Method, Response, StatusCode};
use maverick_domain::ceilings::{self, BandCandidate, PassRate};
use maverick_domain::gofmt::quote;
use maverick_domain::gojson::{self, JsonValue};
use maverick_domain::policy::{self, StandingConstraints, TenantConfig};
use maverick_domain::scoring::observe::dimension;
use maverick_domain::tasks::CAPABILITIES;
use maverick_store::catalog::{self, ModelColumn};
use maverick_store::measurement::{self, Aggregate};
use maverick_store::{corpus, is_not_found, tenancy};
use sqlx::PgPool;

use crate::recommend::policy_error;
use crate::{ApiError, ApiState, ErrorCode, Identity, internal_error, not_found};

/// The bar of a tenant without a weights row for the capability: Go's float32 `0.80`.
const DEFAULT_QUALITY_BAR: f32 = 0.80;

pub(crate) enum Failure {
    Api(ApiError),
    Internal(String),
}

impl From<ApiError> for Failure {
    fn from(error: ApiError) -> Self {
        Self::Api(error)
    }
}

impl From<sqlx::Error> for Failure {
    fn from(error: sqlx::Error) -> Self {
        Self::Internal(error.to_string())
    }
}

/// `GET /v1/policies`.
pub(crate) async fn policies_handler(
    State(api): State<ApiState>,
    Extension(identity): Extension<Identity>,
    request: Request,
) -> Response<Body> {
    if request.method() != Method::GET {
        return not_found();
    }
    let query = request.uri().query().unwrap_or_default().to_owned();
    let Some(pool) = api.pool() else {
        return internal_error();
    };
    match policies(
        pool,
        api.recommend().ceiling_buffer,
        &identity.tenant_id,
        &query,
    )
    .await
    {
        Ok(body) => crate::response_with_json(StatusCode::OK, body.into_bytes()),
        Err(Failure::Api(error)) => error.into_response(),
        Err(Failure::Internal(error)) => {
            tracing::error!(
                method = "GET",
                path = "/v1/policies",
                error = %error,
                "policies handler failure"
            );
            internal_error()
        }
    }
}

/// The response body for one tenant and query: compact JSON and one `\n` (also the MCP
/// `get_policy` tool).
pub(crate) async fn policies(
    pool: &PgPool,
    buffer: f64,
    tenant: &str,
    query: &str,
) -> Result<String, Failure> {
    let (capability, pool_name) = parse_query(query)?;
    if !capability.is_empty() && !CAPABILITIES.contains(&capability.as_str()) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::InvalidRequest,
            format!("capability {} is not a v1 capability", quote(&capability)),
        )
        .detail("capability", capability.as_str())
        .into());
    }

    let pools = tenancy::list_pools_by_tenant(pool, tenant).await?;
    let config = TenantConfig {
        tenant_id: tenant.to_owned(),
        pools: pools
            .into_iter()
            .map(|p| policy::Pool {
                name: p.name,
                model_column_ids: p.model_column_ids,
                is_default: p.is_default,
            })
            .collect(),
        constraints: StandingConstraints::default(),
    };
    let resolved = policy::resolve_pool(&config, &pool_name).map_err(policy_error)?;
    let weights = tenancy::list_tenant_weights_by_tenant(pool, tenant).await?;
    let bars: HashMap<&str, f32> = weights
        .iter()
        .map(|w| (w.capability.as_str(), w.quality_bar))
        .collect();

    // The candidates: pool order, known and not retired.
    let columns = catalog::list_model_columns(pool).await?;
    let by_id: HashMap<&str, &ModelColumn> = columns.iter().map(|c| (c.id.as_str(), c)).collect();
    let candidates: Vec<&ModelColumn> = resolved
        .model_column_ids
        .iter()
        .filter_map(|id| by_id.get(id.as_str()).copied())
        .filter(|column| column.status != "retired")
        .collect();
    let mut wanted: Vec<String> = Vec::new();
    for column in &candidates {
        for id in [column.id.as_str(), quality_source(column)] {
            if !wanted.iter().any(|w| w == id) {
                wanted.push(id.to_owned());
            }
        }
    }
    let rows = measurement::list_latest_aggregates_by_columns(
        pool,
        &wanted,
        &[
            dimension::PASS_RATE.to_owned(),
            dimension::COST_PER_RUN_USD.to_owned(),
        ],
    )
    .await?;
    let cells = Cells::new(&rows);

    let capabilities: Vec<&str> = if capability.is_empty() {
        CAPABILITIES.to_vec()
    } else {
        vec![capability.as_str()]
    };
    let mut policies = Vec::with_capacity(capabilities.len());
    for capability in &capabilities {
        let bar = f64::from(bars.get(capability).copied().unwrap_or(DEFAULT_QUALITY_BAR));
        let band_candidates: Vec<BandCandidate> = candidates
            .iter()
            .map(|column| {
                let source = quality_source(column);
                let pass = cells.by_tier(source, capability, dimension::PASS_RATE);
                let pass_rates: Vec<PassRate> = pass
                    .iter()
                    .map(|(tier, cell)| PassRate {
                        tier: *tier,
                        ewma: f64::from(cell.ewma),
                        measured: cell.measured,
                    })
                    .collect();
                BandCandidate {
                    column_id: column.id.clone(),
                    ceiling: ceilings::ceiling(&pass_rates, bar, buffer),
                    cost_by_tier: cells
                        .by_tier(&column.id, capability, dimension::COST_PER_RUN_USD)
                        .into_iter()
                        .map(|(tier, cell)| (tier, f64::from(cell.ewma)))
                        .collect(),
                    pass_by_tier: pass
                        .into_iter()
                        .map(|(tier, cell)| (tier, f64::from(cell.ewma)))
                        .collect(),
                }
            })
            .collect();
        let mut bands = Vec::new();
        for pick in ceilings::resolve_bands(&band_candidates) {
            let column = match by_id.get(pick.column_id.as_str()) {
                Some(column) if !pick.column_id.is_empty() => object(vec![
                    ("provider", string(&column.provider)),
                    ("model", string(&column.model_id)),
                    ("endpoint", string(&column.endpoint)),
                    ("column_id", string(&column.id)),
                ]),
                _ => JsonValue::Null,
            };
            let pass_rate = match pick.pass_rate {
                // The cell's `real` value, widened on the way in: exact as f32.
                #[allow(clippy::cast_possible_truncation)]
                Some(rate) => f32_number(rate as f32)?,
                None => JsonValue::Null,
            };
            bands.push(object(vec![
                ("band", string(&pick.band)),
                (
                    "tiers",
                    JsonValue::Array(
                        pick.band_tiers
                            .iter()
                            .map(|tier| JsonValue::Number(tier.to_string()))
                            .collect(),
                    ),
                ),
                ("column", column),
                ("ceiling", JsonValue::Number(pick.ceiling.to_string())),
                ("pass_rate", pass_rate),
                ("clears_band", JsonValue::Bool(pick.clears_band)),
            ]));
        }
        policies.push(object(vec![
            ("capability", string(capability)),
            ("bands", JsonValue::Array(bands)),
        ]));
    }

    // QUIRK: the tenant's bar only for a single capability with a weights row.
    let quality_bar = if capabilities.len() == 1 {
        bars.get(capabilities[0])
            .copied()
            .unwrap_or(DEFAULT_QUALITY_BAR)
    } else {
        DEFAULT_QUALITY_BAR
    };
    let matrix_version = match corpus::get_active_corpus_version(pool).await {
        Ok(version) => version.version,
        Err(error) if is_not_found(&error) => String::new(),
        Err(error) => return Err(error.into()),
    };
    let buffer = gojson::encode_f64(buffer).map_err(Failure::Internal)?;
    let body = object(vec![
        ("pool", string(&resolved.name)),
        ("quality_bar", f32_number(quality_bar)?),
        ("buffer", JsonValue::Number(buffer)),
        ("policies", JsonValue::Array(policies)),
        ("matrix_version", string(&matrix_version)),
    ]);
    let mut text = gojson::marshal(&body);
    text.push('\n');
    Ok(text)
}

/// `capability` and `pool` only: another name is `unknown query parameter`. A repeated
/// name keeps its first value; an empty value means not supplied.
fn parse_query(query: &str) -> Result<(String, String), ApiError> {
    let mut capability = None;
    let mut pool = None;
    for (key, value) in crate::query::pairs(query) {
        let slot = match key.as_str() {
            "capability" => &mut capability,
            "pool" => &mut pool,
            other => {
                return Err(ApiError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    ErrorCode::InvalidRequest,
                    format!("unknown query parameter {}", quote(other)),
                )
                .detail("parameter", other));
            }
        };
        slot.get_or_insert(value);
    }
    Ok((capability.unwrap_or_default(), pool.unwrap_or_default()))
}

/// The newest aggregate rows by (column, capability, dimension), keyed by tier.
struct Cells<'a>(HashMap<(&'a str, &'a str, &'a str), BTreeMap<u8, &'a Aggregate>>);

impl<'a> Cells<'a> {
    fn new(rows: &'a [Aggregate]) -> Self {
        let mut map: HashMap<(&str, &str, &str), BTreeMap<u8, &Aggregate>> = HashMap::new();
        for row in rows {
            let Ok(tier) = u8::try_from(row.difficulty_tier) else {
                continue;
            };
            map.entry((
                row.model_column_id.as_str(),
                row.capability.as_str(),
                row.dimension.as_str(),
            ))
            .or_default()
            .insert(tier, row);
        }
        Self(map)
    }

    fn by_tier(
        &self,
        column: &str,
        capability: &str,
        dimension: &str,
    ) -> BTreeMap<u8, &'a Aggregate> {
        self.0
            .get(&(column, capability, dimension))
            .cloned()
            .unwrap_or_default()
    }
}

/// Where a column's pass-rate cells come from (spec §4.2).
fn quality_source(column: &ModelColumn) -> &str {
    column
        .quality_source_column_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .unwrap_or(&column.id)
}

/// A Go `float32` on the wire.
fn f32_number(value: f32) -> Result<JsonValue, Failure> {
    gojson::encode_f32(value)
        .map(JsonValue::Number)
        .map_err(Failure::Internal)
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
