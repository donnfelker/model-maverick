//! The internal provisioning seam, `/internal/v1` on the internal listener (reference
//! Part 5 §7 and §13.12; spec §13). It covers tenants, API keys, tenant config, limits
//! (D4), the alert feed, the model catalog, and corpus activation. Internal callers are
//! root: nothing is tenant scoped. A tenant-scoped route resolves `{id}` before it reads
//! the body. Every write logs its audit line after it succeeds; catalog changes log their
//! own `catalog_changes` lines.

use std::fmt;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Extension, Path, Request, State};
use axum::http::{Method, Response, StatusCode};
use chrono::{DateTime, Duration, NaiveDate, NaiveTime, Utc};
use maverick_alerts::{AlertKind, Severity, insert_alert};
use maverick_config::Config;
use maverick_domain::catalog::{RetirementBlocked, retention_window_error};
use maverick_domain::gofmt::quote;
use maverick_domain::gojson::{self, JsonValue, match_field};
use maverick_domain::pricing::parse_pricing;
use maverick_domain::{ids, keys};
use maverick_store::catalog::{
    self, CatalogChange, ModelColumn, ModelColumnUpdate, NewModelColumn, log_change,
};
use maverick_store::{JsonRawValue, corpus, is_not_found, simulation, sqlstate, tenancy};
use sqlx::PgPool;

use crate::config::{self, ConfigFailure, Resource};
use crate::strict::{self, invalid, malformed, required_field};
use crate::{
    ApiError, ApiState, ErrorCode, InternalActor, audit, escape_go_json, internal_error, not_found,
    read_body, response_with_json,
};

/// The providers a column may name.
const PROVIDERS: [&str; 4] = ["anthropic", "openai", "google", "xai"];
/// The endpoints a column may name (no DB CHECK; enforced here).
const ENDPOINTS: [&str; 5] = ["direct", "vertex", "bedrock", "azure", "openrouter"];
/// The lifecycle states a column update may set.
const STATUSES: [&str; 4] = ["cold_start", "active", "deprecated", "retired"];
/// The retention modes (spec §4.5).
const RETENTION_MODES: [&str; 3] = ["unknown", "standard", "zero"];
/// The surface-neutral parameters a column may constrain (D29; `maverick_core::normalize`).
const KNOWN_PARAMS: [&str; 1] = ["reasoning_effort"];
/// `execution_aliases` limits.
const MAX_ALIASES: usize = 10;
const MAX_ALIAS_RUNES: usize = 200;

/// What the seam needs besides the pool.
#[derive(Clone)]
pub struct InternalSettings {
    /// `catalog.cold_start_nights`: a new column's dense-run window.
    pub cold_start_nights: u32,
    /// `classifier.daily_quota`, the effective quota without a tenant override (D4).
    pub default_classification_quota: u32,
    /// Today's date for `cold_start_until` and the guard's clock (tests pin it).
    pub clock: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>,
}

impl InternalSettings {
    pub fn from_config(config: &Config) -> Self {
        InternalSettings {
            cold_start_nights: config.catalog_cold_start_nights,
            default_classification_quota: config.classifier_daily_quota,
            ..Self::default()
        }
    }

    fn now(&self) -> DateTime<Utc> {
        (self.clock)()
    }
}

impl Default for InternalSettings {
    fn default() -> Self {
        InternalSettings {
            cold_start_nights: 5,
            default_classification_quota: 1000,
            clock: Arc::new(Utc::now),
        }
    }
}

impl fmt::Debug for InternalSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InternalSettings")
            .field("cold_start_nights", &self.cold_start_nights)
            .field(
                "default_classification_quota",
                &self.default_classification_quota,
            )
            .finish_non_exhaustive()
    }
}

// ---- responses ----

type Handled = Result<Response<Body>, Failure>;

/// Why a handler did not answer 2xx.
enum Failure {
    Api(ApiError),
    /// A prepared error response (a body-read failure, or the retirement guard's ordered
    /// details).
    Response(Box<Response<Body>>),
    Internal(String),
}

impl From<ApiError> for Failure {
    fn from(error: ApiError) -> Self {
        Failure::Api(error)
    }
}

impl From<sqlx::Error> for Failure {
    fn from(error: sqlx::Error) -> Self {
        Failure::Internal(error.to_string())
    }
}

impl From<ConfigFailure> for Failure {
    fn from(failure: ConfigFailure) -> Self {
        match failure {
            ConfigFailure::Api(error) => Failure::Api(error),
            ConfigFailure::Internal(error) => Failure::Internal(error),
        }
    }
}

fn respond(route: &str, result: Handled) -> Response<Body> {
    match result {
        Ok(response) => response,
        Err(Failure::Api(error)) => error.into_response(),
        Err(Failure::Response(response)) => *response,
        Err(Failure::Internal(error)) => {
            tracing::error!(route, error = %error, "internal seam failure");
            internal_error()
        }
    }
}

fn json(status: StatusCode, value: &JsonValue) -> Response<Body> {
    let mut body = gojson::marshal(value);
    body.push('\n');
    response_with_json(status, body.into_bytes())
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

fn optional_string(value: Option<&str>) -> JsonValue {
    value.map_or(JsonValue::Null, string)
}

fn time(at: DateTime<Utc>) -> JsonValue {
    JsonValue::String(crate::health::rfc3339_nano(at))
}

fn optional_time(at: Option<DateTime<Utc>>) -> JsonValue {
    at.map_or(JsonValue::Null, time)
}

fn date(day: Option<NaiveDate>) -> JsonValue {
    day.map_or(JsonValue::Null, |day| {
        JsonValue::String(day.format("%Y-%m-%d").to_string())
    })
}

/// A stored jsonb value as JSON (Go wrote `json.RawMessage`, compacted).
fn stored(raw: &JsonRawValue) -> Result<JsonValue, Failure> {
    gojson::unmarshal(raw.get()).map_err(|error| Failure::Internal(error.to_string()))
}

/// The member `key` of a `{"<key>": ...}\n` document.
fn member(document: &str, key: &str) -> Result<JsonValue, Failure> {
    match gojson::unmarshal(document.trim_end()) {
        Ok(JsonValue::Object(members)) => members
            .into_iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value)
            .ok_or_else(|| Failure::Internal(format!("no {key} in the config body"))),
        Ok(_) => Err(Failure::Internal(format!("{key} body is not an object"))),
        Err(error) => Err(Failure::Internal(error.to_string())),
    }
}

fn pool_of(api: &ApiState) -> Result<&PgPool, Failure> {
    api.pool()
        .ok_or_else(|| Failure::Internal("no database".to_owned()))
}

// ---- tenants ----

fn tenant_missing(id: &str) -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        ErrorCode::NotFound,
        format!("tenant {id} does not exist"),
    )
    .detail("tenant_id", id)
}

/// The tenant, or 404 `tenant <id> does not exist`.
async fn tenant_or_404(db: &PgPool, id: &str) -> Result<tenancy::Tenant, Failure> {
    match tenancy::get_tenant(db, id).await {
        Ok(tenant) => Ok(tenant),
        Err(error) if is_not_found(&error) => Err(tenant_missing(id).into()),
        Err(error) => Err(error.into()),
    }
}

fn api_key_json(key: &tenancy::ApiKey) -> JsonValue {
    object(vec![
        ("id", string(&key.id)),
        ("name", string(&key.name)),
        ("last4", string(&key.last4)),
        ("status", string(&key.status)),
        ("created_at", time(key.created_at)),
        ("revoked_at", optional_time(key.revoked_at)),
    ])
}

/// The tenant summary (reference §7.2): the tenant, its pools, weights, and constraints
/// exactly as the public config GETs render them, and its keys by id. Never a raw key or
/// the HMAC key.
async fn summary(db: &PgPool, tenant: &tenancy::Tenant) -> Result<JsonValue, Failure> {
    let pools = config::get(Resource::Pools, db, &tenant.id).await?;
    let weights = config::get(Resource::Weights, db, &tenant.id).await?;
    let constraints = config::get(Resource::Constraints, db, &tenant.id).await?;
    let keys = tenancy::list_api_keys_by_tenant(db, &tenant.id).await?;
    Ok(object(vec![
        (
            "tenant",
            object(vec![
                ("id", string(&tenant.id)),
                ("name", string(&tenant.name)),
                ("status", string(&tenant.status)),
                ("created_at", time(tenant.created_at)),
                ("updated_at", time(tenant.updated_at)),
            ]),
        ),
        ("pools", member(&pools, "pools")?),
        ("weights", member(&weights, "weights")?),
        ("constraints", member(&constraints, "constraints")?),
        (
            "api_keys",
            JsonValue::Array(keys.iter().map(api_key_json).collect()),
        ),
    ]))
}

/// A strict `{"<field>": string}` body; a missing or empty value is `<field> is required`.
fn decode_name(body: &[u8], strukt: &str, field: &'static str) -> Result<String, ApiError> {
    let value = strict::body_value(body)?;
    let mut out = String::new();
    (|| -> Result<(), String> {
        let Some(members) = strict::members(&value, strukt, "")? else {
            return Ok(());
        };
        for (key, value) in members {
            match match_field(key, &[field]) {
                Some(_) => strict::string(value, strukt, field, &mut out)?,
                None => return Err(strict::unknown(key)),
            }
        }
        Ok(())
    })()
    .map_err(malformed)?;
    if out.is_empty() {
        return Err(required_field(field));
    }
    Ok(out)
}

/// `POST /internal/v1/tenants` (§7.1).
pub(crate) async fn tenants(
    State(api): State<ApiState>,
    Extension(actor): Extension<InternalActor>,
    request: Request,
) -> Response<Body> {
    if request.method() != Method::POST {
        return not_found();
    }
    respond("create tenant", create_tenant(&api, &actor, request).await)
}

async fn create_tenant(api: &ApiState, actor: &InternalActor, request: Request) -> Handled {
    let body = read_body(request.into_body())
        .await
        .map_err(|response| Failure::Response(Box::new(response)))?;
    let name = decode_name(&body, "createTenantRequest", "name")?;
    let db = pool_of(api)?;
    let active: Vec<String> = catalog::list_model_columns_by_statuses(db, &["active".to_owned()])
        .await?
        .into_iter()
        .map(|column| column.id)
        .collect();
    let hmac_key: [u8; 32] = rand::random();
    let id = ids::new(ids::prefix::TENANT);
    let mut tx = db.begin().await?;
    let tenant = tenancy::create_tenant(&mut *tx, &id, &name, &hmac_key).await?;
    if !active.is_empty() {
        let pool_id = ids::new(ids::prefix::POOL);
        tenancy::create_pool(&mut *tx, &pool_id, &id, "default", &active, true).await?;
    }
    tx.commit().await?;
    audit::tenant_create(&actor.0, &id, &name, active.len());
    Ok(json(StatusCode::CREATED, &summary(db, &tenant).await?))
}

/// `GET /internal/v1/tenants/{id}` (§7.2).
pub(crate) async fn tenant(
    State(api): State<ApiState>,
    Path(id): Path<String>,
    request: Request,
) -> Response<Body> {
    if request.method() != Method::GET {
        return not_found();
    }
    respond("tenant summary", tenant_summary(&api, &id).await)
}

async fn tenant_summary(api: &ApiState, id: &str) -> Handled {
    let db = pool_of(api)?;
    let tenant = tenant_or_404(db, id).await?;
    Ok(json(StatusCode::OK, &summary(db, &tenant).await?))
}

/// `POST /internal/v1/tenants/{id}/suspend` (§7.3). No body is read.
pub(crate) async fn suspend(
    State(api): State<ApiState>,
    Extension(actor): Extension<InternalActor>,
    Path(id): Path<String>,
    request: Request,
) -> Response<Body> {
    set_status(&api, &actor, &id, request.method(), "suspended").await
}

/// `POST /internal/v1/tenants/{id}/activate` (§7.3). No body is read.
pub(crate) async fn activate(
    State(api): State<ApiState>,
    Extension(actor): Extension<InternalActor>,
    Path(id): Path<String>,
    request: Request,
) -> Response<Body> {
    set_status(&api, &actor, &id, request.method(), "active").await
}

async fn set_status(
    api: &ApiState,
    actor: &InternalActor,
    id: &str,
    method: &Method,
    status: &str,
) -> Response<Body> {
    if method != Method::POST {
        return not_found();
    }
    respond("tenant status", change_status(api, actor, id, status).await)
}

async fn change_status(api: &ApiState, actor: &InternalActor, id: &str, status: &str) -> Handled {
    let db = pool_of(api)?;
    let tenant = tenant_or_404(db, id).await?;
    let tenant = tenancy::update_tenant(db, id, &tenant.name, status).await?;
    if status == "suspended" {
        audit::tenant_suspend(&actor.0, id, status);
    } else {
        audit::tenant_activate(&actor.0, id, status);
    }
    Ok(json(StatusCode::OK, &summary(db, &tenant).await?))
}

/// `POST /internal/v1/tenants/{id}/api-keys` (§7.4): the only time the raw key is
/// returned.
pub(crate) async fn issue_key(
    State(api): State<ApiState>,
    Extension(actor): Extension<InternalActor>,
    Path(id): Path<String>,
    request: Request,
) -> Response<Body> {
    if request.method() != Method::POST {
        return not_found();
    }
    respond("issue api key", mint(&api, &actor, &id, request).await)
}

async fn mint(api: &ApiState, actor: &InternalActor, id: &str, request: Request) -> Handled {
    let db = pool_of(api)?;
    tenant_or_404(db, id).await?;
    let body = read_body(request.into_body())
        .await
        .map_err(|response| Failure::Response(Box::new(response)))?;
    let name = decode_name(&body, "issueKeyRequest", "name")?;
    let minted = keys::mint_key();
    let key_id = ids::new(ids::prefix::API_KEY);
    tenancy::create_api_key(db, &key_id, id, &minted.key_hash, &minted.last4, &name).await?;
    audit::api_key_issue(&actor.0, id, &key_id, &name, &minted.last4);
    Ok(json(
        StatusCode::CREATED,
        &object(vec![
            ("id", string(&key_id)),
            ("key", string(&minted.raw_key)),
        ]),
    ))
}

/// `DELETE /internal/v1/api-keys/{id}` (§7.5): idempotent.
pub(crate) async fn revoke_key(
    State(api): State<ApiState>,
    Extension(actor): Extension<InternalActor>,
    Path(id): Path<String>,
    request: Request,
) -> Response<Body> {
    if request.method() != Method::DELETE {
        return not_found();
    }
    respond("revoke api key", revoke(&api, &actor, &id).await)
}

async fn revoke(api: &ApiState, actor: &InternalActor, id: &str) -> Handled {
    let db = pool_of(api)?;
    let key = match tenancy::get_api_key(db, id).await {
        Ok(key) => key,
        Err(error) if is_not_found(&error) => {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                ErrorCode::NotFound,
                format!("api key {id} does not exist"),
            )
            .detail("api_key_id", id)
            .into());
        }
        Err(error) => return Err(error.into()),
    };
    let key = if key.status == "revoked" {
        key
    } else {
        tenancy::revoke_api_key(db, id).await?
    };
    audit::api_key_revoke(&actor.0, &key.tenant_id, &key.id);
    Ok(json(StatusCode::OK, &api_key_json(&key)))
}

// ---- tenant config ----

/// `PUT /internal/v1/tenants/{id}/pools` (§7.6).
pub(crate) async fn pools(
    State(api): State<ApiState>,
    Extension(actor): Extension<InternalActor>,
    Path(id): Path<String>,
    request: Request,
) -> Response<Body> {
    put_config(Resource::Pools, &api, &actor, &id, request).await
}

/// `PUT /internal/v1/tenants/{id}/weights` (§7.6).
pub(crate) async fn weights(
    State(api): State<ApiState>,
    Extension(actor): Extension<InternalActor>,
    Path(id): Path<String>,
    request: Request,
) -> Response<Body> {
    put_config(Resource::Weights, &api, &actor, &id, request).await
}

/// `PUT /internal/v1/tenants/{id}/constraints` (§7.6).
pub(crate) async fn constraints(
    State(api): State<ApiState>,
    Extension(actor): Extension<InternalActor>,
    Path(id): Path<String>,
    request: Request,
) -> Response<Body> {
    put_config(Resource::Constraints, &api, &actor, &id, request).await
}

/// The public config PUT's exact decoding, validation, envelopes, and bodies, after the
/// tenant lookup.
async fn put_config(
    resource: Resource,
    api: &ApiState,
    actor: &InternalActor,
    id: &str,
    request: Request,
) -> Response<Body> {
    if request.method() != Method::PUT {
        return not_found();
    }
    respond(
        "tenant config",
        replace(resource, api, actor, id, request).await,
    )
}

async fn replace(
    resource: Resource,
    api: &ApiState,
    actor: &InternalActor,
    id: &str,
    request: Request,
) -> Handled {
    let db = pool_of(api)?;
    tenant_or_404(db, id).await?;
    let body = read_body(request.into_body())
        .await
        .map_err(|response| Failure::Response(Box::new(response)))?;
    let replaced = config::put(resource, db, id, &body).await?;
    match resource {
        Resource::Pools => audit::pools_replace(&actor.0, id, replaced.count),
        Resource::Weights => audit::weights_replace(&actor.0, id, replaced.count),
        Resource::Constraints => audit::constraints_replace(&actor.0, id),
    }
    Ok(response_with_json(
        StatusCode::OK,
        replaced.body.into_bytes(),
    ))
}

/// `PUT /internal/v1/tenants/{id}/limits` (D4, spec §13): `{classification_daily_quota?}`.
/// An absent field keeps the override, `null` clears it, and a value must be at least 0.
/// The response echoes the effective values.
pub(crate) async fn limits(
    State(api): State<ApiState>,
    Extension(actor): Extension<InternalActor>,
    Path(id): Path<String>,
    request: Request,
) -> Response<Body> {
    if request.method() != Method::PUT {
        return not_found();
    }
    respond(
        "tenant limits",
        replace_limits(&api, &actor, &id, request).await,
    )
}

async fn replace_limits(
    api: &ApiState,
    actor: &InternalActor,
    id: &str,
    request: Request,
) -> Handled {
    let db = pool_of(api)?;
    let tenant = tenant_or_404(db, id).await?;
    let body = read_body(request.into_body())
        .await
        .map_err(|response| Failure::Response(Box::new(response)))?;
    let quota = decode_limits(&body)?;
    let tenant = match quota {
        None => tenant,
        Some(quota) => tenancy::set_tenant_classification_quota(db, id, quota).await?,
    };
    audit::limits_replace(&actor.0, id);
    let effective = tenant.classification_quota_override.map_or(
        i64::from(api.internal().default_classification_quota),
        i64::from,
    );
    Ok(json(
        StatusCode::OK,
        &object(vec![(
            "classification_daily_quota",
            JsonValue::Number(effective.to_string()),
        )]),
    ))
}

/// `None`: the field was absent (keep). `Some(None)`: `null` (clear).
fn decode_limits(body: &[u8]) -> Result<Option<Option<i32>>, ApiError> {
    const STRUCT: &str = "limitsRequest";
    let value = strict::body_value(body)?;
    let quota = (|| -> Result<Option<Option<i64>>, String> {
        let mut out = None;
        let Some(members) = strict::members(&value, STRUCT, "")? else {
            return Ok(None);
        };
        for (key, value) in members {
            match match_field(key, &["classification_daily_quota"]) {
                Some(_) => {
                    out = Some(strict::integer(
                        value,
                        STRUCT,
                        "classification_daily_quota",
                        "int32",
                        i64::from(i32::MIN),
                        i64::from(i32::MAX),
                    )?);
                }
                None => return Err(strict::unknown(key)),
            }
        }
        Ok(out)
    })()
    .map_err(malformed)?;
    match quota {
        Some(Some(n)) if n < 0 => Err(invalid("classification_daily_quota must be at least 0")
            .detail("field", "classification_daily_quota")),
        Some(quota) => Ok(Some(
            quota.map(|n| i32::try_from(n).expect("range checked")),
        )),
        None => Ok(None),
    }
}

// ---- alerts ----

/// `GET /internal/v1/alerts?since=&kind=` (§7.7): oldest first, no pagination.
pub(crate) async fn alerts(State(api): State<ApiState>, request: Request) -> Response<Body> {
    if request.method() != Method::GET {
        return not_found();
    }
    let query = request.uri().query().unwrap_or("").to_owned();
    respond("alerts feed", feed(&api, &query).await)
}

async fn feed(api: &ApiState, query: &str) -> Handled {
    let (kind, since) = alerts_query(query)?;
    let db = pool_of(api)?;
    let rows = maverick_store::alerts::list_alerts_feed(db, &kind, since).await?;
    let mut items = Vec::with_capacity(rows.len());
    for row in &rows {
        items.push(object(vec![
            ("id", string(&row.id)),
            ("kind", string(&row.kind)),
            ("severity", string(&row.severity)),
            ("subject", stored(&row.subject)?),
            ("message", string(&row.message)),
            ("created_at", time(row.created_at)),
            ("acked_at", optional_time(row.acked_at)),
        ]));
    }
    Ok(json(
        StatusCode::OK,
        &object(vec![("alerts", JsonValue::Array(items))]),
    ))
}

/// `kind` and `since` only. `since` is an RFC 3339 timestamp or a `YYYY-MM-DD` date (UTC
/// midnight), as on the history endpoints.
fn alerts_query(query: &str) -> Result<(String, Option<DateTime<Utc>>), ApiError> {
    let mut kind = None;
    let mut since = None;
    for (key, value) in form_pairs(query) {
        match key.as_str() {
            "kind" => {
                kind.get_or_insert(value);
            }
            "since" => {
                since.get_or_insert(value);
            }
            other => {
                return Err(invalid(format!("unknown query parameter {}", quote(other)))
                    .detail("parameter", other));
            }
        }
    }
    let since = match since.filter(|value| !value.is_empty()) {
        None => None,
        Some(value) => Some(parse_since(&value).ok_or_else(|| {
            invalid(format!(
                "since {} must be an RFC 3339 timestamp or YYYY-MM-DD date",
                quote(&value)
            ))
            .detail("parameter", "since")
            .detail("value", value.as_str())
        })?),
    };
    Ok((kind.unwrap_or_default(), since))
}

fn parse_since(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|at| at.with_timezone(&Utc))
        .ok()
        .or_else(|| {
            NaiveDate::parse_from_str(value, "%Y-%m-%d")
                .ok()
                .map(|day| day.and_time(NaiveTime::MIN).and_utc())
        })
}

/// `application/x-www-form-urlencoded` pairs, in order, percent- and `+`-decoded.
fn form_pairs(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (form_decode(key), form_decode(value))
        })
        .collect()
}

fn form_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' => match bytes
                .get(i + 1..i + 3)
                .and_then(|hex| std::str::from_utf8(hex).ok())
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
            {
                Some(byte) => {
                    out.push(byte);
                    i += 3;
                }
                None => {
                    out.push(b'%');
                    i += 1;
                }
            },
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---- columns ----

fn column_json(column: &ModelColumn) -> Result<JsonValue, Failure> {
    Ok(object(vec![
        ("column_id", string(&column.id)),
        ("provider", string(&column.provider)),
        ("model_id", string(&column.model_id)),
        ("endpoint", string(&column.endpoint)),
        ("settings", stored(&column.settings)?),
        ("execution_aliases", stored(&column.execution_aliases)?),
        ("status", string(&column.status)),
        ("pricing", stored(&column.pricing)?),
        (
            "quality_source_column_id",
            optional_string(column.quality_source_column_id.as_deref()),
        ),
        ("cold_start_until", date(column.cold_start_until)),
        ("deprecation_date", date(column.deprecation_date)),
        ("created_at", time(column.created_at)),
        ("updated_at", time(column.updated_at)),
    ]))
}

fn field_error(field: &str, message: String) -> ApiError {
    invalid(message).detail("field", field)
}

fn one_of(field: &str, value: &str, allowed: &[&str]) -> Result<(), ApiError> {
    if allowed.contains(&value) {
        return Ok(());
    }
    Err(field_error(
        field,
        format!(
            "{field} {} must be one of [{}]",
            quote(value),
            allowed.join(" ")
        ),
    )
    .detail("value", value))
}

/// The column fields a create or update may carry, as decoded. JSON-valued fields keep
/// their value for validation and storage.
#[derive(Default)]
struct ColumnInput {
    provider: String,
    model_id: String,
    endpoint: String,
    status: Option<String>,
    settings: Option<JsonValue>,
    pricing: Option<JsonValue>,
    quality_source_column_id: Option<String>,
    deprecation_date: Option<String>,
    execution_aliases: Option<JsonValue>,
    retention_mode: Option<String>,
    retention_verified_at: Option<String>,
    retention_valid_until: Option<String>,
    retention_source: Option<String>,
    execution_alias_retention: Option<JsonValue>,
    param_constraints: Option<JsonValue>,
}

/// The create body (reference §7.8) plus the retention evidence (spec §4.5; D1, D15) and
/// the parameter constraints (D29).
const CREATE_FIELDS: [&str; 14] = [
    "provider",
    "model_id",
    "endpoint",
    "settings",
    "pricing",
    "quality_source_column_id",
    "deprecation_date",
    "execution_aliases",
    "retention_mode",
    "retention_verified_at",
    "retention_valid_until",
    "retention_source",
    "execution_alias_retention",
    "param_constraints",
];

/// The update body (reference §7.9) plus the retention evidence and the parameter
/// constraints.
const UPDATE_FIELDS: [&str; 10] = [
    "status",
    "pricing",
    "deprecation_date",
    "execution_aliases",
    "retention_mode",
    "retention_verified_at",
    "retention_valid_until",
    "retention_source",
    "execution_alias_retention",
    "param_constraints",
];

fn decode_column(
    body: &[u8],
    strukt: &str,
    fields: &[&'static str],
) -> Result<ColumnInput, ApiError> {
    let value = strict::body_value(body)?;
    let mut out = ColumnInput::default();
    (|| -> Result<(), String> {
        let Some(members) = strict::members(&value, strukt, "")? else {
            return Ok(());
        };
        for (key, value) in members {
            let Some(field) = match_field(key, fields) else {
                return Err(strict::unknown(key));
            };
            let json_value = || (!matches!(value, JsonValue::Null)).then(|| value.clone());
            match field {
                "provider" => strict::string(value, strukt, field, &mut out.provider)?,
                "model_id" => strict::string(value, strukt, field, &mut out.model_id)?,
                "endpoint" => strict::string(value, strukt, field, &mut out.endpoint)?,
                "status" => out.status = strict::optional_string(value, strukt, field)?,
                "settings" => out.settings = json_value(),
                "pricing" => out.pricing = json_value(),
                "execution_aliases" => out.execution_aliases = json_value(),
                "execution_alias_retention" => out.execution_alias_retention = json_value(),
                "param_constraints" => out.param_constraints = json_value(),
                "quality_source_column_id" => {
                    out.quality_source_column_id = strict::optional_string(value, strukt, field)?;
                }
                "deprecation_date" => {
                    out.deprecation_date = strict::optional_string(value, strukt, field)?;
                }
                "retention_mode" => {
                    out.retention_mode = strict::optional_string(value, strukt, field)?;
                }
                "retention_verified_at" => {
                    out.retention_verified_at = strict::optional_string(value, strukt, field)?;
                }
                "retention_valid_until" => {
                    out.retention_valid_until = strict::optional_string(value, strukt, field)?;
                }
                _ => out.retention_source = strict::optional_string(value, strukt, field)?,
            }
        }
        Ok(())
    })()
    .map_err(malformed)?;
    Ok(out)
}

fn validate_settings(settings: Option<&JsonValue>) -> Result<String, ApiError> {
    match settings {
        None => Ok("{}".to_owned()),
        Some(value @ JsonValue::Object(_)) => Ok(gojson::marshal(value)),
        Some(_) => Err(field_error(
            "settings",
            "settings must be a JSON object".to_owned(),
        )),
    }
}

fn validate_pricing(pricing: Option<&JsonValue>) -> Result<String, ApiError> {
    let Some(value) = pricing else {
        return Ok("{}".to_owned());
    };
    let text = gojson::marshal(value);
    parse_pricing(&text).map_err(|error| {
        field_error(
            "pricing",
            format!("pricing is not a valid snapshot: {error}"),
        )
    })?;
    Ok(text)
}

fn validate_date(value: Option<&str>) -> Result<Option<NaiveDate>, ApiError> {
    value
        .map(|text| {
            NaiveDate::parse_from_str(text, "%Y-%m-%d").map_err(|_| {
                field_error(
                    "deprecation_date",
                    format!("deprecation_date {} must be a YYYY-MM-DD date", quote(text)),
                )
                .detail("value", text)
            })
        })
        .transpose()
}

/// `execution_aliases`: an object of strings, at most 10 keys, each value non-empty and at
/// most 200 characters (checked in document order; Go used map order, QUIRK Q4). Returns
/// the keys and the JSON text.
fn validate_aliases(
    aliases: Option<&JsonValue>,
) -> Result<Option<(Vec<String>, String)>, ApiError> {
    let Some(value) = aliases else {
        return Ok(None);
    };
    let not_strings = || {
        field_error(
            "execution_aliases",
            "execution_aliases must be an object with string values".to_owned(),
        )
    };
    let JsonValue::Object(members) = value else {
        return Err(not_strings());
    };
    if members
        .iter()
        .any(|(_, value)| !matches!(value, JsonValue::String(_)))
    {
        return Err(not_strings());
    }
    let mut keys: Vec<String> = members.iter().map(|(key, _)| key.clone()).collect();
    keys.dedup();
    if keys.len() > MAX_ALIASES {
        return Err(field_error(
            "execution_aliases",
            format!(
                "execution_aliases has {} keys, want at most {MAX_ALIASES}",
                keys.len()
            ),
        ));
    }
    for (key, value) in members {
        let JsonValue::String(text) = value else {
            continue;
        };
        if text.is_empty() || text.chars().count() > MAX_ALIAS_RUNES {
            return Err(field_error(
                "execution_aliases",
                format!(
                    "execution_aliases[{}] must be non-empty and at most {MAX_ALIAS_RUNES} characters",
                    quote(key)
                ),
            )
            .detail("key", key.as_str()));
        }
    }
    Ok(Some((keys, gojson::marshal(value))))
}

/// `param_constraints` (D29): an object keyed by a known parameter name, each value
/// `{"allowed": [...], "order": [...]}` with non-empty lists of strings, no repeated
/// order value, and every allowed value in `order` (so "nearest" is defined). Returns
/// the JSON text.
fn validate_param_constraints(value: Option<&JsonValue>) -> Result<Option<String>, ApiError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let JsonValue::Object(fields) = value else {
        return Err(field_error(
            "param_constraints",
            "param_constraints must be an object keyed by parameter name".to_owned(),
        ));
    };
    let strings = |value: &JsonValue| match value {
        JsonValue::Array(items) => items
            .iter()
            .map(|item| match item {
                JsonValue::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect::<Option<Vec<String>>>(),
        _ => None,
    };
    for (name, constraint) in fields {
        if !KNOWN_PARAMS.contains(&name.as_str()) {
            return Err(field_error(
                "param_constraints",
                format!(
                    "param_constraints names unknown parameter {} (known: [{}])",
                    quote(name),
                    KNOWN_PARAMS.join(" ")
                ),
            )
            .detail("key", name.as_str()));
        }
        let malformed = || {
            field_error(
                "param_constraints",
                format!(
                    "param_constraints[{}] must be {{\"allowed\":[...],\"order\":[...]}} with non-empty string lists and every allowed value in order",
                    quote(name)
                ),
            )
            .detail("key", name.as_str())
        };
        let JsonValue::Object(parts) = constraint else {
            return Err(malformed());
        };
        let (mut allowed, mut order) = (None, None);
        for (key, value) in parts {
            match key.as_str() {
                "allowed" => allowed = Some(strings(value).ok_or_else(malformed)?),
                "order" => order = Some(strings(value).ok_or_else(malformed)?),
                _ => return Err(malformed()),
            }
        }
        let (Some(allowed), Some(order)) = (allowed, order) else {
            return Err(malformed());
        };
        let mut unique = order.clone();
        unique.sort();
        unique.dedup();
        if allowed.is_empty()
            || unique.len() != order.len()
            || allowed.iter().any(|value| !order.contains(value))
        {
            return Err(malformed());
        }
    }
    Ok(Some(gojson::marshal(value)))
}

/// Validated retention evidence for the column and its aliases (spec §4.5, D15).
struct Retention {
    mode: Option<String>,
    verified_at: Option<DateTime<Utc>>,
    valid_until: Option<DateTime<Utc>>,
    source: Option<String>,
    aliases: Option<String>,
}

fn parse_time(field: &str, value: Option<&str>) -> Result<Option<DateTime<Utc>>, ApiError> {
    value
        .map(|text| {
            DateTime::parse_from_rfc3339(text)
                .map(|at| at.with_timezone(&Utc))
                .map_err(|_| {
                    field_error(
                        field,
                        format!("{field} {} must be an RFC 3339 timestamp", quote(text)),
                    )
                    .detail("value", text)
                })
        })
        .transpose()
}

/// One evidence record: the mode is one of the three, and `valid_until` needs a
/// `verified_at` at most 90 days before it.
fn check_evidence(
    field_prefix: &str,
    mode: Option<&str>,
    verified_at: Option<DateTime<Utc>>,
    valid_until: Option<DateTime<Utc>>,
) -> Result<(), ApiError> {
    if let Some(mode) = mode {
        one_of(
            &format!("{field_prefix}retention_mode"),
            mode,
            &RETENTION_MODES,
        )?;
    }
    if let Some(until) = valid_until {
        let Some(verified) = verified_at else {
            return Err(field_error(
                &format!("{field_prefix}retention_verified_at"),
                format!(
                    "{field_prefix}retention_verified_at is required with {field_prefix}retention_valid_until"
                ),
            ));
        };
        if let Some(error) = retention_window_error(verified, Some(until)) {
            return Err(field_error(
                &format!("{field_prefix}retention_valid_until"),
                format!("{field_prefix}{}", error.message),
            ));
        }
    }
    Ok(())
}

/// `stored_verified_at` is the column's stored verification time: an update may send a
/// new `valid_until` alone.
fn validate_retention(
    input: &ColumnInput,
    alias_keys: &[String],
    stored_verified_at: Option<DateTime<Utc>>,
) -> Result<Retention, ApiError> {
    let verified_at = parse_time(
        "retention_verified_at",
        input.retention_verified_at.as_deref(),
    )?;
    let valid_until = parse_time(
        "retention_valid_until",
        input.retention_valid_until.as_deref(),
    )?;
    check_evidence(
        "",
        input.retention_mode.as_deref(),
        verified_at.or(stored_verified_at),
        valid_until,
    )?;
    let aliases = match &input.execution_alias_retention {
        None => None,
        Some(JsonValue::Object(members)) => {
            for (key, entry) in members {
                if !alias_keys.contains(key) {
                    return Err(field_error(
                        "execution_alias_retention",
                        format!(
                            "execution_alias_retention[{}] names no execution_aliases key",
                            quote(key)
                        ),
                    )
                    .detail("key", key.as_str()));
                }
                let JsonValue::Object(fields) = entry else {
                    return Err(field_error(
                        "execution_alias_retention",
                        "execution_alias_retention entries must be objects".to_owned(),
                    ));
                };
                let text = |name: &str| {
                    fields.iter().rev().find_map(|(k, v)| match v {
                        JsonValue::String(s) if k == name => Some(s.clone()),
                        _ => None,
                    })
                };
                let prefix = format!("execution_alias_retention[{}].", quote(key));
                let verified = parse_time(
                    &format!("{prefix}retention_verified_at"),
                    text("retention_verified_at").as_deref(),
                )?;
                let until = parse_time(
                    &format!("{prefix}retention_valid_until"),
                    text("retention_valid_until").as_deref(),
                )?;
                check_evidence(&prefix, text("retention_mode").as_deref(), verified, until)?;
            }
            Some(gojson::marshal(
                input.execution_alias_retention.as_ref().expect("some"),
            ))
        }
        Some(_) => {
            return Err(field_error(
                "execution_alias_retention",
                "execution_alias_retention must be an object".to_owned(),
            ));
        }
    };
    Ok(Retention {
        mode: input.retention_mode.clone(),
        verified_at,
        valid_until,
        source: input.retention_source.clone(),
        aliases,
    })
}

fn raw(text: String) -> Result<Box<JsonRawValue>, Failure> {
    JsonRawValue::from_string(text).map_err(|error| Failure::Internal(error.to_string()))
}

/// `POST /internal/v1/columns` (§7.8): a new column enters `cold_start`.
pub(crate) async fn create_column(
    State(api): State<ApiState>,
    Extension(actor): Extension<InternalActor>,
    request: Request,
) -> Response<Body> {
    if request.method() != Method::POST {
        return not_found();
    }
    respond("create column", insert_column(&api, &actor, request).await)
}

async fn insert_column(api: &ApiState, actor: &InternalActor, request: Request) -> Handled {
    let body = read_body(request.into_body())
        .await
        .map_err(|response| Failure::Response(Box::new(response)))?;
    let input = decode_column(&body, "createColumnRequest", &CREATE_FIELDS)?;
    for (field, value) in [
        ("provider", &input.provider),
        ("model_id", &input.model_id),
        ("endpoint", &input.endpoint),
    ] {
        if value.is_empty() {
            return Err(required_field(field).into());
        }
    }
    one_of("provider", &input.provider, &PROVIDERS)?;
    one_of("endpoint", &input.endpoint, &ENDPOINTS)?;
    let settings = validate_settings(input.settings.as_ref())?;
    let pricing = validate_pricing(input.pricing.as_ref())?;
    let deprecation_date = validate_date(input.deprecation_date.as_deref())?;
    let aliases = validate_aliases(input.execution_aliases.as_ref())?;
    let alias_keys = aliases
        .as_ref()
        .map(|(keys, _)| keys.clone())
        .unwrap_or_default();
    let retention = validate_retention(&input, &alias_keys, None)?;
    let param_constraints = validate_param_constraints(input.param_constraints.as_ref())?;
    let db = pool_of(api)?;
    if let Some(source) = &input.quality_source_column_id {
        match catalog::get_model_column(db, source).await {
            Ok(_) => {}
            Err(error) if is_not_found(&error) => {
                return Err(field_error(
                    "quality_source_column_id",
                    format!(
                        "quality_source_column_id {} is not in the model catalog",
                        quote(source)
                    ),
                )
                .detail("value", source.as_str())
                .into());
            }
            Err(error) => return Err(error.into()),
        }
    }
    let settings = raw(settings)?;
    let pricing = raw(pricing)?;
    let aliases_raw = aliases.map(|(_, text)| raw(text)).transpose()?;
    let alias_retention_raw = retention.aliases.clone().map(raw).transpose()?;
    let param_constraints_raw = param_constraints.map(raw).transpose()?;
    let settings_internal = api.internal();
    let today = settings_internal.now().date_naive();
    let cold_start_until = today + Duration::days(i64::from(settings_internal.cold_start_nights));
    let id = ids::new(ids::prefix::MODEL_COLUMN);
    let created = catalog::create_model_column(
        db,
        &NewModelColumn {
            id: &id,
            provider: &input.provider,
            model_id: &input.model_id,
            endpoint: &input.endpoint,
            settings: &settings,
            status: "cold_start",
            pricing: &pricing,
            quality_source_column_id: input.quality_source_column_id.as_deref(),
            cold_start_until: Some(cold_start_until),
            deprecation_date,
            execution_aliases: aliases_raw.as_deref(),
            retention_mode: retention.mode.as_deref(),
            retention_verified_at: retention.verified_at,
            retention_valid_until: retention.valid_until,
            retention_source: retention.source.as_deref(),
            execution_alias_retention: alias_retention_raw.as_deref(),
            param_constraints: param_constraints_raw.as_deref(),
        },
    )
    .await;
    let column = match created {
        Ok(column) => column,
        Err(error) if sqlstate(&error).as_deref() == Some("23505") => {
            return Err(invalid(format!(
                "a column for {}/{}/{} with identical settings already exists",
                input.provider, input.model_id, input.endpoint
            ))
            .detail("provider", input.provider.as_str())
            .detail("model_id", input.model_id.as_str())
            .detail("endpoint", input.endpoint.as_str())
            .into());
        }
        Err(error) => return Err(error.into()),
    };
    audit::column_create(
        &actor.0,
        &column.id,
        &column.provider,
        &column.model_id,
        &column.endpoint,
        &column.status,
    );
    Ok(json(StatusCode::CREATED, &column_json(&column)?))
}

/// `PUT /internal/v1/columns/{id}` (§7.9). Absent fields keep their stored values;
/// retiring a column goes through the retirement guard.
pub(crate) async fn update_column(
    State(api): State<ApiState>,
    Extension(actor): Extension<InternalActor>,
    Path(id): Path<String>,
    request: Request,
) -> Response<Body> {
    if request.method() != Method::PUT {
        return not_found();
    }
    respond(
        "update column",
        change_column(&api, &actor, &id, request).await,
    )
}

async fn change_column(
    api: &ApiState,
    actor: &InternalActor,
    id: &str,
    request: Request,
) -> Handled {
    let db = pool_of(api)?;
    let stored = match catalog::get_model_column(db, id).await {
        Ok(column) => column,
        Err(error) if is_not_found(&error) => {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                ErrorCode::NotFound,
                format!("model column {id} does not exist"),
            )
            .detail("column_id", id)
            .into());
        }
        Err(error) => return Err(error.into()),
    };
    let body = read_body(request.into_body())
        .await
        .map_err(|response| Failure::Response(Box::new(response)))?;
    let input = decode_column(&body, "updateColumnRequest", &UPDATE_FIELDS)?;
    let status = input
        .status
        .clone()
        .unwrap_or_else(|| stored.status.clone());
    one_of("status", &status, &STATUSES)?;
    let pricing = input
        .pricing
        .as_ref()
        .map(|value| validate_pricing(Some(value)))
        .transpose()?;
    let deprecation_date = match &input.deprecation_date {
        Some(text) => validate_date(Some(text))?,
        None => stored.deprecation_date,
    };
    let aliases = validate_aliases(input.execution_aliases.as_ref())?;
    let alias_keys = match &aliases {
        Some((keys, _)) => keys.clone(),
        None => stored_alias_keys(&stored),
    };
    let retention = validate_retention(&input, &alias_keys, stored.retention_verified_at)?;
    let param_constraints = validate_param_constraints(input.param_constraints.as_ref())?;

    let now = api.internal().now();
    if status == "retired" && stored.status != "retired" {
        let mut conn = db.acquire().await?;
        let blocked = simulation::retirement_blockers(&mut conn, id, now).await?;
        drop(conn);
        if !blocked.is_empty() {
            let blocked = RetirementBlocked {
                column_id: id.to_owned(),
                pools: blocked,
            };
            let notifier = api.recommend().alerts.as_deref();
            insert_alert(
                db,
                notifier,
                AlertKind::PoolWouldBeEmpty,
                Severity::Critical,
                &blocked,
                &blocked.message(),
            )
            .await
            .map_err(|error| Failure::Internal(error.to_string()))?;
            return Err(Failure::Response(Box::new(retirement_blocked(&blocked)?)));
        }
    }

    let pricing_raw = pricing.clone().map(raw).transpose()?;
    let aliases_raw = aliases
        .as_ref()
        .map(|(_, text)| raw(text.clone()))
        .transpose()?;
    let alias_retention_raw = retention.aliases.clone().map(raw).transpose()?;
    let param_constraints_raw = param_constraints.map(raw).transpose()?;
    let retention_mode = retention
        .mode
        .clone()
        .unwrap_or_else(|| stored.retention_mode.clone());
    let update = ModelColumnUpdate {
        settings: &stored.settings,
        status: &status,
        pricing: pricing_raw.as_deref().unwrap_or(&stored.pricing),
        quality_source_column_id: stored.quality_source_column_id.as_deref(),
        cold_start_until: stored.cold_start_until,
        deprecation_date,
        execution_aliases: aliases_raw.as_deref(),
        retention_mode: &retention_mode,
        retention_verified_at: retention.verified_at.or(stored.retention_verified_at),
        retention_valid_until: if input.retention_valid_until.is_some() {
            retention.valid_until
        } else {
            stored.retention_valid_until
        },
        retention_source: retention
            .source
            .as_deref()
            .or(stored.retention_source.as_deref()),
        execution_alias_retention: alias_retention_raw.as_deref(),
        param_constraints: param_constraints_raw.as_deref(),
    };
    let column = catalog::update_model_column(db, id, &update).await?;

    if column.status != stored.status {
        log_change(CatalogChange::Status, id, &stored.status, &column.status);
    }
    if pricing.is_some() && column.pricing.get() != stored.pricing.get() {
        log_change(
            CatalogChange::Pricing,
            id,
            stored.pricing.get(),
            column.pricing.get(),
        );
    }
    if aliases.is_some() && column.execution_aliases.get() != stored.execution_aliases.get() {
        log_change(
            CatalogChange::ExecutionAliases,
            id,
            stored.execution_aliases.get(),
            column.execution_aliases.get(),
        );
    }
    if input.param_constraints.is_some()
        && column.param_constraints.get() != stored.param_constraints.get()
    {
        log_change(
            CatalogChange::ParamConstraints,
            id,
            stored.param_constraints.get(),
            column.param_constraints.get(),
        );
    }
    audit::column_update(&actor.0, id, &column.status);
    retention_downgrade_alerts(api, db, &stored, &column, now).await?;
    Ok(json(StatusCode::OK, &column_json(&column)?))
}

fn stored_alias_keys(column: &ModelColumn) -> Vec<String> {
    match gojson::unmarshal(column.execution_aliases.get()) {
        Ok(JsonValue::Object(members)) => members.into_iter().map(|(key, _)| key).collect(),
        _ => Vec::new(),
    }
}

/// The 422 `pool_would_be_empty` of a blocked retirement. The details keep Go's struct
/// order (`column_id`, then each pool's `tenant_id`, `pool`, `constraint`, ...), which the
/// sorted-key error envelope cannot, so the envelope is built here.
fn retirement_blocked(blocked: &RetirementBlocked) -> Result<Response<Body>, Failure> {
    let details =
        serde_json::to_string(blocked).map_err(|error| Failure::Internal(error.to_string()))?;
    let mut message = String::new();
    gojson::write_string(&mut message, &blocked.message());
    let envelope = format!(
        "{{\"error\":{{\"code\":\"{}\",\"message\":{message},\"details\":{details}}}}}",
        ErrorCode::PoolWouldBeEmpty.as_str()
    );
    let mut bytes = escape_go_json(envelope.as_bytes());
    bytes.push(b'\n');
    Ok(response_with_json(StatusCode::UNPROCESSABLE_ENTITY, bytes))
}

/// A retention downgrade is not blocked, but it runs the G4 simulation: each
/// zero-retention tenant whose pool it would empty gets a critical `pool_would_be_empty`
/// alert (spec §4.5, D1). A downgrade is valid zero evidence that stops being valid now.
async fn retention_downgrade_alerts(
    api: &ApiState,
    db: &PgPool,
    before: &ModelColumn,
    after: &ModelColumn,
    now: DateTime<Utc>,
) -> Result<(), Failure> {
    let zero = |column: &ModelColumn| {
        simulation::retention_mode(&column.retention_mode)
            .effective(column.retention_valid_until, now)
            == maverick_domain::policy::RetentionMode::Zero
    };
    if !zero(before) || zero(after) {
        return Ok(());
    }
    let mut conn = db.acquire().await?;
    let pools = tenancy::list_pools_by_column(&mut *conn, &after.id).await?;
    let tenants = simulation::survey(&mut conn, pools).await?;
    drop(conn);
    for tenant in tenants.iter().filter(|t| t.constraints.zero_retention) {
        let errors = maverick_domain::policy::simulate(
            &tenant.pools,
            &tenant.constraints,
            &tenant.state,
            now,
        );
        if errors.is_empty() {
            continue;
        }
        let pools: Vec<&str> = errors
            .iter()
            .filter_map(|error| error.details.get("pool").and_then(|p| p.as_str()))
            .collect();
        let subject = serde_json::json!({
            "column_id": after.id,
            "constraint": "zero_retention",
            "pools": pools,
            "tenant_id": tenant.tenant_id,
        });
        let message = format!(
            "retention downgrade of column {} leaves zero-retention tenant {} with no compliant columns in pool(s) {}",
            after.id,
            tenant.tenant_id,
            pools
                .iter()
                .map(|p| quote(p))
                .collect::<Vec<_>>()
                .join(", ")
        );
        insert_alert(
            db,
            api.recommend().alerts.as_deref(),
            AlertKind::PoolWouldBeEmpty,
            Severity::Critical,
            &subject,
            &message,
        )
        .await
        .map_err(|error| Failure::Internal(error.to_string()))?;
    }
    Ok(())
}

// ---- corpus ----

/// `POST /internal/v1/corpus/activate` (§7.10).
pub(crate) async fn activate_corpus(
    State(api): State<ApiState>,
    Extension(actor): Extension<InternalActor>,
    request: Request,
) -> Response<Body> {
    if request.method() != Method::POST {
        return not_found();
    }
    respond(
        "activate corpus",
        activate_version(&api, &actor, request).await,
    )
}

async fn activate_version(api: &ApiState, actor: &InternalActor, request: Request) -> Handled {
    let body = read_body(request.into_body())
        .await
        .map_err(|response| Failure::Response(Box::new(response)))?;
    let version = decode_name(&body, "activateCorpusRequest", "version")?;
    let db = pool_of(api)?;
    match corpus::activate_corpus(db, &version).await {
        Ok(()) => {}
        Err(corpus::CorpusError::UnknownVersion { .. }) => {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                ErrorCode::NotFound,
                format!(
                    "corpus version {version} does not exist; load it first (maverick corpus load)"
                ),
            )
            .detail("version", version.as_str())
            .into());
        }
        Err(error) => return Err(Failure::Internal(error.to_string())),
    }
    let row = corpus::get_corpus_version(db, &version).await?;
    audit::corpus_activate(&actor.0, &version);
    Ok(json(
        StatusCode::OK,
        &object(vec![
            ("version", string(&row.version)),
            ("description", string(&row.description)),
            ("task_count", JsonValue::Number(row.task_count.to_string())),
            ("active", JsonValue::Bool(row.active)),
            ("created_at", time(row.created_at)),
        ]),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_accepts_rfc3339_and_dates() {
        assert_eq!(
            parse_since("2026-09-28T10:07:51.416239Z").map(|t| t.to_rfc3339()),
            Some("2026-09-28T10:07:51.416239+00:00".to_owned())
        );
        assert_eq!(
            parse_since("2026-09-28").map(|t| t.to_rfc3339()),
            Some("2026-09-28T00:00:00+00:00".to_owned())
        );
        assert_eq!(parse_since("notatime"), None);
    }

    #[test]
    fn form_pairs_decode_percent_and_plus() {
        assert_eq!(
            form_pairs("since=2026-09-28T10%3A07%3A51Z&kind=a+b&flag"),
            vec![
                ("since".to_owned(), "2026-09-28T10:07:51Z".to_owned()),
                ("kind".to_owned(), "a b".to_owned()),
                ("flag".to_owned(), String::new()),
            ]
        );
        assert_eq!(form_decode("%zz%4"), "%zz%4");
    }

    #[test]
    fn limits_decoding_keeps_clears_and_bounds() {
        assert_eq!(decode_limits(b"{}").unwrap(), None);
        assert_eq!(
            decode_limits(br#"{"classification_daily_quota":null}"#).unwrap(),
            Some(None)
        );
        assert_eq!(
            decode_limits(br#"{"classification_daily_quota":25}"#).unwrap(),
            Some(Some(25))
        );
        assert_eq!(
            decode_limits(br#"{"classification_daily_quota":-1}"#)
                .unwrap_err()
                .message,
            "classification_daily_quota must be at least 0"
        );
        assert!(decode_limits(br#"{"rpm":5}"#).is_err());
    }
}
