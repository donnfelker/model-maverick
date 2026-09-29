//! The server each Go test built (README §4.2, §4.3): the public and internal routers
//! over the one shared database, configured as that test's environment constructor did.
//! A test's Go file sets its default constructor (reference Part 5 §13 lists each test
//! under its file); a few tests override it.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use axum::Router;
use chrono::{DateTime, TimeZone, Utc};
use maverick_api::capture::Capturer;
use maverick_api::classify::{Classifier, Resolver, ResolverSettings};
use maverick_api::{
    ApiState, AuthState, Clock, HealthState, InternalSettings, PgAuthBackend, RateLimiter,
    RecommendConfig, internal_router_with, public_router_with,
};
use maverick_domain::capture::KeyRing;
use maverick_domain::keys;
use maverick_domain::ranking::{AVAILABILITY_FLOOR, RankParams};
use maverick_providers::{
    CompletionRequest, CompletionResponse, ProviderClient, ProviderError, ProviderErrorKind,
    ProviderFuture,
};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

/// The fixed clock of `newTestEnv`: 2026-07-06T12:00:30Z.
pub fn fixed_now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 7, 6, 12, 0, 30).unwrap()
}

struct FixedClock;

impl Clock for FixedClock {
    fn unix_seconds(&self) -> i64 {
        fixed_now().timestamp()
    }
}

/// README §4.3: the extraction stub text.
pub const EXTRACTION: &str = r#"{"capability":"structured_extraction","difficulty":3,"domain_tags":["legal"],"confidence":0.91}"#;

/// A classifier provider that answers the current script (`None` fails).
#[derive(Clone, Default)]
pub struct Stub(Arc<Mutex<VecDeque<Option<String>>>>);

impl Stub {
    fn new(reply: Option<&str>) -> Self {
        let stub = Stub::default();
        stub.answer(reply);
        stub
    }

    /// Every later call answers `reply`.
    pub fn answer(&self, reply: Option<&str>) {
        let mut script = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        script.clear();
        script.push_back(reply.map(str::to_owned));
    }
}

impl ProviderClient for Stub {
    fn complete<'a>(&'a self, _request: CompletionRequest) -> ProviderFuture<'a> {
        let reply = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .front()
            .cloned()
            .flatten();
        Box::pin(async move {
            match reply {
                Some(text) => Ok(CompletionResponse {
                    text,
                    tokens_in: 10,
                    tokens_out: 5,
                }),
                None => Err(ProviderError::new(
                    "anthropic",
                    ProviderErrorKind::ProviderError,
                )),
            }
        })
    }
}

/// One test's server.
pub struct Env {
    pub public: Router,
    pub internal: Router,
    /// The prompt-path classifier stub, when the environment has one.
    pub stub: Option<Stub>,
}

/// Each recorded test's Go file, from reference Part 5 §13.
pub fn test_files() -> HashMap<String, String> {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../reference/engine-reference.md");
    let reference = std::fs::read_to_string(path).expect("engine reference");
    let start = reference
        .find("#### 13.1 server_test.go")
        .expect("Part 5 test catalog");
    let mut files = HashMap::new();
    let mut file = String::new();
    for line in reference[start..].lines() {
        if let Some(rest) = line.strip_prefix("#### 13.") {
            match rest.split_whitespace().nth(1) {
                Some(name) if name.ends_with(".go") => file = name.to_owned(),
                _ => break,
            }
            continue;
        }
        let mut rest = line;
        while let Some(at) = rest.find("<a id=\"t-") {
            let tail = &rest[at + 9..];
            let end = tail.find('"').expect("anchor end");
            files.insert(tail[..end].to_owned(), file.clone());
            rest = &tail[end..];
        }
    }
    files
}

#[derive(Clone, Copy, PartialEq)]
enum Ranking {
    /// `newTestEnv`: every knob at its zero value.
    Zero,
    /// `withCeilingBuffer`: only the ceiling buffer.
    CeilingBuffer,
    /// `withRecommendDefaults`.
    Defaults,
}

fn ranking_of(file: &str) -> Ranking {
    match file {
        "policies_test.go" => Ranking::CeilingBuffer,
        "recommend_test.go"
        | "recommend_prompt_test.go"
        | "capture_test.go"
        | "capture_read_test.go"
        | "internalv1_captures_test.go"
        | "mcp_test.go" => Ranking::Defaults,
        _ => Ranking::Zero,
    }
}

/// The prompt-path stub text of a test, when its environment has a classifier column.
fn stub_of(test: &str, file: &str) -> Option<Option<&'static str>> {
    match test {
        "TestRecommendPromptClassifierOutageFallsBack" => Some(None),
        "TestRecommendPromptHighStakesTagDerivation/medical_tag" => Some(Some(
            r#"{"capability":"factual_qa","difficulty":2,"domain_tags":["medical"],"confidence":0.85}"#,
        )),
        "TestRecommendPromptHighStakesTagDerivation/non-stakes_tags" => Some(Some(
            r#"{"capability":"factual_qa","difficulty":2,"domain_tags":["internal","support"],"confidence":0.85}"#,
        )),
        _ => match file {
            "recommend_prompt_test.go"
            | "capture_test.go"
            | "capture_read_test.go"
            | "internalv1_captures_test.go" => Some(Some(EXTRACTION)),
            _ => None,
        },
    }
}

fn rpm_of(test: &str) -> u32 {
    match test {
        "TestV1RateLimitExceeded429" | "TestMCPRateLimitAppliesToMCPRequests" => 2,
        "TestV1RateLimitIsPerKey" => 1,
        _ => 300,
    }
}

/// The server `test` (in Go file `file`) ran against.
pub fn build(test: &str, file: &str, pool: &PgPool) -> Env {
    let pool = if test == "TestUpReturns503WhenDatabaseUnreachable" {
        PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(500))
            .connect_lazy("postgres://replay@127.0.0.1:1/unreachable")
            .expect("lazy pool")
    } else {
        pool.clone()
    };
    let ranking = ranking_of(file);
    let rank = match ranking {
        Ranking::Defaults => RankParams::default(),
        Ranking::Zero | Ranking::CeilingBuffer => RankParams {
            band_quality: 0.0,
            band_balanced: 0.0,
            band_cost: 0.0,
            noise_band: 0.0,
            availability_floor: AVAILABILITY_FLOOR,
        },
    };
    let ceiling_buffer = if ranking == Ranking::Zero { 0.0 } else { 0.05 };
    let stub = stub_of(test, file).map(Stub::new);
    let classifier = match &stub {
        Some(stub) => {
            let column = "claude-haiku-4-5/direct";
            Resolver::new(ResolverSettings {
                column: column.to_owned(),
                ..ResolverSettings::default()
            })
            .with_classifier(Classifier::new(Arc::new(stub.clone()), column))
        }
        None => Resolver::default(),
    };
    let capturer =
        Capturer::new(KeyRing::new(1, [0xC4; 32], None), 72).with_max_window_minutes(1440);
    let recommend = RecommendConfig {
        rank,
        ceiling_buffer,
        classifier: Arc::new(classifier),
        capture: Some(Arc::new(capturer)),
        ..RecommendConfig::default()
    };
    let internal = InternalSettings {
        cold_start_nights: 5,
        clock: Arc::new(fixed_now),
        ..InternalSettings::default()
    };
    let api = ApiState::with_health(
        pool.clone(),
        HealthState::with_providers(pool.clone(), Vec::new()),
    )
    .with_recommend_config(recommend)
    .with_internal(internal);
    let auth = AuthState::new(
        Arc::new(PgAuthBackend::new(pool)),
        keys::parse_internal_tokens("ops:alpha,rails:beta").expect("internal tokens"),
        RateLimiter::with_clock(rpm_of(test), Arc::new(FixedClock)),
    );
    Env {
        public: public_router_with(auth.clone(), api.clone()),
        internal: internal_router_with(auth, api),
        stub,
    }
}
