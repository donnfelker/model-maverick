//! Per-tenant rate limiting (D28; spec v1 §8.1).
//!
//! A fixed window per tenant key, aligned to the clock minute as the API's limiter is: a
//! request is allowed while its key's count in the current minute is at most the limit.
//! Rejected requests still count. A rejection is `429 rate_limited` with `Retry-After`
//! (seconds to the next window, at least 1) and the `X-RateLimit-*` headers, and it calls
//! neither the engine nor a provider.
//!
//! Hosted mode limits each presented tenant key; self-hosted mode has one key, so one
//! bucket. Buckets are keyed by a hash of the tenant key with a per-process random seed,
//! so the limiter keeps no key after the request. All buckets are dropped when the minute
//! turns, so memory is bounded by the keys seen in one minute.

use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::header::RETRY_AFTER;
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::Response;

use crate::envelope;

/// The window length in seconds.
pub const WINDOW_SECONDS: i64 = 60;

/// The source of Unix time in seconds (tests pass a fixed clock).
pub trait Clock: Send + Sync {
    /// Seconds since the Unix epoch.
    fn unix_seconds(&self) -> i64;
}

/// The system clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn unix_seconds(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| {
                i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
            })
    }
}

/// One request's rate-limit decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    /// Whether the request may proceed.
    pub allowed: bool,
    /// The limit per window.
    pub limit: u64,
    /// Requests left in this window (0 once the limit is reached).
    pub remaining: u64,
    /// Unix seconds when the next window starts.
    pub reset_at: i64,
    /// Seconds until the next window, at least 1.
    pub retry_after: u64,
}

struct Window {
    start: i64,
    counts: HashMap<u64, u64>,
}

/// The per-tenant fixed-window limiter.
pub struct RateLimiter {
    limit: u64,
    clock: Arc<dyn Clock>,
    seed: RandomState,
    window: Mutex<Window>,
}

impl std::fmt::Debug for RateLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RateLimiter")
            .field("limit", &self.limit)
            .finish_non_exhaustive()
    }
}

impl RateLimiter {
    /// A limiter of `limit` requests per minute per tenant key, on the system clock.
    #[must_use]
    pub fn new(limit: u32) -> Self {
        Self::with_clock(limit, Arc::new(SystemClock))
    }

    /// The same limiter on another clock.
    #[must_use]
    pub fn with_clock(limit: u32, clock: Arc<dyn Clock>) -> Self {
        Self {
            limit: u64::from(limit),
            clock,
            seed: RandomState::new(),
            window: Mutex::new(Window {
                start: i64::MIN,
                counts: HashMap::new(),
            }),
        }
    }

    /// Count one request for `tenant_key` and decide.
    pub fn check(&self, tenant_key: &str) -> Decision {
        let now = self.clock.unix_seconds();
        let start = now.div_euclid(WINDOW_SECONDS) * WINDOW_SECONDS;
        let reset_at = start.saturating_add(WINDOW_SECONDS);
        let bucket = self.seed.hash_one(tenant_key);
        let count = {
            let mut window = self.window.lock().unwrap_or_else(PoisonError::into_inner);
            if window.start != start {
                window.start = start;
                window.counts.clear();
            }
            let count = window.counts.entry(bucket).or_insert(0);
            *count = count.saturating_add(1);
            *count
        };
        Decision {
            allowed: count <= self.limit,
            limit: self.limit,
            remaining: self.limit.saturating_sub(count),
            reset_at,
            retry_after: u64::try_from(reset_at.saturating_sub(now).max(1)).unwrap_or(1),
        }
    }
}

/// `429 rate_limited` with `Retry-After` and the `X-RateLimit-*` headers.
#[must_use]
pub fn rejection(decision: &Decision) -> Response {
    let mut response = envelope::error(
        StatusCode::TOO_MANY_REQUESTS,
        envelope::RATE_LIMITED,
        envelope::RATE_LIMITED_MESSAGE,
    );
    let headers = response.headers_mut();
    for (name, value) in [
        (RETRY_AFTER, decision.retry_after.to_string()),
        (
            HeaderName::from_static("x-ratelimit-limit"),
            decision.limit.to_string(),
        ),
        (
            HeaderName::from_static("x-ratelimit-remaining"),
            decision.remaining.to_string(),
        ),
        (
            HeaderName::from_static("x-ratelimit-reset"),
            decision.reset_at.to_string(),
        ),
    ] {
        if let Ok(value) = HeaderValue::from_str(&value) {
            headers.insert(name, value);
        }
    }
    response
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicI64, Ordering};

    use super::*;

    struct Fixed(AtomicI64);

    impl Clock for Fixed {
        fn unix_seconds(&self) -> i64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    #[test]
    fn windows_are_minute_aligned_and_per_key() {
        let clock = Arc::new(Fixed(AtomicI64::new(1_000_050)));
        let limiter = RateLimiter::with_clock(2, clock.clone());
        let decide = |key: &str| limiter.check(key);

        let first = decide("mk_live_a");
        assert_eq!(
            first,
            Decision {
                allowed: true,
                limit: 2,
                remaining: 1,
                reset_at: 1_000_080,
                retry_after: 30,
            }
        );
        assert!(decide("mk_live_a").allowed);
        let third = decide("mk_live_a");
        assert!(!third.allowed);
        assert_eq!(third.remaining, 0);
        assert!(
            decide("mk_live_b").allowed,
            "another key has its own bucket"
        );

        // The last second of the window: Retry-After is still at least 1.
        clock.0.store(first.reset_at - 1, Ordering::SeqCst);
        assert_eq!(decide("mk_live_a").retry_after, 1);
        // The next minute starts over.
        clock.0.store(first.reset_at, Ordering::SeqCst);
        let fresh = decide("mk_live_a");
        assert!(fresh.allowed);
        assert_eq!(fresh.remaining, 1);
        assert_eq!(fresh.reset_at, first.reset_at + 60);
        assert_eq!(fresh.retry_after, 60);
    }

    #[test]
    fn negative_time_still_aligns() {
        let limiter = RateLimiter::with_clock(1, Arc::new(Fixed(AtomicI64::new(-1))));
        let decision = limiter.check("k");
        assert_eq!((decision.reset_at, decision.retry_after), (0, 1));
    }

    #[test]
    fn rejection_carries_the_headers() {
        let response = rejection(&Decision {
            allowed: false,
            limit: 60,
            remaining: 0,
            reset_at: 1_700_000_040,
            retry_after: 17,
        });
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        assert_eq!(header("retry-after").as_deref(), Some("17"));
        assert_eq!(header("x-ratelimit-limit").as_deref(), Some("60"));
        assert_eq!(header("x-ratelimit-remaining").as_deref(), Some("0"));
        assert_eq!(header("x-ratelimit-reset").as_deref(), Some("1700000040"));
    }
}
