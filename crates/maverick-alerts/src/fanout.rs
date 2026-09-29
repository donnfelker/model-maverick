//! The process-wide asynchronous delivery of alert rows (reference Part 3 §4.2).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use maverick_store::alerts::Alert;
use tokio::sync::Notify;

use crate::email::Email;
use crate::webhook::Webhook;

/// Retries after the first attempt, so 4 attempts in all.
pub const DELIVERY_RETRIES: u32 = 3;
/// Each attempt gets its own timeout, not tied to any caller.
pub const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(30);

/// Something that hears about each inserted alert row.
pub trait AlertNotifier: Send + Sync {
    /// Starts delivery and returns at once. It never fails the caller.
    fn notify(&self, alert: &Alert);
}

/// A delivery destination.
#[derive(Debug)]
pub enum Destination {
    Email(Email),
    Webhook(Webhook),
}

impl Destination {
    /// The exact name used in logs: `email` or `webhook`.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Email(_) => "email",
            Self::Webhook(_) => "webhook",
        }
    }

    async fn deliver(&self, alert: &Alert) -> Result<(), String> {
        match self {
            Self::Email(email) => email.deliver(alert).await,
            Self::Webhook(webhook) => webhook.deliver(alert).await,
        }
    }
}

/// Delivers every alert to every destination. With no destinations it does nothing.
#[derive(Debug)]
pub struct Fanout {
    destinations: Vec<Arc<Destination>>,
    backoff_base: Duration,
    attempt_timeout: Duration,
    inflight: Arc<Inflight>,
}

#[derive(Debug, Default)]
struct Inflight {
    count: AtomicUsize,
    idle: Notify,
}

/// Decrements the in-flight count when a delivery task ends, even by panic.
struct InflightGuard(Arc<Inflight>);

impl Drop for InflightGuard {
    fn drop(&mut self) {
        if self.0.count.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.idle.notify_waiters();
        }
    }
}

impl Fanout {
    /// Builds the destinations from the config values. An empty value means unset:
    /// email when `smtp_url` is set, then the webhook when `webhook_url` is set.
    pub fn new(smtp_url: &str, webhook_url: &str, webhook_secret: &str) -> Result<Self, String> {
        let mut destinations = Vec::new();
        if !smtp_url.is_empty() {
            let email =
                Email::new(smtp_url).map_err(|err| format!("alerts: MAVERICK_SMTP_URL: {err}"))?;
            destinations.push(Destination::Email(email));
        }
        if !webhook_url.is_empty() {
            let webhook = Webhook::new(webhook_url, webhook_secret)
                .map_err(|err| format!("alerts: MAVERICK_ALERTS_WEBHOOK_URL: {err}"))?;
            destinations.push(Destination::Webhook(webhook));
        }
        Ok(Self::with_destinations(destinations))
    }

    /// A fan-out with these destinations and the production backoff and timeout.
    pub fn with_destinations(destinations: Vec<Destination>) -> Self {
        Self {
            destinations: destinations.into_iter().map(Arc::new).collect(),
            backoff_base: Duration::from_secs(1),
            attempt_timeout: ATTEMPT_TIMEOUT,
            inflight: Arc::default(),
        }
    }

    /// A fan-out that delivers nothing (Go's nil `*Fanout`).
    pub fn disabled() -> Self {
        Self::with_destinations(Vec::new())
    }

    /// Replaces the retry backoff base (1 s in production). Tests set it to zero.
    #[must_use]
    pub fn with_backoff_base(mut self, base: Duration) -> Self {
        self.backoff_base = base;
        self
    }

    /// The destination names in delivery order.
    pub fn destination_names(&self) -> Vec<&'static str> {
        self.destinations.iter().map(|d| d.name()).collect()
    }

    /// Waits until every in-flight delivery finishes. There is no deadline (QUIRK A3).
    /// `serve` and `mcp-stdio` call it at shutdown.
    pub async fn wait(&self) {
        loop {
            let idle = self.inflight.idle.notified();
            tokio::pin!(idle);
            idle.as_mut().enable();
            if self.inflight.count.load(Ordering::Acquire) == 0 {
                return;
            }
            idle.await;
        }
    }
}

impl AlertNotifier for Fanout {
    /// Starts one independent task per destination and returns at once. Needs a Tokio
    /// runtime.
    fn notify(&self, alert: &Alert) {
        for destination in &self.destinations {
            self.inflight.count.fetch_add(1, Ordering::AcqRel);
            let guard = InflightGuard(self.inflight.clone());
            let destination = destination.clone();
            let alert = alert.clone();
            let (backoff, timeout) = (self.backoff_base, self.attempt_timeout);
            tokio::spawn(async move {
                let _guard = guard;
                deliver_with_retries(&destination, &alert, backoff, timeout).await;
            });
        }
    }
}

/// Up to 4 attempts, sleeping 1 s, 2 s, then 4 s between them (no jitter).
async fn deliver_with_retries(
    destination: &Destination,
    alert: &Alert,
    backoff_base: Duration,
    timeout: Duration,
) {
    let attempts = DELIVERY_RETRIES + 1;
    let mut last_error = String::new();
    for attempt in 1..=attempts {
        if attempt > 1 {
            tokio::time::sleep(backoff_base * 2u32.pow(attempt - 2)).await;
        }
        let result = match tokio::time::timeout(timeout, destination.deliver(alert)).await {
            Ok(result) => result,
            Err(_) => Err("context deadline exceeded".to_owned()),
        };
        match result {
            Ok(()) => {
                tracing::info!(
                    destination = destination.name(),
                    alert_id = %alert.id,
                    kind = %alert.kind,
                    attempt,
                    "alert delivered"
                );
                return;
            }
            Err(error) => {
                tracing::warn!(
                    destination = destination.name(),
                    alert_id = %alert.id,
                    attempt,
                    error = %error,
                    "alert delivery attempt failed"
                );
                last_error = error;
            }
        }
    }
    tracing::error!(
        destination = destination.name(),
        alert_id = %alert.id,
        kind = %alert.kind,
        attempts,
        error = %last_error,
        "alert delivery failed"
    );
}
