//! DIVERGENCE: D28. The proxy's per-tenant rate limit (bead 6.9), through the real server:
//! the limit, the reset at the next minute, per-key isolation in hosted mode, rejection
//! before the body is read, and the configured rate.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::time::Duration;

use maverick_core::recommend::{RecommendClient, RecommendClientConfig};
use maverick_proxy::chain::NoExecutor;
use maverick_proxy::config::{Config, load_config};
use maverick_proxy::ratelimit::{Clock, RateLimiter};
use maverick_proxy::server::{Proxy, ServeSettings, serve};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const CHAT: &str = r#"{"model":"maverick/auto","messages":[{"role":"user","content":"hi"}]}"#;
const LIMITED: &str =
    "{\"error\":{\"code\":\"rate_limited\",\"message\":\"rate limit exceeded\"}}\n";

/// A clock the test sets.
struct TestClock(AtomicI64);

impl Clock for TestClock {
    fn unix_seconds(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// A mock engine that answers every recommend call with an empty chain and counts calls.
async fn engine() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind engine");
    let url = format!("http://{}", listener.local_addr().expect("address"));
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let counted = Arc::clone(&counted);
            tokio::spawn(async move {
                let _ = read_response_or_request(&mut socket).await;
                counted.fetch_add(1, Ordering::SeqCst);
                let body = r#"{"recommendation_id":"rec_rl","recommendations":[]}"#;
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(reply.as_bytes()).await;
            });
        }
    });
    (url, calls)
}

/// Read one HTTP/1.1 message with a `Content-Length` body: `(status line, headers, body)`.
async fn read_response_or_request(
    socket: &mut TcpStream,
) -> (String, BTreeMap<String, String>, String) {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    let head_end = loop {
        if let Some(at) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break at + 4;
        }
        let read = socket.read(&mut chunk).await.expect("read");
        assert!(read > 0, "closed inside the head");
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let start = lines.next().unwrap_or_default().to_owned();
    let headers: BTreeMap<String, String> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let length: usize = headers
        .get("content-length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    while buffer.len() < head_end + length {
        let read = socket.read(&mut chunk).await.expect("read body");
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let body = String::from_utf8_lossy(&buffer[head_end..]).into_owned();
    (start, headers, body)
}

fn config(vars: &[(&str, &str)]) -> Config {
    let vars: BTreeMap<&str, &str> = vars.iter().copied().collect();
    load_config(|name| vars.get(name).map(|value| (*value).to_owned())).expect("config")
}

/// The proxy on `config` with `limiter`, or with the limiter the config asks for.
async fn start(config: Config, limiter: Option<RateLimiter>) -> String {
    let engine = RecommendClient::with_http(
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("client"),
        RecommendClientConfig {
            base_url: config.engine_url.clone(),
            timeout: Duration::from_secs(5),
            max_retries: 0,
        },
    );
    let mut proxy = Proxy::with_engine(config, engine, Arc::new(NoExecutor));
    if let Some(limiter) = limiter {
        proxy = proxy.with_limiter(limiter);
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind proxy");
    let addr = listener.local_addr().expect("address").to_string();
    tokio::spawn(serve(
        listener,
        proxy.router(),
        ServeSettings::default(),
        std::future::pending(),
    ));
    addr
}

/// POST a chat request with `headers` and `body`: `(status, headers, body)`.
async fn post(
    addr: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (u16, BTreeMap<String, String>, String) {
    let mut request = String::from("POST /v1/chat/completions HTTP/1.1\r\nHost: proxy.test\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    if !headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("content-length"))
    {
        request.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    request.push_str("Connection: close\r\n\r\n");
    request.push_str(body);
    let mut socket = TcpStream::connect(addr).await.expect("connect");
    socket.write_all(request.as_bytes()).await.expect("send");
    let (start, headers, body) = read_response_or_request(&mut socket).await;
    let status = start
        .split(' ')
        .nth(1)
        .and_then(|status| status.parse().ok())
        .expect("status");
    (status, headers, body)
}

fn self_hosted(engine_url: &str) -> Config {
    config(&[
        ("MAVERICK_API_KEY", "mk_live_test"),
        ("MAVERICK_ENGINE_URL", engine_url),
    ])
}

#[tokio::test]
async fn over_the_limit_is_429_with_headers_and_no_engine_call() {
    let d28 = parity_support::divergence!("D28");
    assert_eq!(d28.id(), "D28");
    let (engine_url, calls) = engine().await;
    let clock = Arc::new(TestClock(AtomicI64::new(1_700_000_010)));
    let addr = start(
        self_hosted(&engine_url),
        Some(RateLimiter::with_clock(2, clock.clone())),
    )
    .await;

    for _ in 0..2 {
        let (status, _, _) = post(&addr, &[("Content-Type", "application/json")], CHAT).await;
        assert_eq!(status, 502, "an empty chain: no_executable_candidates");
    }
    let (status, headers, body) = post(&addr, &[("Content-Type", "application/json")], CHAT).await;
    assert_eq!(status, 429);
    assert_eq!(body, LIMITED);
    assert_eq!(headers["retry-after"], "30");
    assert_eq!(headers["x-ratelimit-limit"], "2");
    assert_eq!(headers["x-ratelimit-remaining"], "0");
    assert_eq!(headers["x-ratelimit-reset"], "1700000040");
    assert_eq!(headers["content-type"], "application/json");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "the rejected request never reached the engine"
    );

    // The next minute starts a fresh window.
    clock.0.store(1_700_000_040, Ordering::SeqCst);
    let (status, _, _) = post(&addr, &[("Content-Type", "application/json")], CHAT).await;
    assert_eq!(status, 502);
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn a_limited_request_is_rejected_before_its_body_is_read() {
    let (engine_url, calls) = engine().await;
    let clock = Arc::new(TestClock(AtomicI64::new(60)));
    let addr = start(
        self_hosted(&engine_url),
        Some(RateLimiter::with_clock(1, clock)),
    )
    .await;
    assert_eq!(post(&addr, &[], CHAT).await.0, 502);
    // Malformed JSON would be a 400, and a declared oversize body a 413: both are 429 now.
    assert_eq!(post(&addr, &[], "not json").await.0, 429);
    let oversize = [("Content-Length", "99999999999")];
    assert_eq!(post(&addr, &oversize, "").await.0, 429);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn hosted_tenant_keys_have_separate_windows() {
    let (engine_url, calls) = engine().await;
    let hosted = config(&[
        ("MAVERICK_PROXY_MODE", "hosted"),
        ("MAVERICK_ENGINE_URL", &engine_url),
    ]);
    let clock = Arc::new(TestClock(AtomicI64::new(120)));
    let addr = start(hosted, Some(RateLimiter::with_clock(1, clock))).await;
    let key = |key: &'static str| [("X-Maverick-API-Key", key)];

    assert_eq!(post(&addr, &key("mk_live_a"), CHAT).await.0, 502);
    assert_eq!(post(&addr, &key("mk_live_a"), CHAT).await.0, 429);
    assert_eq!(
        post(&addr, &key("mk_live_b"), CHAT).await.0,
        502,
        "own window"
    );
    assert_eq!(post(&addr, &key("mk_live_b"), CHAT).await.0, 429);
    // An unauthenticated request is 401 before the limiter, and uses no window.
    assert_eq!(post(&addr, &key("not_a_key"), CHAT).await.0, 401);
    assert_eq!(post(&addr, &[], CHAT).await.0, 401);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

/// The binary's limiter reads `MAVERICK_PROXY_RATELIMIT_RPM`. Three requests at 1 per
/// minute on the system clock cross at most one minute boundary, so one is limited.
#[tokio::test]
async fn the_configured_rate_is_used() {
    let (engine_url, _) = engine().await;
    let limited = config(&[
        ("MAVERICK_API_KEY", "mk_live_test"),
        ("MAVERICK_ENGINE_URL", &engine_url),
        ("MAVERICK_PROXY_RATELIMIT_RPM", "1"),
    ]);
    let addr = start(limited, None).await;
    let mut statuses = Vec::new();
    for _ in 0..3 {
        statuses.push(post(&addr, &[], CHAT).await.0);
    }
    assert!(statuses.contains(&429), "{statuses:?}");
    assert!(statuses.contains(&502), "{statuses:?}");
}
