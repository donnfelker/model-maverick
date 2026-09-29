//! Alert delivery and the row-first insert (reference Part 3 §4, §6 alerts tests).
//!
//! Expected wire bytes come from `parity/fixtures/measurement-core/alerts_wire.json`.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use chrono::{DateTime, Utc};
use hmac::{Hmac, KeyInit, Mac};
use maverick_alerts::{
    AlertKind, AlertNotifier, Destination, Email, Fanout, SIGNATURE_HEADER, Severity, Webhook,
    insert_alert, webhook_body, webhook_signature,
};
use maverick_store::JsonRawValue;
use maverick_store::alerts::Alert;
use maverick_testdb::Schema;
use serde_json::Value;
use sha2::Sha256;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

fn fixture() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../parity/fixtures/measurement-core/alerts_wire.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("fixture JSON")
}

/// The alert of `webhook.cases[i]`, as the store would return it.
fn fixture_alert(case: &Value) -> Alert {
    let a = &case["alert"];
    let s = |key: &str| a[key].as_str().expect(key).to_owned();
    Alert {
        id: s("id"),
        kind: s("kind"),
        severity: s("severity"),
        subject: JsonRawValue::from_string(s("subject_raw")).expect("subject"),
        message: s("message"),
        created_at: DateTime::parse_from_rfc3339(&s("created_at"))
            .expect("created_at")
            .with_timezone(&Utc),
        acked_at: None,
    }
}

fn webhook_cases() -> Vec<Value> {
    fixture()["webhook"]["cases"]
        .as_array()
        .expect("cases")
        .clone()
}

// parity: measurement-core/alerts.TestWebhookSignatureVerifies (the exact bytes half)
#[test]
fn webhook_body_and_signature_match_the_fixture() {
    for case in webhook_cases() {
        let alert = fixture_alert(&case);
        let body = webhook_body(&alert);
        assert_eq!(
            body,
            case["expected_body"].as_str().unwrap(),
            "{}",
            alert.id
        );
        let secret = case["secret"].as_str().unwrap().as_bytes();
        assert_eq!(
            webhook_signature(secret, body.as_bytes()),
            case["expected_signature_header"].as_str().unwrap()
        );
    }
}

// parity: measurement-core/alerts.TestEmailDeliver (the exact message half)
#[test]
fn email_message_matches_the_fixture() {
    let alerts: Vec<Alert> = webhook_cases().iter().map(fixture_alert).collect();
    for case in fixture()["email"]["cases"].as_array().unwrap() {
        let id = case["alert_id"].as_str().unwrap();
        let alert = alerts.iter().find(|a| a.id == id).expect("same alert");
        let to: Vec<&str> = case["to"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap())
            .collect();
        let url = format!(
            "smtp://mail.example.com/?from={}&to={}",
            case["from"].as_str().unwrap(),
            to.join(",")
        );
        let email = Email::new(&url).expect("email url");
        assert_eq!(
            email.message(alert),
            case["expected_data"].as_str().unwrap(),
            "{id}"
        );
    }
}

// ---- a scripted webhook receiver ----

#[derive(Default)]
struct Receiver {
    statuses: Mutex<VecDeque<u16>>,
    requests: Mutex<Vec<(HeaderMap, Vec<u8>)>>,
}

async fn receive(
    State(receiver): State<Arc<Receiver>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> StatusCode {
    receiver
        .requests
        .lock()
        .unwrap()
        .push((headers, body.to_vec()));
    let status = receiver.statuses.lock().unwrap().pop_front().unwrap_or(204);
    StatusCode::from_u16(status).unwrap()
}

async fn start_receiver(statuses: &[u16]) -> (String, Arc<Receiver>) {
    let receiver = Arc::new(Receiver::default());
    receiver.statuses.lock().unwrap().extend(statuses);
    let app = axum::Router::new()
        .route("/hook", post(receive))
        .with_state(receiver.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/hook"), receiver)
}

// ---- a minimal SMTP stub: no extensions, no TLS, no auth ----

#[derive(Debug, Default, Clone)]
struct SmtpSession {
    commands: Vec<String>,
    data: String,
}

async fn start_smtp_stub() -> (SocketAddr, Arc<Mutex<Vec<SmtpSession>>>) {
    start_smtp_stub_with(b"250 stub\r\n").await
}

async fn start_smtp_stub_with(
    ehlo_reply: &'static [u8],
) -> (SocketAddr, Arc<Mutex<Vec<SmtpSession>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().unwrap();
    let sessions = Arc::new(Mutex::new(Vec::new()));
    let store = sessions.clone();
    tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let store = store.clone();
            tokio::spawn(async move {
                let (read, mut write) = socket.into_split();
                let mut read = BufReader::new(read);
                let mut session = SmtpSession::default();
                write.write_all(b"220 stub ready\r\n").await.unwrap();
                loop {
                    let mut line = String::new();
                    if read.read_line(&mut line).await.unwrap() == 0 {
                        break;
                    }
                    let command = line.trim_end().to_owned();
                    session.commands.push(command.clone());
                    let verb = command
                        .split([' ', ':'])
                        .next()
                        .unwrap_or("")
                        .to_uppercase();
                    let reply: &[u8] = match verb.as_str() {
                        "EHLO" => ehlo_reply,
                        "HELO" => b"250 stub\r\n",
                        "AUTH" => b"235 accepted\r\n",
                        "MAIL" | "RCPT" => b"250 ok\r\n",
                        "DATA" => {
                            write.write_all(b"354 go ahead\r\n").await.unwrap();
                            loop {
                                let mut data = String::new();
                                read.read_line(&mut data).await.unwrap();
                                if data == ".\r\n" {
                                    break;
                                }
                                let data =
                                    data.strip_prefix('.').map_or(data.clone(), str::to_owned);
                                session.data.push_str(&data);
                            }
                            b"250 queued\r\n"
                        }
                        "QUIT" => {
                            write.write_all(b"221 bye\r\n").await.unwrap();
                            break;
                        }
                        _ => b"500 unknown\r\n",
                    };
                    write.write_all(reply).await.unwrap();
                }
                store.lock().unwrap().push(session);
            });
        }
    });
    (addr, sessions)
}

fn regression_alert() -> Alert {
    fixture_alert(&webhook_cases()[0])
}

// parity: measurement-core/alerts.TestWebhookSignatureVerifies
#[tokio::test]
async fn the_receiver_verifies_the_webhook_signature() {
    let (url, receiver) = start_receiver(&[204]).await;
    let fanout = Fanout::new("", &url, "whsec_test").expect("fanout");
    let alert = regression_alert();
    fanout.notify(&alert);
    fanout.wait().await;

    let requests = receiver.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let (headers, body) = &requests[0];
    assert_eq!(headers["content-type"], "application/json");
    let header = headers[SIGNATURE_HEADER].to_str().unwrap();
    let hex = header.strip_prefix("sha256=").expect("sha256= prefix");
    let signature: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect();
    let mut mac = Hmac::<Sha256>::new_from_slice(b"whsec_test").unwrap();
    mac.update(body);
    mac.verify_slice(&signature)
        .expect("constant-time HMAC check");

    let payload: Value = serde_json::from_slice(body).expect("JSON body");
    assert_eq!(payload["id"], alert.id);
    assert_eq!(payload["kind"], alert.kind);
    assert_eq!(payload["severity"], alert.severity);
    assert_eq!(payload["message"], alert.message);
    let subject: Value = serde_json::from_str(alert.subject.get()).unwrap();
    assert_eq!(payload["subject"], subject);
    let created = DateTime::parse_from_rfc3339(payload["created_at"].as_str().unwrap()).unwrap();
    assert_eq!(created, alert.created_at);
}

// parity: measurement-core/alerts.TestNewWebhookRequiresSecretAndHTTPURL
#[test]
fn a_webhook_needs_a_secret_and_an_http_url() {
    let err = Webhook::new("https://ops.example.com/hook", "").unwrap_err();
    assert_eq!(
        err,
        "MAVERICK_ALERTS_WEBHOOK_SECRET is required when the webhook URL is set"
    );
    let err = Webhook::new("ftp://ops.example.com/hook", "s").unwrap_err();
    assert_eq!(err, r#"webhook url scheme "ftp", want http or https"#);
    assert!(
        Webhook::new("not a url", "s")
            .unwrap_err()
            .starts_with("invalid webhook url: ")
    );
    assert!(Webhook::new("http://127.0.0.1:9/hook", "s").is_ok());
}

// parity: measurement-core/alerts.TestFanoutRetriesThenSucceeds
#[tokio::test]
async fn delivery_retries_until_it_succeeds() {
    let (url, receiver) = start_receiver(&[500, 500, 200]).await;
    let fanout = Fanout::new("", &url, "s")
        .expect("fanout")
        .with_backoff_base(Duration::ZERO);
    fanout.notify(&regression_alert());
    fanout.wait().await;
    assert_eq!(receiver.requests.lock().unwrap().len(), 3);
}

// parity: measurement-core/alerts.TestFanoutDeliveryFailureIsolation
#[tokio::test]
async fn a_failing_destination_does_not_stop_the_others() {
    let (url, receiver) = start_receiver(&[500, 500, 500, 500, 500]).await;
    let (smtp, sessions) = start_smtp_stub().await;
    let smtp_url = format!("smtp://{smtp}/?from=alerts@example.com&to=ops@example.com");
    let fanout = Fanout::new(&smtp_url, &url, "s")
        .expect("fanout")
        .with_backoff_base(Duration::ZERO);
    assert_eq!(fanout.destination_names(), ["email", "webhook"]);
    fanout.notify(&regression_alert());
    fanout.wait().await;
    assert_eq!(receiver.requests.lock().unwrap().len(), 4, "1 + 3 retries");
    let delivered = sessions.lock().unwrap();
    assert_eq!(delivered.len(), 1, "the email is delivered once");
    assert!(!delivered[0].data.is_empty());
}

// parity: measurement-core/alerts.TestEmailDeliver
#[tokio::test]
async fn email_is_delivered_over_smtp() {
    let (smtp, sessions) = start_smtp_stub().await;
    let url =
        format!("smtp://{smtp}/?from=alerts@example.com&to=ops@example.com,oncall@example.com");
    let fanout = Fanout::with_destinations(vec![Destination::Email(Email::new(&url).unwrap())]);
    let alert = regression_alert();
    fanout.notify(&alert);
    fanout.wait().await;

    let sessions = sessions.lock().unwrap();
    assert_eq!(sessions.len(), 1);
    let session = &sessions[0];
    let commands: Vec<&str> = session.commands.iter().map(String::as_str).collect();
    assert_eq!(
        commands,
        [
            "EHLO localhost",
            "MAIL FROM:<alerts@example.com>",
            "RCPT TO:<ops@example.com>",
            "RCPT TO:<oncall@example.com>",
            "DATA",
            "QUIT",
        ]
    );
    assert!(
        session
            .data
            .contains("Subject: [maverick] warning: regression")
    );
    assert!(session.data.contains(&alert.message));
    assert!(session.data.contains(&alert.id));
    assert!(session.data.contains(r#""model_column_id":"col_x""#));
    // The stub strips dot-stuffing, so the payload is the fixture's DATA exactly.
    assert_eq!(
        session.data,
        fixture()["email"]["cases"][0]["expected_data"]
            .as_str()
            .unwrap()
    );
}

/// Go allows AUTH PLAIN without TLS only to localhost; the stub listens on 127.0.0.1.
#[tokio::test]
async fn auth_plain_is_sent_when_the_server_advertises_auth() {
    let (smtp, sessions) = start_smtp_stub_with(b"250-stub\r\n250 AUTH PLAIN\r\n").await;
    let url = format!("smtp://user:pw@{smtp}/?from=a@example.com&to=b@example.com");
    let fanout = Fanout::with_destinations(vec![Destination::Email(Email::new(&url).unwrap())]);
    fanout.notify(&regression_alert());
    fanout.wait().await;
    let sessions = sessions.lock().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(
        sessions[0].commands[1], "AUTH PLAIN AHVzZXIAcHc=",
        "base64 of NUL user NUL pw"
    );
    assert!(!sessions[0].data.is_empty());
}

// parity: measurement-core/alerts.TestNewEmailValidatesURL
#[test]
fn the_smtp_url_is_validated() {
    for (url, want) in [
        (
            "https://mail.example.com/?from=a@x&to=b@x",
            r#"smtp url scheme "https", want smtp"#,
        ),
        ("smtp:///?from=a@x&to=b@x", "smtp url is missing a host"),
        (
            "smtp://mail.example.com/?to=b@x",
            "smtp url must carry ?from= and ?to= addresses",
        ),
        (
            "smtp://mail.example.com/?from=a@x",
            "smtp url must carry ?from= and ?to= addresses",
        ),
        (
            "smtp://mail.example.com/?from=a@x&to=",
            "smtp url must carry ?from= and ?to= addresses",
        ),
    ] {
        assert_eq!(Email::new(url).unwrap_err(), want, "{url}");
    }
    let email = Email::new("smtp://user:pw@mail.example.com/?from=a@x&to=b@x").unwrap();
    assert_eq!(email.addr(), "mail.example.com:587");
    assert!(email.has_auth());
    assert!(
        !format!("{email:?}").contains("pw"),
        "Debug never prints the password"
    );
}

// parity: measurement-core/alerts.TestNewFromConfig
#[tokio::test]
async fn the_fanout_is_built_from_config() {
    let none = Fanout::new("", "", "").expect("no destinations");
    assert!(none.destination_names().is_empty());
    none.notify(&regression_alert());
    none.wait().await;

    let both = Fanout::new(
        "smtp://mail.example.com/?from=a@x&to=b@x",
        "https://ops.example.com/hook",
        "whsec",
    )
    .expect("both");
    assert_eq!(both.destination_names(), ["email", "webhook"]);
    assert!(
        !format!("{both:?}").contains("whsec"),
        "Debug never prints the secret"
    );

    let err = Fanout::new("", "https://ops.example.com/hook", "").unwrap_err();
    assert_eq!(
        err,
        "alerts: MAVERICK_ALERTS_WEBHOOK_URL: MAVERICK_ALERTS_WEBHOOK_SECRET is required when the webhook URL is set"
    );
    let err = Fanout::new("smtp:///?from=a@x&to=b@x", "", "").unwrap_err();
    assert_eq!(err, "alerts: MAVERICK_SMTP_URL: smtp url is missing a host");
}

// parity: measurement-core/alerts.TestFanoutNilIsSafe
#[tokio::test]
async fn a_disabled_fanout_is_a_no_op() {
    let fanout = Fanout::disabled();
    fanout.notify(&regression_alert());
    fanout.wait().await;
}

// ---- the row-first insert, on real Postgres ----

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&maverick_store::MIGRATOR));

#[derive(Default)]
struct Recorder(Mutex<Vec<Alert>>);

impl AlertNotifier for Recorder {
    fn notify(&self, alert: &Alert) {
        self.0.lock().unwrap().push(alert.clone());
    }
}

fn subject() -> Value {
    serde_json::json!({"model_column_id": "col_x", "capability": "structured_extraction", "difficulty_tier": 3})
}

// parity: measurement-core/alerts.TestStoreNotifiesAfterInsert
#[tokio::test]
async fn insert_notifies_once_with_the_stored_row() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let recorder = Recorder::default();
    let row = insert_alert(
        &pool,
        Some(&recorder),
        AlertKind::Regression,
        Severity::Warning,
        &subject(),
        "m",
    )
    .await
    .expect("insert");
    assert!(row.id.starts_with("alr_"));
    let notified = recorder.0.lock().unwrap().clone();
    assert_eq!(notified.len(), 1);
    assert_eq!(notified[0].id, row.id);
    // The notified subject is the jsonb text Postgres returned, not the input bytes.
    assert_eq!(
        notified[0].subject.get(),
        r#"{"capability": "structured_extraction", "difficulty_tier": 3, "model_column_id": "col_x"}"#
    );
    pool.close().await;
    db.close().await;
}

// parity: measurement-core/alerts.TestStoreFailedInsertDoesNotNotify
#[tokio::test]
async fn a_failed_insert_notifies_nobody() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    pool.close().await;
    let recorder = Recorder::default();
    let err = insert_alert(
        &pool,
        Some(&recorder),
        AlertKind::JobFailure,
        Severity::Critical,
        &subject(),
        "m",
    )
    .await
    .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("jobs: writing job_failure alert: "),
        "{err}"
    );
    assert!(recorder.0.lock().unwrap().is_empty());
    db.close().await;
}

// parity: measurement-core/alerts.TestStoreNilNotifierWrites
#[tokio::test]
async fn a_missing_notifier_still_writes_the_row() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    insert_alert(
        &pool,
        None,
        AlertKind::RetentionExpiring,
        Severity::Warning,
        &subject(),
        "m",
    )
    .await
    .expect("insert");
    let rows = maverick_store::alerts::list_alerts_by_kind(&pool, "retention_expiring")
        .await
        .expect("list");
    assert_eq!(rows.len(), 1);
    pool.close().await;
    db.close().await;
}
