//! `GET /v1/history/*` (spec §9.6; reference Part 5 §6.9 to §6.13, Part 4 §6): the tenant
//! audit log and one full decision trace, the tenant's outcomes, a column's aggregate
//! series, and the calibration series.
//!
//! Query parameters are checked in Go's order: an unknown name; `limit` (default 50, a
//! decimal int32 in 1..=200); `difficulty` (model series only: a decimal int16); `since`
//! (RFC 3339, else `YYYY-MM-DD` at UTC midnight); then the endpoint's own checks and the
//! cursor. A repeated name keeps its first value, and an empty value is not supplied.
//!
//! Each page fetches `limit + 1` rows. `next_cursor` is minted from the last returned row
//! only when the extra row came back; otherwise it is `null`. Cursors are kind-tagged, so a
//! cursor from one endpoint is refused by another. `matrix_version` is the active corpus
//! version, or `""` when none is active. Times are Go's RFC 3339 form in UTC.
//!
//! The recommendation log and outcomes are tenant scoped: another tenant's recommendation
//! is the same 404 as a missing one, and filtering outcomes by it gives an empty page. The
//! model series and calibration are global. D16: the `outcome` filter also takes
//! `completed_unchecked`.

use std::collections::HashSet;

use axum::body::Body;
use axum::extract::{Extension, Path, Request, State};
use axum::http::{Method, Response, StatusCode};
use chrono::{DateTime, FixedOffset, NaiveDate, NaiveTime, Utc};
use maverick_domain::gofmt::quote;
use maverick_domain::gojson::{self, JsonValue};
use maverick_domain::history::{
    self, AggregatePosition, INVALID_CURSOR, INVALID_CURSOR_PARAMETER, KIND_CALIBRATION,
    KIND_OUTCOME, KIND_RECOMMENDATION,
};
use maverick_domain::scoring::observe::dimension;
use maverick_domain::tasks::CAPABILITIES;
use maverick_domain::telemetry::outcome;
use maverick_store::alerts::{self, Alert};
use maverick_store::decisions::{
    self, Outcome, OutcomePageQuery, Recommendation, RecommendationPageQuery,
};
use maverick_store::measurement::{
    self, Aggregate, AggregateCursor, AggregateHistoryQuery, Calibration, CalibrationPageQuery,
};
use maverick_store::{catalog, corpus, is_not_found};
use sqlx::PgPool;

use crate::health::rfc3339_nano;
use crate::{ApiError, ApiState, ErrorCode, Identity, internal_error, not_found};

/// The page size when `limit` is not supplied.
const DEFAULT_LIMIT: i32 = 50;
/// The largest page.
const MAX_LIMIT: i32 = 200;

/// The dimensions a model series can show, in Go's order.
const DIMENSIONS: [&str; 6] = [
    dimension::QUALITY,
    dimension::PASS_RATE,
    dimension::AMBIGUITY,
    dimension::LATENCY_P50_MS,
    dimension::LATENCY_P95_MS,
    dimension::COST_PER_RUN_USD,
];

/// The outcome filter's values: Go's six, with D16's `completed_unchecked` in its schema
/// position (D40).
const OUTCOMES: [&str; 7] = [
    outcome::SUCCESS,
    outcome::PARSE_FAILURE,
    outcome::QUALITY_INSUFFICIENT,
    outcome::LATENCY_EXCEEDED,
    outcome::PROVIDER_ERROR,
    outcome::COMPLETED_UNCHECKED,
    outcome::OTHER,
];

enum Failure {
    Api(ApiError),
    NotFound,
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

// ---- handlers ----

/// `GET /v1/history/recommendations`.
pub(crate) async fn recommendations_handler(
    State(api): State<ApiState>,
    Extension(identity): Extension<Identity>,
    request: Request,
) -> Response<Body> {
    let query = query_of(&request);
    serve(
        request.method(),
        &api,
        "/v1/history/recommendations",
        |pool| recommendations(pool, &identity.tenant_id, query),
    )
    .await
}

/// `GET /v1/history/recommendations/{id}`.
pub(crate) async fn recommendation_handler(
    State(api): State<ApiState>,
    Extension(identity): Extension<Identity>,
    Path(id): Path<String>,
    request: Request,
) -> Response<Body> {
    serve(
        request.method(),
        &api,
        "/v1/history/recommendations/{id}",
        |pool| recommendation(pool, &identity.tenant_id, id),
    )
    .await
}

/// `GET /v1/history/outcomes`.
pub(crate) async fn outcomes_handler(
    State(api): State<ApiState>,
    Extension(identity): Extension<Identity>,
    request: Request,
) -> Response<Body> {
    let query = query_of(&request);
    serve(request.method(), &api, "/v1/history/outcomes", |pool| {
        outcomes(pool, &identity.tenant_id, query)
    })
    .await
}

/// `GET /v1/history/models/{column_id}`.
pub(crate) async fn model_handler(
    State(api): State<ApiState>,
    Path(column_id): Path<String>,
    request: Request,
) -> Response<Body> {
    let query = query_of(&request);
    serve(
        request.method(),
        &api,
        "/v1/history/models/{column_id}",
        |pool| model_series(pool, column_id, query),
    )
    .await
}

/// `GET /v1/history/calibration`.
pub(crate) async fn calibration_handler(
    State(api): State<ApiState>,
    request: Request,
) -> Response<Body> {
    let query = query_of(&request);
    serve(request.method(), &api, "/v1/history/calibration", |pool| {
        calibration(pool, query)
    })
    .await
}

fn query_of(request: &Request) -> String {
    request.uri().query().unwrap_or_default().to_owned()
}

/// GET only (any other method is the plain 404), then one endpoint's body.
async fn serve<'p, F, Fut>(method: &Method, api: &'p ApiState, path: &str, run: F) -> Response<Body>
where
    F: FnOnce(&'p PgPool) -> Fut,
    Fut: Future<Output = Result<String, Failure>>,
{
    if method != Method::GET {
        return not_found();
    }
    let Some(pool) = api.pool() else {
        return internal_error();
    };
    match run(pool).await {
        Ok(body) => crate::response_with_json(StatusCode::OK, body.into_bytes()),
        Err(Failure::Api(error)) => error.into_response(),
        Err(Failure::NotFound) => not_found(),
        Err(Failure::Internal(error)) => {
            tracing::error!(
                method = "GET",
                path = path,
                error = %error,
                "history handler failure"
            );
            internal_error()
        }
    }
}

// ---- endpoints ----

async fn recommendations(pool: &PgPool, tenant: &str, query: String) -> Result<String, Failure> {
    let params = Params::parse(
        &query,
        &["prompt_hash", "capability", "since", "limit", "cursor"],
    )?;
    let limit = limit(params.get("limit"))?;
    let since = since(params.get("since"))?;
    let capability = params.get("capability").unwrap_or_default();
    check_capability(capability)?;
    let cursor_id = match params.get("cursor") {
        Some(token) => single_field(token, KIND_RECOMMENDATION)?,
        None => String::new(),
    };
    let rows = decisions::list_recommendations_page(
        pool,
        &RecommendationPageQuery {
            tenant_id: tenant,
            prompt_hash: params.get("prompt_hash").unwrap_or_default(),
            capability,
            since: since.map(|at| at.with_timezone(&Utc)),
            cursor_id: &cursor_id,
            page_limit: limit + 1,
        },
    )
    .await?;
    let (rows, next) = page(rows, limit, |last| {
        history::encode_cursor(KIND_RECOMMENDATION, &[&last.id])
    });
    let items = rows
        .iter()
        .map(summary)
        .collect::<Result<Vec<_>, _>>()
        .map_err(Failure::Internal)?;
    listing(pool, "recommendations", items, next).await
}

async fn recommendation(pool: &PgPool, tenant: &str, id: String) -> Result<String, Failure> {
    let row = match decisions::get_recommendation_for_tenant(pool, &id, tenant).await {
        Ok(row) => row,
        Err(error) if is_not_found(&error) => return Err(Failure::NotFound),
        Err(error) => return Err(error.into()),
    };
    let body = JsonValue::Object(vec![
        string("id", &row.id),
        string("api_key_id", &row.api_key_id),
        (
            "prompt_hash".to_owned(),
            optional_string(row.prompt_hash.as_deref()),
        ),
        ("request".to_owned(), jsonb(row.request.get())?),
        (
            "classification".to_owned(),
            jsonb(row.classification.get())?,
        ),
        ("candidates".to_owned(), jsonb(row.candidates.get())?),
        ("response".to_owned(), jsonb(row.response.get())?),
        string("matrix_version", &row.matrix_version),
        string("created_at", &rfc3339_nano(row.created_at)),
    ]);
    Ok(document(&body))
}

async fn outcomes(pool: &PgPool, tenant: &str, query: String) -> Result<String, Failure> {
    let params = Params::parse(
        &query,
        &["recommendation_id", "outcome", "since", "limit", "cursor"],
    )?;
    let limit = limit(params.get("limit"))?;
    let since = since(params.get("since"))?;
    let outcome = params.get("outcome").unwrap_or_default();
    if !outcome.is_empty() && !OUTCOMES.contains(&outcome) {
        return Err(not_one_of("outcome", outcome, &OUTCOMES).into());
    }
    let cursor_id = match params.get("cursor") {
        Some(token) => single_field(token, KIND_OUTCOME)?,
        None => String::new(),
    };
    let rows = decisions::list_outcomes_page(
        pool,
        &OutcomePageQuery {
            tenant_id: tenant,
            recommendation_id: params.get("recommendation_id").unwrap_or_default(),
            outcome,
            since: since.map(|at| at.with_timezone(&Utc)),
            cursor_id: &cursor_id,
            page_limit: limit + 1,
        },
    )
    .await?;
    let (rows, next) = page(rows, limit, |last| {
        history::encode_cursor(KIND_OUTCOME, &[&last.id])
    });
    let items = rows
        .iter()
        .map(outcome_entry)
        .collect::<Result<Vec<_>, _>>()?;
    listing(pool, "outcomes", items, next).await
}

async fn model_series(pool: &PgPool, column_id: String, query: String) -> Result<String, Failure> {
    let params = Params::parse(
        &query,
        &[
            "capability",
            "difficulty",
            "dimension",
            "since",
            "limit",
            "cursor",
        ],
    )?;
    let limit = limit(params.get("limit"))?;
    let difficulty = match params.get("difficulty") {
        Some(raw) => Some(
            raw.parse::<i16>()
                .map_err(|_| invalid_param("difficulty", raw, "an integer difficulty tier"))?,
        ),
        None => None,
    };
    let since = since(params.get("since"))?;
    // The service's checks, before the column lookup.
    let dimension = params.get("dimension").unwrap_or(dimension::QUALITY);
    if !DIMENSIONS.contains(&dimension) {
        return Err(not_one_of("dimension", dimension, &DIMENSIONS).into());
    }
    let capability = params.get("capability").unwrap_or_default();
    check_capability(capability)?;
    if let Some(tier) = difficulty.filter(|tier| !(1..=5).contains(tier)) {
        return Err(invalid(format!("difficulty {tier} is out of range 1-5"))
            .detail("difficulty", tier)
            .into());
    }
    let cursor = match params.get("cursor") {
        Some(token) => Some(history::decode_aggregate_cursor(token).map_err(|_| bad_cursor())?),
        None => None,
    };
    match catalog::get_model_column(pool, &column_id).await {
        Ok(_) => {}
        Err(error) if is_not_found(&error) => return Err(Failure::NotFound),
        Err(error) => return Err(error.into()),
    }

    let cursor = cursor.map(|position: AggregatePosition| AggregateCursor {
        as_of: position.as_of,
        capability: position.capability,
        difficulty_tier: position.tier,
    });
    let rows = measurement::list_aggregate_history_page(
        pool,
        &AggregateHistoryQuery {
            model_column_id: &column_id,
            dimension,
            capability,
            difficulty,
            // A timestamp's own calendar date, as Go handed it to a `date` parameter.
            since: since.map(|at| at.date_naive()),
            cursor: cursor.as_ref(),
            page_limit: limit + 1,
        },
    )
    .await?;
    let (rows, next) = page(rows, limit, |last| {
        history::encode_aggregate_cursor(&AggregatePosition {
            as_of: last.as_of,
            capability: last.capability.clone(),
            tier: last.difficulty_tier,
        })
    });
    let markers = regression_markers(
        &alerts::list_regression_alerts_for_column(pool, &column_id).await?,
        dimension,
    );
    let series = rows
        .iter()
        .map(|row| point(row, &markers))
        .collect::<Result<Vec<_>, _>>()
        .map_err(Failure::Internal)?;
    let body = JsonValue::Object(vec![
        string("column_id", &column_id),
        string("dimension", dimension),
        ("series".to_owned(), JsonValue::Array(series)),
        ("next_cursor".to_owned(), next),
        string("matrix_version", &active_version(pool).await?),
    ]);
    Ok(document(&body))
}

async fn calibration(pool: &PgPool, query: String) -> Result<String, Failure> {
    let params = Params::parse(&query, &["capability", "since", "limit", "cursor"])?;
    let limit = limit(params.get("limit"))?;
    let since = since(params.get("since"))?;
    let capability = params.get("capability").unwrap_or_default();
    check_capability(capability)?;
    let (cursor_batch, cursor_capability) = match params.get("cursor") {
        Some(token) => {
            let mut fields = history::decode_cursor(token, KIND_CALIBRATION, 2)
                .map_err(|_| bad_cursor())?
                .into_iter();
            let batch = fields.next().unwrap_or_default();
            (batch, fields.next().unwrap_or_default())
        }
        None => (String::new(), String::new()),
    };
    let rows = measurement::list_calibration_page(
        pool,
        &CalibrationPageQuery {
            capability,
            since: since.map(|at| at.with_timezone(&Utc)),
            cursor_batch: &cursor_batch,
            cursor_capability: &cursor_capability,
            page_limit: limit + 1,
        },
    )
    .await?;
    let (rows, next) = page(rows, limit, |last| {
        history::encode_cursor(KIND_CALIBRATION, &[&last.batch_id, &last.capability])
    });
    let items = rows
        .iter()
        .map(calibration_entry)
        .collect::<Result<Vec<_>, _>>()
        .map_err(Failure::Internal)?;
    listing(pool, "calibration", items, next).await
}

// ---- query parameters ----

/// The first value of each known parameter. An unknown name is refused, and an empty value
/// is not supplied.
struct Params(Vec<(&'static str, String)>);

impl Params {
    fn parse(query: &str, known: &[&'static str]) -> Result<Self, ApiError> {
        let mut values: Vec<(&'static str, String)> = Vec::new();
        for (key, value) in crate::query::pairs(query) {
            let Some(name) = known.iter().find(|name| **name == key) else {
                return Err(invalid(format!("unknown query parameter {}", quote(&key)))
                    .detail("parameter", key.as_str()));
            };
            if !values.iter().any(|(seen, _)| seen == name) {
                values.push((name, value));
            }
        }
        Ok(Self(values))
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_str())
            .filter(|value| !value.is_empty())
    }
}

/// `limit`: a decimal int32 in 1..=200 (Go `strconv.ParseInt(raw, 10, 32)`), default 50.
fn limit(raw: Option<&str>) -> Result<i32, ApiError> {
    let Some(raw) = raw else {
        return Ok(DEFAULT_LIMIT);
    };
    match raw.parse::<i32>() {
        Ok(limit) if (1..=MAX_LIMIT).contains(&limit) => Ok(limit),
        _ => Err(invalid_param("limit", raw, "an integer between 1 and 200")),
    }
}

/// `since`: RFC 3339 with an optional fraction, else a `YYYY-MM-DD` date at UTC midnight.
fn since(raw: Option<&str>) -> Result<Option<DateTime<FixedOffset>>, ApiError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    parse_since(raw)
        .map(Some)
        .ok_or_else(|| invalid_param("since", raw, "an RFC 3339 timestamp or YYYY-MM-DD date"))
}

fn parse_since(raw: &str) -> Option<DateTime<FixedOffset>> {
    let bytes = raw.as_bytes();
    // Go's layouts: an upper-case `T` and `Z`, and a date of exactly `2006-01-02`.
    if bytes.get(10) == Some(&b'T') {
        if raw.ends_with('z') {
            return None;
        }
        return DateTime::parse_from_rfc3339(raw).ok();
    }
    let digits = |range: std::ops::Range<usize>| bytes[range].iter().all(u8::is_ascii_digit);
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    if !(digits(0..4) && digits(5..7) && digits(8..10)) {
        return None;
    }
    let date = NaiveDate::from_ymd_opt(
        raw[0..4].parse().ok()?,
        raw[5..7].parse().ok()?,
        raw[8..10].parse().ok()?,
    )?;
    Some(date.and_time(NaiveTime::MIN).and_utc().fixed_offset())
}

fn check_capability(capability: &str) -> Result<(), ApiError> {
    if capability.is_empty() || CAPABILITIES.contains(&capability) {
        return Ok(());
    }
    Err(invalid(format!(
        "capability {} is not a v1 capability",
        quote(capability)
    ))
    .detail("capability", capability))
}

/// A one-field cursor of `kind`.
fn single_field(token: &str, kind: &str) -> Result<String, ApiError> {
    history::decode_cursor(token, kind, 1)
        .map_err(|_| bad_cursor())?
        .pop()
        .ok_or_else(bad_cursor)
}

fn bad_cursor() -> ApiError {
    invalid(INVALID_CURSOR).detail("parameter", INVALID_CURSOR_PARAMETER)
}

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        ErrorCode::InvalidRequest,
        message,
    )
}

/// `<param> "<raw>" must be <want>`.
fn invalid_param(param: &str, raw: &str, want: &str) -> ApiError {
    invalid(format!("{param} {} must be {want}", quote(raw)))
        .detail("parameter", param)
        .detail("value", raw)
}

/// `<field> "<value>" is not one of [a b c]`.
fn not_one_of(field: &str, value: &str, allowed: &[&str]) -> ApiError {
    invalid(format!(
        "{field} {} is not one of [{}]",
        quote(value),
        allowed.join(" ")
    ))
    .detail("field", field)
    .detail("value", value)
}

// ---- bodies ----

/// The page of `limit` rows and its `next_cursor`: minted from the last returned row when
/// the extra row came back, else `null`.
fn page<T>(mut rows: Vec<T>, limit: i32, cursor: impl Fn(&T) -> String) -> (Vec<T>, JsonValue) {
    let limit = usize::try_from(limit).unwrap_or(0);
    if rows.len() <= limit {
        return (rows, JsonValue::Null);
    }
    rows.truncate(limit);
    let next = rows
        .last()
        .map_or(JsonValue::Null, |last| JsonValue::String(cursor(last)));
    (rows, next)
}

/// `{"<key>":[...],"next_cursor":...,"matrix_version":...}`.
async fn listing(
    pool: &PgPool,
    key: &str,
    items: Vec<JsonValue>,
    next: JsonValue,
) -> Result<String, Failure> {
    let body = JsonValue::Object(vec![
        (key.to_owned(), JsonValue::Array(items)),
        ("next_cursor".to_owned(), next),
        string("matrix_version", &active_version(pool).await?),
    ]);
    Ok(document(&body))
}

/// The active corpus version, or `""` when none is active.
async fn active_version(pool: &PgPool) -> Result<String, Failure> {
    match corpus::get_active_corpus_version(pool).await {
        Ok(version) => Ok(version.version),
        Err(error) if is_not_found(&error) => Ok(String::new()),
        Err(error) => Err(error.into()),
    }
}

/// One audit-log summary, from the stored classification and response: `pick` is the
/// first recommendation (or `null`), and `flags.clears_difficulty` is the pick's.
fn summary(row: &Recommendation) -> Result<JsonValue, String> {
    let classification = stored(row.classification.get())?;
    let response = stored(row.response.get())?;
    let pick = match member(&response, "recommendations") {
        Some(JsonValue::Array(entries)) => entries.first(),
        _ => None,
    };
    let pick_json = match pick {
        None | Some(JsonValue::Null) => JsonValue::Null,
        Some(entry) => JsonValue::Object(vec![
            string("provider", text_member(entry, "provider")),
            string("model", text_member(entry, "model")),
            string("endpoint", text_member(entry, "endpoint")),
            string("column_id", text_member(entry, "column_id")),
            ("score".to_owned(), float_member(entry, "score")?),
        ]),
    };
    Ok(JsonValue::Object(vec![
        string("id", &row.id),
        string("created_at", &rfc3339_nano(row.created_at)),
        string("capability", text_member(&classification, "capability")),
        (
            "difficulty".to_owned(),
            JsonValue::Number(int_member(&classification, "difficulty").to_string()),
        ),
        ("pick".to_owned(), pick_json),
        string("mode", text_member(&response, "mode")),
        string("pool", text_member(&response, "pool")),
        (
            "flags".to_owned(),
            JsonValue::Object(vec![
                bool_entry("constraints_met", bool_member(&response, "constraints_met")),
                bool_entry("measured", bool_member(&response, "measured")),
                bool_entry(
                    "clears_difficulty",
                    pick.is_some_and(|entry| bool_member(entry, "clears_difficulty")),
                ),
            ]),
        ),
    ]))
}

fn outcome_entry(row: &Outcome) -> Result<JsonValue, Failure> {
    let details = match &row.details {
        Some(details) => jsonb(details.get())?,
        None => JsonValue::Null,
    };
    Ok(JsonValue::Object(vec![
        string("id", &row.id),
        string("recommendation_id", &row.recommendation_id),
        string("model_used", &row.model_used),
        string("outcome", &row.outcome),
        (
            "escalated_to".to_owned(),
            optional_string(row.escalated_to.as_deref()),
        ),
        ("details".to_owned(), details),
        string("endpoint_used", &row.endpoint_used),
        string("created_at", &rfc3339_nano(row.created_at)),
    ]))
}

/// One series point; `ewma` and `variance` are float32.
fn point(row: &Aggregate, markers: &HashSet<Marker>) -> Result<JsonValue, String> {
    let as_of = row.as_of.format("%Y-%m-%d").to_string();
    let regression = markers.contains(&Marker {
        capability: row.capability.clone(),
        tier: i64::from(row.difficulty_tier),
        as_of: as_of.clone(),
    });
    Ok(JsonValue::Object(vec![
        string("as_of", &as_of),
        string("capability", &row.capability),
        (
            "difficulty".to_owned(),
            JsonValue::Number(row.difficulty_tier.to_string()),
        ),
        (
            "ewma".to_owned(),
            JsonValue::Number(gojson::encode_f32(row.ewma)?),
        ),
        (
            "variance".to_owned(),
            JsonValue::Number(gojson::encode_f32(row.variance)?),
        ),
        ("n".to_owned(), JsonValue::Number(row.n.to_string())),
        bool_entry("measured", row.measured),
        string("matrix_version", &row.matrix_version),
        bool_entry("regression", regression),
    ]))
}

fn calibration_entry(row: &Calibration) -> Result<JsonValue, String> {
    Ok(JsonValue::Object(vec![
        string("batch_id", &row.batch_id),
        string("capability", &row.capability),
        (
            "brier_score".to_owned(),
            JsonValue::Number(gojson::encode_f32(row.brier_score)?),
        ),
        (
            "overconfidence".to_owned(),
            JsonValue::Number(gojson::encode_f32(row.overconfidence)?),
        ),
        ("n".to_owned(), JsonValue::Number(row.n.to_string())),
        string("scored_at", &rfc3339_nano(row.scored_at)),
    ]))
}

// ---- regression markers ----

/// A regression alert's cell and night.
#[derive(Debug, PartialEq, Eq, Hash)]
struct Marker {
    capability: String,
    tier: i64,
    as_of: String,
}

/// The cells and nights that `regression` alerts name for `dimension`. A subject that does
/// not decode as `{model_column_id, capability, difficulty_tier, dimension, as_of}` (Go's
/// struct: strings, an integer tier) is skipped; an empty dimension means `quality`; a
/// subject for another dimension or without `as_of` is skipped.
fn regression_markers(alerts: &[Alert], dimension: &str) -> HashSet<Marker> {
    alerts
        .iter()
        .filter_map(|alert| {
            let subject = gojson::unmarshal(alert.subject.get()).ok()?;
            let JsonValue::Object(_) = subject else {
                return None;
            };
            let text = |key: &str| match member(&subject, key) {
                None | Some(JsonValue::Null) => Some(String::new()),
                Some(JsonValue::String(value)) => Some(value.clone()),
                Some(_) => None,
            };
            let _column = text("model_column_id")?;
            let capability = text("capability")?;
            let tier = match member(&subject, "difficulty_tier") {
                None | Some(JsonValue::Null) => 0,
                Some(JsonValue::Number(literal)) => literal.parse::<i64>().ok()?,
                Some(_) => return None,
            };
            let mut subject_dimension = text("dimension")?;
            let as_of = text("as_of")?;
            if subject_dimension.is_empty() {
                subject_dimension = dimension::QUALITY.to_owned();
            }
            (subject_dimension == dimension && !as_of.is_empty()).then_some(Marker {
                capability,
                tier,
                as_of,
            })
        })
        .collect()
}

// ---- JSON helpers ----

fn document(body: &JsonValue) -> String {
    let mut text = gojson::marshal(body);
    text.push('\n');
    text
}

/// A stored jsonb value, compacted, with its key order and number spelling.
fn jsonb(text: &str) -> Result<JsonValue, Failure> {
    gojson::unmarshal(text)
        .map_err(|error| Failure::Internal(format!("stored jsonb does not parse: {error}")))
}

fn stored(text: &str) -> Result<JsonValue, String> {
    gojson::unmarshal(text).map_err(|error| format!("stored jsonb does not parse: {error}"))
}

/// The last member named `key` (Go's last-wins rule).
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

fn text_member<'a>(value: &'a JsonValue, key: &str) -> &'a str {
    match member(value, key) {
        Some(JsonValue::String(text)) => text,
        _ => "",
    }
}

fn int_member(value: &JsonValue, key: &str) -> i64 {
    match member(value, key) {
        Some(JsonValue::Number(literal)) => literal.parse().unwrap_or_default(),
        _ => 0,
    }
}

fn bool_member(value: &JsonValue, key: &str) -> bool {
    matches!(member(value, key), Some(JsonValue::Bool(true)))
}

/// A float64 member re-encoded as Go writes a `*float64`, or `null`.
fn float_member(value: &JsonValue, key: &str) -> Result<JsonValue, String> {
    match member(value, key) {
        Some(JsonValue::Number(literal)) => {
            let number: f64 = literal
                .parse()
                .map_err(|_| format!("{key} {literal} is not a number"))?;
            Ok(JsonValue::Number(gojson::encode_f64(number)?))
        }
        _ => Ok(JsonValue::Null),
    }
}

fn string(key: &str, value: &str) -> (String, JsonValue) {
    (key.to_owned(), JsonValue::String(value.to_owned()))
}

fn optional_string(value: Option<&str>) -> JsonValue {
    value.map_or(JsonValue::Null, |value| JsonValue::String(value.to_owned()))
}

fn bool_entry(key: &str, value: bool) -> (String, JsonValue) {
    (key.to_owned(), JsonValue::Bool(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(error: ApiError) -> (String, serde_json::Value) {
        (
            error.message.clone(),
            serde_json::to_value(&error.details).unwrap(),
        )
    }

    #[test]
    fn params_keep_the_first_value_and_refuse_unknown_names() {
        let params = Params::parse(
            "limit=2&limit=9&cursor=&since=x",
            &["limit", "cursor", "since"],
        )
        .unwrap();
        assert_eq!(params.get("limit"), Some("2"));
        assert_eq!(params.get("cursor"), None, "an empty value is not supplied");
        assert_eq!(params.get("since"), Some("x"));
        let error = Params::parse("limit=2&offset=10&tier=3", &["limit"])
            .err()
            .unwrap();
        assert_eq!(
            message(error),
            (
                "unknown query parameter \"offset\"".to_owned(),
                serde_json::json!({"parameter": "offset"})
            )
        );
    }

    #[test]
    fn limit_is_a_decimal_int32_in_range() {
        assert_eq!(limit(None).unwrap(), 50);
        assert_eq!(limit(Some("1")).unwrap(), 1);
        assert_eq!(limit(Some("200")).unwrap(), 200);
        assert_eq!(limit(Some("+7")).unwrap(), 7, "Go ParseInt takes a sign");
        for bad in ["0", "-1", "201", "abc", "2.5", "2147483648", " 1"] {
            assert_eq!(
                message(limit(Some(bad)).unwrap_err()),
                (
                    format!("limit {} must be an integer between 1 and 200", quote(bad)),
                    serde_json::json!({"parameter": "limit", "value": bad})
                ),
                "{bad}"
            );
        }
    }

    #[test]
    fn since_is_rfc3339_or_a_date() {
        let at = parse_since("2026-09-28T06:07:51.128487-04:00").unwrap();
        assert_eq!(
            at.with_timezone(&Utc).to_rfc3339(),
            "2026-09-28T10:07:51.128487+00:00"
        );
        assert_eq!(at.date_naive().to_string(), "2026-09-28");
        assert_eq!(
            parse_since("2026-07-02").unwrap().to_rfc3339(),
            "2026-07-02T00:00:00+00:00"
        );
        for bad in [
            "yesterday",
            "2026-7-2",
            "2026-07-32",
            "2026-07-02t10:00:00Z",
            "2026-07-02 10:00:00Z",
            "2026-07-02T10:00:00z",
            "2026-07-02T10:00:00",
            "+2026-07-02",
        ] {
            assert!(parse_since(bad).is_none(), "{bad}");
        }
        assert_eq!(
            message(since(Some("yesterday")).unwrap_err()),
            (
                "since \"yesterday\" must be an RFC 3339 timestamp or YYYY-MM-DD date".to_owned(),
                serde_json::json!({"parameter": "since", "value": "yesterday"})
            )
        );
    }

    #[test]
    fn enum_errors_match_go() {
        assert_eq!(
            message(not_one_of("dimension", "vibes", &DIMENSIONS)),
            (
                "dimension \"vibes\" is not one of [quality pass_rate ambiguity latency_p50_ms latency_p95_ms cost_per_run_usd]".to_owned(),
                serde_json::json!({"field": "dimension", "value": "vibes"})
            )
        );
        assert_eq!(
            message(check_capability("nope").unwrap_err()),
            (
                "capability \"nope\" is not a v1 capability".to_owned(),
                serde_json::json!({"capability": "nope"})
            )
        );
        assert_eq!(
            message(bad_cursor()),
            (
                "cursor is not a valid pagination token".to_owned(),
                serde_json::json!({"parameter": "cursor"})
            )
        );
    }

    #[test]
    fn pages_mint_a_cursor_only_past_the_limit() {
        let cursor = |last: &u8| format!("after {last}");
        assert_eq!(
            page(vec![1, 2, 3], 2, cursor),
            (vec![1, 2], JsonValue::String("after 2".to_owned()))
        );
        assert_eq!(page(vec![1, 2], 2, cursor), (vec![1, 2], JsonValue::Null));
        assert_eq!(page(Vec::<u8>::new(), 2, cursor), (vec![], JsonValue::Null));
    }
}
