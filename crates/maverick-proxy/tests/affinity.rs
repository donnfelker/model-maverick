//! Conversation affinity in the proxy (D44; spec §8.1 "Conversation header"): the
//! `X-Maverick-Conversation` check, the `affinity` member sent only to an engine that
//! advertised it (D50), `X-Maverick-Affinity` only when the engine answered, and the
//! header never reaching a provider.
//!
//! The engine is a scripted mock that records each request body; the chain runs on the
//! real executor against a recording OpenAI mock. Every other upstream is a closed port.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::extract::Request;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use maverick_core::affinity::context_tokens;
use maverick_core::recommend::{RecommendClient, RecommendClientConfig};
use maverick_proxy::config::load_config;
use maverick_proxy::envelope::envelope_body;
use maverick_proxy::server::Proxy;
use maverick_proxy::surface::{ExecutorRunner, NoReports};
use tokio::net::TcpListener;
use tower::ServiceExt;

/// One scripted engine reply: status, `X-Maverick-API-Features` (when set), and body.
type Reply = (u16, Option<&'static str>, &'static str);

/// A scripted engine: replies in order, then `500`. Returns its URL and the request bodies.
async fn engine(script: Vec<Reply>) -> (String, Arc<Mutex<Vec<String>>>) {
    let script = Arc::new(Mutex::new(script.into_iter().collect::<VecDeque<_>>()));
    let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = Arc::clone(&bodies);
    let app = Router::new().route(
        "/v1/recommend",
        axum::routing::post(move |body: Bytes| {
            let script = Arc::clone(&script);
            let seen = Arc::clone(&seen);
            async move {
                seen.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&body).into_owned());
                let (status, features, reply) = script.lock().unwrap().pop_front().unwrap_or((
                    500,
                    None,
                    r#"{"error":{"code":"internal_error"}}"#,
                ));
                let mut response = Response::builder()
                    .status(status)
                    .header("content-type", "application/json");
                if let Some(features) = features {
                    response = response.header("x-maverick-api-features", features);
                }
                response.body(Body::from(reply)).unwrap()
            }
        }),
    );
    (serve(app).await, bodies)
}

/// A recording OpenAI upstream that answers every chat completion.
async fn upstream() -> (String, Arc<Mutex<Vec<HeaderMap>>>) {
    let headers: Arc<Mutex<Vec<HeaderMap>>> = Arc::default();
    let seen = Arc::clone(&headers);
    let app = Router::new().fallback(move |request: Request| {
        let seen = Arc::clone(&seen);
        async move {
            seen.lock().unwrap().push(request.headers().clone());
            Response::builder()
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"id":"chatcmpl-1","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"OK"},"finish_reason":"stop"}]}"#,
                ))
                .unwrap()
        }
    });
    (serve(app).await, headers)
}

async fn serve(app: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

async fn closed_port() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    url
}

/// A self-hosted proxy on the real executor. No real provider: OpenAI is the mock, and
/// every other upstream is a closed port.
async fn proxy(engine_url: &str, openai_url: &str) -> Router {
    let closed = closed_port().await;
    let vars = [
        ("MAVERICK_API_KEY", "mk_live_affinity"),
        ("MAVERICK_ENGINE_URL", engine_url),
        ("MAVERICK_PROXY_TELEMETRY", "off"),
        ("MAVERICK_UPSTREAM_OPENAI", openai_url),
        ("MAVERICK_UPSTREAM_ANTHROPIC", closed.as_str()),
        ("MAVERICK_UPSTREAM_GOOGLE", closed.as_str()),
        ("MAVERICK_UPSTREAM_XAI", closed.as_str()),
        ("MAVERICK_UPSTREAM_OPENROUTER", closed.as_str()),
    ];
    let config = load_config(|name| {
        vars.iter()
            .find(|(var, _)| *var == name)
            .map(|(_, value)| (*value).to_owned())
    })
    .unwrap();
    for url in [
        &config.upstream_openai,
        &config.upstream_anthropic,
        &config.upstream_google,
        &config.upstream_xai,
        &config.upstream_openrouter,
    ] {
        assert!(url.starts_with("http://127.0.0.1:"), "{url}");
    }
    let runner = ExecutorRunner::new(&config, Arc::new(NoReports)).unwrap();
    let engine = RecommendClient::with_http(
        reqwest::Client::builder().no_proxy().build().unwrap(),
        RecommendClientConfig {
            base_url: config.engine_url.clone(),
            timeout: Duration::from_secs(5),
            max_retries: 0,
        },
    );
    Proxy::with_engine(config, engine, Arc::new(runner)).router()
}

const BODY: &str =
    r#"{"model":"maverick/auto","messages":[{"role":"user","content":"Extract the total."}]}"#;

const ROUTED: &str =
    r#"{"recommendation_id":"rec_1","recommendations":[{"provider":"openai","model":"gpt-a"}]}"#;

const KEPT: &str = r#"{"recommendation_id":"rec_2","recommendations":[{"provider":"openai","model":"gpt-a"}],"affinity":{"status":"kept","previous_column_id":"col_a","reason":null,"cache_warm":true,"cost_aware":false}}"#;

/// (status, `X-Maverick-Affinity`, `X-Maverick-Fallback`, body).
async fn send(
    app: &Router,
    conversation: Option<&[u8]>,
) -> (StatusCode, Option<String>, Option<String>, Bytes) {
    let mut request = Request::post("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", "Bearer sk-caller-openai")
        .header("x-maverick-workload", "structured_extraction:3");
    if let Some(conversation) = conversation {
        request = request.header("x-maverick-conversation", conversation);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(BODY)).unwrap())
        .await
        .unwrap();
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .map(|value| value.to_str().unwrap().to_owned())
    };
    (
        response.status(),
        header("x-maverick-affinity"),
        header("x-maverick-fallback"),
        to_bytes(response.into_body(), 1 << 20).await.unwrap(),
    )
}

/// DIVERGENCE D44: the first request cannot know the engine's features, so it carries no
/// `affinity`. Once the engine advertised `affinity`, the next request sends the key and
/// the token estimate, and the engine's status comes back in `X-Maverick-Affinity`. The
/// header never reaches the provider.
#[tokio::test]
async fn the_conversation_reaches_an_engine_that_advertised_affinity() {
    let (engine_url, bodies) = engine(vec![
        (200, Some("affinity"), ROUTED),
        (200, Some("affinity"), KEPT),
    ])
    .await;
    let (openai_url, upstream_headers) = upstream().await;
    let app = proxy(&engine_url, &openai_url).await;

    let first = send(&app, Some(b"conv_8f2a41".as_slice())).await;
    assert_eq!((first.0, first.1.as_deref()), (StatusCode::OK, None));
    let second = send(&app, Some(b"conv_8f2a41".as_slice())).await;
    assert_eq!(
        (second.0, second.1.as_deref()),
        (StatusCode::OK, Some("kept"))
    );

    let bodies = bodies.lock().unwrap().clone();
    assert!(!bodies[0].contains("affinity"), "{}", bodies[0]);
    let tokens = context_tokens(BODY.len());
    assert_eq!(
        bodies[1],
        format!(
            r#"{{"affinity":{{"context_tokens":{tokens},"key":"conv_8f2a41"}},"mode":"quality","workload":{{"capability":"structured_extraction","difficulty":3}}}}"#
        )
    );
    let upstream_headers = upstream_headers.lock().unwrap().clone();
    assert_eq!(upstream_headers.len(), 2);
    for headers in &upstream_headers {
        assert!(
            !headers.contains_key("x-maverick-conversation"),
            "{headers:?}"
        );
    }
}

/// D44, D50: an engine that never advertised `affinity` never sees it, and the response
/// has no `X-Maverick-Affinity`.
#[tokio::test]
async fn an_engine_without_the_feature_never_sees_affinity() {
    let (engine_url, bodies) =
        engine(vec![(200, None, ROUTED), (200, Some("other"), ROUTED)]).await;
    let (openai_url, _) = upstream().await;
    let app = proxy(&engine_url, &openai_url).await;
    for _ in 0..2 {
        let (status, affinity, _, _) = send(&app, Some(b"conv_8f2a41".as_slice())).await;
        assert_eq!((status, affinity), (StatusCode::OK, None));
    }
    for body in bodies.lock().unwrap().iter() {
        assert!(!body.contains("affinity"), "{body}");
    }
}

/// D44: a present but malformed conversation id is `400 invalid_request`, and the engine
/// is not called.
#[tokio::test]
async fn a_malformed_conversation_is_400_and_the_engine_is_not_called() {
    let (engine_url, bodies) = engine(Vec::new()).await;
    let (openai_url, _) = upstream().await;
    let app = proxy(&engine_url, &openai_url).await;
    let too_long = vec![b'a'; 257];
    for bad in [
        &too_long[..],
        "caf\u{e9}".as_bytes(),
        b"tab\there".as_slice(),
    ] {
        let (status, affinity, fallback, body) = send(&app, Some(bad)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad:?}");
        assert_eq!((affinity, fallback), (None, None), "{bad:?}");
        assert_eq!(
            body,
            Bytes::from(envelope_body(
                "invalid_request",
                "X-Maverick-Conversation must be 1 to 256 printable ASCII characters"
            )),
            "{bad:?}"
        );
    }
    assert!(bodies.lock().unwrap().is_empty());
}

/// D44, D46: a response served from the last good list carries no `X-Maverick-Affinity`:
/// the cached status belongs to an earlier turn.
#[tokio::test]
async fn a_fallback_carries_no_affinity_status() {
    let (engine_url, _) = engine(vec![
        (200, Some("affinity"), ROUTED),
        (200, Some("affinity"), KEPT),
    ])
    .await;
    let (openai_url, _) = upstream().await;
    let app = proxy(&engine_url, &openai_url).await;
    send(&app, Some(b"conv_1".as_slice())).await;
    let kept = send(&app, Some(b"conv_1".as_slice())).await;
    assert_eq!(kept.1.as_deref(), Some("kept"));
    // The script is spent: the engine answers 500, and the proxy falls open.
    let cached = send(&app, Some(b"conv_1".as_slice())).await;
    assert_eq!(cached.0, StatusCode::OK);
    assert_eq!(cached.2.as_deref(), Some("last_known_good"));
    assert_eq!(cached.1, None);
}
