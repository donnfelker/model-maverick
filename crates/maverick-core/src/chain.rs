//! The chain executor (spec v1 §8.1 "Proxy behavior"; sdk-v0 plan §2.1 and §2.3
//! scenarios 2 to 5; reference part 6 §8.4; D26).
//!
//! The executor walks the recommended chain in order. For each entry the caller's
//! `prepare` either skips it (no upstream request, no telemetry item) or returns the
//! upstream request, built by the surface: URL, credential headers, and the body with the
//! entry's settings applied or not (D14). The executor sends the request and decides from
//! the response head alone, before any body byte is read, so fallback always happens
//! before the first byte reaches the caller:
//!
//! | Outcome | `status` | `error_class` | Action |
//! |---|---|---|---|
//! | transport timeout, the response-header timeout included | 0 | `timeout` | advance |
//! | any other transport error | 0 | `connect_error` | advance |
//! | 429 | 429 | none | drain, obey `Retry-After` (D26), advance |
//! | 5xx | status | none | drain, advance |
//! | anything else (1xx, 2xx, 3xx, 4xx other than 429) | status | none | relay, stop |
//!
//! Every attempt produces exactly one [`AttemptRecord`], the source of its telemetry item.
//! `latency_ms` is the time until the response headers arrive (QUIRK 13). There is no limit
//! on body duration after the headers.
//!
//! A 429 or 5xx body is drained before the executor advances, as Go did, so its connection
//! can be reused. The last one is kept whole (status, headers, body) in [`LastFailure`], so
//! an SDK core can surface the last provider error unchanged.
//!
//! **D26 `Retry-After`.** On a 429 the executor waits for `Retry-After` (delta-seconds or
//! an HTTP date) before it advances, when another entry follows. The wait is bounded by
//! the caller's deadline: when it would pass the deadline, the executor advances at once
//! and logs why. Go ignored the header (QUIRK 11).
//!
//! **Cancellation.** When the `cancelled` future resolves (the client went away), the
//! in-flight attempt ends as `connect_error`, a drain or a D26 wait ends at once, and every
//! later entry that `prepare` does not
//! skip is recorded as `connect_error` without a request, as Go's cancelled context did
//! (QUIRK 12).
//!
//! Each attempt logs one DEBUG line with the entry index, provider, model, outcome, status,
//! latency, and any wait. Bodies and credentials are never logged.

use std::future::Future;
use std::time::{Duration, SystemTime};

use reqwest::header::{HeaderMap, RETRY_AFTER};
use tokio::time::Instant;

use crate::recommend::ChainEntry;
use crate::telemetry::TelemetryItem;

/// The per-attempt response-header timeout (Go constant).
pub const RESPONSE_HEADER_TIMEOUT: Duration = Duration::from_secs(30);

/// One upstream attempt, built by the surface for a chain entry.
#[derive(Debug)]
pub struct PreparedAttempt {
    /// The complete upstream request.
    pub request: reqwest::Request,
    /// Whether the entry's settings were applied to the request (D14 `settings_applied`).
    pub settings_applied: bool,
    /// The endpoint the attempt used, such as `direct` or `openrouter`.
    pub endpoint_used: String,
}

/// Why a transport-level attempt failed: `Timeout` (the response-header timeout included)
/// or `ConnectError` (any other transport failure, a cancelled request included). It is the
/// telemetry item's class, so a record converts to an item as is.
pub use crate::telemetry::ErrorClass;

/// One attempt, as telemetry records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptRecord {
    /// The entry's index in the chain.
    pub index: usize,
    /// The entry's provider.
    pub provider: String,
    /// The model that was called (telemetry `model_used`).
    pub model: String,
    /// The endpoint the attempt used (telemetry `endpoint_used`).
    pub endpoint_used: String,
    /// The HTTP status, or 0 for a transport failure.
    pub status: u16,
    /// The transport failure class, when there was one.
    pub error_class: Option<ErrorClass>,
    /// Milliseconds until the response headers arrived (or the transport failed).
    pub latency_ms: u64,
    /// Whether the entry's settings were applied (D14).
    pub settings_applied: bool,
    /// The D26 `Retry-After` wait the executor applied before advancing.
    pub retry_after_wait: Option<Duration>,
    /// True for the attempt whose response was relayed.
    pub relayed: bool,
}

impl AttemptRecord {
    /// The attempt's telemetry item. Token counts are the caller's to add (the settled,
    /// buffered 2xx only), and no check ran.
    #[must_use]
    pub fn telemetry_item(&self, recommendation_id: &str) -> TelemetryItem {
        TelemetryItem {
            recommendation_id: recommendation_id.to_owned(),
            model_used: self.model.clone(),
            status: self.status,
            latency_ms: i64::try_from(self.latency_ms).unwrap_or(i64::MAX),
            tokens_in: None,
            tokens_out: None,
            error_class: self.error_class,
            endpoint_used: self.endpoint_used.clone(),
            check_pass: None,
            settings_applied: Some(self.settings_applied),
        }
    }
}

/// A 429 or 5xx response, drained before the executor advanced.
#[derive(Debug, Clone)]
pub struct FailedResponse {
    /// The status.
    pub status: reqwest::StatusCode,
    /// The response headers.
    pub headers: HeaderMap,
    /// The body, up to the first read error.
    pub body: Vec<u8>,
}

/// The last failed attempt, kept so an SDK core can surface the last provider error.
#[derive(Debug)]
pub enum LastFailure {
    /// A 429 or 5xx response.
    Response(FailedResponse),
    /// A transport failure.
    Transport(ErrorClass),
}

/// How the walk ended.
#[derive(Debug)]
pub enum ChainResult {
    /// An attempt settled: relay this response (head read, body unread).
    Relayed {
        /// The relayed entry's index.
        index: usize,
        /// The upstream response.
        response: reqwest::Response,
    },
    /// No attempt settled: the chain was empty, every entry was skipped, or every attempt
    /// advanced. The proxy answers `502 no_executable_candidates`; an SDK core surfaces
    /// the last failure.
    Exhausted {
        /// The last failed attempt, or `None` when nothing was attempted.
        last_failure: Option<LastFailure>,
    },
}

/// The attempts and the result of one chain walk.
#[derive(Debug)]
pub struct ChainOutcome {
    /// One record per attempt, in order.
    pub attempts: Vec<AttemptRecord>,
    /// How the walk ended.
    pub result: ChainResult,
}

/// Walks recommended chains over one HTTP client.
#[derive(Debug, Clone)]
pub struct ChainExecutor {
    http: reqwest::Client,
    header_timeout: Duration,
}

impl ChainExecutor {
    /// An executor with the 30 s response-header timeout. The client follows redirects, as
    /// Go's did (reqwest's default policy; QUIRK 17), so the executor sees only the final
    /// response.
    #[must_use]
    pub fn new(http: reqwest::Client) -> Self {
        Self {
            http,
            header_timeout: RESPONSE_HEADER_TIMEOUT,
        }
    }

    /// The same executor with another response-header timeout.
    #[must_use]
    pub fn with_header_timeout(mut self, header_timeout: Duration) -> Self {
        self.header_timeout = header_timeout;
        self
    }

    /// Send one explicit provider request, without retries or chain records (D45).
    /// All HTTP statuses are returned unchanged. Only receiving the headers has a timeout.
    pub async fn send_one(
        &self,
        request: reqwest::Request,
    ) -> Result<reqwest::Response, ErrorClass> {
        match tokio::time::timeout(self.header_timeout, self.http.execute(request)).await {
            Err(_) => Err(ErrorClass::Timeout),
            Ok(Err(error)) if error.is_timeout() => Err(ErrorClass::Timeout),
            Ok(Err(_)) => Err(ErrorClass::ConnectError),
            Ok(Ok(response)) => Ok(response),
        }
    }

    /// Walk `chain`. `prepare` returns `None` to skip an entry. `deadline` bounds D26 waits.
    /// `cancelled` resolves when the caller has gone away.
    pub async fn execute<P, C>(
        &self,
        chain: &[ChainEntry],
        mut prepare: P,
        deadline: Option<Instant>,
        cancelled: C,
    ) -> ChainOutcome
    where
        P: FnMut(usize, &ChainEntry) -> Option<PreparedAttempt>,
        C: Future<Output = ()>,
    {
        tokio::pin!(cancelled);
        let mut is_cancelled = false;
        let mut attempts = Vec::new();
        let mut last_failure = None;

        for (index, entry) in chain.iter().enumerate() {
            let Some(prepared) = prepare(index, entry) else {
                tracing::debug!(
                    entry = index,
                    provider = %entry.provider,
                    model = %entry.model,
                    outcome = "skip",
                    "maverick-core: chain attempt"
                );
                continue;
            };
            let mut record = AttemptRecord {
                index,
                provider: entry.provider.clone(),
                model: entry.model.clone(),
                endpoint_used: prepared.endpoint_used,
                status: 0,
                error_class: None,
                latency_ms: 0,
                settings_applied: prepared.settings_applied,
                retry_after_wait: None,
                relayed: false,
            };

            let started = Instant::now();
            let sent = if is_cancelled {
                Err(ErrorClass::ConnectError)
            } else {
                tokio::select! {
                    biased;
                    () = &mut cancelled => {
                        is_cancelled = true;
                        Err(ErrorClass::ConnectError)
                    }
                    sent = self.send_one(prepared.request) => sent,
                }
            };
            record.latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

            let response = match sent {
                Err(class) => {
                    record.error_class = Some(class);
                    log_attempt(&record, "advance");
                    attempts.push(record);
                    last_failure = Some(LastFailure::Transport(class));
                    continue;
                }
                Ok(response) => response,
            };
            record.status = response.status().as_u16();

            if record.status == 429 || record.status >= 500 {
                let status = response.status();
                let headers = response.headers().clone();
                let body = tokio::select! {
                    biased;
                    () = &mut cancelled => {
                        is_cancelled = true;
                        Vec::new()
                    }
                    body = drain(response) => body,
                };
                let wait = if record.status == 429 && index + 1 < chain.len() && !is_cancelled {
                    retry_after_wait(&headers, deadline, &record)
                } else {
                    None
                };
                record.retry_after_wait = wait;
                log_attempt(&record, "advance");
                attempts.push(record);
                last_failure = Some(LastFailure::Response(FailedResponse {
                    status,
                    headers,
                    body,
                }));
                if let Some(wait) = wait {
                    tokio::select! {
                        biased;
                        () = &mut cancelled => is_cancelled = true,
                        () = tokio::time::sleep(wait) => {}
                    }
                }
                continue;
            }

            record.relayed = true;
            log_attempt(&record, "relay");
            attempts.push(record);
            return ChainOutcome {
                attempts,
                result: ChainResult::Relayed { index, response },
            };
        }

        ChainOutcome {
            attempts,
            result: ChainResult::Exhausted { last_failure },
        }
    }
}

/// Read a response body to its end. A read error ends it early; Go discarded both.
async fn drain(mut response: reqwest::Response) -> Vec<u8> {
    let mut body = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        body.extend_from_slice(&chunk);
    }
    body
}

fn log_attempt(record: &AttemptRecord, outcome: &'static str) {
    tracing::debug!(
        entry = record.index,
        provider = %record.provider,
        model = %record.model,
        outcome,
        status = record.status,
        error_class = record.error_class.map(ErrorClass::as_str),
        latency_ms = record.latency_ms,
        wait_ms = record.retry_after_wait.map(|wait| wait.as_millis()),
        "maverick-core: chain attempt"
    );
}

/// The D26 wait for a 429: the `Retry-After` delay, unless it would pass the deadline.
fn retry_after_wait(
    headers: &HeaderMap,
    deadline: Option<Instant>,
    record: &AttemptRecord,
) -> Option<Duration> {
    let value = headers.get(RETRY_AFTER)?.to_str().ok()?;
    let wait = parse_retry_after(value, SystemTime::now())?;
    if let Some(deadline) = deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if wait > remaining {
            tracing::debug!(
                entry = record.index,
                wait_ms = wait.as_millis(),
                remaining_ms = remaining.as_millis(),
                "maverick-core: Retry-After passes the deadline, advancing now"
            );
            return None;
        }
    }
    Some(wait)
}

/// `Retry-After` as delta-seconds (ASCII digits) or an HTTP date (RFC 9110 §10.2.3). A
/// date in the past is no wait. Anything else is ignored.
#[must_use]
pub fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        return value.parse::<u64>().ok().map(Duration::from_secs);
    }
    let at = httpdate::parse_http_date(value).ok()?;
    Some(at.duration_since(now).unwrap_or(Duration::ZERO))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_accepts_seconds_and_http_dates() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(784_111_777); // Sun, 06 Nov 1994 08:49:37 GMT
        assert_eq!(parse_retry_after("30", now), Some(Duration::from_secs(30)));
        assert_eq!(parse_retry_after(" 0 ", now), Some(Duration::ZERO));
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:50:07 GMT", now),
            Some(Duration::from_secs(30))
        );
        // The two obsolete date forms RFC 9110 requires recipients to accept.
        assert_eq!(
            parse_retry_after("Sunday, 06-Nov-94 08:50:07 GMT", now),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            parse_retry_after("Sun Nov  6 08:50:07 1994", now),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:00:00 GMT", now),
            Some(Duration::ZERO),
            "a past date is no wait"
        );
        for ignored in [
            "",
            "-5",
            "1.5",
            "+30",
            "soon",
            "30s",
            "99999999999999999999999",
        ] {
            assert_eq!(parse_retry_after(ignored, now), None, "{ignored:?}");
        }
    }

    #[test]
    fn records_become_telemetry_items() {
        let record = AttemptRecord {
            index: 2,
            provider: "openai".to_owned(),
            model: "gpt-a".to_owned(),
            endpoint_used: "direct".to_owned(),
            status: 0,
            error_class: Some(ErrorClass::Timeout),
            latency_ms: 30_000,
            settings_applied: true,
            retry_after_wait: None,
            relayed: false,
        };
        let mut json = String::new();
        record.telemetry_item("rec_1").write_json(&mut json);
        assert_eq!(
            json,
            "{\"recommendation_id\":\"rec_1\",\"model_used\":\"gpt-a\",\"status\":0,\
             \"latency_ms\":30000,\"error_class\":\"timeout\",\"endpoint_used\":\"direct\",\
             \"settings_applied\":true}"
        );
        let huge = AttemptRecord {
            latency_ms: u64::MAX,
            ..record
        };
        assert_eq!(huge.telemetry_item("r").latency_ms, i64::MAX);
    }
}
