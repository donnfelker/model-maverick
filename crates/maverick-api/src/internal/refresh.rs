//! `POST /internal/v1/models/refresh` (spec §4.6, §13; D51): run provider model
//! discovery now, write the same alerts as the daily `model_discovery` job, and, with
//! `{"enroll": [...]}`, add new models to the catalog as `cold_start` columns, all or
//! nothing. Enrollment spends operator money under G8, so the response carries the next
//! nightly batch's spend preview, and nothing enrolls without an explicit entry.

use std::collections::HashSet;

use maverick_domain::discovery::base_id;
use maverick_domain::spend::SpendDecision;
use maverick_jobs::batch::{SpendPreview, preview_spend};
use maverick_jobs::discovery::{self, Discovery, ProviderStatus};
use maverick_jobs::{JobArgs, JobKind};

use super::*;

/// The refresh body's fields.
const REFRESH_FIELDS: [&str; 1] = ["enroll"];

/// `POST /internal/v1/models/refresh`.
pub(crate) async fn models_refresh(
    State(api): State<ApiState>,
    Extension(actor): Extension<InternalActor>,
    request: Request,
) -> Response<Body> {
    if request.method() != Method::POST {
        return not_found();
    }
    respond("models refresh", refresh(&api, &actor, request).await)
}

/// The enrollment entries of a refresh body. An empty body, `{}`, or `{"enroll": null}`
/// only reports. Each entry takes the fields of `POST /internal/v1/columns`.
fn decode_refresh(body: &[u8]) -> Result<Vec<ColumnInput>, ApiError> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(Vec::new());
    }
    let value = strict::body_value(body)?;
    let mut entries = Vec::new();
    (|| -> Result<(), String> {
        let Some(members) = strict::members(&value, "refreshRequest", "")? else {
            return Ok(());
        };
        for (key, value) in members {
            if match_field(key, &REFRESH_FIELDS).is_none() {
                return Err(strict::unknown(key));
            }
            match value {
                JsonValue::Null => {}
                JsonValue::Array(items) => entries = items.clone(),
                other => {
                    return Err(strict::type_error(
                        other,
                        "refreshRequest",
                        "enroll",
                        "[]enrollEntry",
                    ));
                }
            }
        }
        Ok(())
    })()
    .map_err(malformed)?;
    entries
        .iter()
        .map(|entry| {
            let mut input = decode_column(
                gojson::marshal(entry).as_bytes(),
                "enrollEntry",
                &CREATE_FIELDS,
            )?;
            if input.endpoint.is_empty() {
                input.endpoint = DIRECT_ENDPOINT.to_owned();
            }
            Ok(input)
        })
        .collect()
}

/// The endpoint an enrollment entry defaults to.
const DIRECT_ENDPOINT: &str = "direct";

/// An entry for a provider that did not answer this run.
fn provider_not_ok(provider: &str, status: &str) -> ApiError {
    invalid(format!(
        "provider {provider} did not answer this refresh ({status}); enroll its models after it does"
    ))
    .detail("provider", provider)
    .detail("provider_status", status)
}

/// An entry whose model is not listed, or whose base id is already in the catalog.
fn not_new(provider: &str, model_id: &str, why: &str) -> ApiError {
    invalid(format!(
        "model {} of provider {provider} {why}",
        quote(model_id)
    ))
    .detail("provider", provider)
    .detail("model_id", model_id)
}

/// The run-dependent checks of one entry (docs/api.md): its provider answered, its
/// model is listed, and its base id is in this run's `new` set.
fn check_entry(run: &Discovery, input: &ColumnInput) -> Result<(), ApiError> {
    let status = run
        .providers
        .iter()
        .find(|report| report.provider == input.provider)
        .map_or(ProviderStatus::SkippedNoKey, |report| report.status);
    if status != ProviderStatus::Ok {
        return Err(provider_not_ok(&input.provider, status.as_str()));
    }
    let listed = run
        .listed
        .iter()
        .any(|model| model.provider == input.provider && model.model_id == input.model_id);
    if !listed {
        return Err(not_new(
            &input.provider,
            &input.model_id,
            "is not in the provider's list",
        ));
    }
    let base = base_id(&input.model_id);
    let new = run
        .new
        .iter()
        .any(|entry| entry.provider == input.provider && entry.base_model_id == base);
    if !new {
        return Err(not_new(
            &input.provider,
            &input.model_id,
            "is already covered by a catalog column",
        ));
    }
    Ok(())
}

/// The night of the next nightly batch after `now`: the next firing of the nightly
/// schedule, or the next day without schedules.
fn next_night(api: &ApiState, now: DateTime<Utc>) -> NaiveDate {
    api.internal()
        .jobs
        .as_ref()
        .and_then(|jobs| jobs.scheduled_jobs_after(now).ok())
        .and_then(|jobs| {
            jobs.into_iter().find_map(|job| match job.args {
                JobArgs::NightlySmoke(args) => {
                    NaiveDate::parse_from_str(&args.night, "%Y-%m-%d").ok()
                }
                _ => None,
            })
        })
        .unwrap_or_else(|| now.date_naive() + Duration::days(1))
}

fn decision(decision: SpendDecision) -> &'static str {
    match decision {
        SpendDecision::Allow => "allow",
        SpendDecision::Warn => "warn",
        SpendDecision::Refuse => "refuse",
    }
}

/// A float in Go's JSON form; a non-finite value (never a real projection) is `null`.
fn number(value: f64) -> JsonValue {
    gojson::encode_f64(value).map_or(JsonValue::Null, JsonValue::Number)
}

fn integer(value: impl ToString) -> JsonValue {
    JsonValue::Number(value.to_string())
}

/// The spend preview: what the next nightly batch projects with the new columns, and
/// what the G8 guard would decide.
fn spend_preview_json(preview: &SpendPreview) -> JsonValue {
    object(vec![
        ("job", string(preview.kind.as_str())),
        (
            "night",
            string(&preview.night.format("%Y-%m-%d").to_string()),
        ),
        ("runs", integer(preview.remaining.runs)),
        ("projected_usd", number(preview.remaining.projected_usd)),
        ("night_spent_usd", number(preview.night_spent_usd)),
        ("nightly_cap_usd", number(preview.caps.nightly_usd)),
        ("week_spent_usd", number(preview.week_spent_usd)),
        ("weekly_cap_usd", number(preview.caps.weekly_usd)),
        ("decision", string(decision(preview.decision))),
    ])
}

fn report_json(run: &Discovery) -> Vec<(&'static str, JsonValue)> {
    let providers = run
        .providers
        .iter()
        .map(|report| {
            let mut members = vec![
                ("provider", string(&report.provider)),
                ("status", string(report.status.as_str())),
            ];
            if let Some(listed) = report.listed {
                members.push(("listed", integer(listed)));
            }
            if let Some(error) = &report.error {
                members.push(("error", string(error)));
            }
            object(members)
        })
        .collect();
    let new = run
        .new
        .iter()
        .map(|entry| {
            let mut members = vec![
                ("provider", string(&entry.provider)),
                ("model_id", string(&entry.model_id)),
                ("base_model_id", string(&entry.base_model_id)),
            ];
            if let Some(name) = &entry.display_name {
                members.push(("display_name", string(name)));
            }
            if let Some(created) = entry.created_at {
                members.push(("created_at", time(created)));
            }
            object(members)
        })
        .collect();
    let withdrawn = run
        .withdrawn
        .iter()
        .map(|column| {
            object(vec![
                ("column_id", string(&column.column_id)),
                ("provider", string(&column.provider)),
                ("model_id", string(&column.model_id)),
            ])
        })
        .collect();
    vec![
        ("providers", JsonValue::Array(providers)),
        ("new", JsonValue::Array(new)),
        ("withdrawn", JsonValue::Array(withdrawn)),
    ]
}

async fn refresh(api: &ApiState, actor: &InternalActor, request: Request) -> Handled {
    let body = read_body(request.into_body())
        .await
        .map_err(|response| Failure::Response(Box::new(response)))?;
    let entries = decode_refresh(&body)?;
    let db = pool_of(api)?;
    let ran_at = api.internal().now();
    let run = discovery::discover(db, &api.internal().model_listers)
        .await
        .map_err(|error| Failure::Internal(error.to_string()))?;

    // Every entry passes every check before anything is written.
    let mut prepared = Vec::with_capacity(entries.len());
    let mut columns = HashSet::new();
    for input in entries {
        // Provider lists carry no prices, so an entry must bring its own.
        let has_pricing = input.pricing.is_some();
        let checked = prepare_column(api, input).await?;
        if !has_pricing {
            return Err(required_field("pricing").into());
        }
        let input = &checked.input;
        let column = (
            input.provider.clone(),
            input.model_id.clone(),
            input.endpoint.clone(),
            checked.settings.get().to_owned(),
        );
        if !columns.insert(column) {
            return Err(invalid(format!(
                "two enroll entries name the column {}/{}/{}",
                input.provider, input.model_id, input.endpoint
            ))
            .detail("provider", input.provider.as_str())
            .detail("model_id", input.model_id.as_str())
            .detail("endpoint", input.endpoint.as_str())
            .into());
        }
        check_entry(&run, input)?;
        prepared.push(checked);
    }

    let mut enrolled = Vec::with_capacity(prepared.len());
    if !prepared.is_empty() {
        let mut tx = db.begin().await?;
        for column in &prepared {
            enrolled.push(write_column(&mut *tx, api, column).await?);
        }
        tx.commit().await?;
        for column in &enrolled {
            audit::column_create(
                &actor.0,
                &column.id,
                &column.provider,
                &column.model_id,
                &column.endpoint,
                &column.status,
            );
        }
    }

    // The job's alerts, without the models this call just enrolled.
    let covered: HashSet<(&str, &str)> = enrolled
        .iter()
        .map(|column| (column.provider.as_str(), base_id(&column.model_id)))
        .collect();
    let mut alerted = run.clone();
    alerted.new.retain(|entry| {
        !covered.contains(&(entry.provider.as_str(), entry.base_model_id.as_str()))
    });
    discovery::write_alerts(db, api.recommend().alerts.as_deref(), &alerted)
        .await
        .map_err(|error| Failure::Internal(error.to_string()))?;

    let spend_preview = if enrolled.is_empty() {
        JsonValue::Null
    } else {
        let preview = preview_spend(
            db,
            JobKind::NightlySmoke,
            next_night(api, ran_at),
            api.internal().runner_repeats,
            api.internal().spend_caps,
        )
        .await
        .map_err(|error| Failure::Internal(error.to_string()))?;
        spend_preview_json(&preview)
    };

    let mut members = vec![("ran_at", time(ran_at))];
    members.extend(report_json(&run));
    members.push((
        "enrolled",
        JsonValue::Array(
            enrolled
                .iter()
                .map(column_json)
                .collect::<Result<Vec<_>, Failure>>()?,
        ),
    ));
    members.push(("spend_preview", spend_preview));
    Ok(json(StatusCode::OK, &object(members)))
}
