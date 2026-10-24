//! The recommend client: `POST {engine}/v1/recommend` for the proxy and the SDK
//! cores (sdk-v0 plan §2.1; master plan §3.5; reference part 6 §7; spec §8.1, §9.2).
//!
//! - The request carries the declared form (capability, difficulty, domain tags) or the
//!   prompt form (the workload projection and its version). [`Workload::for_chat_request`]
//!   picks the form for a chat request: `X-Maverick-Workload` first, then projection `p1`,
//!   then the declared fallback (D5).
//! - The body is written as the Go proxy wrote it: compact JSON, members in lexicographic
//!   order, HTML-escaped strings, `pool` omitted when empty.
//! - The response is decoded with Go's `encoding/json` rules, and it carries the Rust
//!   fields: entry `settings` (D14), `retention_mode` and `zero_retention` (D9),
//!   execution aliases with their retention (D15), and the tenant's `telemetry` flag
//!   (D39).
//! - `422 zero_retention_requires_declared` and `409 no_compliant_columns` are terminal
//!   privacy errors, typed apart from every other failure. They never trigger fail-open
//!   (D17). Every other failure is an engine error.
//! - An empty chain is not an engine error: it decodes, and the chain executor reports
//!   `no_executable_candidates`, as Go did.
//!
//! - Optional request fields are sent only when the engine advertised their feature in
//!   `X-Maverick-API-Features` on its last response (spec §14.5, D50), so a newer core never
//!   sends a field that an older self-hosted engine rejects with `422 invalid_request`.
//!
//! The client sends only the tenant's Maverick key, never a provider credential. Its only
//! state is that advertised feature list, in memory. It writes nothing to disk and logs
//! nothing; [`Workload`] and [`TenantKey`] redact themselves in `Debug` output.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde_json::{Map, Value};

use crate::affinity::{Affinity, AffinityRequest};
use crate::alias::{Alias, Mode};
use crate::projection::{self, PROJECTION_VERSION, Projection, WorkloadHeaderError};

mod wire;

/// The SDK cores' recommend timeout (sdk-v0 plan §2.1). The engine's 1.5 s classifier
/// deadline fits inside it. The proxy configures its own (the Go proxy used 10 s).
pub const SDK_TIMEOUT: Duration = Duration::from_secs(2);

/// The most retries a client makes (sdk-v0 plan §2.2: none beyond one).
pub const MAX_RETRIES: u8 = 1;

/// The declared fallback workload's capability, used when nothing can be projected.
pub const DECLARED_FALLBACK_CAPABILITY: &str = "multi_step_reasoning";

/// The declared fallback workload's difficulty.
pub const DECLARED_FALLBACK_DIFFICULTY: u32 = 3;

/// A tenant's Maverick API key. `Debug` never shows it.
#[derive(Clone, PartialEq, Eq)]
pub struct TenantKey(String);

impl TenantKey {
    /// Wrap a key such as `mk_live_...`.
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    /// The raw key, for the `Authorization` header only.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for TenantKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TenantKey(<redacted>)")
    }
}

/// The workload sent to the engine (spec §8.1).
#[derive(Clone, PartialEq, Eq)]
pub enum Workload {
    /// The declared form. Nothing about the prompt leaves the process.
    Declared {
        /// The capability; the engine validates it.
        capability: String,
        /// The tier; when absent the engine uses 3 and reports `difficulty_source: default`.
        difficulty: Option<u32>,
        /// Optional domain tags.
        domain_tags: Vec<String>,
    },
    /// The prompt form: the workload projection text and its version.
    Prompt {
        /// The prompt text sent for classification.
        prompt: String,
        /// `workload.projection_version`, when the prompt is a projection.
        projection_version: Option<String>,
    },
}

impl Workload {
    /// `{"capability":"multi_step_reasoning","difficulty":3}`, the Go fallback.
    #[must_use]
    pub fn declared_fallback() -> Self {
        Self::Declared {
            capability: DECLARED_FALLBACK_CAPABILITY.to_owned(),
            difficulty: Some(DECLARED_FALLBACK_DIFFICULTY),
            domain_tags: Vec::new(),
        }
    }

    /// The prompt form for a workload projection, tagged with its version.
    #[must_use]
    pub fn from_projection(projection: Projection) -> Self {
        Self::Prompt {
            prompt: projection.text,
            projection_version: Some(PROJECTION_VERSION.to_owned()),
        }
    }

    /// The workload for a chat request (D5). A present `X-Maverick-Workload` value selects
    /// the declared form. Without it, the body's projection selects the prompt form, and a
    /// body with nothing to project gets the declared fallback.
    ///
    /// A malformed header is an error, never a fall back to the projection: the caller
    /// asked that the prompt not leave the process.
    pub fn for_chat_request(
        body: &[u8],
        workload_header: Option<&str>,
    ) -> Result<Self, WorkloadHeaderError> {
        if let Some(value) = workload_header {
            let declared = projection::parse_workload_header(value)?;
            return Ok(Self::Declared {
                capability: declared.capability,
                difficulty: declared.difficulty,
                domain_tags: Vec::new(),
            });
        }
        Ok(projection::project(body).map_or_else(Self::declared_fallback, Self::from_projection))
    }
}

impl fmt::Debug for Workload {
    /// The prompt text never appears in `Debug` output (spec §14.3).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Declared {
                capability,
                difficulty,
                domain_tags,
            } => f
                .debug_struct("Declared")
                .field("capability", capability)
                .field("difficulty", difficulty)
                .field("domain_tags", domain_tags)
                .finish(),
            Self::Prompt {
                prompt,
                projection_version,
            } => f
                .debug_struct("Prompt")
                .field(
                    "prompt",
                    &format_args!("<redacted, {} chars>", prompt.chars().count()),
                )
                .field("projection_version", projection_version)
                .finish(),
        }
    }
}

/// One `POST /v1/recommend` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecommendRequest {
    /// The ranking mode.
    pub mode: Mode,
    /// The pool; `None` or empty means the tenant's default pool.
    pub pool: Option<String>,
    /// The workload.
    pub workload: Workload,
    /// Conversation affinity (D44). Written only when the engine advertised the `affinity`
    /// feature (D50).
    pub affinity: Option<AffinityRequest>,
}

impl RecommendRequest {
    /// The request an alias selects.
    #[must_use]
    pub fn for_alias(alias: &Alias, workload: Workload) -> Self {
        Self {
            mode: alias.mode,
            pool: alias.pool.clone(),
            workload,
            affinity: None,
        }
    }

    /// The exact request body for an engine that advertised no optional feature: the
    /// first-release fields only.
    #[must_use]
    pub fn to_body(&self) -> String {
        self.to_body_for(&EngineFeatures::default())
    }

    /// The exact request body for an engine that advertised `features`: an optional field
    /// is written only when its feature is listed (spec §14.5, D50).
    #[must_use]
    pub fn to_body_for(&self, features: &EngineFeatures) -> String {
        wire::encode_request(self, features)
    }
}

/// The response header that lists the optional request features the engine accepts.
pub const API_FEATURES_HEADER: &str = "x-maverick-api-features";

/// The optional request features an engine advertised in `X-Maverick-API-Features` (spec
/// §14.5, D50). The default, no features, is what a core assumes before the engine's first
/// response and what an engine older than D50 means by sending no header.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EngineFeatures(BTreeSet<String>);

impl EngineFeatures {
    /// Read the header value: comma-separated names, each trimmed of spaces and tabs, with
    /// empty names dropped. Names are case sensitive. No header means no features.
    #[must_use]
    pub fn parse(value: Option<&str>) -> Self {
        Self(
            value
                .unwrap_or_default()
                .split(',')
                .map(|name| name.trim_matches([' ', '\t']))
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .collect(),
        )
    }

    /// The features a response's headers advertise. A missing or non-text header is none.
    #[must_use]
    pub fn from_headers(headers: &HeaderMap) -> Self {
        Self::parse(
            headers
                .get(API_FEATURES_HEADER)
                .and_then(|value| value.to_str().ok()),
        )
    }

    /// True when the engine advertised `feature`, so its optional field may be sent.
    #[must_use]
    pub fn allows(&self, feature: &str) -> bool {
        self.0.contains(feature)
    }

    /// The advertised names, sorted.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }
}

/// A column's or an execution alias's effective retention mode (spec §4.5). Anything but
/// `standard` or `zero` reads as [`RetentionMode::Unknown`], which is never zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RetentionMode {
    /// Not known to retain nothing. Also the reading of a missing or unrecognized value.
    #[default]
    Unknown,
    /// The provider's standard retention.
    Standard,
    /// Verified zero retention.
    Zero,
}

impl RetentionMode {
    /// Read the engine's value. Only exact `standard` and `zero` are recognized.
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value {
            "standard" => Self::Standard,
            "zero" => Self::Zero,
            _ => Self::Unknown,
        }
    }

    /// The engine's name for the mode.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Standard => "standard",
            Self::Zero => "zero",
        }
    }
}

/// One chain entry.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ChainEntry {
    /// The provider, such as `anthropic`.
    pub provider: String,
    /// The model id to call.
    pub model: String,
    /// The endpoint, such as `direct` or `vertex`.
    pub endpoint: String,
    /// The engine's column id.
    pub column_id: String,
    /// The column's settings (reasoning effort, thinking mode), applied unless the alias
    /// has `?settings=caller` (D14).
    pub settings: Map<String, Value>,
    /// The entry's effective retention mode (D9).
    pub retention_mode: RetentionMode,
    /// Execution aliases by backend, such as `openrouter` to a model slug (D15).
    pub execution_aliases: BTreeMap<String, String>,
    /// Each execution alias's effective retention mode (D15).
    pub execution_alias_retention: BTreeMap<String, RetentionMode>,
    /// The model's parameter constraints from the catalog (D29), keyed by
    /// surface-neutral field. Empty means the Go mappings apply.
    pub param_constraints: crate::normalize::ParamConstraints,
}

/// A decoded recommend response.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Recommendation {
    /// `recommendation_id`.
    pub recommendation_id: String,
    /// `recommendations`: the fallback chain. It can be empty; the chain executor reports
    /// `no_executable_candidates` for that.
    pub chain: Vec<ChainEntry>,
    /// `measured`.
    pub measured: bool,
    /// `classification.method`.
    pub classification_method: String,
    /// `zero_retention`: the tenant is zero-retention, so the core switches to declared
    /// mode and applies the zero-retention fail-open rules (D17).
    pub zero_retention: bool,
    /// `classification_quota_exceeded`.
    pub classification_quota_exceeded: bool,
    /// `telemetry: false` (D39): the tenant opted out, so no MM component sends telemetry
    /// for this request. An absent or `null` member (a Go-era engine) leaves it `false`.
    pub telemetry_opt_out: bool,
    /// `affinity`: the conversation affinity decision, present only when the request sent
    /// `affinity` (D44).
    pub affinity: Option<Affinity>,
}

impl Recommendation {
    /// Decode a 2xx `POST /v1/recommend` body with the rules the client applies to every
    /// response (Go's decoding, reference part 6 §7, plus the Rust-only members). The
    /// conformance vectors replay it.
    pub fn decode(body: &[u8]) -> Result<Self, String> {
        wire::decode_response(body)
    }
}

/// Why a recommend call produced no recommendation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RecommendError {
    /// `422 zero_retention_requires_declared`: terminal. The prompt form is refused for
    /// this zero-retention tenant; the caller must send the declared form.
    #[error("zero_retention_requires_declared")]
    ZeroRetentionRequiresDeclared {
        /// The HTTP status (422).
        status: u16,
        /// The engine's message.
        message: String,
    },
    /// `409 no_compliant_columns`: terminal. No column in the pool holds valid
    /// zero-retention evidence.
    #[error("no_compliant_columns")]
    NoCompliantColumns {
        /// The HTTP status (409).
        status: u16,
        /// The engine's message.
        message: String,
    },
    /// Any other failure. The Go proxy answered these with `502 engine_error`; an SDK core
    /// falls open.
    #[error("engine error: {0}")]
    Engine(#[from] EngineFailure),
}

impl RecommendError {
    /// True for the terminal privacy errors, which must never trigger fail-open.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        !matches!(self, Self::Engine(_))
    }
}

/// A non-terminal recommend failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EngineFailure {
    /// The request could not be sent or the response not read.
    #[error("transport: {0}")]
    Transport(String),
    /// The deadline passed.
    #[error("timed out")]
    Timeout,
    /// A non-2xx status other than the terminal privacy errors.
    #[error("status {status}")]
    Status {
        /// The HTTP status.
        status: u16,
        /// The envelope's `error.code`, when the body had one.
        code: Option<String>,
    },
    /// A 2xx body that Go's decoding would reject.
    #[error("malformed response: {0}")]
    Decode(String),
    /// The tenant key cannot be sent as a header value.
    #[error("the tenant key is not a valid header value")]
    InvalidTenantKey,
}

impl EngineFailure {
    /// Connection failures and 5xx statuses may be retried; nothing else is.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Transport(_) => true,
            Self::Status { status, .. } => *status >= 500,
            Self::Timeout | Self::Decode(_) | Self::InvalidTenantKey => false,
        }
    }
}

/// The caller's W3C trace context (`traceparent`, `tracestate`) to pass to the engine.
/// A value that is not a valid header value is dropped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TraceHeaders {
    pub traceparent: Option<String>,
    pub tracestate: Option<String>,
}

impl TraceHeaders {
    /// The header pairs to send, in a fixed order.
    fn pairs(&self) -> Vec<(&'static str, HeaderValue)> {
        [
            ("traceparent", &self.traceparent),
            ("tracestate", &self.tracestate),
        ]
        .into_iter()
        .filter_map(|(name, value)| {
            let value = HeaderValue::from_str(value.as_deref()?).ok()?;
            Some((name, value))
        })
        .collect()
    }
}

/// Recommend client settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecommendClientConfig {
    /// The engine base URL, such as `https://api.modelmaverick.dev`.
    pub base_url: String,
    /// One deadline for the whole call, retries included.
    pub timeout: Duration,
    /// Retries after a retryable failure, at most [`MAX_RETRIES`].
    pub max_retries: u8,
}

impl RecommendClientConfig {
    /// SDK-core defaults: [`SDK_TIMEOUT`] and no retries.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            timeout: SDK_TIMEOUT,
            max_retries: 0,
        }
    }
}

/// The recommend client. Cheap to clone; clones share one connection pool and one
/// remembered feature list.
#[derive(Debug, Clone)]
pub struct RecommendClient {
    http: reqwest::Client,
    url: String,
    timeout: Duration,
    attempts: u8,
    /// The features the engine at `url` advertised on its last response (D50). Every
    /// response replaces it, an error status included; a call that got no response keeps it.
    features: Arc<Mutex<EngineFeatures>>,
}

impl RecommendClient {
    /// A client with its own HTTP connection pool.
    pub fn new(config: RecommendClientConfig) -> Result<Self, reqwest::Error> {
        Ok(Self::with_http(reqwest::Client::builder().build()?, config))
    }

    /// A client on an existing `reqwest::Client`.
    #[must_use]
    pub fn with_http(http: reqwest::Client, config: RecommendClientConfig) -> Self {
        Self {
            http,
            url: format!("{}/v1/recommend", config.base_url.trim_end_matches('/')),
            timeout: config.timeout,
            attempts: 1 + config.max_retries.min(MAX_RETRIES),
            features: Arc::default(),
        }
    }

    /// The features the engine advertised on its last response; none before the first.
    #[must_use]
    pub fn features(&self) -> EngineFeatures {
        self.features
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn remember_features(&self, headers: &HeaderMap) {
        *self.features.lock().unwrap_or_else(PoisonError::into_inner) =
            EngineFeatures::from_headers(headers);
    }

    /// Call `POST /v1/recommend` with the tenant's key.
    pub async fn recommend(
        &self,
        key: &TenantKey,
        request: &RecommendRequest,
    ) -> Result<Recommendation, RecommendError> {
        self.recommend_traced(key, request, &TraceHeaders::default())
            .await
    }

    /// [`RecommendClient::recommend`] carrying the caller's W3C trace context, so the
    /// engine's request span joins the caller's trace (spec §14.3).
    pub async fn recommend_traced(
        &self,
        key: &TenantKey,
        request: &RecommendRequest,
        trace: &TraceHeaders,
    ) -> Result<Recommendation, RecommendError> {
        let body = request.to_body_for(&self.features());
        let deadline = Instant::now() + self.timeout;
        let mut attempt = 1;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(EngineFailure::Timeout.into());
            }
            match self.attempt(key, &body, remaining, trace).await {
                Err(RecommendError::Engine(failure))
                    if attempt < self.attempts && failure.is_retryable() =>
                {
                    attempt += 1;
                }
                outcome => return outcome,
            }
        }
    }

    async fn attempt(
        &self,
        key: &TenantKey,
        body: &str,
        timeout: Duration,
        trace: &TraceHeaders,
    ) -> Result<Recommendation, RecommendError> {
        let mut authorization = HeaderValue::from_str(&format!("Bearer {}", key.expose()))
            .map_err(|_| EngineFailure::InvalidTenantKey)?;
        authorization.set_sensitive(true);
        let mut builder = self
            .http
            .post(&self.url)
            .header(CONTENT_TYPE, "application/json")
            .header(AUTHORIZATION, authorization);
        for (name, value) in trace.pairs() {
            builder = builder.header(name, value);
        }
        let response = builder
            .body(body.to_owned())
            .timeout(timeout)
            .send()
            .await
            .map_err(transport_failure)?;
        self.remember_features(response.headers());
        let status = response.status().as_u16();
        let bytes = response.bytes().await.map_err(transport_failure)?;
        if !(200..300).contains(&status) {
            return Err(wire::error_response(status, &bytes));
        }
        wire::decode_response(&bytes).map_err(|reason| EngineFailure::Decode(reason).into())
    }
}

fn transport_failure(error: reqwest::Error) -> EngineFailure {
    if error.is_timeout() {
        EngineFailure::Timeout
    } else {
        EngineFailure::Transport(error.without_url().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alias::parse_alias;

    fn body(alias: &str, workload: Workload) -> String {
        let alias = parse_alias(alias)
            .expect("alias")
            .into_route()
            .expect("route");
        RecommendRequest::for_alias(&alias, workload).to_body()
    }

    #[test]
    fn bodies_sort_members_and_omit_empty_pool() {
        assert_eq!(
            body("maverick/auto", Workload::declared_fallback()),
            r#"{"mode":"quality","workload":{"capability":"multi_step_reasoning","difficulty":3}}"#
        );
        assert_eq!(
            body(
                "maverick/p:cost",
                Workload::Declared {
                    capability: "code_generation".to_owned(),
                    difficulty: None,
                    domain_tags: vec!["legal".to_owned(), "a&b".to_owned()],
                }
            ),
            r#"{"mode":"cost","pool":"p","workload":{"capability":"code_generation","domain_tags":["legal","a\u0026b"]}}"#
        );
        assert_eq!(
            body(
                "maverick/q:quality",
                Workload::Prompt {
                    prompt: "a<b".to_owned(),
                    projection_version: Some("p1".to_owned()),
                }
            ),
            r#"{"mode":"quality","pool":"q","workload":{"projection_version":"p1","prompt":"a\u003cb"}}"#
        );
        let request = RecommendRequest {
            mode: Mode::Balanced,
            pool: Some(String::new()),
            workload: Workload::Prompt {
                prompt: "x".to_owned(),
                projection_version: None,
            },
            affinity: None,
        };
        assert_eq!(
            request.to_body(),
            r#"{"mode":"balanced","workload":{"prompt":"x"}}"#
        );
    }

    #[test]
    fn workload_selection_follows_d5() {
        let chat = br#"{"messages":[{"role":"user","content":"hi"}]}"#;
        assert_eq!(
            Workload::for_chat_request(chat, None),
            Ok(Workload::Prompt {
                prompt: "[conversation]\nuser: hi".to_owned(),
                projection_version: Some("p1".to_owned()),
            })
        );
        assert_eq!(
            Workload::for_chat_request(chat, Some("code_generation:4")),
            Ok(Workload::Declared {
                capability: "code_generation".to_owned(),
                difficulty: Some(4),
                domain_tags: Vec::new(),
            })
        );
        assert_eq!(
            Workload::for_chat_request(chat, Some("code_generation:hard")),
            Err(WorkloadHeaderError::InvalidTier)
        );
        assert_eq!(
            Workload::for_chat_request(b"not json", None),
            Ok(Workload::declared_fallback())
        );
    }

    /// D44, D50: `affinity` is written only when the engine advertised it, before `mode`,
    /// with its set members in order.
    #[test]
    fn affinity_is_written_only_when_advertised() {
        let mut request = RecommendRequest::for_alias(
            &Alias {
                pool: None,
                mode: Mode::Quality,
            },
            Workload::declared_fallback(),
        );
        request.affinity = Some(AffinityRequest {
            key: "conv<1>".to_owned(),
            context_tokens: Some(30),
            cost_aware: Some(true),
        });
        let plain =
            r#"{"mode":"quality","workload":{"capability":"multi_step_reasoning","difficulty":3}}"#;
        assert_eq!(request.to_body(), plain);
        assert_eq!(
            request.to_body_for(&EngineFeatures::parse(Some("other"))),
            plain
        );
        let advertised = EngineFeatures::parse(Some("affinity"));
        assert_eq!(
            request.to_body_for(&advertised),
            r#"{"affinity":{"context_tokens":30,"cost_aware":true,"key":"conv\u003c1\u003e"},"mode":"quality","workload":{"capability":"multi_step_reasoning","difficulty":3}}"#
        );
        request.affinity = Some(AffinityRequest {
            key: "k".to_owned(),
            context_tokens: None,
            cost_aware: Some(false),
        });
        assert_eq!(
            request.to_body_for(&advertised),
            r#"{"affinity":{"cost_aware":false,"key":"k"},"mode":"quality","workload":{"capability":"multi_step_reasoning","difficulty":3}}"#
        );
        request.affinity = None;
        assert_eq!(request.to_body_for(&advertised), plain);
    }

    #[test]
    fn debug_output_redacts_prompts_and_keys() {
        let workload = Workload::Prompt {
            prompt: "secret prompt text".to_owned(),
            projection_version: Some("p1".to_owned()),
        };
        let request = RecommendRequest {
            mode: Mode::Cost,
            pool: None,
            workload,
            affinity: Some(crate::affinity::AffinityRequest::for_turn(
                "conv-secret-key",
                8,
            )),
        };
        let shown = format!("{request:?}");
        assert!(!shown.contains("secret prompt text"), "{shown}");
        assert!(!shown.contains("conv-secret-key"), "{shown}");
        assert!(shown.contains("<redacted, 18 chars>"), "{shown}");
        let key = format!("{:?}", TenantKey::new("mk_live_abc"));
        assert_eq!(key, "TenantKey(<redacted>)");
    }

    /// W3C trace context is sent only as valid header values, `traceparent` first.
    #[test]
    fn trace_headers_skip_absent_and_invalid_values() {
        let trace = TraceHeaders {
            traceparent: Some("00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01".into()),
            tracestate: Some("bad\nvalue".into()),
        };
        let pairs = trace.pairs();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].0, "traceparent");
        assert!(TraceHeaders::default().pairs().is_empty());
    }

    /// D29: entries carry the catalog constraints; a malformed value is no constraint.
    #[test]
    fn entries_carry_param_constraints() {
        let body = br#"{"recommendations":[
            {"model":"a","param_constraints":{"reasoning_effort":{"allowed":["low","high"],"order":["low","medium","high"]}}},
            {"model":"b","param_constraints":"garbage"},
            {"model":"c"}
        ]}"#;
        let decoded = wire::decode_response(body).expect("decodes");
        let effort = &decoded.chain[0].param_constraints[crate::normalize::REASONING_EFFORT];
        assert_eq!(effort.allowed.len(), 2);
        assert_eq!(effort.order.len(), 3);
        assert!(decoded.chain[1].param_constraints.is_empty());
        assert!(decoded.chain[2].param_constraints.is_empty());
    }

    #[test]
    fn retention_modes_parse_exactly() {
        assert_eq!(RetentionMode::parse("zero"), RetentionMode::Zero);
        assert_eq!(RetentionMode::parse("standard"), RetentionMode::Standard);
        for other in ["", "unknown", "Zero", "ZERO", " zero"] {
            assert_eq!(
                RetentionMode::parse(other),
                RetentionMode::Unknown,
                "{other:?}"
            );
        }
        for mode in [
            RetentionMode::Unknown,
            RetentionMode::Standard,
            RetentionMode::Zero,
        ] {
            assert_eq!(RetentionMode::parse(mode.as_str()), mode);
        }
    }

    #[test]
    fn only_transport_and_5xx_are_retryable() {
        assert!(EngineFailure::Transport("x".into()).is_retryable());
        assert!(
            EngineFailure::Status {
                status: 500,
                code: None
            }
            .is_retryable()
        );
        assert!(
            EngineFailure::Status {
                status: 503,
                code: None
            }
            .is_retryable()
        );
        assert!(
            !EngineFailure::Status {
                status: 499,
                code: None
            }
            .is_retryable()
        );
        assert!(
            !EngineFailure::Status {
                status: 422,
                code: None
            }
            .is_retryable()
        );
        assert!(!EngineFailure::Timeout.is_retryable());
        assert!(!EngineFailure::Decode("x".into()).is_retryable());
        assert!(!EngineFailure::InvalidTenantKey.is_retryable());
    }
}
