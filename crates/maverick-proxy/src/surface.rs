//! The chat and messages surfaces on the `maverick-core` chain executor (reference part 6
//! §8.1, §8.2, §8.4, §11, §14).
//!
//! For each chain entry the surface builds one upstream attempt or skips the entry. A
//! skipped entry makes no request and no telemetry item.
//!
//! - **Chat** (native provider `openai`): skip when there is no key for the entry
//!   ([`resolve_key`]). The URL is the provider's OpenAI-compatible endpoint: openai and
//!   xai `{base}/v1/chat/completions`, anthropic `{base}/v1/chat/completions`, google
//!   `{base}/chat/completions`; any other provider is skipped. The credential goes in
//!   `Authorization: Bearer`.
//! - **Messages** (native provider `anthropic`): skip every entry whose provider is not
//!   exactly `anthropic` (no schema translation), then skip when there is no key. The URL
//!   is `{anthropic}/v1/messages`. The credential goes in `X-Api-Key`.
//!
//! - **OpenRouter** (D15; hosted mode, chat surface, an `sk-or-` caller key): every entry
//!   goes to `{openrouter}/chat/completions` with its `execution_aliases.openrouter` slug as
//!   the model and `endpoint_used: openrouter`; an entry without the alias is skipped. For a
//!   zero-retention tenant each attempt forces `provider.zdr: true`, an alias that is not
//!   verified `zero` is skipped, and a chain with none left answers `409
//!   no_compliant_columns` before any upstream request. There is never a direct fallback.
//!
//! Both append `?` and the inbound raw query when it is not empty, rewrite the body per
//! attempt ([`rewrite_attempt`]: the model, the entry's settings (D14), and parameter
//! normalization (D29); a body that is not an object skips the entry), and send the base
//! upstream headers plus the credential ([`attempt_headers`]).
//!
//! The walk runs in its own task. When the client goes away, axum drops the handler; the
//! walk then sees its `cancelled` signal and ends every remaining attempt as
//! `connect_error`, which still reaches telemetry (QUIRK 12). The proxy has no deadline of
//! its own, so a D26 `Retry-After` wait is bounded by the caller's timeout through the same
//! signal.
//!
//! After the relay ends (the whole stream for SSE), the walk hands one [`ChainReport`] to
//! the [`ReportSink`]: [`crate::reports::ProxyReports`] in the binary (telemetry, D27).

use std::sync::Arc;
use std::time::Instant;

use axum::http::StatusCode;
use axum::response::Response;
use maverick_core::alias::Mode;
use maverick_core::chain::{ChainExecutor, ChainResult, PreparedAttempt};
use maverick_core::openrouter::{self, Eligibility, Verdict};
use maverick_core::recommend::{ChainEntry, TenantKey};
use maverick_core::settings::SettingsMode;
use maverick_core::telemetry::TelemetryItem;
use tokio::sync::oneshot;

use crate::chain::{BoxFuture, ChainRequest, ChainRunner, Surface, exhausted};
use crate::config::Config;
use crate::credentials::{CallerCredential, attempt_headers, resolve_key};
use crate::envelope;
use crate::relay::{Done, relay};
use crate::rewrite::{AttemptParams, rewrite_attempt, wire_format};

/// At most this many idle upstream connections are kept per host (Go transport setting).
pub const MAX_IDLE_PER_HOST: usize = 2;

/// The upstream base URLs, one trailing `/` already stripped by the configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upstreams {
    /// `MAVERICK_UPSTREAM_OPENAI`.
    pub openai: String,
    /// `MAVERICK_UPSTREAM_ANTHROPIC`.
    pub anthropic: String,
    /// `MAVERICK_UPSTREAM_GOOGLE`.
    pub google: String,
    /// `MAVERICK_UPSTREAM_XAI`.
    pub xai: String,
    /// `MAVERICK_UPSTREAM_OPENROUTER` (D15).
    pub openrouter: String,
}

impl Upstreams {
    /// The configured upstream base URLs.
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        Self {
            openai: config.upstream_openai.clone(),
            anthropic: config.upstream_anthropic.clone(),
            google: config.upstream_google.clone(),
            xai: config.upstream_xai.clone(),
            openrouter: config.upstream_openrouter.clone(),
        }
    }

    /// The OpenRouter chat completions URL (D15), with the inbound raw query.
    #[must_use]
    pub fn openrouter_url(&self, raw_query: &str) -> String {
        let mut url = format!("{}/chat/completions", self.openrouter);
        if !raw_query.is_empty() {
            url.push('?');
            url.push_str(raw_query);
        }
        url
    }

    /// The upstream URL for one entry, or `None` when the surface cannot call the provider.
    #[must_use]
    pub fn url(&self, surface: Surface, provider: &str, raw_query: &str) -> Option<String> {
        let mut url = match (surface, provider) {
            (Surface::Chat, "openai") => format!("{}/v1/chat/completions", self.openai),
            (Surface::Chat, "anthropic") => format!("{}/v1/chat/completions", self.anthropic),
            (Surface::Chat, "google") => format!("{}/chat/completions", self.google),
            (Surface::Chat, "xai") => format!("{}/v1/chat/completions", self.xai),
            (Surface::Messages, "anthropic") => format!("{}/v1/messages", self.anthropic),
            _ => return None,
        };
        if !raw_query.is_empty() {
            url.push('?');
            url.push_str(raw_query);
        }
        Some(url)
    }
}

/// One request's chain walk, reported after the relay ends.
#[derive(Debug, Clone)]
pub struct ChainReport {
    /// The tenant key the recommend call used; telemetry goes out under it (D27).
    pub tenant_key: TenantKey,
    /// The inbound surface.
    pub surface: Surface,
    /// The engine's recommendation id.
    pub recommendation_id: String,
    /// The engine's `measured` flag.
    pub measured: bool,
    /// The engine's `classification.method`.
    pub classification_method: String,
    /// The engine mode the alias asked for: `balanced`, `quality`, or `cost`.
    pub mode: String,
    /// The relayed entry's model, or `""` when the chain was exhausted.
    pub model_used: String,
    /// The relayed status, or 502 when the chain was exhausted.
    pub final_status: u16,
    /// One item per dispatched attempt, in chain order. The relayed attempt's buffered 2xx
    /// body gives it token counts. Empty when nothing was dispatched.
    pub items: Vec<TelemetryItem>,
    /// The engine's `telemetry: false` (D39): the tenant opted out, so the items are not
    /// sent.
    pub telemetry_opt_out: bool,
    /// When the proxy received the request.
    pub received_at: Instant,
}

/// Receives every request's [`ChainReport`].
pub trait ReportSink: Send + Sync + 'static {
    /// Take one report. Must not block.
    fn report(&self, report: ChainReport);

    /// Count one request the fail-open ladder handled (D46). Must not block.
    fn fallback(&self, _step: crate::chain::FallbackStep) {}
}

/// A sink that drops every report.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoReports;

impl ReportSink for NoReports {
    fn report(&self, _report: ChainReport) {}
}

/// The chain runner for both surfaces.
#[derive(Clone)]
pub struct ExecutorRunner {
    executor: ChainExecutor,
    upstreams: Upstreams,
    reports: Arc<dyn ReportSink>,
}

impl std::fmt::Debug for ExecutorRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutorRunner")
            .field("executor", &self.executor)
            .field("upstreams", &self.upstreams)
            .finish_non_exhaustive()
    }
}

impl ExecutorRunner {
    /// A runner on its own upstream client: no proxy from the environment, at most
    /// [`MAX_IDLE_PER_HOST`] idle connections per host, HTTP/2 when the upstream offers it,
    /// redirects followed (QUIRK 17), and the executor's 30 s response-header timeout.
    pub fn new(config: &Config, reports: Arc<dyn ReportSink>) -> Result<Self, reqwest::Error> {
        let http = reqwest::Client::builder()
            .no_proxy()
            .pool_max_idle_per_host(MAX_IDLE_PER_HOST)
            .build()?;
        Ok(Self::with_executor(
            ChainExecutor::new(http),
            Upstreams::from_config(config),
            reports,
        ))
    }

    /// A runner on an existing executor.
    #[must_use]
    pub fn with_executor(
        executor: ChainExecutor,
        upstreams: Upstreams,
        reports: Arc<dyn ReportSink>,
    ) -> Self {
        Self {
            executor,
            upstreams,
            reports,
        }
    }
}

impl ChainRunner for ExecutorRunner {
    fn record_fallback(&self, step: crate::chain::FallbackStep) {
        self.reports.fallback(step);
    }

    fn run_direct(&self, request: crate::direct::DirectRequest) -> BoxFuture<'_, Response> {
        Box::pin(crate::direct::execute(
            &self.executor,
            &self.upstreams,
            request,
        ))
    }

    fn run(&self, request: ChainRequest) -> BoxFuture<'_, Response> {
        let surface = request.surface;
        let recommendation_id = request.recommendation.recommendation_id.clone();
        let (disconnect, cancelled) = oneshot::channel::<()>();
        let walk = tokio::spawn(walk(self.clone(), request, async move {
            let _ = cancelled.await;
        }));
        Box::pin(async move {
            // Dropping this future (the client went away) drops `disconnect`, which
            // resolves `cancelled` in the walk.
            let _disconnect = disconnect;
            match walk.await {
                Ok(response) => response,
                Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
                Err(_) => exhausted(surface, &recommendation_id),
            }
        })
    }
}

/// Walk the chain, relay the settled attempt or answer `no_executable_candidates`, and
/// report.
async fn walk(
    runner: ExecutorRunner,
    request: ChainRequest,
    cancelled: impl Future<Output = ()> + Send,
) -> Response {
    let ChainRequest {
        surface,
        credential,
        upstream_headers,
        env_keys,
        body,
        raw_query,
        alias,
        settings_mode,
        recommendation,
        tenant_key,
        received_at,
        ..
    } = request;
    let format = wire_format(surface);
    // D15: an OpenRouter key on the chat surface runs every entry on OpenRouter.
    let openrouter_key = match (&credential, surface) {
        (CallerCredential::OpenRouter(key), Surface::Chat) => Some(key.as_str()),
        _ => None,
    };
    let zero_retention = recommendation.zero_retention;
    if openrouter_key.is_some()
        && openrouter::verdict(&recommendation.chain, zero_retention) == Verdict::NoCompliantColumns
    {
        tracing::warn!("maverick-proxy: no OpenRouter alias with zero-retention evidence; refused");
        return envelope::error(
            StatusCode::CONFLICT,
            envelope::NO_COMPLIANT_COLUMNS,
            envelope::NO_COMPLIANT_OPENROUTER_ALIAS_MESSAGE,
        );
    }
    let prepare = |_index: usize, entry: &ChainEntry| -> Option<PreparedAttempt> {
        // The model, the key, the URL, and the telemetry endpoint of this attempt.
        let (model, key, url, endpoint_used, zdr) = if let Some(key) = openrouter_key {
            let slug = match openrouter::eligibility(entry, zero_retention) {
                Eligibility::Slug(slug) => slug,
                skipped => {
                    tracing::debug!(
                        provider = %entry.provider,
                        reason = ?skipped,
                        "maverick-proxy: entry skipped in OpenRouter mode"
                    );
                    return None;
                }
            };
            (
                slug,
                key,
                runner.upstreams.openrouter_url(&raw_query),
                openrouter::ENDPOINT_USED,
                zero_retention,
            )
        } else {
            if surface == Surface::Messages && entry.provider != "anthropic" {
                return None;
            }
            let key = resolve_key(&entry.provider, surface, &credential, &env_keys)?;
            let url = runner.upstreams.url(surface, &entry.provider, &raw_query)?;
            (entry.model.as_str(), key, url, "direct", false)
        };
        // D14: the entry's settings unless the caller opted out; D29: its constraints.
        let settings = match settings_mode {
            SettingsMode::Apply => Some(&entry.settings),
            SettingsMode::Caller => None,
        };
        let rewritten = rewrite_attempt(
            &body,
            model,
            &AttemptParams {
                format,
                settings,
                constraints: &entry.param_constraints,
                zdr,
            },
        )
        .ok()?;
        let headers = attempt_headers(surface, &upstream_headers, key)?;
        let Ok(url) = reqwest::Url::parse(&url) else {
            tracing::warn!(
                provider = %entry.provider,
                "maverick-proxy: the upstream URL does not parse; entry skipped"
            );
            return None;
        };
        let mut upstream = reqwest::Request::new(reqwest::Method::POST, url);
        *upstream.headers_mut() = headers;
        *upstream.body_mut() = Some(rewritten.body.into());
        Some(PreparedAttempt {
            request: upstream,
            settings_applied: rewritten.settings_applied,
            endpoint_used: endpoint_used.to_owned(),
        })
    };

    let chain = &recommendation.chain;
    let outcome = runner
        .executor
        .execute(chain, prepare, None, cancelled)
        .await;
    let recommendation_id = recommendation.recommendation_id.clone();
    let mut items: Vec<TelemetryItem> = outcome
        .attempts
        .iter()
        .map(|record| record.telemetry_item(&recommendation_id))
        .collect();
    let reports = Arc::clone(&runner.reports);
    let report = ReportParts {
        tenant_key,
        surface,
        recommendation_id: recommendation_id.clone(),
        measured: recommendation.measured,
        classification_method: recommendation.classification_method.clone(),
        mode: engine_mode(alias.mode).to_owned(),
        received_at,
        telemetry_opt_out: recommendation.telemetry_opt_out,
    };

    match outcome.result {
        ChainResult::Relayed { index, response } => {
            let model_used = chain[index].model.clone();
            let final_status = response.status().as_u16();
            let settled_model = model_used.clone();
            let done: Done = Box::new(move |(tokens_in, tokens_out)| {
                if let Some(settled) = items.last_mut() {
                    settled.tokens_in = tokens_in;
                    settled.tokens_out = tokens_out;
                }
                reports.report(report.finish(settled_model, final_status, items));
            });
            relay(surface, &recommendation_id, &model_used, response, done).await
        }
        ChainResult::Exhausted { .. } => {
            reports.report(report.finish(String::new(), 502, items));
            exhausted(surface, &recommendation_id)
        }
    }
}

/// A report waiting for its items.
struct ReportParts {
    tenant_key: TenantKey,
    surface: Surface,
    recommendation_id: String,
    measured: bool,
    classification_method: String,
    mode: String,
    received_at: Instant,
    telemetry_opt_out: bool,
}

impl ReportParts {
    fn finish(
        self,
        model_used: String,
        final_status: u16,
        items: Vec<TelemetryItem>,
    ) -> ChainReport {
        ChainReport {
            tenant_key: self.tenant_key,
            surface: self.surface,
            recommendation_id: self.recommendation_id,
            measured: self.measured,
            classification_method: self.classification_method,
            mode: self.mode,
            model_used,
            final_status,
            items,
            telemetry_opt_out: self.telemetry_opt_out,
            received_at: self.received_at,
        }
    }
}

/// The engine's name for an alias mode.
fn engine_mode(mode: Mode) -> &'static str {
    match mode {
        Mode::Balanced => "balanced",
        Mode::Quality => "quality",
        Mode::Cost => "cost",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upstreams() -> Upstreams {
        Upstreams {
            openrouter: "http://or".to_owned(),
            openai: "http://o".to_owned(),
            anthropic: "http://a".to_owned(),
            google: "http://g/v1beta/openai".to_owned(),
            xai: "http://x".to_owned(),
        }
    }

    /// D15: OpenRouter's OpenAI-compatible endpoint, with the inbound query.
    #[test]
    fn the_openrouter_url_is_its_chat_completions_endpoint() {
        let up = upstreams();
        assert_eq!(up.openrouter_url(""), "http://or/chat/completions");
        assert_eq!(up.openrouter_url("a=1"), "http://or/chat/completions?a=1");
    }

    #[test]
    fn upstream_urls_follow_the_go_table() {
        let up = upstreams();
        let chat = |provider: &str| up.url(Surface::Chat, provider, "");
        assert_eq!(
            chat("openai").as_deref(),
            Some("http://o/v1/chat/completions")
        );
        assert_eq!(
            chat("anthropic").as_deref(),
            Some("http://a/v1/chat/completions")
        );
        assert_eq!(
            chat("google").as_deref(),
            Some("http://g/v1beta/openai/chat/completions")
        );
        assert_eq!(chat("xai").as_deref(), Some("http://x/v1/chat/completions"));
        assert_eq!(chat("mistral"), None);
        assert_eq!(chat("OpenAI"), None, "provider names are exact");
        assert_eq!(
            up.url(Surface::Messages, "anthropic", "beta=true")
                .as_deref(),
            Some("http://a/v1/messages?beta=true")
        );
        assert_eq!(up.url(Surface::Messages, "openai", ""), None);
        assert_eq!(
            up.url(Surface::Chat, "openai", "a=1&b=%20").as_deref(),
            Some("http://o/v1/chat/completions?a=1&b=%20")
        );
    }
}
