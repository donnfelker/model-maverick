//! Tenant configuration: `GET|PUT /v1/config/{pools,weights,constraints}` (spec §9.7;
//! reference Part 5 §6.1 to §6.3; Part 4 §1.4 and §1.5).
//!
//! A PUT is a full replace in one transaction: decode (Go-strict), the handler's required
//! fields, the policy validation, then (pools and constraints) the G4 simulation against
//! the stored state read in the same transaction. Only the first violation is answered,
//! and a rejected PUT writes nothing. The internal seam's
//! `PUT /internal/v1/tenants/{id}/{pools|weights|constraints}` calls [`put`] too, so no
//! caller can store a configuration a tenant could not (reference §7.6).

use std::collections::{BTreeMap, HashSet};
use std::str::FromStr;

use axum::body::Body;
use axum::extract::{Extension, Request, State};
use axum::http::{Method, Response, StatusCode};
use chrono::Utc;
use maverick_domain::gojson::{self, JsonValue, match_field};
use maverick_domain::ids;
use maverick_domain::policy::{
    self, ConfigError, ConfigErrorCode, Pool, StandingConstraints, WeightRow,
};
use maverick_store::tenancy::{self, TenantConstraint, TenantWeight};
use maverick_store::{Decimal, catalog, is_not_found, numeric_to_f64, simulation};
use sqlx::{PgConnection, PgPool};

use crate::strict::{self, invalid, malformed, required_field};
use crate::{ApiError, ApiState, ErrorCode, Identity, internal_error, not_found, read_body};

/// The default `quality_bar` of a weights row that names none.
const DEFAULT_QUALITY_BAR: f32 = 0.8;

/// A tenant configuration resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resource {
    Pools,
    Weights,
    Constraints,
}

impl Resource {
    fn name(self) -> &'static str {
        match self {
            Self::Pools => "pools",
            Self::Weights => "weights",
            Self::Constraints => "constraints",
        }
    }
}

/// A constraints payload: the standing constraints (policy filters), the tenant's
/// telemetry opt-out (D39), and its cost-aware affinity setting (D44). The last two
/// filter no column.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ConstraintsInput {
    pub standing: StandingConstraints,
    pub telemetry_opt_out: bool,
    pub affinity_cost_aware: bool,
}

/// A stored configuration after a successful replace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replaced {
    /// The response body: the stored resource, compact JSON and one `\n`.
    pub body: String,
    /// How many pools or weights rows were stored (1 for constraints), for audit lines.
    pub count: usize,
}

/// Why a configuration read or write failed.
#[derive(Debug)]
pub enum ConfigFailure {
    /// A request the API refuses, with its envelope. Nothing was written.
    Api(ApiError),
    /// Anything else: logged, answered `500 internal_error`.
    Internal(String),
}

impl From<ApiError> for ConfigFailure {
    fn from(error: ApiError) -> Self {
        Self::Api(error)
    }
}

impl From<sqlx::Error> for ConfigFailure {
    fn from(error: sqlx::Error) -> Self {
        Self::Internal(error.to_string())
    }
}

/// `/v1/config/pools`.
pub(crate) async fn pools_handler(
    State(api): State<ApiState>,
    Extension(identity): Extension<Identity>,
    request: Request,
) -> Response<Body> {
    handle(Resource::Pools, &api, &identity, request).await
}

/// `/v1/config/weights`.
pub(crate) async fn weights_handler(
    State(api): State<ApiState>,
    Extension(identity): Extension<Identity>,
    request: Request,
) -> Response<Body> {
    handle(Resource::Weights, &api, &identity, request).await
}

/// `/v1/config/constraints`.
pub(crate) async fn constraints_handler(
    State(api): State<ApiState>,
    Extension(identity): Extension<Identity>,
    request: Request,
) -> Response<Body> {
    handle(Resource::Constraints, &api, &identity, request).await
}

/// GET or PUT; any other method is the plain 404.
async fn handle(
    resource: Resource,
    api: &ApiState,
    identity: &Identity,
    request: Request,
) -> Response<Body> {
    let method = request.method().clone();
    if method != Method::GET && method != Method::PUT {
        return not_found();
    }
    let path = request.uri().path().to_owned();
    let result = if method == Method::PUT {
        let body = match read_body(request.into_body()).await {
            Ok(body) => body,
            Err(response) => return response,
        };
        let Some(pool) = api.pool() else {
            return internal_error();
        };
        put(resource, pool, &identity.tenant_id, &body)
            .await
            .map(|replaced| replaced.body)
    } else {
        let Some(pool) = api.pool() else {
            return internal_error();
        };
        get(resource, pool, &identity.tenant_id).await
    };
    match result {
        Ok(body) => crate::response_with_json(StatusCode::OK, body.into_bytes()),
        Err(ConfigFailure::Api(error)) => error.into_response(),
        Err(ConfigFailure::Internal(error)) => {
            tracing::error!(
                method = %method,
                path = %path,
                error = %error,
                "config handler failure"
            );
            internal_error()
        }
    }
}

/// The stored resource of a tenant, as the GET body.
pub async fn get(
    resource: Resource,
    db: &PgPool,
    tenant_id: &str,
) -> Result<String, ConfigFailure> {
    Ok(match resource {
        Resource::Pools => pools_body(&tenancy::list_pools_by_tenant(db, tenant_id).await?),
        Resource::Weights => {
            weights_body(&tenancy::list_tenant_weights_by_tenant(db, tenant_id).await?)?
        }
        Resource::Constraints => {
            let row = match tenancy::get_tenant_constraints(db, tenant_id).await {
                Ok(row) => Some(row),
                Err(error) if is_not_found(&error) => None,
                Err(error) => return Err(error.into()),
            };
            constraints_body(row.as_ref())?
        }
    })
}

/// Validates and fully replaces one resource from a request body.
pub async fn put(
    resource: Resource,
    db: &PgPool,
    tenant_id: &str,
    body: &[u8],
) -> Result<Replaced, ConfigFailure> {
    match resource {
        Resource::Pools => replace_pools(db, tenant_id, &decode_pools(body)?).await,
        Resource::Weights => replace_weights(db, tenant_id, &decode_weights(body)?).await,
        Resource::Constraints => {
            replace_constraints(db, tenant_id, &decode_constraints(body)?).await
        }
    }
}

// ---- decoding ----

/// The pools of a `{"pools":[...]}` body, after the handler's required-field checks:
/// `pools` present, then per entry `name`, then `model_column_ids`.
pub fn decode_pools(body: &[u8]) -> Result<Vec<Pool>, ApiError> {
    const STRUCT: &str = "PoolsRequest";
    let value = strict::body_value(body)?;
    let entries = (|| -> Result<Option<Vec<PoolInput>>, String> {
        let mut entries = None;
        let Some(members) = strict::members(&value, STRUCT, "")? else {
            return Ok(None);
        };
        for (key, value) in members {
            match match_field(key, &["pools"]) {
                Some(_) => {
                    entries = strict::items(value, STRUCT, "pools", "[]api.PoolInput")?
                        .map(|items| items.iter().map(pool_input).collect::<Result<_, _>>())
                        .transpose()?;
                }
                None => return Err(strict::unknown(key)),
            }
        }
        Ok(entries)
    })()
    .map_err(malformed)?;

    let Some(entries) = entries else {
        return Err(invalid("pools is required"));
    };
    entries
        .into_iter()
        .enumerate()
        .map(|(i, entry)| {
            let name = entry
                .name
                .ok_or_else(|| required_field(&format!("pools[{i}].name")))?;
            let model_column_ids = entry
                .model_column_ids
                .ok_or_else(|| required_field(&format!("pools[{i}].model_column_ids")))?;
            Ok(Pool {
                name,
                model_column_ids,
                is_default: entry.is_default,
            })
        })
        .collect()
}

#[derive(Default)]
struct PoolInput {
    name: Option<String>,
    model_column_ids: Option<Vec<String>>,
    is_default: bool,
}

fn pool_input(value: &JsonValue) -> Result<PoolInput, String> {
    const STRUCT: &str = "PoolInput";
    let mut out = PoolInput::default();
    let Some(members) = strict::members(value, STRUCT, "pools")? else {
        return Ok(out);
    };
    for (key, value) in members {
        match match_field(key, &["name", "model_column_ids", "is_default"]) {
            Some("name") => out.name = strict::optional_string(value, STRUCT, "pools.name")?,
            Some("model_column_ids") => {
                out.model_column_ids = strict::strings(value, STRUCT, "pools.model_column_ids")?;
            }
            Some(_) => strict::boolean(value, STRUCT, "pools.is_default", &mut out.is_default)?,
            None => return Err(strict::unknown(key)),
        }
    }
    Ok(out)
}

/// The rows of a `{"weights":[...]}` body, after the handler's required-field checks:
/// `weights` present, then per entry `w_quality`, `w_cost`, `w_latency`. Numbers round to
/// the nearest float32; `quality_bar` defaults to 0.8.
pub fn decode_weights(body: &[u8]) -> Result<Vec<WeightRow>, ApiError> {
    const STRUCT: &str = "WeightsRequest";
    let value = strict::body_value(body)?;
    let entries = (|| -> Result<Option<Vec<WeightInput>>, String> {
        let mut entries = None;
        let Some(members) = strict::members(&value, STRUCT, "")? else {
            return Ok(None);
        };
        for (key, value) in members {
            match match_field(key, &["weights"]) {
                Some(_) => {
                    entries = strict::items(value, STRUCT, "weights", "[]api.WeightInput")?
                        .map(|items| items.iter().map(weight_input).collect::<Result<_, _>>())
                        .transpose()?;
                }
                None => return Err(strict::unknown(key)),
            }
        }
        Ok(entries)
    })()
    .map_err(malformed)?;

    let Some(entries) = entries else {
        return Err(invalid("weights is required"));
    };
    entries
        .into_iter()
        .enumerate()
        .map(|(i, entry)| {
            let field = |name: &str| required_field(&format!("weights[{i}].{name}"));
            Ok(WeightRow {
                w_quality: entry.w_quality.ok_or_else(|| field("w_quality"))?,
                w_cost: entry.w_cost.ok_or_else(|| field("w_cost"))?,
                w_latency: entry.w_latency.ok_or_else(|| field("w_latency"))?,
                quality_bar: entry.quality_bar.unwrap_or(DEFAULT_QUALITY_BAR),
                capability: entry.capability,
            })
        })
        .collect()
}

#[derive(Default)]
struct WeightInput {
    capability: String,
    w_quality: Option<f32>,
    w_cost: Option<f32>,
    w_latency: Option<f32>,
    quality_bar: Option<f32>,
}

fn weight_input(value: &JsonValue) -> Result<WeightInput, String> {
    const STRUCT: &str = "WeightInput";
    let mut out = WeightInput::default();
    let Some(members) = strict::members(value, STRUCT, "weights")? else {
        return Ok(out);
    };
    for (key, value) in members {
        match match_field(
            key,
            &[
                "capability",
                "w_quality",
                "w_cost",
                "w_latency",
                "quality_bar",
            ],
        ) {
            Some("capability") => {
                strict::string(value, STRUCT, "weights.capability", &mut out.capability)?;
            }
            Some("w_quality") => {
                out.w_quality = strict::float32(value, STRUCT, "weights.w_quality")?
            }
            Some("w_cost") => out.w_cost = strict::float32(value, STRUCT, "weights.w_cost")?,
            Some("w_latency") => {
                out.w_latency = strict::float32(value, STRUCT, "weights.w_latency")?
            }
            Some(_) => out.quality_bar = strict::float32(value, STRUCT, "weights.quality_bar")?,
            None => return Err(strict::unknown(key)),
        }
    }
    Ok(out)
}

/// The constraints of a `{"constraints":{...}}` body. Missing keys are null or false, so
/// `{"constraints":{}}` clears everything, `telemetry_opt_out` (D39) and
/// `affinity_cost_aware` (D44) included.
pub fn decode_constraints(body: &[u8]) -> Result<ConstraintsInput, ApiError> {
    const STRUCT: &str = "ConstraintsRequest";
    const INNER: &str = "ConstraintsInput";
    let value = strict::body_value(body)?;
    let constraints = (|| -> Result<Option<ConstraintsInput>, String> {
        let mut out = None;
        let Some(members) = strict::members(&value, STRUCT, "")? else {
            return Ok(None);
        };
        for (key, value) in members {
            if match_field(key, &["constraints"]).is_none() {
                return Err(strict::unknown(key));
            }
            let Some(fields) = strict::members(value, INNER, "constraints")? else {
                out = None;
                continue;
            };
            let mut input: ConstraintsInput = out.unwrap_or_default();
            let constraints = &mut input.standing;
            for (key, value) in fields {
                match match_field(
                    key,
                    &[
                        "max_cost_per_run",
                        "max_latency_ms",
                        "zero_retention",
                        "telemetry_opt_out",
                        "affinity_cost_aware",
                    ],
                ) {
                    Some("max_cost_per_run") => {
                        constraints.max_cost_per_run =
                            strict::float64(value, INNER, "constraints.max_cost_per_run")?;
                    }
                    Some("max_latency_ms") => {
                        constraints.max_latency_ms = strict::integer(
                            value,
                            INNER,
                            "constraints.max_latency_ms",
                            "int32",
                            i64::from(i32::MIN),
                            i64::from(i32::MAX),
                        )?
                        .map(|ms| i32::try_from(ms).expect("range checked"));
                    }
                    Some("zero_retention") => strict::boolean(
                        value,
                        INNER,
                        "constraints.zero_retention",
                        &mut constraints.zero_retention,
                    )?,
                    // D39.
                    Some("telemetry_opt_out") => strict::boolean(
                        value,
                        INNER,
                        "constraints.telemetry_opt_out",
                        &mut input.telemetry_opt_out,
                    )?,
                    // D44.
                    Some(_) => strict::boolean(
                        value,
                        INNER,
                        "constraints.affinity_cost_aware",
                        &mut input.affinity_cost_aware,
                    )?,
                    None => return Err(strict::unknown(key)),
                }
            }
            out = Some(input);
        }
        Ok(out)
    })()
    .map_err(malformed)?;
    constraints.ok_or_else(|| invalid("constraints is required"))
}

// ---- replace ----

/// Validates pools against the stored constraints and replaces them (reference Part 4
/// §1.4): shapes, then the names the model grammar reserves (D45), then unknown columns,
/// then G4.
pub async fn replace_pools(
    db: &PgPool,
    tenant_id: &str,
    pools: &[Pool],
) -> Result<Replaced, ConfigFailure> {
    if let Some(first) = policy::validate_pool_shapes(pools)
        .into_iter()
        .chain(policy::validate_pool_names(pools))
        .next()
    {
        return Err(config_error(first).into());
    }
    let mut tx = db.begin().await?;
    let constraints = stored_constraints(&mut tx, tenant_id)
        .await?
        .map_or_else(StandingConstraints::default, |row| standing(&row));
    let ids = referenced_ids(pools);
    let state = simulation::load_state(&mut tx, &ids).await?;
    let known: HashSet<String> = catalog::list_model_columns_by_ids(&mut *tx, &ids)
        .await?
        .into_iter()
        .map(|column| column.id)
        .collect();
    for pool in pools {
        if let Some(entry) = pool.model_column_ids.iter().find(|id| !known.contains(*id)) {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                ErrorCode::UnknownModel,
                format!(
                    "model_column_ids entry {} in pool {} is not in the model catalog",
                    maverick_domain::gofmt::quote(entry),
                    maverick_domain::gofmt::quote(&pool.name)
                ),
            )
            .detail("entry", entry.clone())
            .detail("pool", pool.name.clone())
            .into());
        }
    }
    if let Some(first) = policy::simulate(pools, &constraints, &state, Utc::now())
        .into_iter()
        .next()
    {
        return Err(config_error(first).into());
    }

    tenancy::delete_pools_by_tenant(&mut *tx, tenant_id).await?;
    for pool in pools {
        tenancy::create_pool(
            &mut *tx,
            &ids::new(ids::prefix::POOL),
            tenant_id,
            &pool.name,
            &pool.model_column_ids,
            pool.is_default,
        )
        .await?;
    }
    let stored = tenancy::list_pools_by_tenant(&mut *tx, tenant_id).await?;
    tx.commit().await?;
    Ok(Replaced {
        body: pools_body(&stored),
        count: stored.len(),
    })
}

/// Validates weights and replaces them. Weights alone are never simulated.
pub async fn replace_weights(
    db: &PgPool,
    tenant_id: &str,
    weights: &[WeightRow],
) -> Result<Replaced, ConfigFailure> {
    if let Some(first) = policy::validate_weights(weights).into_iter().next() {
        return Err(config_error(first).into());
    }
    let mut tx = db.begin().await?;
    tenancy::delete_tenant_weights_by_tenant(&mut *tx, tenant_id).await?;
    for row in weights {
        tenancy::create_tenant_weight(
            &mut *tx,
            tenant_id,
            &row.capability,
            row.w_quality,
            row.w_cost,
            row.w_latency,
            row.quality_bar,
        )
        .await?;
    }
    let stored = tenancy::list_tenant_weights_by_tenant(&mut *tx, tenant_id).await?;
    tx.commit().await?;
    Ok(Replaced {
        body: weights_body(&stored)?,
        count: stored.len(),
    })
}

/// Validates constraints against the stored pools (G4, including D1 zero retention) and
/// upserts them with the payload's `telemetry_opt_out` (D39) and `affinity_cost_aware`
/// (D44), which G4 ignores.
pub async fn replace_constraints(
    db: &PgPool,
    tenant_id: &str,
    input: &ConstraintsInput,
) -> Result<Replaced, ConfigFailure> {
    let constraints = &input.standing;
    if let Some(first) = policy::validate_constraint_values(constraints)
        .into_iter()
        .next()
    {
        return Err(config_error(first).into());
    }
    let mut tx = db.begin().await?;
    let pools: Vec<Pool> = tenancy::list_pools_by_tenant(&mut *tx, tenant_id)
        .await?
        .into_iter()
        .map(|pool| Pool {
            name: pool.name,
            model_column_ids: pool.model_column_ids,
            is_default: pool.is_default,
        })
        .collect();
    let state = simulation::load_state(&mut tx, &referenced_ids(&pools)).await?;
    if let Some(first) = policy::simulate(&pools, constraints, &state, Utc::now())
        .into_iter()
        .next()
    {
        return Err(config_error(first).into());
    }
    let max_cost = constraints
        .max_cost_per_run
        .map(|cost| {
            // Go writes `strconv.FormatFloat(v, 'f', -1, 64)`: plain shortest digits.
            Decimal::from_str(&format!("{cost}")).map_err(|error| {
                ConfigFailure::Internal(format!("max_cost_per_run {cost}: {error}"))
            })
        })
        .transpose()?;
    let stored = tenancy::upsert_tenant_constraints(
        &mut *tx,
        tenant_id,
        max_cost,
        constraints.max_latency_ms,
        constraints.zero_retention,
        input.telemetry_opt_out,
        input.affinity_cost_aware,
    )
    .await?;
    tx.commit().await?;
    Ok(Replaced {
        body: constraints_body(Some(&stored))?,
        count: 1,
    })
}

async fn stored_constraints(
    conn: &mut PgConnection,
    tenant_id: &str,
) -> Result<Option<TenantConstraint>, sqlx::Error> {
    match tenancy::get_tenant_constraints(&mut *conn, tenant_id).await {
        Ok(row) => Ok(Some(row)),
        Err(error) if is_not_found(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

fn standing(row: &TenantConstraint) -> StandingConstraints {
    StandingConstraints {
        max_cost_per_run: row.max_cost_per_run.map(numeric_to_f64),
        max_latency_ms: row.max_latency_ms,
        zero_retention: row.zero_retention,
    }
}

/// Every column id of the pools, once, in pool then entry order.
fn referenced_ids(pools: &[Pool]) -> Vec<String> {
    let mut seen = HashSet::new();
    pools
        .iter()
        .flat_map(|pool| &pool.model_column_ids)
        .filter(|id| seen.insert(id.as_str()))
        .cloned()
        .collect()
}

/// The API envelope of a policy violation. Every config violation is a 422.
fn config_error(error: ConfigError) -> ApiError {
    let code = match error.code {
        ConfigErrorCode::InvalidRequest => ErrorCode::InvalidRequest,
        ConfigErrorCode::UnknownModel => ErrorCode::UnknownModel,
        ConfigErrorCode::PoolWouldBeEmpty => ErrorCode::PoolWouldBeEmpty,
    };
    let mut api = ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, code, error.message);
    if let serde_json::Value::Object(details) = error.details {
        api.details = details.into_iter().collect::<BTreeMap<_, _>>();
    }
    api
}

// ---- bodies ----

fn object(members: Vec<(&str, JsonValue)>) -> JsonValue {
    JsonValue::Object(
        members
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    )
}

fn document(key: &str, value: JsonValue) -> String {
    let mut body = gojson::marshal(&object(vec![(key, value)]));
    body.push('\n');
    body
}

/// `{"pools":[{"name","model_column_ids","is_default"}]}`, in stored (name) order.
fn pools_body(pools: &[tenancy::Pool]) -> String {
    let items = pools
        .iter()
        .map(|pool| {
            object(vec![
                ("name", JsonValue::String(pool.name.clone())),
                (
                    "model_column_ids",
                    JsonValue::Array(
                        pool.model_column_ids
                            .iter()
                            .map(|id| JsonValue::String(id.clone()))
                            .collect(),
                    ),
                ),
                ("is_default", JsonValue::Bool(pool.is_default)),
            ])
        })
        .collect();
    document(Resource::Pools.name(), JsonValue::Array(items))
}

/// `{"weights":[{"capability","w_quality","w_cost","w_latency","quality_bar"}]}`, with
/// float32 numbers, in stored (capability) order.
fn weights_body(rows: &[TenantWeight]) -> Result<String, ConfigFailure> {
    let f32_value = |x: f32| {
        gojson::encode_f32(x)
            .map(JsonValue::Number)
            .map_err(ConfigFailure::Internal)
    };
    let mut items = Vec::with_capacity(rows.len());
    for row in rows {
        items.push(object(vec![
            ("capability", JsonValue::String(row.capability.clone())),
            ("w_quality", f32_value(row.w_quality)?),
            ("w_cost", f32_value(row.w_cost)?),
            ("w_latency", f32_value(row.w_latency)?),
            ("quality_bar", f32_value(row.quality_bar)?),
        ]));
    }
    Ok(document(Resource::Weights.name(), JsonValue::Array(items)))
}

/// `{"constraints":{"max_cost_per_run","max_latency_ms","zero_retention",
/// "telemetry_opt_out","affinity_cost_aware"}}` (Go's three keys, then D39 and D44); no
/// row is all null and false.
fn constraints_body(row: Option<&TenantConstraint>) -> Result<String, ConfigFailure> {
    let max_cost = match row.and_then(|row| row.max_cost_per_run) {
        Some(cost) => JsonValue::Number(
            gojson::encode_f64(numeric_to_f64(cost)).map_err(ConfigFailure::Internal)?,
        ),
        None => JsonValue::Null,
    };
    let max_latency = row
        .and_then(|row| row.max_latency_ms)
        .map_or(JsonValue::Null, |ms| JsonValue::Number(ms.to_string()));
    let zero_retention = row.is_some_and(|row| row.zero_retention);
    Ok(document(
        Resource::Constraints.name(),
        object(vec![
            ("max_cost_per_run", max_cost),
            ("max_latency_ms", max_latency),
            ("zero_retention", JsonValue::Bool(zero_retention)),
            // D39.
            (
                "telemetry_opt_out",
                JsonValue::Bool(row.is_some_and(|row| row.telemetry_opt_out)),
            ),
            // D44.
            (
                "affinity_cost_aware",
                JsonValue::Bool(row.is_some_and(|row| row.affinity_cost_aware)),
            ),
        ]),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message<T: std::fmt::Debug>(result: Result<T, ApiError>) -> (String, serde_json::Value) {
        let error = result.unwrap_err();
        (
            error.message,
            serde_json::to_value(&error.details).expect("details"),
        )
    }

    #[test]
    fn pools_required_fields_in_order() {
        let (m, d) = message(decode_pools(b"{}"));
        assert_eq!(
            (m.as_str(), d),
            ("pools is required", serde_json::json!({}))
        );
        let (m, _) = message(decode_pools(br#"{"pools":null}"#));
        assert_eq!(m, "pools is required");
        let (m, d) = message(decode_pools(br#"{"pools":[{"model_column_ids":[]}]}"#));
        assert_eq!(
            (m.as_str(), d),
            (
                "pools[0].name is required",
                serde_json::json!({"field": "pools[0].name"})
            )
        );
        let (m, _) = message(decode_pools(
            br#"{"pools":[{"name":"a","model_column_ids":["x"]},{"name":"b"}]}"#,
        ));
        assert_eq!(m, "pools[1].model_column_ids is required");
        let (m, _) = message(decode_pools(br#"{"pools":[{"name":"a","color":"red"}]}"#));
        assert_eq!(m, "malformed request body: json: unknown field \"color\"");
        let (m, _) = message(decode_pools(br#"{"surprise":1}"#));
        assert_eq!(
            m,
            "malformed request body: json: unknown field \"surprise\""
        );
        let pools = decode_pools(
            br#"{"POOLS":[{"Name":"a","model_column_ids":["x","y"],"is_default":true},{"name":"","model_column_ids":[]}]}"#,
        )
        .unwrap();
        assert_eq!(
            pools,
            [
                Pool {
                    name: "a".into(),
                    model_column_ids: vec!["x".into(), "y".into()],
                    is_default: true
                },
                Pool {
                    name: String::new(),
                    model_column_ids: Vec::new(),
                    is_default: false
                },
            ]
        );
    }

    #[test]
    fn weights_required_fields_and_float32() {
        let (m, d) = message(decode_weights(
            br#"{"weights":[{"capability":"code_generation","w_quality":0.8,"w_cost":0.1}]}"#,
        ));
        assert_eq!(
            (m.as_str(), d),
            (
                "weights[0].w_latency is required",
                serde_json::json!({"field": "weights[0].w_latency"})
            )
        );
        let (m, _) = message(decode_weights(b"{}"));
        assert_eq!(m, "weights is required");
        let rows = decode_weights(
            br#"{"weights":[{"capability":"x","w_quality":0.5,"w_cost":0.35,"w_latency":0.1505}]}"#,
        )
        .unwrap();
        assert_eq!(rows[0].w_latency, 0.1505f32);
        assert_eq!(rows[0].quality_bar, DEFAULT_QUALITY_BAR);
        assert_eq!(rows[0].capability, "x");
    }

    #[test]
    fn constraints_decode_and_clear() {
        let (m, d) = message(decode_constraints(b"{}"));
        assert_eq!(
            (m.as_str(), d),
            ("constraints is required", serde_json::json!({}))
        );
        let (m, _) = message(decode_constraints(br#"{"constraints":null}"#));
        assert_eq!(m, "constraints is required");
        assert_eq!(
            decode_constraints(br#"{"constraints":{}}"#).unwrap(),
            ConstraintsInput::default()
        );
        assert_eq!(
            decode_constraints(
                br#"{"constraints":{"max_cost_per_run":0.25,"max_latency_ms":4000,"zero_retention":true}}"#
            )
            .unwrap(),
            ConstraintsInput {
                standing: StandingConstraints {
                    max_cost_per_run: Some(0.25),
                    max_latency_ms: Some(4000),
                    zero_retention: true,
                },
                telemetry_opt_out: false,
                affinity_cost_aware: false,
            }
        );
        // D44: the same rules as D39.
        assert!(
            decode_constraints(br#"{"constraints":{"Affinity_Cost_Aware":true}}"#)
                .unwrap()
                .affinity_cost_aware
        );
        let both = decode_constraints(
            br#"{"constraints":{"telemetry_opt_out":true,"affinity_cost_aware":false}}"#,
        )
        .unwrap();
        assert!(both.telemetry_opt_out && !both.affinity_cost_aware);
        let (m, _) = message(decode_constraints(
            br#"{"constraints":{"affinity_cost_aware":1}}"#,
        ));
        assert!(
            m.starts_with("malformed request body: json: cannot unmarshal number 1 into Go struct field ConstraintsInput.constraints.affinity_cost_aware of type bool"),
            "{m}"
        );
        // D39: Go-style field matching; null leaves false.
        assert!(
            decode_constraints(br#"{"constraints":{"Telemetry_Opt_Out":true}}"#)
                .unwrap()
                .telemetry_opt_out
        );
        assert!(
            !decode_constraints(br#"{"constraints":{"telemetry_opt_out":null}}"#)
                .unwrap()
                .telemetry_opt_out
        );
        let (m, _) = message(decode_constraints(
            br#"{"constraints":{"telemetry_opt_out":"yes"}}"#,
        ));
        assert!(
            m.starts_with("malformed request body: json: cannot unmarshal string into Go struct field ConstraintsInput.constraints.telemetry_opt_out of type bool"),
            "{m}"
        );
        let (m, _) = message(decode_constraints(br#"{"constraints":{"max_tokens":5}}"#));
        assert_eq!(
            m,
            "malformed request body: json: unknown field \"max_tokens\""
        );
        let (m, _) = message(decode_constraints(
            br#"{"constraints":{"max_latency_ms":1.5}}"#,
        ));
        assert!(
            m.starts_with("malformed request body: json: cannot unmarshal number 1.5"),
            "{m}"
        );
    }

    #[test]
    fn bodies_use_go_number_forms() {
        let weights = [TenantWeight {
            tenant_id: "t".into(),
            capability: "structured_extraction".into(),
            w_quality: 0.5,
            w_cost: 0.35,
            w_latency: 0.15,
            quality_bar: 0.9,
        }];
        assert_eq!(
            weights_body(&weights).unwrap(),
            "{\"weights\":[{\"capability\":\"structured_extraction\",\"w_quality\":0.5,\
             \"w_cost\":0.35,\"w_latency\":0.15,\"quality_bar\":0.9}]}\n"
        );
        assert_eq!(weights_body(&[]).unwrap(), "{\"weights\":[]}\n");
        assert_eq!(pools_body(&[]), "{\"pools\":[]}\n");
        assert_eq!(
            constraints_body(None).unwrap(),
            "{\"constraints\":{\"max_cost_per_run\":null,\"max_latency_ms\":null,\
             \"zero_retention\":false,\"telemetry_opt_out\":false,\"affinity_cost_aware\":false}}\n"
        );
        assert_eq!(
            referenced_ids(&[
                Pool {
                    name: "a".into(),
                    model_column_ids: vec!["x".into(), "y".into()],
                    is_default: true
                },
                Pool {
                    name: "b".into(),
                    model_column_ids: vec!["y".into(), "z".into()],
                    is_default: false
                },
            ]),
            ["x", "y", "z"]
        );
    }
}
