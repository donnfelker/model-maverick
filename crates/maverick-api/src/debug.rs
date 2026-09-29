//! Tenant debug capture (spec §9.8, §10.4; reference Part 4 §4.6 and Part 5 §6.14 to
//! §6.18; D18).
//!
//! - `POST /v1/debug/capture-windows` opens a window: `duration_minutes` (1 to
//!   `capture.max_window_minutes`), an optional `confidence_below` in (0, 1] that is
//!   stored as float32, and an optional `api_key_id` that must name one of the caller's
//!   keys. QUIRK Q8: the key check comes before the zero-retention refusal (409).
//! - `DELETE /v1/debug/capture-windows/{id}` ends the caller's window early (204).
//! - `POST /v1/debug/replay/{capture_id}` decrypts the prompt in memory, classifies it
//!   again without the cache, and compares capability and difficulty with the stored
//!   classification. It never returns the prompt.
//! - `GET /v1/debug/captures` lists the caller's captures, newest first, without prompt
//!   text.
//! - `GET /v1/debug/captures/{id}/prompt` returns the decrypted prompt.
//!
//! Opening and ending a window drop the tenant's cached windows, so the next prompt sees
//! the change. Ownership is checked before any decrypt: another tenant's capture and an
//! unknown id get the same 404. Every replay and prompt read appends a
//! `capture_access_log` row (id prefix `cal`, actor = the caller's API key id), and a
//! failed append fails the call. The log has no FK and outlives the capture, so a
//! hard-deleted capture answers 410 and one that never existed answers 404. The prompt is
//! never logged.
//!
//! The operator capture reads (`/internal/v1/captures`) share [`read_tail`],
//! [`log_access`], and the page helpers.

use axum::body::Body;
use axum::extract::{Extension, Path, Request, State};
use axum::http::{Method, Response, StatusCode};
use chrono::{DateTime, Utc};
use maverick_domain::classify::Classification;
use maverick_domain::gofmt::quote;
use maverick_domain::gojson::{self, JsonValue, match_field};
use maverick_domain::history::{base64url_decode, base64url_encode};
use maverick_domain::ids;
use maverick_store::capture::{
    self as store, CaptureWindow, DebugCapture, TenantCapturePageQuery, TenantCaptureSummary,
};
use maverick_store::decisions::{self, Recommendation};
use maverick_store::{catalog, is_not_found, tenancy};
use serde_json::Value;
use sqlx::PgPool;

use crate::capture::{Capturer, DEFAULT_MAX_WINDOW_MINUTES};
use crate::classify::{self, ResolveInput, Resolver};
use crate::health::rfc3339_nano;
use crate::strict::{self, invalid, malformed, required_field};
use crate::{
    ApiError, ApiState, ErrorCode, Identity, internal_error, not_found, read_body,
    response_with_json,
};

/// The access-log action of a replay.
pub(crate) const ACTION_REPLAY: &str = "replay";
/// The access-log action of a prompt read (and of an expired replay).
pub(crate) const ACTION_READ: &str = "read";

/// The capture list's page size when `limit` is not given.
pub(crate) const DEFAULT_LIMIT: i32 = 50;
/// The largest `limit`.
pub(crate) const MAX_LIMIT: i32 = 200;

const EXPIRED_MESSAGE: &str =
    "capture has expired and its ciphertext is deleted or pending deletion";
const ZERO_RETENTION_MESSAGE: &str = "tenant is configured zero-retention; debug capture \
                                      windows cannot be created (spec §9.8, G5)";

/// The Go struct names in decode errors (D25 leaves the text free).
const WINDOW_STRUCT: &str = "captureWindowRequest";
const REPLAY_STRUCT: &str = "replayRequest";

/// Why a capture call did not succeed.
#[derive(Debug)]
pub(crate) enum Failure {
    /// 404 `resource not found`: unknown, another tenant's, or never existed.
    NotFound,
    /// 410 `capture_expired`.
    Expired,
    /// A 4xx with its own envelope.
    Api(ApiError),
    /// 500 `internal error`. The text is logged, never sent, and never holds the prompt.
    Internal(String),
}

impl From<ApiError> for Failure {
    fn from(error: ApiError) -> Self {
        Self::Api(error)
    }
}

impl Failure {
    fn store(error: sqlx::Error) -> Self {
        Self::Internal(error.to_string())
    }
}

/// The response for a call's result. A 500 logs `capture handler failure`.
pub(crate) fn respond(
    method: &str,
    path: &str,
    result: Result<Response<Body>, Failure>,
) -> Response<Body> {
    match result {
        Ok(response) => response,
        Err(Failure::NotFound) => not_found(),
        Err(Failure::Expired) => {
            ApiError::new(StatusCode::GONE, ErrorCode::CaptureExpired, EXPIRED_MESSAGE)
                .into_response()
        }
        Err(Failure::Api(error)) => error.into_response(),
        Err(Failure::Internal(error)) => {
            tracing::error!(method, path, error = %error, "capture handler failure");
            internal_error()
        }
    }
}

// ---- handlers ----

/// `/v1/debug/capture-windows`: `POST` opens a window.
pub(crate) async fn windows_handler(
    State(api): State<ApiState>,
    Extension(identity): Extension<Identity>,
    request: Request,
) -> Response<Body> {
    if request.method() != Method::POST {
        return not_found();
    }
    let path = request.uri().path().to_owned();
    let Some(pool) = api.pool() else {
        return internal_error();
    };
    let body = match read_body(request.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let capturer = api.recommend().capture.as_deref();
    let result = create_window(pool, capturer, &identity, &body, Utc::now()).await;
    respond("POST", &path, result)
}

/// `/v1/debug/capture-windows/{id}`: `DELETE` ends the caller's window early.
pub(crate) async fn window_handler(
    State(api): State<ApiState>,
    Extension(identity): Extension<Identity>,
    Path(id): Path<String>,
    request: Request,
) -> Response<Body> {
    if request.method() != Method::DELETE {
        return not_found();
    }
    let path = request.uri().path().to_owned();
    let Some(pool) = api.pool() else {
        return internal_error();
    };
    let capturer = api.recommend().capture.as_deref();
    respond(
        "DELETE",
        &path,
        end_window(pool, capturer, &identity, &id).await,
    )
}

/// `/v1/debug/replay/{capture_id}`: `POST` re-classifies a captured prompt.
pub(crate) async fn replay_handler(
    State(api): State<ApiState>,
    Extension(identity): Extension<Identity>,
    Path(capture_id): Path<String>,
    request: Request,
) -> Response<Body> {
    if request.method() != Method::POST {
        return not_found();
    }
    let path = request.uri().path().to_owned();
    let Some(pool) = api.pool() else {
        return internal_error();
    };
    let body = match read_body(request.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let config = api.recommend();
    let result = replay(
        pool,
        &config.classifier,
        config.capture.as_deref(),
        &identity,
        &capture_id,
        &body,
        Utc::now(),
    )
    .await;
    respond("POST", &path, result)
}

/// `/v1/debug/captures`: `GET` lists the caller's captures.
pub(crate) async fn captures_handler(
    State(api): State<ApiState>,
    Extension(identity): Extension<Identity>,
    request: Request,
) -> Response<Body> {
    if request.method() != Method::GET {
        return not_found();
    }
    let path = request.uri().path().to_owned();
    let query = request.uri().query().unwrap_or("").to_owned();
    let Some(pool) = api.pool() else {
        return internal_error();
    };
    respond(
        "GET",
        &path,
        list(pool, &identity, &query, Utc::now()).await,
    )
}

/// `/v1/debug/captures/{id}/prompt`: `GET` reads the caller's captured prompt.
pub(crate) async fn prompt_handler(
    State(api): State<ApiState>,
    Extension(identity): Extension<Identity>,
    Path(id): Path<String>,
    request: Request,
) -> Response<Body> {
    if request.method() != Method::GET {
        return not_found();
    }
    let path = request.uri().path().to_owned();
    let Some(pool) = api.pool() else {
        return internal_error();
    };
    let capturer = api.recommend().capture.as_deref();
    let result = read_prompt(pool, capturer, &identity, &id, Utc::now()).await;
    respond("GET", &path, result)
}

// ---- windows ----

/// The `POST /v1/debug/capture-windows` body. Every field is a Go pointer.
#[derive(Debug, Default, PartialEq)]
struct WindowRequest {
    api_key_id: Option<String>,
    confidence_below: Option<f64>,
    duration_minutes: Option<i64>,
}

fn decode_window(body: &[u8]) -> Result<WindowRequest, ApiError> {
    let value = strict::body_value(body)?;
    let mut request = WindowRequest::default();
    let members = strict::members(&value, WINDOW_STRUCT, "").map_err(malformed)?;
    for (key, value) in members.unwrap_or_default() {
        match match_field(key, &["api_key_id", "confidence_below", "duration_minutes"]) {
            Some("api_key_id") => {
                request.api_key_id = strict::optional_string(value, WINDOW_STRUCT, "api_key_id")
                    .map_err(malformed)?;
            }
            Some("confidence_below") => {
                request.confidence_below =
                    strict::float64(value, WINDOW_STRUCT, "confidence_below").map_err(malformed)?;
            }
            Some("duration_minutes") => {
                request.duration_minutes = strict::integer(
                    value,
                    WINDOW_STRUCT,
                    "duration_minutes",
                    "int",
                    i64::MIN,
                    i64::MAX,
                )
                .map_err(malformed)?;
            }
            _ => return Err(malformed(strict::unknown(key))),
        }
    }
    Ok(request)
}

/// Validates in the reference's order (duration, confidence, key, zero retention), then
/// stores the window from `now` and drops the tenant's cached windows.
async fn create_window(
    pool: &PgPool,
    capturer: Option<&Capturer>,
    identity: &Identity,
    body: &[u8],
    now: DateTime<Utc>,
) -> Result<Response<Body>, Failure> {
    let request = decode_window(body)?;
    let max = capturer.map_or(DEFAULT_MAX_WINDOW_MINUTES, Capturer::max_window_minutes);
    let Some(duration) = request.duration_minutes else {
        return Err(required_field("duration_minutes").into());
    };
    if !(1..=i64::from(max)).contains(&duration) {
        return Err(invalid(format!(
            "duration_minutes must be between 1 and {max} (capture.max_window_minutes, spec §9.8)"
        ))
        .detail("duration_minutes", duration)
        .detail("max_window_minutes", max)
        .into());
    }
    if let Some(below) = request.confidence_below {
        if below <= 0.0 || below > 1.0 {
            return Err(invalid("confidence_below must be a number in (0, 1]")
                .detail("confidence_below", go_float(below))
                .into());
        }
    }
    if let Some(key_id) = &request.api_key_id {
        let owned = match tenancy::get_api_key(pool, key_id).await {
            Ok(key) => key.tenant_id == identity.tenant_id,
            Err(error) if is_not_found(&error) => false,
            Err(error) => return Err(Failure::store(error)),
        };
        if !owned {
            return Err(invalid("api_key_id does not name one of your API keys")
                .detail("api_key_id", key_id.as_str())
                .into());
        }
    }
    if zero_retention(pool, &identity.tenant_id).await? {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            ErrorCode::ZeroRetentionConflict,
            ZERO_RETENTION_MESSAGE,
        )
        .into());
    }
    let window = store::create_capture_window(
        pool,
        &ids::new(ids::prefix::CAPTURE_WINDOW),
        &identity.tenant_id,
        request.api_key_id.as_deref(),
        // QUIRK: stored as float32, and echoed widened (0.8 reads 0.800000011920929).
        request.confidence_below.map(|below| below as f32),
        now,
        now + chrono::Duration::minutes(duration),
    )
    .await
    .map_err(Failure::store)?;
    if let Some(capturer) = capturer {
        capturer.invalidate(&identity.tenant_id);
    }
    Ok(json(StatusCode::CREATED, &window_body(&window)?))
}

/// `{"id","api_key_id","confidence_below","starts_at","ends_at","created_at"}`.
fn window_body(window: &CaptureWindow) -> Result<JsonValue, Failure> {
    let below = match window.confidence_below {
        Some(below) => {
            JsonValue::Number(gojson::encode_f64(f64::from(below)).map_err(Failure::Internal)?)
        }
        None => JsonValue::Null,
    };
    Ok(object(vec![
        ("id", string(&window.id)),
        ("api_key_id", optional(window.api_key_id.as_deref())),
        ("confidence_below", below),
        ("starts_at", time(window.starts_at)),
        ("ends_at", time(window.ends_at)),
        ("created_at", time(window.created_at)),
    ]))
}

/// Deletes the caller's window: 404 when there is none (unknown, gone, or another
/// tenant's). Its captures keep their `window_id` set to null.
async fn end_window(
    pool: &PgPool,
    capturer: Option<&Capturer>,
    identity: &Identity,
    id: &str,
) -> Result<Response<Body>, Failure> {
    let deleted = store::delete_capture_window_for_tenant(pool, id, &identity.tenant_id)
        .await
        .map_err(Failure::store)?;
    if deleted == 0 {
        return Err(Failure::NotFound);
    }
    if let Some(capturer) = capturer {
        capturer.invalidate(&identity.tenant_id);
    }
    Ok(Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .expect("a 204 without headers is valid"))
}

/// The tenant's zero-retention constraint. No constraints row means false.
async fn zero_retention(pool: &PgPool, tenant_id: &str) -> Result<bool, Failure> {
    match tenancy::get_tenant_constraints(pool, tenant_id).await {
        Ok(row) => Ok(row.zero_retention),
        Err(error) if is_not_found(&error) => Ok(false),
        Err(error) => Err(Failure::store(error)),
    }
}

// ---- replay ----

/// The optional replay body: absent (or only whitespace) is allowed; a present body is
/// strict `{"classifier_version": string?}`.
fn decode_replay(body: &[u8]) -> Result<Option<String>, ApiError> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    let value = strict::body_value(body)?;
    let mut version = None;
    let members = strict::members(&value, REPLAY_STRUCT, "").map_err(malformed)?;
    for (key, value) in members.unwrap_or_default() {
        if match_field(key, &["classifier_version"]).is_none() {
            return Err(malformed(strict::unknown(key)));
        }
        version = strict::optional_string(value, REPLAY_STRUCT, "classifier_version")
            .map_err(malformed)?;
    }
    Ok(version)
}

/// Part 4 §4.6 `Replay`: the version check, the tenant-scoped lookup, then decrypt,
/// re-classify, log `replay`, and answer. An expired capture logs `read` and is 410.
async fn replay(
    pool: &PgPool,
    resolver: &Resolver,
    capturer: Option<&Capturer>,
    identity: &Identity,
    capture_id: &str,
    body: &[u8],
    now: DateTime<Utc>,
) -> Result<Response<Body>, Failure> {
    if let Some(version) = decode_replay(body)? {
        let current = resolver.current_version();
        if version != current {
            return Err(
                invalid("only the current classifier_version can be replayed in v1")
                    .detail("classifier_version", version)
                    .detail("current", current)
                    .into(),
            );
        }
    }
    let capture =
        match store::get_debug_capture_for_tenant(pool, capture_id, &identity.tenant_id).await {
            Ok(capture) => capture,
            Err(error) if is_not_found(&error) => {
                return Err(missing(pool, capture_id, &identity.tenant_id).await);
            }
            Err(error) => return Err(Failure::store(error)),
        };
    if capture.expires_at <= now {
        log_access(
            pool,
            &capture.id,
            &capture.tenant_id,
            &identity.api_key_id,
            ACTION_READ,
        )
        .await?;
        return Err(Failure::Expired);
    }
    let prompt = open(capturer, &capture)?;
    let recommendation = recommendation(pool, &capture).await?;
    let (replayed, method) = reclassify(pool, resolver, &capture.tenant_id, &prompt, now).await?;
    let original = jsonb(recommendation.classification.get())?;
    let matched = same_workload(&original, &replayed);
    log_access(
        pool,
        &capture.id,
        &capture.tenant_id,
        &identity.api_key_id,
        ACTION_REPLAY,
    )
    .await?;
    Ok(json(
        StatusCode::OK,
        &object(vec![
            ("original_classification", original),
            (
                "replayed_classification",
                classification_block(&replayed, method)?,
            ),
            ("match", JsonValue::Bool(matched)),
        ]),
    ))
}

/// The prompt classified again, with the tenant's D2 and D4 inputs (see
/// [`Resolver::replay`]).
async fn reclassify(
    pool: &PgPool,
    resolver: &Resolver,
    tenant_id: &str,
    prompt: &str,
    now: DateTime<Utc>,
) -> Result<(Classification, classify::Method), Failure> {
    let tenant = tenancy::get_tenant(pool, tenant_id)
        .await
        .map_err(Failure::store)?;
    let zero_retention = zero_retention(pool, tenant_id).await?;
    let columns = catalog::list_model_columns(pool)
        .await
        .map_err(Failure::store)?;
    Ok(resolver
        .replay(
            pool,
            &ResolveInput {
                tenant_id,
                prompt_hmac_key: &tenant.prompt_hmac_key,
                quota_override: tenant.classification_quota_override,
                zero_retention,
                prompt,
                projection_version: None,
                columns: &columns,
                now,
            },
        )
        .await)
}

/// `match`: the stored classification has the replayed capability and difficulty.
fn same_workload(original: &JsonValue, replayed: &Classification) -> bool {
    let capability = matches!(
        member(original, "capability"),
        Some(JsonValue::String(capability)) if *capability == replayed.capability
    );
    let difficulty = matches!(
        member(original, "difficulty"),
        Some(JsonValue::Number(difficulty))
            if difficulty.parse::<i64>().ok() == Some(replayed.difficulty)
    );
    capability && difficulty
}

/// `replayed_classification`: `{capability, difficulty, domain_tags, high_stakes,
/// confidence, method, truncated}`, `truncated` only when true.
fn classification_block(
    classification: &Classification,
    method: classify::Method,
) -> Result<JsonValue, Failure> {
    let confidence = gojson::encode_f64(classification.confidence).map_err(Failure::Internal)?;
    let mut members = vec![
        ("capability", string(&classification.capability)),
        (
            "difficulty",
            JsonValue::Number(classification.difficulty.to_string()),
        ),
        (
            "domain_tags",
            JsonValue::Array(
                classification
                    .domain_tags
                    .iter()
                    .map(|t| string(t))
                    .collect(),
            ),
        ),
        ("high_stakes", JsonValue::Bool(classification.high_stakes())),
        ("confidence", JsonValue::Number(confidence)),
        ("method", string(method.as_str())),
    ];
    if classification.truncated {
        members.push(("truncated", JsonValue::Bool(true)));
    }
    Ok(object(members))
}

// ---- list ----

/// The list parameters (Part 5 §6.17).
#[derive(Debug, PartialEq, Eq)]
struct ListParams {
    window_id: String,
    limit: i32,
    cursor_id: String,
}

/// `window_id`, `limit`, and `cursor` only, checked in that order after the unknown-name
/// check. A repeated name keeps its first value; an empty value means not supplied.
fn parse_list(query: &str) -> Result<ListParams, Failure> {
    let mut window_id = None;
    let mut limit = None;
    let mut cursor = None;
    for (key, value) in crate::query::pairs(query) {
        let slot = match key.as_str() {
            "window_id" => &mut window_id,
            "limit" => &mut limit,
            "cursor" => &mut cursor,
            other => {
                return Err(invalid(format!("unknown query parameter {}", quote(other)))
                    .detail("parameter", other)
                    .into());
            }
        };
        slot.get_or_insert(value);
    }
    Ok(ListParams {
        limit: parse_limit(limit.as_deref().unwrap_or(""))?,
        cursor_id: parse_cursor(cursor.as_deref().unwrap_or(""))?,
        window_id: window_id.unwrap_or_default(),
    })
}

/// `limit`: 50 when not supplied, else a decimal int32 in 1..=200.
pub(crate) fn parse_limit(raw: &str) -> Result<i32, ApiError> {
    if raw.is_empty() {
        return Ok(DEFAULT_LIMIT);
    }
    raw.parse::<i32>()
        .ok()
        .filter(|limit| (1..=MAX_LIMIT).contains(limit))
        .ok_or_else(|| invalid_param("limit", raw, "an integer between 1 and 200"))
}

/// `cursor`: unpadded base64url of the last capture id of the previous page. Empty is the
/// first page. A value that does not decode, or decodes to nothing, is 422. Go passed the
/// decoded bytes to Postgres as they were, which refused non-UTF-8 text (500).
pub(crate) fn parse_cursor(raw: &str) -> Result<String, Failure> {
    if raw.is_empty() {
        return Ok(String::new());
    }
    let bytes = base64url_decode(raw)
        .filter(|bytes| !bytes.is_empty())
        .ok_or_else(|| invalid_param("cursor", raw, "an opaque pagination token"))?;
    String::from_utf8(bytes).map_err(|_| Failure::Internal("cursor is not UTF-8 text".to_owned()))
}

/// `<param> "<value>" must be <want>`, details `{"parameter","value"}`.
fn invalid_param(parameter: &str, value: &str, want: &str) -> ApiError {
    invalid(format!("{parameter} {} must be {want}", quote(value)))
        .detail("parameter", parameter)
        .detail("value", value)
}

/// A page from `limit + 1` rows: at most `limit` rows, and the cursor of the last one
/// when more came back.
pub(crate) fn page<T>(
    mut rows: Vec<T>,
    limit: i32,
    id: impl Fn(&T) -> &str,
) -> (Vec<T>, Option<String>) {
    let limit = usize::try_from(limit).unwrap_or(0);
    if rows.len() <= limit {
        return (rows, None);
    }
    rows.truncate(limit);
    let next = rows.last().map(|row| base64url_encode(id(row).as_bytes()));
    (rows, next)
}

async fn list(
    pool: &PgPool,
    identity: &Identity,
    query: &str,
    now: DateTime<Utc>,
) -> Result<Response<Body>, Failure> {
    let params = parse_list(query)?;
    let rows = store::list_debug_captures_page_for_tenant(
        pool,
        &TenantCapturePageQuery {
            tenant_id: &identity.tenant_id,
            window_id: &params.window_id,
            cursor_id: &params.cursor_id,
            page_limit: params.limit + 1,
        },
    )
    .await
    .map_err(Failure::store)?;
    let (rows, next) = page(rows, params.limit, |row| row.id.as_str());
    let captures = rows
        .iter()
        .map(|row| capture_entry(row, now))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(json(
        StatusCode::OK,
        &object(vec![
            ("captures", JsonValue::Array(captures)),
            ("next_cursor", optional(next.as_deref())),
        ]),
    ))
}

/// One list entry: no tenant id and no prompt text.
fn capture_entry(row: &TenantCaptureSummary, now: DateTime<Utc>) -> Result<JsonValue, Failure> {
    Ok(object(vec![
        ("id", string(&row.id)),
        ("window_id", optional(row.window_id.as_deref())),
        ("recommendation_id", string(&row.recommendation_id)),
        ("prompt_hash", optional(row.prompt_hash.as_deref())),
        ("classification", jsonb(row.classification.get())?),
        ("captured_at", time(row.captured_at)),
        ("expires_at", time(row.expires_at)),
        ("expired", JsonValue::Bool(row.expires_at <= now)),
    ]))
}

// ---- prompt reads ----

/// Part 4 §4.6 `ReadPromptForTenant`. A miss logs `read` under the caller's tenant for
/// the probed id, then answers 410 when the caller's capture was hard-deleted, else 404.
async fn read_prompt(
    pool: &PgPool,
    capturer: Option<&Capturer>,
    identity: &Identity,
    id: &str,
    now: DateTime<Utc>,
) -> Result<Response<Body>, Failure> {
    let capture = match store::get_debug_capture_for_tenant(pool, id, &identity.tenant_id).await {
        Ok(capture) => capture,
        Err(error) if is_not_found(&error) => {
            log_access(
                pool,
                id,
                &identity.tenant_id,
                &identity.api_key_id,
                ACTION_READ,
            )
            .await?;
            return Err(missing(pool, id, &identity.tenant_id).await);
        }
        Err(error) => return Err(Failure::store(error)),
    };
    let read = read_tail(pool, capturer, capture, &identity.api_key_id, now).await?;
    Ok(json(
        StatusCode::OK,
        &object(vec![
            ("id", string(&read.capture.id)),
            ("prompt", string(&read.prompt)),
            (
                "prompt_hash",
                optional(read.recommendation.prompt_hash.as_deref()),
            ),
            ("recommendation_id", string(&read.capture.recommendation_id)),
            (
                "classification",
                jsonb(read.recommendation.classification.get())?,
            ),
            ("captured_at", time(read.capture.captured_at)),
            ("expires_at", time(read.capture.expires_at)),
        ]),
    ))
}

/// A prompt read past its lookup. It holds the prompt, so it has no `Debug`.
pub(crate) struct Read {
    pub(crate) capture: DebugCapture,
    pub(crate) prompt: String,
    pub(crate) recommendation: Recommendation,
}

/// The shared tail of the tenant and operator prompt reads (Part 4 §4.6): an expired
/// capture logs `read` under its tenant and is 410. Otherwise the prompt is decrypted,
/// the recommendation loaded, and `read` logged.
pub(crate) async fn read_tail(
    pool: &PgPool,
    capturer: Option<&Capturer>,
    capture: DebugCapture,
    actor: &str,
    now: DateTime<Utc>,
) -> Result<Read, Failure> {
    if capture.expires_at <= now {
        log_access(pool, &capture.id, &capture.tenant_id, actor, ACTION_READ).await?;
        return Err(Failure::Expired);
    }
    let prompt = open(capturer, &capture)?;
    let recommendation = recommendation(pool, &capture).await?;
    log_access(pool, &capture.id, &capture.tenant_id, actor, ACTION_READ).await?;
    Ok(Read {
        capture,
        prompt,
        recommendation,
    })
}

// ---- shared ----

/// Appends a `capture_access_log` row. A failed append fails the call.
pub(crate) async fn log_access(
    pool: &PgPool,
    capture_id: &str,
    tenant_id: &str,
    actor: &str,
    action: &str,
) -> Result<(), Failure> {
    store::insert_capture_access_log(
        pool,
        &ids::new(ids::prefix::CAPTURE_ACCESS),
        capture_id,
        tenant_id,
        actor,
        action,
    )
    .await
    .map(|_| ())
    .map_err(Failure::store)
}

/// A tenant-scoped miss: 410 when a `delete` log row shows the capture was hard-deleted,
/// else 404.
async fn missing(pool: &PgPool, capture_id: &str, tenant_id: &str) -> Failure {
    match store::capture_was_deleted(pool, capture_id, tenant_id).await {
        Ok(true) => Failure::Expired,
        Ok(false) => Failure::NotFound,
        Err(error) => Failure::store(error),
    }
}

/// The capture's prompt, decrypted in memory.
fn open(capturer: Option<&Capturer>, capture: &DebugCapture) -> Result<String, Failure> {
    capturer
        .ok_or_else(|| Failure::Internal("debug capture has no key ring".to_owned()))?
        .open(capture)
        .map_err(Failure::Internal)
}

/// The recommendation (trace row) the capture belongs to.
async fn recommendation(pool: &PgPool, capture: &DebugCapture) -> Result<Recommendation, Failure> {
    decisions::get_recommendation_for_tenant(pool, &capture.recommendation_id, &capture.tenant_id)
        .await
        .map_err(Failure::store)
}

/// A jsonb pass-through value: Postgres's text with the whitespace removed (Part 5 §3
/// rule 9). An empty stored value is `{}`.
pub(crate) fn jsonb(text: &str) -> Result<JsonValue, Failure> {
    if text.trim().is_empty() {
        return Ok(JsonValue::Object(Vec::new()));
    }
    gojson::unmarshal(text)
        .map_err(|error| Failure::Internal(format!("stored classification: {error}")))
}

/// The member `key` of an object (the last one when repeated).
fn member<'a>(value: &'a JsonValue, key: &str) -> Option<&'a JsonValue> {
    match value {
        JsonValue::Object(members) => members
            .iter()
            .rev()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value),
        _ => None,
    }
}

/// A float64 detail as Go's encoder writes it (`0`, not `0.0`).
fn go_float(x: f64) -> Value {
    gojson::encode_f64(x)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(Value::Null)
}

pub(crate) fn object(members: Vec<(&str, JsonValue)>) -> JsonValue {
    JsonValue::Object(
        members
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    )
}

pub(crate) fn string(text: &str) -> JsonValue {
    JsonValue::String(text.to_owned())
}

pub(crate) fn optional(text: Option<&str>) -> JsonValue {
    text.map_or(JsonValue::Null, string)
}

pub(crate) fn time(at: DateTime<Utc>) -> JsonValue {
    JsonValue::String(rfc3339_nano(at))
}

/// The compact body and its one `\n`.
pub(crate) fn json(status: StatusCode, body: &JsonValue) -> Response<Body> {
    let mut text = gojson::marshal(body);
    text.push('\n');
    response_with_json(status, text.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(body: &str) -> Result<WindowRequest, String> {
        decode_window(body.as_bytes()).map_err(|error| error.message)
    }

    #[test]
    fn window_bodies_decode_like_go() {
        assert_eq!(
            window(r#"{"api_key_id":"key_1","confidence_below":0.8,"duration_minutes":30}"#),
            Ok(WindowRequest {
                api_key_id: Some("key_1".to_owned()),
                confidence_below: Some(0.8),
                duration_minutes: Some(30),
            })
        );
        // Case-insensitive keys, the last duplicate wins, and null clears a pointer.
        assert_eq!(
            window(r#"{"DURATION_MINUTES":5,"duration_minutes":7,"api_key_id":null}"#),
            Ok(WindowRequest {
                duration_minutes: Some(7),
                ..WindowRequest::default()
            })
        );
        assert_eq!(window("null"), Ok(WindowRequest::default()));
        assert_eq!(
            window(r#"{"duration_minutes":60,"nope":true}"#),
            Err(r#"malformed request body: json: unknown field "nope""#.to_owned())
        );
        assert_eq!(
            window(r#"{"duration_minutes":1.5}"#),
            Err(
                "malformed request body: json: cannot unmarshal number 1.5 into Go struct \
                 field captureWindowRequest.duration_minutes of type int"
                    .to_owned()
            )
        );
        assert_eq!(window(""), Err("request body is required".to_owned()));
    }

    #[test]
    fn replay_bodies_are_optional() {
        let replay = |body: &str| decode_replay(body.as_bytes()).map_err(|error| error.message);
        assert_eq!(replay(""), Ok(None));
        assert_eq!(replay(" \n"), Ok(None));
        assert_eq!(replay("{}"), Ok(None));
        assert_eq!(
            replay(r#"{"Classifier_Version":"a/b/v2"}"#),
            Ok(Some("a/b/v2".to_owned()))
        );
        assert_eq!(replay(r#"{"classifier_version":null}"#), Ok(None));
        assert_eq!(
            replay(r#"{"x":1}"#),
            Err(r#"malformed request body: json: unknown field "x""#.to_owned())
        );
        assert_eq!(
            replay("{}{}"),
            Err("request body has trailing content".to_owned())
        );
    }

    fn params(query: &str) -> Result<ListParams, String> {
        parse_list(query).map_err(|failure| match failure {
            Failure::Api(error) => format!("{} {:?}", error.message, error.details),
            other => format!("{other:?}"),
        })
    }

    #[test]
    fn list_parameters_follow_the_reference_order() {
        assert_eq!(
            params(""),
            Ok(ListParams {
                window_id: String::new(),
                limit: 50,
                cursor_id: String::new(),
            })
        );
        assert_eq!(
            params("window_id=cw_1&limit=200&cursor=Y2FwXzE&limit=2"),
            Ok(ListParams {
                window_id: "cw_1".to_owned(),
                limit: 200,
                cursor_id: "cap_1".to_owned(),
            })
        );
        // Go `ParseInt` takes a sign; a raw `+` in a query is a space.
        assert_eq!(params("limit=%2B1").map(|p| p.limit), Ok(1));
        assert!(params("limit=+1").is_err());
        assert_eq!(params("limit=&cursor=").map(|p| p.limit), Ok(50));
        assert_eq!(
            params("limit=0&tenant_id=t"),
            Err(
                r#"unknown query parameter "tenant_id" {"parameter": String("tenant_id")}"#
                    .to_owned()
            )
        );
        // `limit` is checked before a bad `cursor`.
        for bad in ["0", "201", "-1", "1.5", "x", "2147483648"] {
            assert_eq!(
                params(&format!("cursor=not-valid&limit={bad}")),
                Err(format!(
                    r#"limit "{bad}" must be an integer between 1 and 200 {{"parameter": String("limit"), "value": String("{bad}")}}"#
                )),
                "{bad}"
            );
        }
        // Outside the URL alphabet, padded, a lone trailing character, or nothing at all
        // once `\n` is skipped.
        for (sent, raw) in [
            ("not%20a%20token", "not a token"),
            ("Y2Fw%3D", "Y2Fw="),
            ("A", "A"),
            ("Y2F%2B", "Y2F+"),
            ("%0A", "\n"),
        ] {
            assert_eq!(
                params(&format!("cursor={sent}")),
                Err(format!(
                    r#"cursor {} must be an opaque pagination token {{"parameter": String("cursor"), "value": String({raw:?})}}"#,
                    quote(raw),
                )),
                "{sent}"
            );
        }
        // Go sent non-UTF-8 cursor bytes to Postgres, which refused them.
        assert!(matches!(parse_cursor("_w"), Err(Failure::Internal(_))));
    }

    #[test]
    fn pages_peek_one_row() {
        let ids = |n: usize| (0..n).map(|i| format!("cap_{i}")).collect::<Vec<_>>();
        let (rows, next) = page(ids(3), 2, String::as_str);
        assert_eq!(rows, ["cap_0", "cap_1"]);
        assert_eq!(next.as_deref(), Some(base64url_encode(b"cap_1").as_str()));
        assert_eq!(page(ids(2), 2, String::as_str), (ids(2), None));
        assert_eq!(page(ids(0), 2, String::as_str), (ids(0), None));
    }

    #[test]
    fn match_needs_the_same_capability_and_difficulty() {
        let stored = gojson::unmarshal(
            r#"{"method": "classified", "capability": "structured_extraction", "difficulty": 3}"#,
        )
        .unwrap();
        let mut replayed = Classification::fallback();
        replayed.capability = "structured_extraction".to_owned();
        assert!(same_workload(&stored, &replayed));
        replayed.difficulty = 4;
        assert!(!same_workload(&stored, &replayed));
        replayed.difficulty = 3;
        replayed.capability = "code_generation".to_owned();
        assert!(!same_workload(&stored, &replayed));
        assert!(!same_workload(
            &JsonValue::Null,
            &Classification::fallback()
        ));
    }

    #[test]
    fn replayed_blocks_and_details_use_go_forms() {
        let block = classification_block(
            &Classification::fallback(),
            classify::Method::FallbackDefault,
        )
        .unwrap();
        assert_eq!(
            gojson::marshal(&block),
            r#"{"capability":"multi_step_reasoning","difficulty":3,"domain_tags":[],"high_stakes":false,"confidence":0,"method":"fallback_default"}"#
        );
        let mut truncated = Classification::fallback();
        truncated.truncated = true;
        assert!(
            gojson::marshal(
                &classification_block(&truncated, classify::Method::Classified).unwrap()
            )
            .ends_with(r#""method":"classified","truncated":true}"#)
        );
        assert_eq!(go_float(0.0).to_string(), "0");
        assert_eq!(go_float(1.5).to_string(), "1.5");
        assert_eq!(
            jsonb(r#"{"a": 3.0, "b": [1, "x"]}"#)
                .map(|v| gojson::marshal(&v))
                .ok(),
            Some(r#"{"a":3.0,"b":[1,"x"]}"#.to_owned())
        );
        assert_eq!(
            jsonb("").map(|v| gojson::marshal(&v)).ok(),
            Some("{}".to_owned())
        );
    }
}
