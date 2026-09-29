//! The debug-capture hook of the prompt path (spec §10.4; reference Part 4 §4.5; D18).
//!
//! After a prompt-path trace row is written, [`Capturer::maybe`] checks the tenant's active
//! capture windows. The first window that is active, scoped to this API key (or to none),
//! and under its confidence bar captures the prompt: it is sealed with the current key of
//! the master-key ring (D18) under a new `cap_` id and stored with its key version, until
//! the window ends plus the retention. A zero-retention tenant is never captured. Every
//! failure is logged (`debug capture failed; recommendation unaffected`) and swallowed.
//!
//! Active windows are cached per tenant for 10 s. [`Capturer::invalidate`] drops a
//! tenant's entry; the window endpoints call it when they create or end a window.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use maverick_config::Config;
use maverick_domain::capture::{self, CaptureWindow, KeyRing};
use maverick_domain::ids;
use maverick_store::capture::{self as store, NewDebugCapture};
use sqlx::PgPool;

/// How long a tenant's active windows are reused (Go `window cache TTL`).
pub const WINDOW_CACHE_TTL: Duration = Duration::from_secs(10);

/// One prompt-path request, as the capture decision sees it.
#[derive(Debug, Clone, Copy)]
pub struct CaptureRequest<'a> {
    /// The tenant.
    pub tenant_id: &'a str,
    /// The API key that made the request.
    pub api_key_id: &'a str,
    /// The tenant's zero-retention constraint: never captured.
    pub zero_retention: bool,
    /// The trace row the capture belongs to.
    pub recommendation_id: &'a str,
    /// The prompt, sealed if captured.
    pub prompt: &'a str,
    /// The classification confidence, for `confidence_below`.
    pub confidence: f64,
    /// The request time.
    pub now: DateTime<Utc>,
}

/// The capture hook: the key ring, the retention, and the per-tenant window cache.
pub struct Capturer {
    keys: KeyRing,
    retention: chrono::Duration,
    ttl: Option<Duration>,
    windows: Mutex<HashMap<String, (Instant, Vec<CaptureWindow>)>>,
}

impl fmt::Debug for Capturer {
    /// Never prints a key.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Capturer")
            .field("key_version", &self.keys.current_version())
            .field("retention", &self.retention)
            .field("ttl", &self.ttl)
            .finish_non_exhaustive()
    }
}

impl Capturer {
    /// A capturer sealing with `keys` and keeping captures `retention_hours` past their
    /// window's end.
    #[must_use]
    pub fn new(keys: KeyRing, retention_hours: u32) -> Self {
        Self {
            keys,
            retention: chrono::Duration::hours(i64::from(retention_hours)),
            ttl: Some(WINDOW_CACHE_TTL),
            windows: Mutex::new(HashMap::new()),
        }
    }

    /// The capturer the configuration describes.
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        Self::new(
            KeyRing::new(
                config.capture_master_key_version,
                config.capture_master_key,
                config
                    .capture_previous_master_key
                    .as_ref()
                    .map(|previous| (previous.version, previous.key)),
            ),
            config.capture_retention_hours,
        )
    }

    /// The same capturer with another window-cache lifetime; `None` reads the windows on
    /// every request.
    #[must_use]
    pub fn with_window_ttl(mut self, ttl: Option<Duration>) -> Self {
        self.ttl = ttl;
        self
    }

    /// Forget a tenant's cached windows.
    pub fn invalidate(&self, tenant_id: &str) {
        self.windows
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(tenant_id);
    }

    /// Capture the prompt when a window asks for it. Never fails the request.
    pub async fn maybe(&self, pool: &PgPool, request: &CaptureRequest<'_>) {
        if request.zero_retention {
            return;
        }
        if let Err(error) = self.capture(pool, request).await {
            tracing::warn!(error = %error, "debug capture failed; recommendation unaffected");
        }
    }

    async fn capture(&self, pool: &PgPool, request: &CaptureRequest<'_>) -> Result<(), String> {
        let windows = self
            .active_windows(pool, request.tenant_id, request.now)
            .await?;
        let decision = capture::decide(
            &windows,
            request.zero_retention,
            request.api_key_id,
            request.confidence,
            request.now,
        );
        let Some(window_id) = decision.window_id else {
            return Ok(());
        };
        let window = windows
            .iter()
            .find(|window| window.id == window_id)
            .ok_or_else(|| format!("capture window {window_id} vanished"))?;
        let id = ids::new(ids::prefix::DEBUG_CAPTURE);
        let (version, nonce, ciphertext) = self
            .keys
            .seal(&id, request.prompt.as_bytes())
            .map_err(|error| error.to_string())?;
        store::create_debug_capture(
            pool,
            &NewDebugCapture {
                id: &id,
                tenant_id: request.tenant_id,
                window_id: Some(&window.id),
                recommendation_id: request.recommendation_id,
                ciphertext: &ciphertext,
                nonce: &nonce,
                capture_key_version: i32::try_from(version).map_err(|error| error.to_string())?,
                expires_at: window.ends_at + self.retention,
            },
        )
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
    }

    /// The tenant's active windows, from the cache while it is fresh.
    async fn active_windows(
        &self,
        pool: &PgPool,
        tenant_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Vec<CaptureWindow>, String> {
        if let Some(ttl) = self.ttl {
            let cache = self.windows.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some((fetched_at, windows)) = cache.get(tenant_id) {
                if fetched_at.elapsed() < ttl {
                    return Ok(windows.clone());
                }
            }
        }
        let windows: Vec<CaptureWindow> = store::list_active_capture_windows(pool, tenant_id, now)
            .await
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|window| CaptureWindow {
                id: window.id,
                api_key_id: window.api_key_id,
                confidence_below: window.confidence_below,
                starts_at: window.starts_at,
                ends_at: window.ends_at,
            })
            .collect();
        if self.ttl.is_some() {
            self.windows
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(tenant_id.to_owned(), (Instant::now(), windows.clone()));
        }
        Ok(windows)
    }
}
