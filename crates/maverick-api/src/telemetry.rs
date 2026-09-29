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

use std::collections::HashSet;
use std::fmt;

use axum::body::Body;
use axum::extract::{Extension, Request, State};
use axum::http::{Method, Response, StatusCode};
use maverick_domain::gojson::match_field;
use maverick_domain::{ids, telemetry};
use maverick_store::{JsonRawValue, decisions, is_not_found, tenancy};
use serde::de::{MapAccess, Visitor};

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
    counts(accepted, dropped)
}

/// `200 {"accepted":n,"dropped":m}`.
fn counts(accepted: u64, dropped: u64) -> Response<Body> {
    let body = format!("{{\"accepted\":{accepted},\"dropped\":{dropped}}}\n");
    response_with_json(StatusCode::OK, body.into_bytes())
}
