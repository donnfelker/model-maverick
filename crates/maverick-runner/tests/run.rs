//! Parity: `parity/fixtures/measurement-io/runner.json` and the runner test catalog
//! (reference Part 2 §5.7), with an in-memory sink and the fake provider. Timing tests
//! run on paused tokio time.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use maverick_providers::fake::{FakeProvider, FixtureSpec, hash_prompt};
use maverick_providers::{
    CompletionRequest, ContextCanceled, DeadlineExceeded, ProviderClient, ProviderError,
    ProviderErrorKind, ProviderFuture,
};
use maverick_runner::{
    Batch, BatchColumn, BatchTask, Cancellation, Cancelled, Clients, Pacer, ProviderLimits, RunKey,
    RunRow, RunSink, Runner, RunnerConfig, SinkFuture, interval_for_rpm, status_for_error,
};
use serde_json::Value;
use tokio::time::Instant;

fn fixture() -> Value {
    parity_support::fixture("measurement-io", "runner.json").expect("fixture loads")
}

/// Collects rows in completion order; optionally fails every insert.
#[derive(Default)]
struct MemorySink {
    rows: Mutex<Vec<RunRow>>,
    fail: bool,
}

impl RunSink for MemorySink {
    fn insert_run<'a>(&'a self, row: &'a RunRow) -> SinkFuture<'a> {
        Box::pin(async move {
            if self.fail {
                return Err("database is down".into());
            }
            self.rows.lock().expect("lock").push(row.clone());
            Ok(())
        })
    }
}

impl MemorySink {
    fn rows(&self) -> Vec<RunRow> {
        self.rows.lock().expect("lock").clone()
    }
}

fn spec(model: &str, prompt: &str) -> FixtureSpec {
    FixtureSpec {
        model: model.to_owned(),
        prompt: prompt.to_owned(),
        prompt_sha256: String::new(),
        output: String::new(),
        tokens_in: 0,
        tokens_out: 0,
        latency_ms: 0,
        error: String::new(),
    }
}

fn task(id: &str, prompt: &str) -> BatchTask {
    BatchTask {
        id: id.to_owned(),
        prompt: prompt.to_owned(),
        attachments_json: "[]".to_owned(),
        temperature: 0.0,
        max_tokens: 256,
    }
}

fn column(id: &str, provider: &str, model: &str, status: &str) -> BatchColumn {
    BatchColumn {
        id: id.to_owned(),
        provider: provider.to_owned(),
        model_id: model.to_owned(),
        status: status.to_owned(),
        settings_json: "{}".to_owned(),
        pricing_json: r#"{"input_per_mtok": 3.0, "output_per_mtok": 15.0}"#.to_owned(),
    }
}

fn runner(clients: Clients, sink: Arc<MemorySink>, config: RunnerConfig) -> Runner {
    Runner::new(clients, sink, config)
}

fn batch(tasks: Vec<BatchTask>, columns: Vec<BatchColumn>, repeats: u32) -> Batch {
    Batch {
        id: "bat_fixed".to_owned(),
        matrix_version: "2026-07-01".to_owned(),
        repeats,
        tasks,
        columns,
        done: HashSet::new(),
    }
}

fn text(value: &Value, key: &str) -> String {
    value[key].as_str().expect(key).to_owned()
}

/// The five scripted kinds, a prompt with no fixture, and a 300 ms task under a 100 ms
/// runner timeout. Rows compare as a set keyed by task id.
///
/// parity: runner.TestRunStatusTaxonomy
#[tokio::test(start_paused = true)]
async fn status_taxonomy() {
    let doc = fixture();
    let run = &doc["taxonomy_run"];
    let col = &run["column"];
    let model = text(col, "model_id");
    let mut specs = Vec::new();
    let mut ok = spec(&model, "tx ok");
    ok.output = r#"{"fine": true}"#.to_owned();
    ok.tokens_in = 100;
    ok.tokens_out = 10;
    specs.push(ok);
    for (prompt, error) in [
        ("tx rate", "rate_limited"),
        ("tx 5xx", "provider_error"),
        ("tx timeout", "timeout"),
        ("tx invalid", "invalid_request"),
    ] {
        let mut s = spec(&model, prompt);
        s.error = error.to_owned();
        specs.push(s);
    }
    let mut slow = spec(&model, "tx slow");
    slow.latency_ms = 300;
    specs.push(slow);
    let fake = Arc::new(FakeProvider::from_fixtures(&specs).expect("fixtures"));
    let tasks = [
        ("task_tx_ok", "tx ok"),
        ("task_tx_rate", "tx rate"),
        ("task_tx_5xx", "tx 5xx"),
        ("task_tx_timeout", "tx timeout"),
        ("task_tx_invalid", "tx invalid"),
        ("task_tx_unknown", "tx unknown"),
        ("task_tx_slow", "tx slow"),
    ]
    .map(|(id, prompt)| task(id, prompt))
    .to_vec();
    let columns = vec![BatchColumn {
        id: text(col, "id"),
        provider: text(col, "provider"),
        model_id: model.clone(),
        status: text(col, "status"),
        settings_json: text(col, "settings_json"),
        pricing_json: text(col, "pricing_json"),
    }];
    let sink = Arc::new(MemorySink::default());
    let config = RunnerConfig {
        timeout: Duration::from_millis(run["config"]["timeout_ms"].as_u64().expect("ms")),
        repeats: 1,
        limits: HashMap::new(),
    };
    let summary = runner(
        Clients::new().with("anthropic", fake),
        Arc::clone(&sink),
        config,
    )
    .run(&batch(tasks, columns, 0), &Cancellation::new())
    .await;

    let rows: BTreeMap<String, RunRow> = sink
        .rows()
        .into_iter()
        .map(|r| (r.task_id.clone(), r))
        .collect();
    let want_rows = run["rows"].as_array().expect("rows");
    assert_eq!(rows.len(), want_rows.len());
    for want in want_rows {
        let task_id = text(want, "TaskID");
        let got = &rows[&task_id];
        assert_eq!(got.status, want["Status"], "{task_id}");
        let error = got.error.clone().unwrap_or_default();
        if task_id == "task_tx_unknown" {
            // The Go prompt is not in the fixture; the message format is the same with
            // this prompt's hash.
            assert_eq!(
                error,
                format!(
                    "fake provider: no fixture for model \"{model}\" prompt hash {} (add it to testdata)",
                    hash_prompt("tx unknown")
                )
            );
        } else {
            assert_eq!(error, want["Error"], "{task_id}");
        }
        assert_eq!(got.error.is_some(), want["ErrorValid"], "{task_id}");
        assert_eq!(got.output.is_some(), want["OutputValid"], "{task_id}");
        assert_eq!(
            got.output.clone().unwrap_or_default(),
            want["Output"],
            "{task_id}"
        );
        assert_eq!(got.cost.is_some(), want["CostValid"], "{task_id}");
        assert_eq!(
            got.cost.clone().unwrap_or_default(),
            want["Cost"],
            "{task_id}"
        );
        assert_eq!(got.latency_ms.is_some(), want["LatencyValid"], "{task_id}");
        assert_eq!(got.tokens_in.is_some(), want["TokensValid"], "{task_id}");
        assert_eq!(got.tokens_in.unwrap_or(0), want["TokensIn"], "{task_id}");
        assert_eq!(got.tokens_out.unwrap_or(0), want["TokensOut"], "{task_id}");
        assert_eq!(got.repeat_index, want["RepeatIndex"], "{task_id}");
        assert_eq!(got.batch_id, want["BatchID"], "{task_id}");
        assert_eq!(got.matrix_version, want["MatrixVersion"], "{task_id}");
        assert_eq!(f64::from(got.temperature), want["Temperature"], "{task_id}");
        assert!(got.id.starts_with("run_"), "{}", got.id);
    }
    // The slow task was cut at the runner timeout, and its latency is recorded.
    assert_eq!(rows["task_tx_slow"].latency_ms, Some(100));

    let want = &run["summary"]["summary"];
    assert_eq!(summary.batch_id, want["BatchID"]);
    assert_eq!(summary.total, 7);
    let by_status: BTreeMap<String, usize> =
        serde_json::from_value(want["ByStatus"].clone()).expect("by status");
    assert_eq!(summary.by_status, by_status);
    parity_support::assert_f64_within(
        summary.cost_usd,
        want["CostUSD"].as_f64().expect("cost"),
        1e-12,
    );
    assert_eq!(summary.error(), None);
}

/// parity: runner.TestRunSkipsRetiredColumns
#[tokio::test(start_paused = true)]
async fn skips_retired_columns() {
    let mut ok = spec("m-live", "p");
    ok.output = "fine".to_owned();
    let fake = Arc::new(FakeProvider::from_fixtures(&[ok]).expect("fixtures"));
    let sink = Arc::new(MemorySink::default());
    let summary = runner(
        Clients::new().with("anthropic", fake),
        Arc::clone(&sink),
        RunnerConfig::default(),
    )
    .run(
        &batch(
            vec![task("task_a", "p")],
            vec![
                column("col_live", "anthropic", "m-live", "active"),
                column("col_retired", "anthropic", "m-retired", "retired"),
            ],
            2,
        ),
        &Cancellation::new(),
    )
    .await;
    assert_eq!(summary.total, 2);
    assert!(sink.rows().iter().all(|r| r.model_column_id == "col_live"));
}

/// parity: runner.TestRunSetupFailureRecordsProviderError
#[tokio::test(start_paused = true)]
async fn setup_failure_records_provider_error() {
    let sink = Arc::new(MemorySink::default());
    let mut bad = column("col_bad", "anthropic", "m", "active");
    bad.settings_json = "{not json".to_owned();
    let summary = runner(
        Clients::new().with("anthropic", Arc::new(FakeProvider::new())),
        Arc::clone(&sink),
        RunnerConfig::default(),
    )
    .run(
        &batch(vec![task("task_a", "p")], vec![bad], 2),
        &Cancellation::new(),
    )
    .await;
    let rows = sink.rows();
    assert_eq!(rows.len(), 2);
    for row in &rows {
        assert_eq!(row.status, "provider_error");
        assert!(row.error.as_deref().unwrap_or("").contains("settings"));
        assert_eq!(row.latency_ms, None);
        assert_eq!(row.cost, None);
    }
    assert_eq!(summary.error(), None);
    assert_eq!(summary.cost_usd, 0.0);
}

/// Each setup failure, in precedence order, with its exact text.
#[tokio::test(start_paused = true)]
async fn setup_failure_texts() {
    let doc = fixture();
    let cases = &doc["setup_failures"];
    let fake: Arc<dyn ProviderClient> = Arc::new(FakeProvider::new());
    let check = |name: &'static str, t: BatchTask, c: BatchColumn, clients: Clients| {
        let want = cases[name].clone();
        async move {
            let sink = Arc::new(MemorySink::default());
            let summary = runner(clients, Arc::clone(&sink), RunnerConfig::default())
                .run(
                    &Batch {
                        id: "bat_x".to_owned(),
                        ..batch(vec![t], vec![c], 1)
                    },
                    &Cancellation::new(),
                )
                .await;
            let rows: Vec<String> = sink
                .rows()
                .iter()
                .map(|r| format!("{} | {}", r.status, r.error.clone().unwrap_or_default()))
                .collect();
            let want_rows = if want.is_array() {
                &want
            } else {
                &want["rows"]
            };
            assert_eq!(serde_json::json!(rows), *want_rows, "{name}");
            if let Some(expected) = want.get("summary") {
                assert_eq!(summary.batch_id, expected["BatchID"]);
                assert_eq!(summary.total, 1);
                assert_eq!(summary.by_status.get("provider_error"), Some(&1));
                assert_eq!(summary.cost_usd, 0.0);
            }
        }
    };
    let anthropic = || Clients::new().with("anthropic", Arc::clone(&fake));
    let mut t = task("task_att", "p");
    t.attachments_json = r#"[{"name": broken"#.to_owned();
    check(
        "bad_attachments",
        t,
        column("col_a", "anthropic", "m", "active"),
        anthropic(),
    )
    .await;
    let mut c = column("col_p", "anthropic", "m", "active");
    c.pricing_json = r#"{"input_per_mtok": -1}"#.to_owned();
    check("bad_pricing", task("task_a", "p"), c, anthropic()).await;
    let mut c = column("col_p", "anthropic", "m", "active");
    c.pricing_json = "{".to_owned();
    check("bad_pricing_json", task("task_a", "p"), c, anthropic()).await;
    let mut c = column("col_b", "anthropic", "m", "active");
    c.settings_json = "{not json".to_owned();
    check("bad_settings", task("task_a", "p"), c, anthropic()).await;
    check(
        "no_client",
        task("task_a", "p"),
        column("col_n", "openai", "m", "active"),
        anthropic(),
    )
    .await;
}

/// Counts in-flight calls and flags overlapping calls of the same (model, prompt).
struct Probe {
    inner: FakeProvider,
    state: Mutex<ProbeState>,
}

#[derive(Default)]
struct ProbeState {
    in_flight: usize,
    peak: usize,
    active: HashSet<(String, String)>,
    overlap: bool,
}

impl ProviderClient for Probe {
    fn complete<'a>(&'a self, request: CompletionRequest) -> ProviderFuture<'a> {
        Box::pin(async move {
            let key = (request.model.clone(), request.prompt.clone());
            {
                let mut s = self.state.lock().expect("lock");
                s.in_flight += 1;
                s.peak = s.peak.max(s.in_flight);
                if !s.active.insert(key.clone()) {
                    s.overlap = true;
                }
            }
            let result = self.inner.complete(request).await;
            let mut s = self.state.lock().expect("lock");
            s.in_flight -= 1;
            s.active.remove(&key);
            result
        })
    }
}

/// parity: runner.TestConcurrencyBoundAndSequentialRepeats
#[tokio::test(start_paused = true)]
async fn concurrency_bound_and_sequential_repeats() {
    let prompts: Vec<String> = (0..8).map(|i| format!("prompt {i}")).collect();
    let specs: Vec<FixtureSpec> = prompts
        .iter()
        .map(|p| {
            let mut s = spec("m", p);
            s.output = "ok".to_owned();
            s.latency_ms = 50;
            s
        })
        .collect();
    let probe = Arc::new(Probe {
        inner: FakeProvider::from_fixtures(&specs).expect("fixtures"),
        state: Mutex::new(ProbeState::default()),
    });
    let tasks: Vec<BatchTask> = prompts
        .iter()
        .enumerate()
        .map(|(i, p)| task(&format!("task_{i}"), p))
        .collect();
    let sink = Arc::new(MemorySink::default());
    let config = RunnerConfig {
        limits: HashMap::from([(
            "anthropic".to_owned(),
            ProviderLimits {
                max_concurrency: 3,
                rpm_limit: 0,
            },
        )]),
        ..RunnerConfig::default()
    };
    let summary = runner(
        Clients::new().with("anthropic", Arc::clone(&probe) as Arc<dyn ProviderClient>),
        Arc::clone(&sink),
        config,
    )
    .run(
        &batch(tasks, vec![column("col_a", "anthropic", "m", "active")], 2),
        &Cancellation::new(),
    )
    .await;
    assert_eq!(summary.total, 16);
    assert_eq!(summary.by_status.get("ok"), Some(&16));
    let state = probe.state.lock().expect("lock");
    assert!(state.peak <= 3 && state.peak >= 2, "peak {}", state.peak);
    assert!(!state.overlap);
    let mut seen: HashMap<String, Vec<i16>> = HashMap::new();
    for row in sink.rows() {
        seen.entry(row.task_id).or_default().push(row.repeat_index);
    }
    assert_eq!(seen.len(), 8);
    assert!(seen.values().all(|order| order == &[0, 1]));
}

/// parity: runner.TestPacerSpacing
#[tokio::test(start_paused = true)]
async fn pacer_spacing() {
    let cancel = Cancellation::new();
    let pacer = Pacer::new(6000);
    let started = Instant::now();
    for _ in 0..3 {
        pacer.wait(&cancel).await.expect("wait");
    }
    assert!(started.elapsed() >= Duration::from_millis(19));

    let cancelled = Cancellation::new();
    cancelled.cancel();
    let slow = Pacer::new(1);
    assert_eq!(slow.wait(&cancelled).await, Err(Cancelled));
    assert_eq!(slow.wait(&cancelled).await, Err(Cancelled));

    assert_eq!(Pacer::new(0).wait(&cancel).await, Ok(()));
    assert_eq!(Pacer::new(0).wait(&cancelled).await, Err(Cancelled));
}

/// A cancel during a pacer sleep ends the wait, and the claimed slot is not returned.
#[tokio::test(start_paused = true)]
async fn pacer_wait_ends_on_cancel() {
    let cancel = Cancellation::new();
    let pacer = Arc::new(Pacer::new(1));
    pacer.wait(&cancel).await.expect("first slot is now");
    let waiter = {
        let pacer = Arc::clone(&pacer);
        let cancel = cancel.clone();
        tokio::spawn(async move { pacer.wait(&cancel).await })
    };
    tokio::time::sleep(Duration::from_secs(1)).await;
    cancel.cancel();
    assert_eq!(waiter.await.expect("joins"), Err(Cancelled));
    assert_eq!(pacer.interval(), Duration::from_secs(60));
}

/// The interval is `time.Minute / rpm` in integer nanoseconds.
#[test]
fn pacer_interval_by_rpm() {
    let doc = fixture();
    for (rpm, want) in doc["pacer_interval_by_rpm"].as_object().expect("table") {
        let nanos: u64 = want
            .as_str()
            .and_then(|s| s.split_once('(').map(|(_, n)| n.trim_end_matches("ns)")))
            .and_then(|n| n.parse().ok())
            .expect("nanos");
        let rpm: i64 = rpm.parse().expect("rpm");
        assert_eq!(interval_for_rpm(rpm), Duration::from_nanos(nanos), "{rpm}");
    }
    assert_eq!(interval_for_rpm(0), Duration::ZERO);
    assert_eq!(interval_for_rpm(-5), Duration::ZERO);
}

/// parity: runner.TestRunnerHonorsRPMLimit
#[tokio::test(start_paused = true)]
async fn honors_rpm_limit() {
    let prompts: Vec<String> = (0..4).map(|i| format!("rpm {i}")).collect();
    let specs: Vec<FixtureSpec> = prompts.iter().map(|p| spec("m", p)).collect();
    let fake = Arc::new(FakeProvider::from_fixtures(&specs).expect("fixtures"));
    let tasks: Vec<BatchTask> = prompts
        .iter()
        .enumerate()
        .map(|(i, p)| task(&format!("task_{i}"), p))
        .collect();
    let sink = Arc::new(MemorySink::default());
    let config = RunnerConfig {
        limits: HashMap::from([(
            "anthropic".to_owned(),
            ProviderLimits {
                max_concurrency: 4,
                rpm_limit: 3000,
            },
        )]),
        ..RunnerConfig::default()
    };
    let started = Instant::now();
    let summary = runner(
        Clients::new().with("anthropic", fake),
        Arc::clone(&sink),
        config,
    )
    .run(
        &batch(tasks, vec![column("col_a", "anthropic", "m", "active")], 1),
        &Cancellation::new(),
    )
    .await;
    assert_eq!(summary.total, 4);
    // Starts at 0, 20, 40, and 60 ms.
    assert!(started.elapsed() >= Duration::from_millis(55));
    assert!(started.elapsed() < Duration::from_millis(80));
}

fn provider_error(kind: ProviderErrorKind) -> ProviderError {
    ProviderError::new("p", kind)
}

/// `statusForError` over the fixture's error shapes.
#[test]
fn status_for_error_rows() {
    let doc = fixture();
    let table = &doc["status_for_error"];
    let cases: Vec<(&str, ProviderError)> = vec![
        (
            "canceled",
            provider_error(ProviderErrorKind::Unknown).with_source(ContextCanceled),
        ),
        (
            "deadline",
            provider_error(ProviderErrorKind::Unknown).with_source(DeadlineExceeded),
        ),
        (
            "plain",
            provider_error(ProviderErrorKind::Unknown).with_source("boom"),
        ),
        (
            "rate limited",
            provider_error(ProviderErrorKind::RateLimited).with_status(429),
        ),
        ("timeout kind", provider_error(ProviderErrorKind::Timeout)),
        (
            // `fmt.Errorf("...: %w", context.DeadlineExceeded)`: a deadline two links
            // down the chain.
            "wrapped deadline",
            provider_error(ProviderErrorKind::Unknown).with_source(Box::new(
                provider_error(ProviderErrorKind::Unknown).with_source(DeadlineExceeded),
            )
                as Box<dyn std::error::Error + Send + Sync>),
        ),
    ];
    for (name, error) in cases {
        assert_eq!(status_for_error(&error), table[name], "{name}");
    }
}

/// A cancelled batch writes no rows; a cancel during a call ends it as a timeout.
#[tokio::test(start_paused = true)]
async fn cancellation_stops_the_batch() {
    let mut slow = spec("m", "slow");
    slow.latency_ms = 10_000;
    let fake = Arc::new(FakeProvider::from_fixtures(&[slow]).expect("fixtures"));
    let clients = Clients::new().with("anthropic", fake);
    let cols = vec![column("col_a", "anthropic", "m", "active")];

    let sink = Arc::new(MemorySink::default());
    let cancel = Cancellation::new();
    cancel.cancel();
    let summary = runner(clients.clone(), Arc::clone(&sink), RunnerConfig::default())
        .run(
            &batch(vec![task("task_a", "slow")], cols.clone(), 3),
            &cancel,
        )
        .await;
    assert_eq!(summary.total, 0);
    assert!(sink.rows().is_empty());

    let sink = Arc::new(MemorySink::default());
    let cancel = Cancellation::new();
    let run = {
        let runner = runner(clients, Arc::clone(&sink), RunnerConfig::default());
        let cancel = cancel.clone();
        let batch = batch(vec![task("task_a", "slow")], cols, 3);
        tokio::spawn(async move { runner.run(&batch, &cancel).await })
    };
    tokio::time::sleep(Duration::from_secs(1)).await;
    cancel.cancel();
    let summary = run.await.expect("joins");
    let rows = sink.rows();
    assert_eq!(rows.len(), 1, "later repeats are not started");
    assert_eq!(rows[0].status, "timeout");
    assert_eq!(
        rows[0].error.as_deref(),
        Some("fake: timeout: context canceled")
    );
    assert_eq!(rows[0].latency_ms, Some(1000));
    assert_eq!(summary.total, 1);
}

/// D10: a resumed batch skips the runs that already have a row.
#[tokio::test(start_paused = true)]
async fn resume_skips_done_runs() {
    let mut ok = spec("m", "p");
    ok.output = "fine".to_owned();
    let fake = Arc::new(FakeProvider::from_fixtures(&[ok]).expect("fixtures"));
    let sink = Arc::new(MemorySink::default());
    let mut resumed = batch(
        vec![task("task_a", "p"), task("task_b", "p")],
        vec![column("col_a", "anthropic", "m", "active")],
        3,
    );
    resumed.done = HashSet::from([
        RunKey {
            task_id: "task_a".to_owned(),
            model_column_id: "col_a".to_owned(),
            repeat_index: 0,
        },
        RunKey {
            task_id: "task_a".to_owned(),
            model_column_id: "col_a".to_owned(),
            repeat_index: 2,
        },
    ]);
    let summary = runner(
        Clients::new().with("anthropic", fake),
        Arc::clone(&sink),
        RunnerConfig::default(),
    )
    .run(&resumed, &Cancellation::new())
    .await;
    assert_eq!(summary.total, 4);
    let mut got: Vec<(String, i16)> = sink
        .rows()
        .into_iter()
        .map(|r| (r.task_id, r.repeat_index))
        .collect();
    got.sort();
    assert_eq!(
        got,
        [
            ("task_a".to_owned(), 1),
            ("task_b".to_owned(), 0),
            ("task_b".to_owned(), 1),
            ("task_b".to_owned(), 2),
        ]
    );
    assert!(sink.rows().iter().all(|r| r.batch_id == "bat_fixed"));
}

/// Failed inserts are collected and joined; they are not tallied.
#[tokio::test(start_paused = true)]
async fn persistence_failures_are_collected() {
    let mut ok = spec("m", "p");
    ok.output = "fine".to_owned();
    let fake = Arc::new(FakeProvider::from_fixtures(&[ok]).expect("fixtures"));
    let sink = Arc::new(MemorySink {
        fail: true,
        ..MemorySink::default()
    });
    let summary = runner(
        Clients::new().with("anthropic", fake),
        Arc::clone(&sink) as Arc<MemorySink>,
        RunnerConfig::default(),
    )
    .run(
        &batch(
            vec![task("task_a", "p")],
            vec![column("col_a", "anthropic", "m", "active")],
            2,
        ),
        &Cancellation::new(),
    )
    .await;
    assert_eq!(summary.total, 0);
    assert!(summary.by_status.is_empty());
    assert_eq!(summary.persistence_errors.len(), 2);
    for (repeat, message) in summary.persistence_errors.iter().enumerate() {
        assert!(message.starts_with("persisting run run_"), "{message}");
        assert!(
            message.ends_with(&format!(
                "(task_a \u{d7} col_a repeat {repeat}): database is down"
            )),
            "{message}"
        );
    }
    assert_eq!(summary.error(), Some(summary.persistence_errors.join("\n")));
}

/// Batch repeats win over the config, the config over the default of 3; an empty batch
/// id mints `bat_<ULID>`.
#[tokio::test(start_paused = true)]
async fn repeats_and_batch_id_defaults() {
    let mut ok = spec("m", "p");
    ok.output = "fine".to_owned();
    let fake: Arc<dyn ProviderClient> =
        Arc::new(FakeProvider::from_fixtures(&[ok]).expect("fixtures"));
    let one = || vec![task("task_a", "p")];
    let cols = || vec![column("col_a", "anthropic", "m", "active")];
    for (batch_repeats, config_repeats, want) in [(0, 0, 3), (0, 2, 2), (4, 2, 4)] {
        let sink = Arc::new(MemorySink::default());
        let config = RunnerConfig {
            repeats: config_repeats,
            ..RunnerConfig::default()
        };
        let mut b = batch(one(), cols(), batch_repeats);
        b.id = String::new();
        let summary = runner(
            Clients::new().with("anthropic", Arc::clone(&fake)),
            Arc::clone(&sink),
            config,
        )
        .run(&b, &Cancellation::new())
        .await;
        assert_eq!(summary.total, want);
        assert!(summary.batch_id.starts_with("bat_") && summary.batch_id.len() == 30);
        assert!(sink.rows().iter().all(|r| r.batch_id == summary.batch_id));
    }
}

/// The default per-run timeout is 120 s.
#[tokio::test(start_paused = true)]
async fn default_timeout_is_120_seconds() {
    let mut slow = spec("m", "p");
    slow.latency_ms = 120_001;
    let fake = Arc::new(FakeProvider::from_fixtures(&[slow]).expect("fixtures"));
    let sink = Arc::new(MemorySink::default());
    runner(
        Clients::new().with("anthropic", fake),
        Arc::clone(&sink),
        RunnerConfig::default(),
    )
    .run(
        &batch(
            vec![task("task_a", "p")],
            vec![column("col_a", "anthropic", "m", "active")],
            1,
        ),
        &Cancellation::new(),
    )
    .await;
    let rows = sink.rows();
    assert_eq!(rows[0].status, "timeout");
    assert_eq!(rows[0].latency_ms, Some(120_000));
}
