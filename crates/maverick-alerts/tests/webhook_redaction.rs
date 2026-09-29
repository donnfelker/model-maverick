//! A failed webhook delivery never logs the webhook URL (G5, D19; bead model-maverick-rz6z).
//!
//! A webhook URL often carries its token in the path or the query. reqwest's error text
//! ends with the full URL, so the fan-out's failure lines and `Webhook`'s Debug must drop
//! everything after the origin.

use std::net::TcpListener;
use std::time::Duration;

use chrono::Utc;
use maverick_alerts::{AlertNotifier, Destination, Fanout, Webhook};
use maverick_store::JsonRawValue;
use maverick_store::alerts::Alert;
use serde_json::Value;
use test_support::{TestTracing, init_test_tracing_with_secrets};

const CANARY: &str = "RZ6Z-WEBHOOK-CANARY-4417";
const ATTEMPT_FAILED: &str = "alert delivery attempt failed";
const DELIVERY_FAILED: &str = "alert delivery failed";

/// A loopback URL with the canary as the password, in the path, and in the query, on a
/// port where nothing listens (bound, read, closed).
fn unreachable_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);
    format!("http://ops:{CANARY}@127.0.0.1:{port}/hooks/{CANARY}?token={CANARY}")
}

fn alert() -> Alert {
    Alert {
        id: "alr_rz6z".to_owned(),
        kind: "classifier_degraded".to_owned(),
        severity: "critical".to_owned(),
        subject: JsonRawValue::from_string("{}".to_owned()).expect("subject"),
        message: "classifier unresolved".to_owned(),
        created_at: Utc::now(),
        acked_at: None,
    }
}

fn lines_with(tracing: &TestTracing, msg: &str) -> Vec<Value> {
    tracing
        .captured_json()
        .into_iter()
        .filter(|line| line["msg"] == msg)
        .collect()
}

#[tokio::test]
async fn a_failed_delivery_logs_no_part_of_the_webhook_url() {
    let tracing = init_test_tracing_with_secrets(&[CANARY]);
    let webhook = Webhook::new(&unreachable_url(), "whsec_test").expect("webhook");
    let shown = format!("{webhook:?}");
    assert!(
        !shown.contains(CANARY),
        "Debug shows only the origin: {shown}"
    );
    assert!(shown.contains("http://127.0.0.1:"), "{shown}");

    let fanout = Fanout::with_destinations(vec![Destination::Webhook(webhook)])
        .with_backoff_base(Duration::ZERO);
    fanout.notify(&alert());
    fanout.wait().await;

    let attempts = lines_with(&tracing, ATTEMPT_FAILED);
    let gave_up = lines_with(&tracing, DELIVERY_FAILED);
    assert_eq!(attempts.len(), 4, "{:#?}", tracing.captured_lines());
    assert_eq!(gave_up.len(), 1, "{:#?}", tracing.captured_lines());
    for line in attempts.iter().chain(&gave_up) {
        assert_eq!(line["destination"], "webhook");
        let error = line["error"].as_str().expect("error field");
        // Why it failed stays: the request error and its cause chain.
        assert!(error.starts_with("error sending request: "), "{error}");
        assert!(!error.contains("for url"), "{error}");
    }
    let leaks: Vec<String> = tracing
        .captured_lines()
        .into_iter()
        .filter(|line| line.contains(CANARY))
        .collect();
    assert!(
        leaks.is_empty(),
        "the webhook URL reached the log: {leaks:?}"
    );
    assert!(tracing.secret_violations().is_empty());
}
