//! `GET /up` and `GET /health/detail` (spec §9.9; reference Part 5 §8; D35).
//!
//! `/up` is the Kamal health check on the public listener. `/health/detail` is on the
//! internal listener behind the internal token. The job queue component reads
//! graphile_worker's worker registry from Postgres, so an `--api-only` node reports the
//! workers of a separate `--worker-only` node.

use std::collections::BTreeMap;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{Method, Response, StatusCode};
use chrono::{DateTime, Timelike, Utc};
use maverick_config::{Config, Role};
use maverick_store::health::{GRAPHILE_WORKER_SCHEMA, WorkerHeartbeat, list_worker_heartbeats};
use maverick_store::{batches, corpus};
use serde::Serialize;
use sqlx::PgPool;

use crate::{escape_go_json, not_found, response_with_json};

/// Both endpoints ping the database with this timeout.
pub const PING_TIMEOUT: Duration = Duration::from_secs(2);
/// A worker whose freshest heartbeat is older than this many seconds is stale.
pub const STALE_AFTER_SECONDS: f64 = 60.0;
/// The engine runs graphile_worker's default queue only.
const QUEUE_NAMES: &str = "default";

/// What the health endpoints read.
#[derive(Clone)]
pub struct HealthState {
    role: Role,
    pool: Option<PgPool>,
    providers: Vec<&'static str>,
    queue_schema: String,
}

impl HealthState {
    /// The production state: the engine pool and the configured provider keys.
    pub fn new(pool: PgPool, config: &Config) -> Self {
        let providers = [
            ("anthropic", &config.anthropic_api_key),
            ("openai", &config.openai_api_key),
            ("google", &config.google_api_key),
            ("xai", &config.xai_api_key),
        ]
        .into_iter()
        .filter(|(_, key)| !key.is_empty())
        .map(|(name, _)| name)
        .collect();
        Self::with_providers(pool, providers).with_role(config.role)
    }

    /// A state with an explicit provider list, in the order anthropic, openai, google,
    /// xai.
    pub fn with_providers(pool: PgPool, providers: Vec<&'static str>) -> Self {
        Self {
            role: Role::Origin,
            pool: Some(pool),
            providers,
            queue_schema: GRAPHILE_WORKER_SCHEMA.to_owned(),
        }
    }

    pub fn with_role(mut self, role: Role) -> Self {
        self.role = role;
        self
    }

    /// No database: `/up` answers ok and `/health/detail` reports the database as an
    /// error. For router tests that do not start Postgres.
    pub fn detached() -> Self {
        Self {
            role: Role::Origin,
            pool: None,
            providers: Vec::new(),
            queue_schema: GRAPHILE_WORKER_SCHEMA.to_owned(),
        }
    }

    async fn ping(&self) -> Result<(), String> {
        let Some(pool) = &self.pool else {
            return Err("database not configured".to_owned());
        };
        match tokio::time::timeout(PING_TIMEOUT, sqlx::query("select 1").execute(pool)).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(err)) => Err(err.to_string()),
            Err(_) => Err("context deadline exceeded".to_owned()),
        }
    }
}

/// One health component. `detail` is omitted when empty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Component {
    pub status: &'static str,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

impl Component {
    fn new(status: &'static str, detail: impl Into<String>) -> Self {
        Self {
            status,
            detail: detail.into(),
        }
    }
}

/// The `job_queue` component (D35): `no_workers` with no registered worker, `stale`
/// when the freshest heartbeat is older than 60 s, else `ok`. There is no `paused`.
pub fn job_queue_component(workers: &[WorkerHeartbeat]) -> Component {
    let Some(freshest) = workers
        .iter()
        .map(|w| w.age_seconds)
        .min_by(|a, b| a.total_cmp(b))
    else {
        return Component::new("no_workers", "no graphile_worker worker is registered");
    };
    if freshest > STALE_AFTER_SECONDS {
        // Go's int(freshest): truncation toward zero.
        let seconds = freshest.trunc() as i64;
        return Component::new(
            "stale",
            format!("no worker heartbeat for {seconds}s (graphile_worker queues: {QUEUE_NAMES})"),
        );
    }
    Component::new("ok", format!("graphile_worker queues: {QUEUE_NAMES}"))
}

/// The `provider_client_config` component.
pub fn provider_client_config_component(providers: &[&str]) -> Component {
    if providers.is_empty() {
        Component::new("none_configured", "no provider API keys set")
    } else {
        Component::new("ok", format!("configured: {}", providers.join(", ")))
    }
}

#[derive(Serialize)]
struct Detail {
    status: &'static str,
    role: Role,
    components: BTreeMap<&'static str, Component>,
    active_matrix_version: Option<String>,
    last_batch_times: Option<BTreeMap<String, String>>,
}

/// `GET /up`: 200 `{"status":"ok"}`, or 503 `{"status":"unavailable"}` when the
/// database does not answer within 2 s. Other methods are 404.
pub(crate) async fn up(State(state): State<HealthState>, request: Request) -> Response<Body> {
    if request.method() != Method::GET {
        return not_found();
    }
    if state.pool.is_none() {
        return json(StatusCode::OK, br#"{"status":"ok"}"#);
    }
    match state.ping().await {
        Ok(()) => json(StatusCode::OK, br#"{"status":"ok"}"#),
        Err(error) => {
            tracing::warn!(error = %error, "up: database ping failed");
            json(
                StatusCode::SERVICE_UNAVAILABLE,
                br#"{"status":"unavailable"}"#,
            )
        }
    }
}

/// `GET /health/detail`.
pub(crate) async fn detail(State(state): State<HealthState>, request: Request) -> Response<Body> {
    if request.method() != Method::GET {
        return not_found();
    }
    let mut components = BTreeMap::new();
    components.insert(
        "provider_client_config",
        provider_client_config_component(&state.providers),
    );

    let (status, code, active_matrix_version, last_batch_times) = match state.ping().await {
        Err(error) => {
            components.insert("db", Component::new("error", error));
            components.insert(
                "job_queue",
                Component::new("unknown", "database unreachable"),
            );
            ("degraded", StatusCode::SERVICE_UNAVAILABLE, None, None)
        }
        Ok(()) => {
            let pool = state
                .pool
                .as_ref()
                .expect("a ping succeeded, so there is a pool");
            components.insert("db", Component::new("ok", ""));
            let active = if state.role == Role::Replica {
                match maverick_store::matrix::active_bundle(pool).await {
                    Ok(bundle) => Some(bundle.map_or_else(String::new, |b| b.matrix_version)),
                    Err(err) => {
                        tracing::warn!(error = %err, "health: reading active matrix bundle failed");
                        None
                    }
                }
            } else {
                match corpus::get_active_corpus_version(pool).await {
                    Ok(version) => Some(version.version),
                    Err(err) if maverick_store::is_not_found(&err) => None,
                    Err(err) => {
                        tracing::warn!(error = %err, "health: reading the active corpus version failed");
                        None
                    }
                }
            };
            match maverick_store::health::queue_stats(pool, &state.queue_schema).await {
                Ok(stats) => crate::metrics::job_queue(stats.depth, stats.oldest_age_seconds),
                Err(err) => tracing::debug!(error = %err, "health: queue stats unavailable"),
            }
            let queue = async {
                let workers = list_worker_heartbeats(pool, &state.queue_schema).await?;
                let last = batches::list_last_completed_batches(pool).await?;
                Ok::<_, sqlx::Error>((workers, last))
            };
            let last_batch_times = match queue.await {
                Ok((workers, last)) => {
                    components.insert("job_queue", job_queue_component(&workers));
                    Some(
                        last.into_iter()
                            .map(|(kind, at)| (kind, rfc3339_nano(at)))
                            .collect(),
                    )
                }
                Err(err) => {
                    tracing::warn!(error = %err, "health: reading the job queue state failed");
                    components.insert(
                        "job_queue",
                        Component::new("unknown", "querying job queue state failed"),
                    );
                    None
                }
            };
            ("ok", StatusCode::OK, active, last_batch_times)
        }
    };

    let body = Detail {
        status,
        role: state.role,
        components,
        active_matrix_version,
        last_batch_times,
    };
    let encoded = serde_json::to_vec(&body).expect("the health body serializes");
    let mut bytes = escape_go_json(&encoded);
    bytes.push(b'\n');
    response_with_json(code, bytes)
}

/// A JSON response with Go's trailing newline.
fn json(code: StatusCode, body: &[u8]) -> Response<Body> {
    let mut bytes = body.to_vec();
    bytes.push(b'\n');
    response_with_json(code, bytes)
}

/// Go's `time.Time` JSON form in UTC: RFC 3339 with trailing fractional zeros trimmed.
pub(crate) fn rfc3339_nano(at: DateTime<Utc>) -> String {
    let base = at.format("%Y-%m-%dT%H:%M:%S").to_string();
    let nanos = at.nanosecond() % 1_000_000_000;
    if nanos == 0 {
        return format!("{base}Z");
    }
    let fraction = format!("{nanos:09}");
    format!("{base}.{}Z", fraction.trim_end_matches('0'))
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn worker(id: &str, age_seconds: f64) -> WorkerHeartbeat {
        WorkerHeartbeat {
            id: id.to_owned(),
            age_seconds,
        }
    }

    // The five fixture cases of parity/fixtures/api/health_river_queues.json, restated
    // for graphile_worker workers (D35).

    #[test]
    // parity: api.TestRiverQueuesComponent (the table test; D35 restates its cases)
    // parity: api.TestRiverQueuesComponent/no_queues_means_no_workers (D35 wording)
    fn no_worker_means_no_workers() {
        assert_eq!(
            job_queue_component(&[]),
            Component::new("no_workers", "no graphile_worker worker is registered")
        );
    }

    #[test]
    // parity: api.TestRiverQueuesComponent/fresh_heartbeat_is_ok (D35 wording)
    fn a_fresh_heartbeat_is_ok() {
        assert_eq!(
            job_queue_component(&[worker("w1", 3.0)]),
            Component::new("ok", "graphile_worker queues: default")
        );
    }

    #[test]
    // parity: api.TestRiverQueuesComponent/one_live_queue_keeps_the_cluster_ok_despite_a_stale_sibling
    fn one_live_worker_keeps_the_cluster_ok() {
        assert_eq!(
            job_queue_component(&[worker("w1", 3.0), worker("w2", 9999.0)]).status,
            "ok"
        );
    }

    #[test]
    // parity: api.TestRiverQueuesComponent/a_paused_queue_is_surfaced_over_ok
    // D35: graphile_worker has no paused queues, so there is no `paused` status. A
    // fresh worker is `ok` whatever River would have reported.
    fn there_is_no_paused_status() {
        assert_eq!(job_queue_component(&[worker("w1", 3.0)]).status, "ok");
    }

    #[test]
    // parity: api.TestRiverQueuesComponent/every_heartbeat_stale_is_stale (D35 wording)
    fn every_heartbeat_stale_is_stale() {
        assert_eq!(
            job_queue_component(&[worker("w1", 61.9), worker("w2", 300.0)]),
            Component::new(
                "stale",
                "no worker heartbeat for 61s (graphile_worker queues: default)"
            )
        );
        assert_eq!(
            job_queue_component(&[worker("w1", 60.0)]).status,
            "ok",
            "60 s is not stale"
        );
    }

    #[test]
    fn provider_config_lists_the_configured_providers_in_order() {
        assert_eq!(
            provider_client_config_component(&[]),
            Component::new("none_configured", "no provider API keys set")
        );
        assert_eq!(
            provider_client_config_component(&["anthropic", "openai"]),
            Component::new("ok", "configured: anthropic, openai")
        );
    }

    #[test]
    fn batch_times_use_go_time_json() {
        let at = Utc.with_ymd_and_hms(2026, 7, 8, 3, 41, 12).unwrap();
        assert_eq!(rfc3339_nano(at), "2026-07-08T03:41:12Z");
        let micros = at + chrono::Duration::microseconds(250_000);
        assert_eq!(rfc3339_nano(micros), "2026-07-08T03:41:12.25Z");
    }
}
