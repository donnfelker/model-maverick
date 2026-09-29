//! A cancellation signal for a batch: the Rust stand-in for the Go runner's `ctx`.
//!
//! Cancelling stops workers from taking more pairs, stops each pair before its next
//! repeat, wakes pacer waits, and ends in-flight provider calls as timeouts.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;

/// A cloneable cancellation flag. Every clone observes the same cancel.
#[derive(Debug, Clone, Default)]
pub struct Cancellation(Arc<Inner>);

#[derive(Debug, Default)]
struct Inner {
    cancelled: AtomicBool,
    notify: Notify,
}

/// Returned when the batch was cancelled (Go's `context.Canceled`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("context canceled")]
pub struct Cancelled;

impl Cancellation {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancels every clone. Idempotent.
    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::SeqCst);
        self.0.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::SeqCst)
    }

    /// `ctx.Err()`: `Err(Cancelled)` once cancelled.
    pub fn check(&self) -> Result<(), Cancelled> {
        if self.is_cancelled() {
            Err(Cancelled)
        } else {
            Ok(())
        }
    }

    /// Completes when the flag is cancelled (at once if it already is).
    pub async fn cancelled(&self) {
        let notified = self.0.notify.notified();
        tokio::pin!(notified);
        // Register before the check, so a cancel between the check and the await is not
        // missed.
        notified.as_mut().enable();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn clones_share_the_flag_and_waiters_wake() {
        let cancel = Cancellation::new();
        let clone = cancel.clone();
        assert_eq!(clone.check(), Ok(()));
        let waiter = tokio::spawn(async move { clone.cancelled().await });
        tokio::task::yield_now().await;
        cancel.cancel();
        waiter.await.expect("waiter finishes");
        assert!(cancel.is_cancelled());
        assert_eq!(cancel.check(), Err(Cancelled));
        // Already cancelled: returns at once.
        cancel.cancelled().await;
        assert_eq!(Cancelled.to_string(), "context canceled");
    }
}
