//! The `request` log line and the request span (reference Part 5 §10.1, spec §14.3,
//! D19), driven through the real middleware chain. The fields are captured with a
//! plain `tracing` layer; the Go JSON rendering is tested with the slog layer in the
//! `maverick` crate.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::middleware;
use axum::routing::{get, post};
use maverick_api::{
    AuthState, Identity, RateLimiter, StaticAuthBackend, internal_router_with_state, public_auth,
    public_router_with_state, recoverer, request_id, request_logger,
};
use maverick_domain::keys;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
use tower::ServiceExt;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

const SECRET_KEY: &str = "mk_live_supersecretvalue";

#[derive(Debug, Clone, PartialEq)]
enum Val {
    Str(String),
    U64(u64),
    F64(f64),
    Bool(bool),
    Other(String),
}

#[derive(Debug, Clone, Default)]
struct Line {
    message: String,
    fields: Vec<(String, Val)>,
}

impl Line {
    fn names(&self) -> Vec<&str> {
        self.fields.iter().map(|(name, _)| name.as_str()).collect()
    }

    fn get(&self, name: &str) -> Option<&Val> {
        self.fields.iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }
}

#[derive(Default)]
struct LineVisitor(Line);

impl Visit for LineVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0
            .fields
            .push((field.name().to_owned(), Val::Str(value.to_owned())));
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0
            .fields
            .push((field.name().to_owned(), Val::U64(value)));
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.0
            .fields
            .push((field.name().to_owned(), Val::F64(value)));
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0
            .fields
            .push((field.name().to_owned(), Val::Bool(value)));
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0.message = format!("{value:?}");
        } else {
            self.0
                .fields
                .push((field.name().to_owned(), Val::Other(format!("{value:?}"))));
        }
    }
}

/// Records every event.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<Line>>>);

impl<S: Subscriber> Layer<S> for Captured {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = LineVisitor::default();
        event.record(&mut visitor);
        self.0.lock().expect("lock").push(visitor.0);
    }
}

impl Captured {
    fn request_lines(&self) -> Vec<Line> {
        self.0
            .lock()
            .expect("lock")
            .iter()
            .filter(|line| line.message == "request")
            .cloned()
            .collect()
    }

    fn everything(&self) -> String {
        format!("{:?}", self.0.lock().expect("lock"))
    }
}

fn auth_state(tenant_id: &str, key_id: &str) -> AuthState {
    AuthState::new(
        Arc::new(StaticAuthBackend::authenticated(Identity {
            tenant_id: tenant_id.to_owned(),
            api_key_id: key_id.to_owned(),
        })),
        keys::parse_internal_tokens("ops:alpha-token").expect("tokens"),
        RateLimiter::new(300),
    )
}

/// The outer chain as the listeners build it: RequestID -> Logger -> Recoverer.
fn with_outer(router: Router) -> Router {
    router
        .layer(middleware::from_fn(recoverer))
        .layer(middleware::from_fn(request_logger))
        .layer(middleware::from_fn(request_id))
}

/// parity: middleware.TestLoggerEmitsRequestFieldsAndNeverAuthorization
#[tokio::test]
async fn logger_emits_request_fields_and_never_authorization() {
    let captured = Captured::default();
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(captured.clone()));
    let app = with_outer(Router::new().route("/v1/things", post(|| async { StatusCode::CREATED })));
    let response = app
        .oneshot(
            Request::post("/v1/things?token=querysecret")
                .header("authorization", format!("Bearer {SECRET_KEY}"))
                .body(Body::from("{\"prompt\":\"bodysecret\"}"))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::CREATED);

    let lines = captured.request_lines();
    assert_eq!(lines.len(), 1);
    let line = &lines[0];
    assert_eq!(
        line.names(),
        ["method", "path", "status", "ms", "request_id"]
    );
    assert_eq!(line.get("method"), Some(&Val::Str("POST".to_owned())));
    assert_eq!(line.get("path"), Some(&Val::Str("/v1/things".to_owned())));
    assert_eq!(line.get("status"), Some(&Val::U64(201)));
    assert!(matches!(line.get("ms"), Some(Val::F64(ms)) if *ms >= 0.0));
    assert!(matches!(line.get("request_id"), Some(Val::Str(id)) if !id.is_empty()));
    let everything = captured.everything();
    for secret in [
        "supersecretvalue",
        "Authorization",
        "authorization",
        "querysecret",
        "bodysecret",
    ] {
        assert!(!everything.contains(secret), "{secret} in {everything}");
    }
}

/// parity: middleware.TestLoggerIncludesTenantWhenAuthed
#[tokio::test]
async fn logger_includes_tenant_when_authed() {
    let captured = Captured::default();
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(captured.clone()));
    let app = public_router_with_state(auth_state("ten_42", "key_42"));
    let response = app
        .oneshot(
            Request::get("/v1/config/pools")
                .header(
                    "authorization",
                    "Bearer mk_live_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                )
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let lines = captured.request_lines();
    assert_eq!(lines.len(), 1);
    let line = &lines[0];
    assert_eq!(
        line.names(),
        [
            "method",
            "path",
            "status",
            "ms",
            "request_id",
            "tenant_id",
            "key_id"
        ]
    );
    assert_eq!(line.get("tenant_id"), Some(&Val::Str("ten_42".to_owned())));
    assert_eq!(line.get("key_id"), Some(&Val::Str("key_42".to_owned())));
    assert_eq!(
        line.get("status"),
        Some(&Val::U64(u64::from(response.status().as_u16())))
    );
}

#[tokio::test]
async fn unauthenticated_lines_have_no_tenant() {
    let captured = Captured::default();
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(captured.clone()));
    let app = public_router_with_state(auth_state("ten_42", "key_42"));
    let response = app
        .oneshot(
            Request::get("/v1/config/pools")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let lines = captured.request_lines();
    assert_eq!(
        lines[0].names(),
        ["method", "path", "status", "ms", "request_id"]
    );
    assert_eq!(lines[0].get("status"), Some(&Val::U64(401)));
}

#[tokio::test]
async fn internal_lines_carry_the_actor() {
    let captured = Captured::default();
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(captured.clone()));
    let app = internal_router_with_state(auth_state("ten_42", "key_42"));
    app.oneshot(
        Request::get("/internal/v1/alerts")
            .header("authorization", "Bearer ops:alpha-token")
            .body(Body::empty())
            .expect("request"),
    )
    .await
    .expect("response");
    let lines = captured.request_lines();
    assert_eq!(lines.len(), 1);
    assert_eq!(
        lines[0].names(),
        ["method", "path", "status", "ms", "request_id", "actor"]
    );
    assert_eq!(lines[0].get("actor"), Some(&Val::Str("ops".to_owned())));
    assert!(!captured.everything().contains("alpha-token"));
}

/// Auth records the identity before the handler runs, so a panic keeps it on the line.
#[tokio::test]
async fn a_panicking_handler_still_logs_the_tenant() {
    let captured = Captured::default();
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(captured.clone()));
    let state = auth_state("ten_9", "key_9");
    let app = with_outer(
        Router::new()
            .route(
                "/v1/boom",
                get(|| async {
                    panic!("boom");
                    #[allow(unreachable_code)]
                    StatusCode::OK
                }),
            )
            .layer(middleware::from_fn_with_state(state, public_auth)),
    );
    let response = app
        .oneshot(
            Request::get("/v1/boom")
                .header("authorization", format!("Bearer {SECRET_KEY}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let lines = captured.request_lines();
    assert_eq!(lines[0].get("status"), Some(&Val::U64(500)));
    assert_eq!(
        lines[0].get("tenant_id"),
        Some(&Val::Str("ten_9".to_owned()))
    );
}

/// W3C trace context: the request span continues the caller's `traceparent`, records
/// only the method and the path, and exports no credential, query, or body.
#[tokio::test]
async fn request_span_continues_the_callers_trace() {
    opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let subscriber = tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));
    let _guard = tracing::subscriber::set_default(subscriber);

    let app = public_router_with_state(auth_state("ten_otel", "key_otel"));
    for traceparent in [
        Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
        None,
    ] {
        let mut request = Request::get("/v1/config/pools?token=querysecret")
            .header("authorization", format!("Bearer {SECRET_KEY}"));
        if let Some(traceparent) = traceparent {
            request = request.header("traceparent", traceparent);
        }
        app.clone()
            .oneshot(request.body(Body::empty()).expect("request"))
            .await
            .expect("response");
    }
    provider.force_flush().expect("flush");

    let spans = exporter.get_finished_spans().expect("spans");
    let requests: Vec<_> = spans.iter().filter(|s| s.name == "request").collect();
    assert_eq!(requests.len(), 2);
    let continued = requests[0];
    assert_eq!(
        continued.span_context.trace_id().to_string(),
        "4bf92f3577b34da6a3ce929d0e0e4736"
    );
    assert_eq!(continued.parent_span_id.to_string(), "00f067aa0ba902b7");
    let fresh = requests[1];
    assert_ne!(
        fresh.span_context.trace_id(),
        continued.span_context.trace_id()
    );
    assert_eq!(fresh.parent_span_id, opentelemetry::trace::SpanId::INVALID);
    for span in &requests {
        let attributes: Vec<(String, String)> = span
            .attributes
            .iter()
            .map(|kv| (kv.key.to_string(), kv.value.to_string()))
            .collect();
        assert!(attributes.contains(&("method".to_owned(), "GET".to_owned())));
        assert!(attributes.contains(&("path".to_owned(), "/v1/config/pools".to_owned())));
        // The request line is a span event with the tenant fields.
        assert!(span.events.iter().any(|e| e.name == "request"));
    }
    let exported = format!("{spans:?}");
    for secret in [
        "supersecretvalue",
        "querysecret",
        "authorization",
        "Authorization",
    ] {
        assert!(!exported.contains(secret), "{secret} exported");
    }
}
