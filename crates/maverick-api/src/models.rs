//! `GET /v1/models`: the column catalog (spec §9.4, §4.5; reference Part 5 §6.7;
//! openrouter spec §2.3; D1, D15).
//!
//! Every column with status `cold_start`, `active`, or `deprecated`, by column id. Query
//! parameters are ignored (QUIRK Q13). Not tenant scoped. No scores.
//!
//! Each entry starts with Go's eleven keys in Go's order: `column_id, provider, model_id,
//! endpoint, settings, execution_aliases, status, pricing, deprecation_date, measured,
//! quality_source_column_id`. `measured` means the column has at least one measured
//! aggregate row. The Rust fields follow, so a Go-shaped entry is a prefix:
//! - D1: `retention_mode` (effective: expired evidence reads as `unknown`),
//!   `retention_verified_at`, `retention_valid_until`, `retention_source`;
//! - D15: `execution_alias_retention`: each `execution_aliases` key, in that map's order,
//!   with its effective retention mode (openrouter spec §2.3). A key without stored
//!   evidence, or with expired evidence, reads as `unknown`.
//! - D29: `param_constraints`: the column's stored per-model parameter constraints,
//!   verbatim; `{}` means none.

use std::collections::HashSet;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{Method, Response, StatusCode};
use chrono::{DateTime, Utc};
use maverick_domain::gojson::{self, JsonValue};
use maverick_domain::policy::RetentionMode;
use maverick_store::catalog::{self, ModelColumn};
use maverick_store::{corpus, is_not_found, measurement};
use sqlx::PgPool;

use crate::health::rfc3339_nano;
use crate::{ApiState, internal_error, not_found};

/// The statuses the catalog shows.
const LISTED_STATUSES: [&str; 3] = ["cold_start", "active", "deprecated"];

/// `GET /v1/models`.
pub(crate) async fn models_handler(
    State(api): State<ApiState>,
    request: Request,
) -> Response<Body> {
    if request.method() != Method::GET {
        return not_found();
    }
    let Some(pool) = api.pool() else {
        return internal_error();
    };
    match models(pool, Utc::now()).await {
        Ok(body) => crate::response_with_json(StatusCode::OK, body.into_bytes()),
        Err(error) => {
            tracing::error!(
                method = "GET",
                path = "/v1/models",
                error = %error,
                "models handler failure"
            );
            internal_error()
        }
    }
}

/// The response body: compact JSON and one `\n`.
async fn models(pool: &PgPool, now: DateTime<Utc>) -> Result<String, String> {
    let statuses: Vec<String> = LISTED_STATUSES.iter().map(|s| (*s).to_owned()).collect();
    let columns = catalog::list_model_columns_by_statuses(pool, &statuses)
        .await
        .map_err(|error| error.to_string())?;
    let measured: HashSet<String> = measurement::list_measured_column_ids(pool)
        .await
        .map_err(|error| error.to_string())?
        .into_iter()
        .collect();
    let matrix_version = match corpus::get_active_corpus_version(pool).await {
        Ok(version) => version.version,
        Err(error) if is_not_found(&error) => String::new(),
        Err(error) => return Err(error.to_string()),
    };
    let entries = columns
        .iter()
        .map(|column| entry(column, measured.contains(&column.id), now))
        .collect::<Result<Vec<_>, _>>()?;
    let body = JsonValue::Object(vec![
        ("models".to_owned(), JsonValue::Array(entries)),
        (
            "matrix_version".to_owned(),
            JsonValue::String(matrix_version),
        ),
    ]);
    let mut text = gojson::marshal(&body);
    text.push('\n');
    Ok(text)
}

/// One catalog entry.
fn entry(column: &ModelColumn, measured: bool, now: DateTime<Utc>) -> Result<JsonValue, String> {
    let aliases = jsonb(column.execution_aliases.get())?;
    let per_alias = alias_retention(column, now)?;

    let mut members = vec![
        string("column_id", &column.id),
        string("provider", &column.provider),
        string("model_id", &column.model_id),
        string("endpoint", &column.endpoint),
        ("settings".to_owned(), jsonb(column.settings.get())?),
        ("execution_aliases".to_owned(), aliases),
        string("status", &column.status),
        ("pricing".to_owned(), jsonb(column.pricing.get())?),
        (
            "deprecation_date".to_owned(),
            column.deprecation_date.map_or(JsonValue::Null, |date| {
                JsonValue::String(date.format("%Y-%m-%d").to_string())
            }),
        ),
        ("measured".to_owned(), JsonValue::Bool(measured)),
        (
            "quality_source_column_id".to_owned(),
            optional_string(column.quality_source_column_id.as_deref()),
        ),
    ];
    // D1.
    members.extend(retention_fields(
        Some(column.retention_mode.as_str()),
        column.retention_verified_at,
        column.retention_valid_until,
        column.retention_source.as_deref(),
        now,
    ));
    // D15.
    members.push(("execution_alias_retention".to_owned(), per_alias));
    // D29.
    members.push((
        "param_constraints".to_owned(),
        jsonb(column.param_constraints.get())?,
    ));
    Ok(JsonValue::Object(members))
}

/// D15: `{alias: effective retention mode}` for each `execution_aliases` key, in that
/// map's order. Missing or expired evidence reads as `unknown` (spec §4.5).
pub(crate) fn alias_retention(
    column: &ModelColumn,
    now: DateTime<Utc>,
) -> Result<JsonValue, String> {
    let aliases = jsonb(column.execution_aliases.get())?;
    let stored = jsonb(column.execution_alias_retention.get())?;
    let JsonValue::Object(aliases) = aliases else {
        return Ok(JsonValue::Object(Vec::new()));
    };
    let mut seen = Vec::new();
    let mut modes = Vec::new();
    for (key, _) in aliases {
        if seen.contains(&key) {
            continue;
        }
        let evidence = member(&stored, &key);
        let mode = effective(
            evidence.and_then(|value| string_member(value, "retention_mode")),
            evidence
                .and_then(|value| string_member(value, "retention_valid_until"))
                .and_then(parse_time),
            now,
        );
        seen.push(key.clone());
        modes.push((key, JsonValue::String(mode.as_str().to_owned())));
    }
    Ok(JsonValue::Object(modes))
}

/// A stored mode, effective at `now`: only `zero` or `standard` with unexpired evidence
/// keeps its value; anything else reads as `unknown`.
fn effective(
    stored_mode: Option<&str>,
    valid_until: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> RetentionMode {
    let stored = match stored_mode {
        Some("zero") => RetentionMode::Zero,
        Some("standard") => RetentionMode::Standard,
        _ => RetentionMode::Unknown,
    };
    stored.effective(valid_until, now)
}

/// `retention_mode` (effective at `now`), `retention_verified_at`,
/// `retention_valid_until`, `retention_source`.
fn retention_fields(
    stored_mode: Option<&str>,
    verified_at: Option<DateTime<Utc>>,
    valid_until: Option<DateTime<Utc>>,
    source: Option<&str>,
    now: DateTime<Utc>,
) -> Vec<(String, JsonValue)> {
    let time = |at: Option<DateTime<Utc>>| {
        at.map_or(JsonValue::Null, |at| JsonValue::String(rfc3339_nano(at)))
    };
    vec![
        (
            "retention_mode".to_owned(),
            JsonValue::String(effective(stored_mode, valid_until, now).as_str().to_owned()),
        ),
        ("retention_verified_at".to_owned(), time(verified_at)),
        ("retention_valid_until".to_owned(), time(valid_until)),
        ("retention_source".to_owned(), optional_string(source)),
    ]
}

/// A stored jsonb value with its key order and number spelling; `null` is `{}`.
fn jsonb(text: &str) -> Result<JsonValue, String> {
    match gojson::unmarshal(text) {
        Ok(JsonValue::Null) => Ok(JsonValue::Object(Vec::new())),
        Ok(value) => Ok(value),
        Err(error) => Err(format!("stored jsonb does not parse: {error}")),
    }
}

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

fn string_member<'a>(value: &'a JsonValue, key: &str) -> Option<&'a str> {
    match member(value, key)? {
        JsonValue::String(text) => Some(text),
        _ => None,
    }
}

fn parse_time(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

fn string(key: &str, value: &str) -> (String, JsonValue) {
    (key.to_owned(), JsonValue::String(value.to_owned()))
}

fn optional_string(value: Option<&str>) -> JsonValue {
    value.map_or(JsonValue::Null, |value| JsonValue::String(value.to_owned()))
}
