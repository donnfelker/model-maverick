//! The black-box API replay (bead 5.15; master plan §7.0 step 2(d);
//! `parity/blackbox/README.md`). Every recorded seed statement and HTTP exchange runs in
//! `gseq` order against one database, and each response is compared with the recorded one
//! after the README §5 masking. Each remaining difference must match a reviewed
//! `parity/blackbox/replay-allowlist.json` entry naming its DIVERGENCES row.

pub mod allow;
pub mod env;
pub mod script;
pub mod seed;
pub mod tokens;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::LazyLock;

use axum::body::Body;
use axum::http::{HeaderName, HeaderValue, Request};
use chrono::{DateTime, Utc};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

use env::Env;
use script::{Event, Exchange, Listener, Statement};
use seed::{Keys, Returned};
use tokens::Tokens;

/// The one test that builds its own database and runs the jobs system before its two
/// requests (README §4.1); `crates/maverick/tests/e2e.rs` covers it (bead 5.16).
pub const OWN_DATABASE: &str = "TestPhase2ExitEndToEndRecommend";

/// Response headers Go's server writes after the handler (README §2.1), and the request id
/// (minted per request; checked present instead).
const IGNORED_HEADERS: [&str; 4] = [
    "date",
    "content-length",
    "transfer-encoding",
    "x-request-id",
];

/// Request values the Go test read back from rows its own seed statements returned
/// (README §5 `since=`): (test, statement, occurrence in the test, 1-based).
const SEED_RETURNED: [(&str, &str, usize); 4] = [
    (
        "TestHistoryRecommendationsFilters",
        "CreateRecommendation :one",
        3,
    ),
    (
        "TestHistoryCalibrationListShape",
        "UpsertCalibration :one",
        1,
    ),
    (
        "TestHistoryCalibrationFiltersAndPagination",
        "UpsertCalibration :one",
        1,
    ),
    ("TestInternalAlertsFeedFilters", "InsertAlert :one", 2),
];

/// A raw key the server minted, in a `POST /internal/v1/tenants/{id}/api-keys` body.
static MINTED_KEY: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r#""key":"(mk_live_[0-9A-Za-z]{43})""#).unwrap());

/// One difference between the recorded response and ours.
#[derive(Debug, Clone)]
pub struct Diff {
    pub test: String,
    pub seq: u64,
    pub method: String,
    pub path: String,
    pub part: String,
    pub expected: String,
    pub actual: String,
}

impl Diff {
    fn to_json(&self) -> Value {
        json!({
            "test": self.test, "seq": self.seq, "method": self.method, "path": self.path,
            "part": self.part, "expected": self.expected, "actual": self.actual,
        })
    }
}

#[derive(Debug, Default)]
pub struct Report {
    pub exchanges: usize,
    pub matched: usize,
    pub diffs: Vec<Diff>,
    pub explained: Vec<(Diff, String)>,
    pub seed_errors: Vec<String>,
    pub notes: Vec<String>,
}

/// The recording's clock at each `gseq`: timestamps the Go run wrote into response
/// bodies and seed arguments, latest first at or before a given `gseq`.
struct Anchors(Vec<(u64, DateTime<Utc>)>);

impl Anchors {
    fn new(events: &[Event]) -> Self {
        let ts = regex::Regex::new(r#""(?:created_at|captured_at|scored_at|starts_at)":"([^"]+)""#)
            .unwrap();
        let mut out = Vec::new();
        for event in events {
            match event {
                Event::Http(x) => {
                    for caps in ts.captures_iter(&x.response_body) {
                        if let Some(at) = tokens::instant(&caps[1]) {
                            out.push((x.gseq, at));
                        }
                    }
                }
                Event::Seed(s) if s.name == "DeleteExpiredDebugCaptures :many" => {
                    if let Some(at) = s.args[0]["json"].as_str().and_then(tokens::instant) {
                        out.push((s.gseq, at));
                    }
                }
                Event::Seed(_) => {}
            }
        }
        out.sort_by_key(|(g, _)| *g);
        Anchors(out)
    }

    fn at(&self, gseq: u64) -> DateTime<Utc> {
        self.0
            .iter()
            .rev()
            .find(|(g, _)| *g <= gseq)
            .or_else(|| self.0.first())
            .map(|(_, at)| *at)
            .expect("the recording has timestamps")
    }
}

/// Per-test state.
struct Current {
    test: String,
    env: Env,
    /// Every statement's returned values, in order.
    returned: Vec<(String, Returned)>,
    /// Ids returned by read statements, not yet matched to a recorded id.
    read_ids: VecDeque<String>,
}

fn args_text(statement: &Statement) -> String {
    statement
        .args
        .iter()
        .map(|arg| {
            arg.get("hex")
                .and_then(Value::as_str)
                .and_then(|h| String::from_utf8(seed::hex(h)).ok())
                .unwrap_or_else(|| arg["json"].to_string())
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn authorization(exchange: &Exchange, keys: &Keys) -> Option<String> {
    let auth = exchange.authorization.as_str();
    match exchange.auth_kind.as_str() {
        "none" => None,
        "minted_api_key" => {
            let hash = auth
                .strip_prefix("Bearer <api_key sha256=")
                .and_then(|rest| rest.strip_suffix('>'))
                .expect("redacted api key");
            Some(match keys.raw(hash) {
                Some(raw) => format!("Bearer {raw}"),
                None => format!("Bearer unknown-key-{hash}"),
            })
        }
        "non_bearer_literal" => Some("Basic cmVwbGF5".to_owned()),
        _ => Some(auth.to_owned()),
    }
}

/// Maps the unmapped values a request carries: ids the test read back from the database,
/// and timestamps it copied from rows its seeds returned.
fn resolve_request(
    exchange: &Exchange,
    current: &mut Current,
    tokens: &mut Tokens,
    report: &mut Report,
) {
    let body = String::from_utf8_lossy(&exchange.body).into_owned();
    let path = tokens::query_unescape(&exchange.path);
    let text = format!("{path} {body}");
    for id in tokens.unmapped_ids(&text) {
        let prefix = &id[..id.find('_').unwrap() + 1];
        let position = current
            .read_ids
            .iter()
            .position(|ours| ours.starts_with(prefix));
        match position.and_then(|at| current.read_ids.remove(at)) {
            Some(ours) => {
                if let Err(conflict) = tokens.insert(&id, &ours) {
                    report
                        .notes
                        .push(format!("{} #{}: {conflict}", exchange.test, exchange.seq));
                }
            }
            None => tokens.fix(&id),
        }
    }
    for ts in tokens.unmapped_timestamps(&text) {
        let rule = SEED_RETURNED
            .iter()
            .find(|(test, _, _)| *test == exchange.test);
        let ours = rule.and_then(|(_, name, nth)| {
            current
                .returned
                .iter()
                .filter(|(n, _)| n == name)
                .nth(nth - 1)
                .and_then(|(_, returned)| returned.timestamps.first())
        });
        match ours {
            Some(at) => {
                let text = rfc3339_nano(*at);
                if let Err(conflict) = tokens.insert(&ts, &text) {
                    report
                        .notes
                        .push(format!("{} #{}: {conflict}", exchange.test, exchange.seq));
                }
            }
            None => report.notes.push(format!(
                "{} #{}: no value for timestamp {ts}",
                exchange.test, exchange.seq
            )),
        }
    }
}

/// A timestamp as the server writes it (Go RFC 3339 nano: UTC, trailing fraction zeros
/// trimmed), so a value copied into a request is the same text a response carries.
fn rfc3339_nano(at: DateTime<Utc>) -> String {
    let base = at.format("%Y-%m-%dT%H:%M:%S");
    match at.timestamp_subsec_nanos() {
        0 => format!("{base}Z"),
        nanos => format!("{base}.{}Z", format!("{nanos:09}").trim_end_matches('0')),
    }
}

#[test]
fn timestamps_are_written_as_the_server_writes_them() {
    use chrono::TimeZone;
    let at = Utc.with_ymd_and_hms(2026, 9, 29, 18, 29, 51).unwrap();
    assert_eq!(rfc3339_nano(at), "2026-09-29T18:29:51Z");
    let at = at + chrono::Duration::microseconds(343_040);
    assert_eq!(rfc3339_nano(at), "2026-09-29T18:29:51.34304Z");
}

struct Sent {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

async fn send(
    exchange: &Exchange,
    env: &Env,
    tokens: &Tokens,
    keys: &Keys,
    sessions: &HashMap<String, String>,
) -> (String, Sent) {
    let path = tokens::apply_path(tokens, &exchange.path);
    let body: Vec<u8> = match String::from_utf8(exchange.body.clone()) {
        Ok(text) => tokens.apply(&text).into_bytes(),
        Err(_) => exchange.body.clone(),
    };
    let mut request = Request::builder()
        .method(exchange.method.as_str())
        .uri(&path);
    for (name, value) in &exchange.headers {
        let value = if name.eq_ignore_ascii_case("mcp-session-id") {
            sessions
                .get(value)
                .cloned()
                .unwrap_or_else(|| value.clone())
        } else {
            value.clone()
        };
        request = request.header(
            HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(&value).unwrap(),
        );
    }
    if let Some(auth) = authorization(exchange, keys) {
        request = request.header("authorization", auth);
    }
    let body = if exchange.content_length < 0 {
        // Chunked: no declared length.
        let chunks = futures_util::stream::iter(
            body.chunks(64 * 1024)
                .map(|c| Ok::<_, std::io::Error>(axum::body::Bytes::copy_from_slice(c)))
                .collect::<Vec<_>>(),
        );
        Body::from_stream(chunks)
    } else {
        request = request.header("content-length", body.len());
        Body::from(body)
    };
    let router = match exchange.listener {
        Listener::Public => env.public.clone(),
        Listener::Internal => env.internal.clone(),
    };
    let response = router
        .oneshot(request.body(body).unwrap())
        .await
        .expect("router is infallible");
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .map(|(n, v)| {
            (
                n.as_str().to_owned(),
                v.to_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    let bytes = response
        .into_body()
        .collect()
        .await
        .map(|c| c.to_bytes())
        .unwrap_or_default();
    (
        path,
        Sent {
            status,
            headers,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        },
    )
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Vec<&'a str> {
    headers
        .iter()
        .filter(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
        .collect()
}

fn compare(
    exchange: &Exchange,
    path: &str,
    sent: &Sent,
    tokens: &mut Tokens,
    sessions: &mut HashMap<String, String>,
    report: &mut Report,
) -> Vec<Diff> {
    let diff = |part: &str, expected: String, actual: String| Diff {
        test: exchange.test.clone(),
        seq: exchange.seq,
        method: exchange.method.clone(),
        path: path.to_owned(),
        part: part.to_owned(),
        expected,
        actual,
    };
    let mut diffs = Vec::new();
    if sent.status != exchange.status {
        diffs.push(diff(
            "status",
            exchange.status.to_string(),
            sent.status.to_string(),
        ));
    } else {
        for conflict in tokens.pair(&exchange.response_body, &sent.body) {
            report
                .notes
                .push(format!("{} #{}: {conflict}", exchange.test, exchange.seq));
        }
        if let (Some(recorded), Some(ours)) = (
            header(&exchange.response_headers, "mcp-session-id").first(),
            header(&sent.headers, "mcp-session-id").first(),
        ) {
            sessions.insert((*recorded).to_owned(), (*ours).to_owned());
        }
    }
    let expected = tokens.apply(&exchange.response_body);
    if expected != sent.body {
        diffs.push(diff("body", expected, sent.body.clone()));
    }
    let mut recorded_names: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, value) in &exchange.response_headers {
        recorded_names
            .entry(name.to_ascii_lowercase())
            .or_default()
            .push(value.clone());
    }
    for (name, values) in &recorded_names {
        if IGNORED_HEADERS.contains(&name.as_str()) {
            continue;
        }
        let expected: Vec<String> = values
            .iter()
            .map(|v| {
                if name == "mcp-session-id" {
                    sessions.get(v).cloned().unwrap_or_else(|| v.clone())
                } else {
                    tokens.apply(v)
                }
            })
            .collect();
        let actual: Vec<String> = header(&sent.headers, name)
            .into_iter()
            .map(str::to_owned)
            .collect();
        if expected != actual {
            diffs.push(diff(
                &format!("header {name}"),
                expected.join(", "),
                actual.join(", "),
            ));
        }
    }
    if header(&sent.headers, "x-request-id")
        .first()
        .is_none_or(|v| v.is_empty())
    {
        diffs.push(diff(
            "header x-request-id",
            "present".into(),
            "absent".into(),
        ));
    }
    for (name, value) in &sent.headers {
        let lower = name.to_ascii_lowercase();
        if !recorded_names.contains_key(&lower) && !IGNORED_HEADERS.contains(&lower.as_str()) {
            diffs.push(diff(
                &format!("header {lower}"),
                "absent".into(),
                value.clone(),
            ));
        }
    }
    diffs
}

/// Replays the whole recording against `pool` (a fresh migrated database).
pub async fn run(pool: &PgPool) -> Report {
    let events = script::load();
    let files = env::test_files();
    let anchors = Anchors::new(&events);
    let allowlist = allow::Allowlist::load();
    let mut conn = pool.acquire().await.expect("seed connection");
    let mut tokens = Tokens::default();
    let mut keys = Keys::default();
    let mut sessions: HashMap<String, String> = HashMap::new();
    let mut current: Option<Current> = None;
    let mut report = Report::default();

    for event in &events {
        let test = event.test();
        if test == OWN_DATABASE {
            continue;
        }
        if current.as_ref().is_none_or(|c| c.test != test) {
            let base = test.split('/').next().unwrap();
            let file = files
                .get(base)
                .unwrap_or_else(|| panic!("{test} is not in the reference catalog"));
            current = Some(Current {
                test: test.to_owned(),
                env: env::build(test, file, pool),
                returned: Vec::new(),
                read_ids: VecDeque::new(),
            });
        }
        let current = current.as_mut().unwrap();
        match event {
            Event::Seed(statement) => {
                tokens.mint_seeded(&args_text(statement), &mut current.read_ids);
                let shift = Utc::now() - anchors.at(statement.gseq);
                match seed::apply(&mut conn, statement, &tokens, &mut keys, shift).await {
                    Ok(returned) => {
                        if statement.name.starts_with("List") || statement.name.starts_with("Get") {
                            current.read_ids.extend(
                                returned
                                    .texts
                                    .iter()
                                    .filter(|t| Tokens::is_id(t) && !tokens.is_ours(t))
                                    .cloned(),
                            );
                        }
                        current.returned.push((statement.name.clone(), returned));
                    }
                    Err(error) => report.seed_errors.push(format!(
                        "{} gseq {} {}: {error}",
                        statement.test, statement.gseq, statement.name
                    )),
                }
            }
            Event::Http(exchange) => {
                report.exchanges += 1;
                if exchange.test.starts_with("TestReplay")
                    && exchange.path.starts_with("/v1/debug/replay/")
                {
                    if let Some(stub) = &current.env.stub {
                        match exchange.test.as_str() {
                            "TestReplayMismatch" => stub.answer(Some(
                                r#"{"capability":"code_generation","difficulty":4,"domain_tags":[],"confidence":0.88}"#,
                            )),
                            "TestReplayClassifierFailureFallsBack" => stub.answer(None),
                            _ => {}
                        }
                    }
                }
                resolve_request(exchange, current, &mut tokens, &mut report);
                let (path, sent) = send(exchange, &current.env, &tokens, &keys, &sessions).await;
                // A raw key the server minted maps by the Go key's hash.
                if let (Some(recorded), Some(ours)) = (
                    MINTED_KEY.captures(&exchange.response_body),
                    MINTED_KEY.captures(&sent.body),
                ) {
                    use sha2::Digest;
                    let hash = seed::to_hex(&sha2::Sha256::digest(recorded[1].as_bytes()));
                    keys.insert(&hash, &ours[1]);
                }
                let diffs = compare(
                    exchange,
                    &path,
                    &sent,
                    &mut tokens,
                    &mut sessions,
                    &mut report,
                );
                if diffs.is_empty() {
                    report.matched += 1;
                }
                for diff in diffs {
                    match allowlist.explain(&diff, exchange.status) {
                        Some(reason) => report.explained.push((diff, reason)),
                        None => report.diffs.push(diff),
                    }
                }
            }
        }
    }
    if let Some(path) = std::env::var_os("MAVERICK_REPLAY_REPORT") {
        let doc = json!({
            "exchanges": report.exchanges,
            "matched": report.matched,
            "diffs": report.diffs.iter().map(Diff::to_json).collect::<Vec<_>>(),
            "explained": report.explained.iter().map(|(d, r)| json!({"diff": d.to_json(), "by": r})).collect::<Vec<_>>(),
            "seed_errors": report.seed_errors,
            "notes": report.notes,
        });
        std::fs::write(path, serde_json::to_string_pretty(&doc).unwrap()).expect("write report");
    }
    report
}
