//! The per-provider request pacer (reference Part 2 §5.5).

use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;

use crate::cancel::{Cancellation, Cancelled};

/// Spaces call starts `60 s / rpm` apart. The first call starts at once; each later
/// start is one interval after the previous claimed slot. A slot claimed by a caller
/// that is then cancelled is not given back.
#[derive(Debug)]
pub struct Pacer {
    interval: Duration,
    next: Mutex<Option<Instant>>,
}

/// `time.Minute / rpm` in integer nanoseconds; zero (unthrottled) for `rpm <= 0`.
pub fn interval_for_rpm(rpm: i64) -> Duration {
    match u64::try_from(rpm) {
        Ok(rpm) if rpm > 0 => Duration::from_nanos(60_000_000_000 / rpm),
        _ => Duration::ZERO,
    }
}

impl Pacer {
    /// `newPacer(rpm)`.
    pub fn new(rpm: i64) -> Self {
        Pacer {
            interval: interval_for_rpm(rpm),
            next: Mutex::new(None),
        }
    }

    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// `Wait(ctx)`: waits for this caller's slot. Unthrottled, or when the slot is now,
    /// it returns the cancellation state at once (a cancelled batch fails even with no
    /// wait); otherwise it sleeps until the slot or the cancel.
    pub async fn wait(&self, cancel: &Cancellation) -> Result<(), Cancelled> {
        if self.interval.is_zero() {
            return cancel.check();
        }
        let now = Instant::now();
        let at = {
            let mut next = self
                .next
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let at = next.map_or(now, |next| next.max(now));
            *next = Some(at + self.interval);
            at
        };
        if at <= now {
            return cancel.check();
        }
        tokio::select! {
            () = tokio::time::sleep_until(at) => Ok(()),
            () = cancel.cancelled() => Err(Cancelled),
        }
    }
}
