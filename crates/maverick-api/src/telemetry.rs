//! `POST /v1/telemetry`: the only way outcome data reaches the engine (G13; spec §9.10;
//! reference Part 5 §6.8, Part 4 §6.3).
//!
//! Only a malformed envelope is a 4xx. Each item decodes alone, and a malformed item,
//! an unknown recommendation id, and another tenant's id are all just counted as
//! dropped, so the endpoint is no existence oracle. Each accepted item appends one
//! `outcomes` row. There is no `POST /v1/outcomes`.
//!
//! D39: when the caller's tenant has opted out of telemetry, every item is dropped the
//! same way and no `outcomes` row is written. The response has the same shape.
//!
//! D44: an accepted 2xx item can move a conversation's affinity row to the chain entry
//! that served the turn (spec §9.10). It never changes the counts.

use std::collections::{HashMap, HashSet};
use std::fmt;

use axum::body::Body;
use axum::extract::{Extension, Request, State};
use axum::http::{Method, Response, StatusCode};
use maverick_domain::affinity::{ServedEntry, served_column};
use maverick_domain::gojson::match_field;
use maverick_domain::telemetry::TelemetryItem;
use maverick_domain::{ids, telemetry};
use maverick_store::{JsonRawValue, affinity, decisions, is_not_found, tenancy};
use serde::de::{MapAccess, Visitor};
use sqlx::PgPool;

use crate::{
    ApiError, ApiState, ErrorCode, Identity, internal_error, invalid_request, not_found, read_body,
    response_with_json,
};

/// The top-level members of the envelope, in document order.
struct Members(Vec<(String, Box<JsonRawValue>)>);

impl<'de> serde::Deserialize<'de> for Members {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct MembersVisitor;
        impl<'de> Visitor<'de> for MembersVisitor {
            type Value = Members;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Members, A::Error> {
                let mut members = Vec::new();
                while let Some(entry) = map.next_entry::<String, Box<JsonRawValue>>()? {
                    members.push(entry);
                }
                Ok(Members(members))
            }
        }
        deserializer.deserialize_map(MembersVisitor)
    }
}

/// Decodes `{"items": [...]}` leniently: unknown top-level members are ignored, the
/// `items` key matches case-insensitively and the last duplicate wins (Go
/// `encoding/json`). Returns each item as raw JSON text.
#[allow(clippy::result_large_err)]
fn decode_envelope(bytes: &[u8]) -> Result<Vec<Box<JsonRawValue>>, Response<Body>> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Err(invalid_request("request body is required"));
    }
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    // D25: the text after `malformed request body: ` is serde's, not Go's.
    let members = serde::Deserialize::deserialize(&mut deserializer)
        .map(|Members(members)| members)
        .map_err(|error| invalid_request(format!("malformed request body: {error}")))?;
    if deserializer.end().is_err() {
        return Err(invalid_request("request body has trailing content"));
    }
    let items = members
        .into_iter()
        .rfind(|(key, _)| match_field(key, &["items"]).is_some())
        .map(|(_, raw)| raw);
    let items = match items {
        Some(raw) if raw.get() != "null" => raw,
        _ => {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                ErrorCode::InvalidRequest,
                "items is required",
            )
            .detail("field", "items")
            .into_response());
        }
    };
    let items: Vec<Box<JsonRawValue>> = serde_json::from_str(items.get())
        .map_err(|error| invalid_request(format!("malformed request body: {error}")))?;
    if items.len() > telemetry::MAX_ITEMS {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::InvalidRequest,
            format!(
                "items has {} entries, exceeding the {}-entry limit",
                items.len(),
                telemetry::MAX_ITEMS
            ),
        )
        .detail("count", items.len())
        .detail("limit", telemetry::MAX_ITEMS)
        .into_response());
    }
    Ok(items)
}

/// `POST /v1/telemetry`. Answers `{"accepted":n,"dropped":m}`.
pub(crate) async fn telemetry(
    State(api): State<ApiState>,
    Extension(identity): Extension<Identity>,
    request: Request,
) -> Response<Body> {
    if request.method() != Method::POST {
        return not_found();
    }
    let bytes = match read_body(request.into_body()).await {
        Ok(bytes) => bytes,
        Err(response) => return response,
    };
    let raw_items = match decode_envelope(&bytes) {
        Ok(items) => items,
        Err(response) => return response,
    };
    let Some(pool) = api.pool() else {
        return internal_error();
    };

    // D39: an opted-out tenant's items are all dropped, and nothing is read or written.
    match tenancy::get_tenant_constraints(pool, &identity.tenant_id).await {
        Ok(row) if row.telemetry_opt_out => return counts(0, raw_items.len() as u64),
        Ok(_) => {}
        Err(error) if is_not_found(&error) => {}
        Err(error) => {
            tracing::error!(error = %error, "telemetry: constraints query failed");
            return internal_error();
        }
    }

    let mut dropped: u64 = 0;
    let mut items = Vec::with_capacity(raw_items.len());
    for raw in &raw_items {
        match telemetry::decode_item(raw.get()) {
            Ok(item) => items.push(item),
            Err(reason) => {
                tracing::debug!(reason = %reason.0, "telemetry item dropped");
                dropped += 1;
            }
        }
    }

    // One ownership query. An id that is unknown or another tenant's is dropped the
    // same way, so a caller learns nothing about other tenants.
    let owned: HashSet<String> = if items.is_empty() {
        HashSet::new()
    } else {
        let ids: Vec<String> = items.iter().map(|i| i.recommendation_id.clone()).collect();
        match decisions::recommendation_ids_for_tenant(pool, &identity.tenant_id, &ids).await {
            Ok(owned) => owned.into_iter().collect(),
            Err(error) => {
                tracing::error!(error = %error, "telemetry: ownership query failed");
                return internal_error();
            }
        }
    };

    let mut accepted: u64 = 0;
    for item in &items {
        if !owned.contains(&item.recommendation_id) {
            dropped += 1;
            continue;
        }
        let details =
            JsonRawValue::from_string(item.details_json()).expect("details_json writes valid JSON");
        let id = ids::new(ids::prefix::OUTCOME);
        if let Err(error) = decisions::create_outcome(
            pool,
            &id,
            &item.recommendation_id,
            &item.model_used,
            item.outcome(),
            &item.endpoint_used,
            Some(&details),
        )
        .await
        {
            tracing::error!(error = %error, "telemetry: writing an outcome failed");
            return internal_error();
        }
        accepted += 1;
    }
    if api.recommend().affinity.enabled {
        follow_served_columns(pool, &identity.tenant_id, &items, &owned).await;
    }
    counts(accepted, dropped)
}

/// D44: an accepted 2xx item that names a chain entry after the pick moves the
/// conversation's row to that entry's column (spec §9.10), with a compare and set on the
/// recommendation. A failure is logged and never fails the batch; the items stay
/// accepted.
async fn follow_served_columns(
    pool: &PgPool,
    tenant_id: &str,
    items: &[TelemetryItem],
    owned: &HashSet<String>,
) {
    let served: Vec<&TelemetryItem> = items
        .iter()
        .filter(|item| (200..300).contains(&item.status) && owned.contains(&item.recommendation_id))
        .collect();
    if served.is_empty() {
        return;
    }
    let ids: Vec<String> = served
        .iter()
        .map(|item| item.recommendation_id.clone())
        .collect();
    let follows: HashMap<String, (String, Vec<ServedEntry>)> =
        match decisions::affinity_follows(pool, tenant_id, &ids).await {
            Ok(rows) => rows
                .into_iter()
                .filter_map(|row| {
                    let chain = serde_json::from_str(row.chain.get()).ok()?;
                    Some((row.id, (row.affinity_hash, chain)))
                })
                .collect(),
            Err(error) => {
                tracing::warn!(error = %error, "telemetry: reading affinity follows failed");
                return;
            }
        };
    for item in served {
        let Some((hash, chain)) = follows.get(&item.recommendation_id) else {
            continue;
        };
        let Some(column_id) = served_column(chain, &item.model_used, &item.endpoint_used) else {
            continue;
        };
        if let Err(error) = affinity::follow_served_column(
            pool,
            tenant_id,
            hash,
            &item.recommendation_id,
            column_id,
        )
        .await
        {
            tracing::warn!(
                recommendation_id = %item.recommendation_id,
                error = %error,
                "telemetry: following the served column failed"
            );
        }
    }
}

/// `200 {"accepted":n,"dropped":m}`.
fn counts(accepted: u64, dropped: u64) -> Response<Body> {
    let body = format!("{{\"accepted\":{accepted},\"dropped\":{dropped}}}\n");
    response_with_json(StatusCode::OK, body.into_bytes())
}
