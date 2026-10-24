//! The proxy fail-open ladder (D46; spec §8.1 "Fail-open"): the last good list for the
//! tenant key and route, then the surface's default model called directly, then
//! `503 engine_unavailable`, with the zero-retention rule and the `401`/`403` mapping.
//!
//! The engine is a scripted mock; the chain runner records what it was asked to run, so
//! no provider is called.

mod otlp;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::http::{Request, StatusCode};
use axum::response::Response;
use maverick_core::recommend::{RecommendClient, RecommendClientConfig};
use maverick_core::telemetry::Telemetry;
use maverick_proxy::chain::{BoxFuture, ChainRequest, ChainRunner, FallbackStep};
use maverick_proxy::config::{Config, load_config};
use maverick_proxy::direct::DirectRequest;
use maverick_proxy::envelope::envelope_body;
use maverick_proxy::otel::{FALLBACK_COUNTER, ProxyOtel};
use maverick_proxy::reports::ProxyReports;
use maverick_proxy::server::Proxy;
use maverick_proxy::surface::ReportSink;
use opentelemetry_proto::tonic::metrics::v1::metric::Data;
use opentelemetry_proto::tonic::metrics::v1::number_data_point::Value as Point;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tower::ServiceExt;

/// A mock engine: scripted replies in order, then `500` for anything unscripted.
async fn engine(replies: Vec<(u16, &str)>) -> (String, Arc<Mutex<usize>>) {
    let replies: VecDeque<(u16, String)> = replies
        .into_iter()
        .map(|(status, body)| (status, body.to_owned()))
        .collect();
    let replies = Arc::new(Mutex::new(replies));
    let calls = Arc::new(Mutex::new(0));
    let counted = Arc::clone(&calls);
    let app = Router::new().route(
        "/v1/recommend",
        axum::routing::post(move || {
            let replies = Arc::clone(&replies);
            let counted = Arc::clone(&counted);
            async move {
                *counted.lock().unwrap() += 1;
                let (status, body) = replies
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or((500, "{\"error\":\"down\"}".to_owned()));
                Response::builder()
                    .status(status)
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap()
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, calls)
}

/// An engine URL nothing listens on: every call is a connect error.
async fn closed_engine() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    url
}

/// What the proxy asked the runner to do.
#[derive(Debug, Clone, PartialEq)]
enum Ran {
    /// A chain: the recommendation id and the chain's models.
    Chain(String, Vec<String>),
    /// A direct call: the model and the body's `model` field.
    Direct(String, String),
}

#[derive(Default)]
struct Recorder {
    ran: Mutex<Vec<Ran>>,
    fallbacks: Mutex<Vec<FallbackStep>>,
}

impl Recorder {
    fn ran(&self) -> Vec<Ran> {
        self.ran.lock().unwrap().clone()
    }

    fn fallbacks(&self) -> Vec<FallbackStep> {
        self.fallbacks.lock().unwrap().clone()
    }
}

impl ChainRunner for Recorder {
    fn run(&self, request: ChainRequest) -> BoxFuture<'_, Response> {
        let recommendation = request.recommendation;
        self.ran.lock().unwrap().push(Ran::Chain(
            recommendation.recommendation_id,
            recommendation
                .chain
                .iter()
                .map(|entry| entry.model.clone())
                .collect(),
        ));
        Box::pin(async { Response::new(Body::from("chain")) })
    }

    fn run_direct(&self, request: DirectRequest) -> BoxFuture<'_, Response> {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        self.ran.lock().unwrap().push(Ran::Direct(
            request.model,
            body["model"].as_str().unwrap_or_default().to_owned(),
        ));
        Box::pin(async { Response::new(Body::from("direct")) })
    }

    fn record_fallback(&self, step: FallbackStep) {
        self.fallbacks.lock().unwrap().push(step);
    }
}

fn config(engine_url: &str, vars: &[(&str, &str)]) -> Config {
    let mut vars: Vec<(String, String)> = vars
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
    vars.push(("MAVERICK_ENGINE_URL".to_owned(), engine_url.to_owned()));
    if !vars.iter().any(|(name, _)| name == "MAVERICK_PROXY_MODE") {
        vars.push(("MAVERICK_API_KEY".to_owned(), "mk_live_self".to_owned()));
    }
    load_config(|name| {
        vars.iter()
            .find(|(var, _)| var == name)
            .map(|(_, value)| value.clone())
    })
    .unwrap()
}

fn proxy(config: Config, runner: Arc<Recorder>) -> Router {
    let engine = RecommendClient::with_http(
        reqwest::Client::builder().no_proxy().build().unwrap(),
        RecommendClientConfig {
            base_url: config.engine_url.clone(),
            timeout: Duration::from_secs(5),
            max_retries: 0,
        },
    );
    Proxy::with_engine(config, engine, runner).router()
}

struct Answer {
    status: StatusCode,
    fallback: Option<String>,
    retry_after: Option<String>,
    body: Bytes,
}

async fn send(app: &Router, path: &str, model: &str, headers: &[(&str, &str)]) -> Answer {
    let mut request = Request::post(path).header("content-type", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let body = format!(
        r#"{{"model":"{model}","max_tokens":8,"messages":[{{"role":"user","content":"hi"}}]}}"#
    );
    let response = app
        .clone()
        .oneshot(request.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .map(|value| value.to_str().unwrap().to_owned())
    };
    Answer {
        status: response.status(),
        fallback: header("x-maverick-fallback"),
        retry_after: header("retry-after"),
        body: to_bytes(response.into_body(), 1 << 20).await.unwrap(),
    }
}

const MESSAGES: &str = "/v1/messages";
const CHAT: &str = "/v1/chat/completions";

fn chain(id: &str, models: &[&str]) -> Ran {
    Ran::Chain(
        id.to_owned(),
        models.iter().map(|m| (*m).to_owned()).collect(),
    )
}

fn unavailable() -> Bytes {
    Bytes::from(envelope_body(
        "engine_unavailable",
        "the engine is unavailable and no fallback model is configured",
    ))
}

/// DIVERGENCE D46: with the engine down, the route's last good list is served and marked;
/// another route has none, so with no default model the answer is `503`.
#[tokio::test]
async fn an_engine_outage_serves_the_last_good_list_then_503() {
    let (url, calls) = engine(vec![(
        200,
        r#"{"recommendation_id":"rec_1","recommendations":[{"provider":"anthropic","model":"claude-a"},{"provider":"anthropic","model":"claude-b"}]}"#,
    )])
    .await;
    let runner = Arc::new(Recorder::default());
    let app = proxy(config(&url, &[]), Arc::clone(&runner));

    let fresh = send(&app, MESSAGES, "maverick/auto", &[]).await;
    assert_eq!((fresh.status, fresh.fallback), (StatusCode::OK, None));
    let warm = send(&app, MESSAGES, "maverick/auto", &[]).await;
    assert_eq!(warm.status, StatusCode::OK);
    assert_eq!(warm.fallback.as_deref(), Some("last_known_good"));
    // `maverick/auto:quality` is the same route.
    let same = send(&app, CHAT, "maverick/auto:quality", &[]).await;
    assert_eq!(same.fallback.as_deref(), Some("last_known_good"));
    let other = send(&app, MESSAGES, "maverick/auto:cost", &[]).await;
    assert_eq!(other.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(other.fallback, None);
    assert_eq!(other.retry_after.as_deref(), Some("5"));
    assert_eq!(other.body, unavailable());

    let both = chain("rec_1", &["claude-a", "claude-b"]);
    assert_eq!(runner.ran(), [both.clone(), both.clone(), both]);
    assert_eq!(
        runner.fallbacks(),
        [
            FallbackStep::LastKnownGood,
            FallbackStep::LastKnownGood,
            FallbackStep::Unavailable
        ]
    );
    assert_eq!(*calls.lock().unwrap(), 4);
}

/// D46: a cold store uses the surface's default model as a direct call, with the body's
/// model replaced; each surface has its own default.
#[tokio::test]
async fn a_cold_store_calls_the_surface_default_model() {
    let url = closed_engine().await;
    let runner = Arc::new(Recorder::default());
    let app = proxy(
        config(
            &url,
            &[
                ("MAVERICK_PROXY_DEFAULT_MODEL_MESSAGES", "claude-haiku-4-5"),
                ("MAVERICK_PROXY_DEFAULT_MODEL_CHAT", "openai/gpt-5.5"),
            ],
        ),
        Arc::clone(&runner),
    );
    let messages = send(&app, MESSAGES, "maverick/team:cost", &[]).await;
    assert_eq!(messages.status, StatusCode::OK);
    assert_eq!(messages.fallback.as_deref(), Some("default_model"));
    let chat = send(&app, CHAT, "maverick/auto", &[]).await;
    assert_eq!(chat.fallback.as_deref(), Some("default_model"));
    assert_eq!(
        runner.ran(),
        [
            Ran::Direct("claude-haiku-4-5".to_owned(), "claude-haiku-4-5".to_owned()),
            Ran::Direct("openai/gpt-5.5".to_owned(), "openai/gpt-5.5".to_owned()),
        ]
    );
    assert_eq!(
        runner.fallbacks(),
        [FallbackStep::DefaultModel, FallbackStep::DefaultModel]
    );
}

/// D46: a zero-retention tenant falls open only to `zero` entries, or to a default the
/// operator marked verified zero retention; otherwise it gets the `503`.
#[tokio::test]
async fn a_zero_retention_tenant_falls_open_only_to_compliant_models() {
    let zr = r#"{"recommendation_id":"rec_z","zero_retention":true,"recommendations":[{"provider":"anthropic","model":"claude-std","retention_mode":"standard"},{"provider":"anthropic","model":"claude-zero","retention_mode":"zero"}]}"#;
    let standard_only = r#"{"recommendation_id":"rec_s","zero_retention":true,"recommendations":[{"provider":"anthropic","model":"claude-std","retention_mode":"standard"}]}"#;
    for (verified, cold_status, cold_fallback) in [
        ("false", StatusCode::SERVICE_UNAVAILABLE, None),
        ("true", StatusCode::OK, Some("default_model")),
    ] {
        let (url, _) = engine(vec![(200, zr), (200, standard_only)]).await;
        let runner = Arc::new(Recorder::default());
        let app = proxy(
            config(
                &url,
                &[
                    ("MAVERICK_PROXY_DEFAULT_MODEL_MESSAGES", "claude-haiku-4-5"),
                    ("MAVERICK_PROXY_DEFAULT_MODEL_ZERO_RETENTION", verified),
                ],
            ),
            Arc::clone(&runner),
        );
        send(&app, MESSAGES, "maverick/auto", &[]).await;
        send(&app, MESSAGES, "maverick/auto:cost", &[]).await;
        let warm = send(&app, MESSAGES, "maverick/auto", &[]).await;
        assert_eq!(
            warm.fallback.as_deref(),
            Some("last_known_good"),
            "{verified}"
        );
        let cold = send(&app, MESSAGES, "maverick/auto:cost", &[]).await;
        assert_eq!(
            (cold.status, cold.fallback.as_deref()),
            (cold_status, cold_fallback),
            "{verified}"
        );
        assert_eq!(
            runner.ran()[2],
            chain("rec_z", &["claude-zero"]),
            "{verified}"
        );
    }
}

/// D46: the terminal privacy errors are relayed, purge the tenant key's lists, and mark
/// the tenant zero retention, so an unverified default is not used afterwards.
#[tokio::test]
async fn terminal_privacy_errors_purge_the_last_good_lists() {
    let (url, _) = engine(vec![
        (
            200,
            r#"{"recommendation_id":"rec_1","recommendations":[{"provider":"anthropic","model":"claude-a","retention_mode":"zero"}]}"#,
        ),
        (
            409,
            r#"{"error":{"code":"no_compliant_columns","message":"no column holds valid zero-retention evidence"}}"#,
        ),
    ])
    .await;
    let runner = Arc::new(Recorder::default());
    let app = proxy(
        config(
            &url,
            &[("MAVERICK_PROXY_DEFAULT_MODEL_MESSAGES", "claude-haiku-4-5")],
        ),
        Arc::clone(&runner),
    );
    send(&app, MESSAGES, "maverick/auto", &[]).await;
    let refused = send(&app, MESSAGES, "maverick/auto", &[]).await;
    assert_eq!(refused.status, StatusCode::CONFLICT);
    assert_eq!(refused.fallback, None);
    let down = send(&app, MESSAGES, "maverick/auto", &[]).await;
    assert_eq!(down.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(down.body, unavailable());
    assert_eq!(runner.fallbacks(), [FallbackStep::Unavailable]);
}

/// D46: an engine 4xx is the engine answering, so it is not a fail-open trigger: a 429
/// stays `502 engine_error`, and hosted mode keeps the D13 mapping for 401 and 403. The
/// last good list is not used for any of them.
#[tokio::test]
async fn engine_4xx_answers_never_fall_open() {
    let ok = r#"{"recommendation_id":"rec_1","recommendations":[{"provider":"anthropic","model":"claude-a"}]}"#;
    let (url, _) = engine(vec![
        (200, ok),
        (
            429,
            r#"{"error":{"code":"rate_limited","message":"rate limit exceeded"}}"#,
        ),
        (
            401,
            r#"{"error":{"code":"unauthorized","message":"missing or invalid API key"}}"#,
        ),
        (
            403,
            r#"{"error":{"code":"tenant_suspended","message":"tenant is suspended"}}"#,
        ),
    ])
    .await;
    let runner = Arc::new(Recorder::default());
    let app = proxy(
        config(&url, &[("MAVERICK_PROXY_MODE", "hosted")]),
        Arc::clone(&runner),
    );
    let tenant = [("x-maverick-api-key", "mk_live_tenant")];
    send(&app, MESSAGES, "maverick/auto", &tenant).await;
    let limited = send(&app, MESSAGES, "maverick/auto", &tenant).await;
    assert_eq!(limited.status, StatusCode::BAD_GATEWAY);
    assert_eq!(
        limited.body,
        Bytes::from(envelope_body(
            "engine_error",
            "the engine's POST /v1/recommend call failed"
        ))
    );
    let revoked = send(&app, MESSAGES, "maverick/auto", &tenant).await;
    assert_eq!(revoked.status, StatusCode::UNAUTHORIZED);
    let suspended = send(&app, MESSAGES, "maverick/auto", &tenant).await;
    assert_eq!(suspended.status, StatusCode::FORBIDDEN);
    for answer in [&limited, &revoked, &suspended] {
        assert_eq!(answer.fallback, None);
    }
    assert_eq!(runner.ran().len(), 1);
    assert_eq!(runner.fallbacks(), []);
}

/// D46: each tenant key has its own lists, and a list expires after
/// `MAVERICK_PROXY_LKG_MAX_AGE`.
#[tokio::test]
async fn last_good_lists_are_per_tenant_key_and_expire() {
    let ok = r#"{"recommendation_id":"rec_1","recommendations":[{"provider":"anthropic","model":"claude-a"}]}"#;
    let (url, _) = engine(vec![(200, ok)]).await;
    let runner = Arc::new(Recorder::default());
    let app = proxy(
        config(
            &url,
            &[
                ("MAVERICK_PROXY_MODE", "hosted"),
                ("MAVERICK_PROXY_LKG_MAX_AGE", "300ms"),
            ],
        ),
        Arc::clone(&runner),
    );
    let first = [("x-maverick-api-key", "mk_live_first")];
    let second = [("x-maverick-api-key", "mk_live_second")];
    send(&app, MESSAGES, "maverick/auto", &first).await;
    let other_tenant = send(&app, MESSAGES, "maverick/auto", &second).await;
    assert_eq!(other_tenant.status, StatusCode::SERVICE_UNAVAILABLE);
    let warm = send(&app, MESSAGES, "maverick/auto", &first).await;
    assert_eq!(warm.fallback.as_deref(), Some("last_known_good"));
    tokio::time::sleep(Duration::from_millis(400)).await;
    let expired = send(&app, MESSAGES, "maverick/auto", &first).await;
    assert_eq!(expired.status, StatusCode::SERVICE_UNAVAILABLE);
}

/// D46, D43: a last good list whose recommendation opted out of telemetry is still served
/// and marked, but the fail-open counter does not count it.
#[tokio::test]
async fn an_opted_out_last_good_list_is_not_counted() {
    let opted_out = r#"{"recommendation_id":"rec_o","telemetry":false,"recommendations":[{"provider":"anthropic","model":"claude-a"}]}"#;
    let (url, _) = engine(vec![(200, opted_out)]).await;
    let runner = Arc::new(Recorder::default());
    let app = proxy(config(&url, &[]), Arc::clone(&runner));
    send(&app, MESSAGES, "maverick/auto", &[]).await;
    let warm = send(&app, MESSAGES, "maverick/auto", &[]).await;
    assert_eq!(warm.fallback.as_deref(), Some("last_known_good"));
    assert_eq!(runner.ran()[1], chain("rec_o", &["claude-a"]));
    assert_eq!(runner.fallbacks(), []);
}

/// D46: `maverick_proxy_fallbacks_total` counts each step, and `step` is its only label:
/// no tenant, key, or model.
#[tokio::test(flavor = "multi_thread")]
async fn the_fallback_counter_is_labeled_by_step_only() {
    let (url, received) = otlp::collector().await;
    let otel = Arc::new(ProxyOtel::init_with(&url, "maverick-proxy").expect("enabled"));
    let reports = ProxyReports::new(Telemetry::off()).with_otel(Some(Arc::clone(&otel)));
    reports.fallback(FallbackStep::LastKnownGood);
    reports.fallback(FallbackStep::LastKnownGood);
    reports.fallback(FallbackStep::DefaultModel);
    reports.fallback(FallbackStep::Unavailable);
    reports.fallback(FallbackStep::Unavailable);
    reports.fallback(FallbackStep::Unavailable);
    let flushing = Arc::clone(&otel);
    tokio::task::spawn_blocking(move || flushing.force_flush())
        .await
        .unwrap();
    let exports = otlp::received_metrics(&received);
    let scope = &exports.last().expect("a metrics export").resource_metrics[0].scope_metrics[0];
    let metric = scope
        .metrics
        .iter()
        .find(|metric| metric.name == FALLBACK_COUNTER)
        .expect("the fallback counter");
    let Some(Data::Sum(sum)) = &metric.data else {
        panic!("{FALLBACK_COUNTER} is not a sum: {metric:?}");
    };
    assert!(sum.is_monotonic);
    let mut points: Vec<(Value, i64)> = sum
        .data_points
        .iter()
        .map(|point| {
            let Some(Point::AsInt(count)) = point.value else {
                panic!("not an int point: {point:?}");
            };
            (
                otlp::sorted(otlp::json_attributes(&point.attributes)),
                count,
            )
        })
        .collect();
    points.sort_by_key(|(_, count)| *count);
    let step = |value: &str| json!([{"key": "step", "value": {"stringValue": value}}]);
    assert_eq!(
        points,
        [
            (step("default_model"), 1),
            (step("last_known_good"), 2),
            (step("unavailable"), 3),
        ]
    );
    otel.shutdown();
}
