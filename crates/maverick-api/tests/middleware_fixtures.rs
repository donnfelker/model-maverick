//! The Go `apierrors` and `middleware` package cases (`parity/fixtures/api/apierrors.json`,
//! `middleware.json`) and the `auth` verifier cases (reference Part 4 "auth"), run against
//! the Rust envelope writer, middleware, and `PgAuthBackend` (bead 5.18).
//!
//! The Go body-limit cases use a configurable 16-byte cap; the Rust cap is the fixed
//! 2 MiB the spec names (§9.1), so those cases run at 2 MiB with the same boundaries.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use axum::body::{Body, Bytes};
use axum::extract::{Extension, Request};
use axum::http::{Response, StatusCode, header};
use axum::routing::any;
use axum::{Router, middleware};
use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use http_body_util::BodyExt;
use maverick_api::{
    ApiState, AuthBackendError, AuthState, Clock, ERROR_CODES, ErrorCode, GO_ERROR_CODES, Identity,
    InternalActor, PgAuthBackend, PublicAuthBackend, PublicAuthDecision, RateLimiter, RequestId,
    StaticAuthBackend, error_response, internal_auth, public_auth, public_router_with, read_body,
    recoverer, request_id,
};
use maverick_domain::keys;
use maverick_store::{MIGRATOR, tenancy};
use maverick_testdb::Schema;
use serde_json::Value;
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

static APIERRORS: LazyLock<Value> =
    LazyLock::new(|| parity_support::fixture("api", "apierrors.json").expect("apierrors.json"));
static MIDDLEWARE: LazyLock<Value> =
    LazyLock::new(|| parity_support::fixture("api", "middleware.json").expect("middleware.json"));
static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

const CAP: usize = 2 * 1024 * 1024;
const WELL_FORMED: &str = "Bearer mk_live_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn case<'a>(fixture: &'a Value, id: &str) -> &'a Value {
    fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == id)
        .unwrap_or_else(|| panic!("no case {id}"))
}

async fn text(response: Response<Body>) -> (u16, String) {
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn code_of(name: &str) -> ErrorCode {
    use ErrorCode::*;
    [
        Unauthorized,
        TenantSuspended,
        NotFound,
        ZeroRetentionConflict,
        NoCompliantColumns,
        CaptureExpired,
        PayloadTooLarge,
        InvalidRequest,
        UnknownModel,
        ModelNotInPool,
        UnknownPool,
        PoolWouldBeEmpty,
        ZeroRetentionRequiresDeclared,
        GraderUnsupported,
        RateLimited,
        InternalError,
    ]
    .into_iter()
    .find(|code| code.as_str() == name)
    .unwrap_or_else(|| panic!("no code {name}"))
}

// ---- apierrors --------------------------------------------------------------------------

// parity: apierrors.TestWriteEnvelopeByteExactAppendixB
// parity: apierrors.TestWriteNilDetailsMarshalsEmptyObject
#[tokio::test]
async fn the_envelope_writer_matches_the_go_cases() {
    for id in ["write-envelope-byte-exact", "nil-details-empty-object"] {
        let case = case(&APIERRORS, id);
        let input = &case["input"];
        let details: BTreeMap<String, Value> = input["details"]
            .as_object()
            .map(|map| map.clone().into_iter().collect())
            .unwrap_or_default();
        let response = error_response(
            StatusCode::from_u16(u16::try_from(input["status"].as_u64().unwrap()).unwrap())
                .unwrap(),
            code_of(input["code"].as_str().unwrap()),
            input["message"].as_str().unwrap(),
            details,
        );
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/json",
            "{id}"
        );
        let (status, body) = text(response).await;
        assert_eq!(
            u64::from(status),
            case["expected"]["status"].as_u64().unwrap(),
            "{id}"
        );
        assert_eq!(
            body,
            case["expected"]["body_exact"].as_str().unwrap(),
            "{id}"
        );
    }
}

// parity: apierrors.TestCodeConstantsMatchAppendixB
#[test]
fn the_code_list_is_go_s_plus_the_two_privacy_codes() {
    let expected: Vec<&str> = case(&APIERRORS, "code-constants")["expected"]["codes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap())
        .collect();
    assert_eq!(GO_ERROR_CODES.as_slice(), expected.as_slice());
    // Spec Appendix B (changed 2026-09-28 for the Rust port): the two privacy codes (D2
    // and the §7.1 zero-retention constraint) join the closed list.
    let rust_only: Vec<&str> = ERROR_CODES
        .iter()
        .copied()
        .filter(|code| !GO_ERROR_CODES.contains(code))
        .collect();
    assert_eq!(
        rust_only,
        ["no_compliant_columns", "zero_retention_requires_declared"]
    );
    for code in GO_ERROR_CODES {
        assert_eq!(code_of(code).as_str(), code);
    }
}

// parity: apierrors.TestNotFoundHandlerWritesEnvelope
#[tokio::test]
async fn the_not_found_handler_writes_the_envelope() {
    let case = case(&APIERRORS, "not-found-handler");
    let router = public_router_with(
        AuthState::new(
            Arc::new(StaticAuthBackend::missing()),
            keys::parse_internal_tokens("ops:alpha").unwrap(),
            RateLimiter::new(10),
        ),
        ApiState::detached(),
    );
    let response = router
        .oneshot(
            Request::builder()
                .method(case["input"]["method"].as_str().unwrap())
                .uri(case["input"]["path"].as_str().unwrap())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, body) = text(response).await;
    assert_eq!(
        u64::from(status),
        case["expected"]["status"].as_u64().unwrap()
    );
    assert_eq!(body, case["expected"]["body_exact"].as_str().unwrap());
}

// ---- request id and recoverer -------------------------------------------------------------

async fn echo_request_id(Extension(id): Extension<RequestId>) -> String {
    id.0
}

// parity: middleware.TestRequestIDMintsWhenAbsent
// parity: middleware.TestRequestIDAcceptsInboundHeader
#[tokio::test]
async fn the_request_id_is_minted_or_accepted_and_matches_the_context() {
    let router = Router::new()
        .route("/", any(echo_request_id))
        .layer(middleware::from_fn(request_id));
    for id in ["request-id-mints", "request-id-accepts-inbound"] {
        let case = case(&MIDDLEWARE, id);
        let mut request = Request::builder().uri("/");
        if let Some(inbound) = case["given"]["inbound_x_request_id"].as_str() {
            request = request.header("X-Request-ID", inbound);
        }
        let response = router
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let header = response.headers()["X-Request-ID"]
            .to_str()
            .unwrap()
            .to_owned();
        let (_, context) = text(response).await;
        assert_eq!(header, context, "{id}: the header is the context value");
        match case["given"]["inbound_x_request_id"].as_str() {
            Some(inbound) => assert_eq!(context, inbound),
            None => {
                assert_eq!(context.len(), 26, "a ULID");
                assert!(
                    context
                        .bytes()
                        .all(|b| b.is_ascii_digit() || b.is_ascii_uppercase())
                );
            }
        }
    }
}

/// Every event's fields, rendered.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<String>>>);

struct Fields(String);

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.push_str(&format!(" {}={value:?}", field.name()));
    }
}

impl<S: Subscriber> Layer<S> for Captured {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut fields = Fields(String::new());
        event.record(&mut fields);
        self.0.lock().unwrap().push(fields.0);
    }
}

async fn boom() -> &'static str {
    panic!("boom")
}

async fn late_boom() -> Response<Body> {
    let chunks =
        futures_util::stream::iter([Ok::<_, std::io::Error>(Bytes::from_static(b"partial"))])
            .chain(futures_util::stream::once(async { panic!("late boom") }));
    Response::new(Body::from_stream(chunks))
}

// parity: middleware.TestRecovererWritesEnvelopeAndLogsStack
// parity: middleware.TestRecovererDoesNotOverwriteStartedResponse
#[tokio::test]
async fn the_recoverer_answers_the_envelope_and_never_rewrites_a_started_response() {
    let captured = Captured::default();
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(captured.clone()));
    let router = Router::new()
        .route("/boom", any(boom))
        .route("/late", any(late_boom))
        .layer(middleware::from_fn(recoverer));

    let panicked = case(&MIDDLEWARE, "recoverer-500");
    let response = router
        .clone()
        .oneshot(Request::builder().uri("/boom").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let (status, body) = text(response).await;
    assert_eq!(
        u64::from(status),
        panicked["expected"]["status"].as_u64().unwrap()
    );
    assert_eq!(body, panicked["expected"]["body_exact"].as_str().unwrap());
    let logged = captured.0.lock().unwrap().join("\n");
    for needle in panicked["expected"]["log_contains"].as_array().unwrap() {
        assert!(
            logged.contains(needle.as_str().unwrap()),
            "log has {needle}: {logged}"
        );
    }

    // A handler that started its response keeps it: the status and the bytes already
    // produced stand; the panic ends the body stream.
    let started = case(&MIDDLEWARE, "recoverer-started-response");
    let response = router
        .oneshot(Request::builder().uri("/late").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(
        u64::from(response.status().as_u16()),
        started["expected"]["status"].as_u64().unwrap()
    );
    let mut body = response.into_body();
    let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
    assert_eq!(
        first,
        started["expected"]["body_exact"]
            .as_str()
            .unwrap()
            .as_bytes()
    );
}

// ---- public auth ---------------------------------------------------------------------------

/// A backend with a fixed answer that counts its calls.
struct Counting {
    answer: Result<PublicAuthDecision, ()>,
    calls: AtomicUsize,
}

impl PublicAuthBackend for Counting {
    fn authenticate<'a>(
        &'a self,
        _token: &'a str,
    ) -> BoxFuture<'a, Result<PublicAuthDecision, AuthBackendError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let answer = self
            .answer
            .clone()
            .map_err(|()| AuthBackendError::Store(sqlx::Error::PoolTimedOut));
        Box::pin(async move { answer })
    }
}

async fn echo_identity(identity: Option<Extension<Identity>>) -> String {
    identity.map_or_else(
        || "none".to_owned(),
        |Extension(i)| format!("{}/{}", i.tenant_id, i.api_key_id),
    )
}

fn auth_router(backend: Arc<dyn PublicAuthBackend>) -> Router {
    let state = AuthState::new(
        backend,
        keys::parse_internal_tokens("ops:alpha").unwrap(),
        RateLimiter::new(100),
    );
    Router::new()
        .route("/", any(echo_identity))
        .layer(middleware::from_fn_with_state(state, public_auth))
}

async fn authorized(router: &Router, authorization: Option<&str>) -> (u16, String) {
    let mut request = Request::builder().uri("/");
    if let Some(value) = authorization {
        request = request.header("authorization", value);
    }
    text(
        router
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap(),
    )
    .await
}

fn envelope_code(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .map(|v| v["error"]["code"].as_str().unwrap_or_default().to_owned())
        .unwrap_or_default()
}

// parity: middleware.TestAuthMiddlewareEnvelopes/missing_header
// parity: middleware.TestAuthMiddlewareEnvelopes/unknown_key
// parity: middleware.TestAuthMiddlewareEnvelopes/suspended_tenant
// parity: middleware.TestAuthMiddlewareEnvelopes/store_failure
// parity: middleware.TestAuthMiddlewarePutsIdentityInContext
#[tokio::test]
async fn public_auth_answers_each_go_case() {
    let identity = Identity {
        tenant_id: "ten_1".to_owned(),
        api_key_id: "key_1".to_owned(),
    };
    for (id, answer) in [
        ("auth-missing-header", Ok(PublicAuthDecision::Missing)),
        ("auth-unknown-key", Ok(PublicAuthDecision::Missing)),
        ("auth-suspended", Ok(PublicAuthDecision::Suspended)),
        ("auth-store-failure", Err(())),
        (
            "auth-identity-in-context",
            Ok(PublicAuthDecision::Authenticated(identity.clone())),
        ),
    ] {
        let case = case(&MIDDLEWARE, id);
        let backend = Arc::new(Counting {
            answer,
            calls: AtomicUsize::new(0),
        });
        let router = auth_router(backend.clone());
        let header = case["given"]["authorization"]
            .as_str()
            .filter(|a| !a.is_empty());
        let (status, body) = authorized(&router, header).await;
        let expected = &case["expected"];
        if let Some(want) = expected["identity"].as_object() {
            assert_eq!(status, 200, "{id}");
            assert_eq!(
                body,
                format!(
                    "{}/{}",
                    want["tenant_id"].as_str().unwrap(),
                    want["api_key_id"].as_str().unwrap()
                )
            );
            continue;
        }
        assert_eq!(
            u64::from(status),
            expected["status"].as_u64().unwrap(),
            "{id}: {body}"
        );
        assert_eq!(
            envelope_code(&body),
            expected["code"].as_str().unwrap(),
            "{id}"
        );
        if let Some(message) = expected["message"].as_str() {
            assert!(body.contains(message), "{id}: {body}");
        }
        assert_ne!(body, "none", "{id}: the handler must not run");
        if header.is_none() {
            assert_eq!(
                backend.calls.load(Ordering::SeqCst),
                0,
                "{id}: no store lookup"
            );
        }
    }
}

// parity: auth.TestVerifierRejectsMalformedAuthorizationWithoutStoreLookup
#[tokio::test]
async fn malformed_authorization_is_rejected_without_a_store_lookup() {
    let backend = Arc::new(Counting {
        answer: Ok(PublicAuthDecision::Missing),
        calls: AtomicUsize::new(0),
    });
    let router = auth_router(backend.clone());
    for header in [
        "",
        "Bearer",
        "Bearer ",
        "Basic bWs6bGl2ZQ==",
        "bearer mk_live_abc",
        "Bearer sk_live_wrongprefix",
        "mk_live_abcnoscheme",
    ] {
        let (status, body) = authorized(&router, Some(header)).await;
        assert_eq!(
            (status, envelope_code(&body).as_str()),
            (401, "unauthorized"),
            "{header:?}"
        );
    }
    assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
}

// parity: auth.TestIdentityContextRoundTrip
// parity: auth.TestInternalActorContextRoundTrip
#[tokio::test]
async fn identity_and_actor_are_absent_until_set_and_round_trip() {
    // Without the middleware, no identity.
    let bare = Router::new().route("/", any(echo_identity));
    assert_eq!(authorized(&bare, None).await, (200, "none".to_owned()));
    let identity = Identity {
        tenant_id: "ten_1".to_owned(),
        api_key_id: "key_1".to_owned(),
    };
    let router = auth_router(Arc::new(StaticAuthBackend::authenticated(identity)));
    assert_eq!(
        authorized(&router, Some(WELL_FORMED)).await,
        (200, "ten_1/key_1".to_owned())
    );

    async fn echo_actor(actor: Option<Extension<InternalActor>>) -> String {
        actor.map_or_else(|| "none".to_owned(), |Extension(a)| a.0)
    }
    let bare = Router::new().route("/", any(echo_actor));
    assert_eq!(authorized(&bare, None).await, (200, "none".to_owned()));
    let state = AuthState::new(
        Arc::new(StaticAuthBackend::missing()),
        keys::parse_internal_tokens("ops:alpha,rails:beta").unwrap(),
        RateLimiter::new(10),
    );
    let router = Router::new()
        .route("/", any(echo_actor))
        .layer(middleware::from_fn_with_state(state, internal_auth));
    assert_eq!(
        authorized(&router, Some("Bearer rails:beta")).await,
        (200, "rails".to_owned())
    );
}

// parity: middleware.TestInternalAuthMiddleware
#[tokio::test]
async fn internal_auth_answers_the_go_steps() {
    async fn echo_actor(Extension(actor): Extension<InternalActor>) -> String {
        actor.0
    }
    let case = case(&MIDDLEWARE, "internal-auth");
    let tokens: Vec<String> = case["given"]["tokens"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            format!(
                "{}:{}",
                t["label"].as_str().unwrap(),
                t["secret"].as_str().unwrap()
            )
        })
        .collect();
    let state = AuthState::new(
        Arc::new(StaticAuthBackend::missing()),
        keys::parse_internal_tokens(&tokens.join(",")).unwrap(),
        RateLimiter::new(10),
    );
    let router = Router::new()
        .route("/", any(echo_actor))
        .layer(middleware::from_fn_with_state(state, internal_auth));
    for step in case["steps"].as_array().unwrap() {
        let (status, body) = authorized(&router, step["authorization"].as_str()).await;
        let expected = &step["expected"];
        assert_eq!(
            u64::from(status),
            expected["status"].as_u64().unwrap(),
            "{step}"
        );
        match expected["actor"].as_str() {
            Some(actor) => assert_eq!(body, actor),
            None => {
                assert_eq!(envelope_code(&body), expected["code"].as_str().unwrap());
                assert!(body.contains(expected["message"].as_str().unwrap()));
            }
        }
    }
}

// ---- rate limiting ---------------------------------------------------------------------------

struct At(Mutex<i64>);

impl Clock for At {
    fn unix_seconds(&self) -> i64 {
        *self.0.lock().unwrap()
    }
}

fn unix(rfc3339: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .unwrap()
        .timestamp()
}

// parity: middleware.TestRateLimiterFixedWindow
// parity: middleware.TestRateLimiterIsolatesKeys
#[test]
fn the_rate_limiter_follows_the_go_steps() {
    for id in ["rate-limiter-fixed-window", "rate-limiter-isolates-keys"] {
        let case = case(&MIDDLEWARE, id);
        let given = &case["given"];
        let start = given["clock"]
            .as_str()
            .filter(|c| c.contains('T'))
            .map_or(1_783_339_230, unix);
        let clock = Arc::new(At(Mutex::new(start)));
        let limiter = RateLimiter::with_clock(
            u32::try_from(given["limit"].as_u64().unwrap()).unwrap(),
            clock.clone(),
        );
        for step in case["steps"].as_array().unwrap() {
            if let Some(at) = step["clock"].as_str() {
                *clock.0.lock().unwrap() = unix(at);
            }
            let decision = limiter.check(step["key"].as_str().unwrap());
            let expected = &step["expected"];
            assert_eq!(
                decision.allowed,
                expected["allowed"].as_bool().unwrap(),
                "{id} {step}"
            );
            if let Some(limit) = expected["limit"].as_u64() {
                assert_eq!(decision.limit, limit, "{id} {step}");
            }
            if let Some(remaining) = expected["remaining"].as_u64() {
                assert_eq!(decision.remaining, remaining, "{id} {step}");
            }
            if let Some(reset) = expected["reset"].as_str() {
                assert_eq!(decision.reset_at, unix(reset), "{id} {step}");
            }
            if let Some(retry) = expected["retry_after_seconds"].as_u64() {
                assert_eq!(decision.retry_after, retry, "{id} {step}");
            }
        }
    }
}

// parity: middleware.TestRateLimitMiddlewareHeadersAndRetryAfter
#[tokio::test]
async fn the_rate_limit_middleware_sets_the_go_headers() {
    let case = case(&MIDDLEWARE, "rate-limit-middleware-headers");
    let given = &case["given"];
    let identity = Identity {
        tenant_id: given["identity"]["tenant_id"].as_str().unwrap().to_owned(),
        api_key_id: given["identity"]["api_key_id"].as_str().unwrap().to_owned(),
    };
    let state = AuthState::new(
        Arc::new(StaticAuthBackend::authenticated(identity)),
        keys::parse_internal_tokens("ops:alpha").unwrap(),
        RateLimiter::with_clock(
            u32::try_from(given["limit"].as_u64().unwrap()).unwrap(),
            Arc::new(At(Mutex::new(unix(given["clock"].as_str().unwrap())))),
        ),
    );
    let router = Router::new()
        .route("/", any(|| async { "ok" }))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            maverick_api::rate_limit,
        ))
        .layer(middleware::from_fn_with_state(state, public_auth));
    for step in case["steps"].as_array().unwrap() {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header("authorization", WELL_FORMED)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let headers = response.headers().clone();
        let (status, body) = text(response).await;
        let expected = &step["expected"];
        assert_eq!(
            u64::from(status),
            expected["status"].as_u64().unwrap(),
            "{step}"
        );
        for (name, value) in expected["headers"].as_object().unwrap() {
            assert_eq!(headers[name.as_str()], value.as_str().unwrap(), "{name}");
        }
        if let Some(exact) = expected["body_exact"].as_str() {
            assert_eq!(body, exact);
        }
    }
}

// ---- body limit ------------------------------------------------------------------------------

async fn read_all(request: Request) -> Response<Body> {
    match read_body(request.into_body()).await {
        Ok(bytes) => Response::new(Body::from(bytes.len().to_string())),
        Err(response) => response,
    }
}

// parity: middleware.TestBodyLimitRejectsOversizeContentLength
// parity: middleware.TestBodyLimitPassesBodiesAtTheCap
// parity: middleware.TestBodyLimitWrapsChunkedBodies
#[tokio::test]
async fn the_body_limit_follows_the_go_cases_at_the_rust_cap() {
    let router = Router::new()
        .route("/", any(read_all))
        .layer(middleware::from_fn(maverick_api::body_limit));
    let send = |body: Vec<u8>, declared: bool| {
        let router = router.clone();
        async move {
            let mut request = Request::builder().method("POST").uri("/");
            let body = if declared {
                request = request.header("content-length", body.len());
                Body::from(body)
            } else {
                Body::from_stream(futures_util::stream::iter(
                    body.chunks(64 * 1024)
                        .map(|c| Ok::<_, std::io::Error>(Bytes::copy_from_slice(c)))
                        .collect::<Vec<_>>(),
                ))
            };
            text(router.oneshot(request.body(body).unwrap()).await.unwrap()).await
        }
    };
    let over = format!(
        "{{\"error\":{{\"code\":\"payload_too_large\",\"message\":\"request body exceeds the limit\",\"details\":{{\"limit_bytes\":{CAP}}}}}}}\n"
    );
    // The Go case with its 16-byte cap, as a shape check of the same envelope.
    let go = case(&MIDDLEWARE, "body-limit-oversize-content-length")["expected"]["body_exact"]
        .as_str()
        .unwrap()
        .replace("\"limit_bytes\":16", &format!("\"limit_bytes\":{CAP}"));
    assert_eq!(go, over);
    assert_eq!(send(vec![b'a'; CAP + 1], true).await, (413, over.clone()));
    assert_eq!(send(vec![b'a'; CAP], true).await, (200, CAP.to_string()));
    assert_eq!(send(vec![b'a'; CAP * 2], false).await, (413, over));
}

// ---- the verifier on Postgres ----------------------------------------------------------------

// parity: auth.TestVerifierUnknownKey
// parity: auth.TestVerifierRevokedKey
// parity: auth.TestVerifierSuspendedTenant
// parity: auth.TestVerifierActiveKeyReturnsIdentity
// parity: auth.TestVerifierStoreFailureIsNotAnAuthError
#[tokio::test]
async fn the_verifier_resolves_keys_against_the_store() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    tenancy::create_tenant(&pool, "ten_1", "Verifier", &[0; 32])
        .await
        .unwrap();
    tenancy::create_tenant(&pool, "ten_2", "Suspended", &[0; 32])
        .await
        .unwrap();
    tenancy::update_tenant(&pool, "ten_2", "Suspended", "suspended")
        .await
        .unwrap();
    let mut raw = Vec::new();
    for (id, tenant) in [
        ("key_1", "ten_1"),
        ("key_revoked", "ten_1"),
        ("key_2", "ten_2"),
    ] {
        let minted = keys::mint_key();
        tenancy::create_api_key(&pool, id, tenant, &minted.key_hash, &minted.last4, "k")
            .await
            .unwrap();
        raw.push(minted.raw_key);
    }
    tenancy::revoke_api_key(&pool, "key_revoked").await.unwrap();
    let backend = PgAuthBackend::new(pool.clone());
    let unknown = keys::mint_key().raw_key;
    assert!(matches!(
        backend.authenticate(&unknown).await,
        Ok(PublicAuthDecision::Missing)
    ));
    assert!(matches!(
        backend.authenticate(&raw[1]).await,
        Ok(PublicAuthDecision::Missing)
    ));
    assert!(matches!(
        backend.authenticate(&raw[2]).await,
        Ok(PublicAuthDecision::Suspended)
    ));
    match backend.authenticate(&raw[0]).await {
        Ok(PublicAuthDecision::Authenticated(identity)) => {
            assert_eq!(
                (identity.tenant_id.as_str(), identity.api_key_id.as_str()),
                ("ten_1", "key_1")
            );
        }
        other => panic!("active key: {other:?}"),
    }
    // A store failure is neither 401 nor 403: the middleware answers 500.
    let down = PgPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_millis(300))
        .connect_lazy("postgres://verifier@127.0.0.1:1/down")
        .unwrap();
    assert!(
        PgAuthBackend::new(down.clone())
            .authenticate(&raw[0])
            .await
            .is_err()
    );
    let router = auth_router(Arc::new(PgAuthBackend::new(down)));
    let (status, body) = authorized(&router, Some(&format!("Bearer {}", raw[0]))).await;
    assert_eq!(
        (status, envelope_code(&body).as_str()),
        (500, "internal_error")
    );
}
