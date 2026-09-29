//! Bead 4.8: the one opt-in, spend-capped live smoke test (reference Part 2 §14; spec §15).
//!
//! `live_smoke` is the ONLY test that may call real providers. It is `#[ignore]`d and skips
//! unless `MAVERICK_LIVE_SMOKE=1`, so `cargo test` never runs it by accident. Run it with
//! `scripts/live-smoke.sh` and the provider keys.
//!
//! `live_smoke_flow_runs_against_the_scripted_server` proves the same flow, the spend cap, and
//! the logging with no keys and no real network: it points the real provider clients at a local
//! `ScriptedTestServer`. CI runs this one; it never touches a real provider.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use maverick_domain::spend::{SpendColumn, SpendTask, project_spend};
use maverick_providers::anthropic::AnthropicClient;
use maverick_providers::google::GoogleClient;
use maverick_providers::openai::OpenAiClient;
use maverick_providers::{ProviderClient, ScriptedResponse, ScriptedTestServer};
use maverick_runner::{
    Batch, BatchColumn, BatchTask, Cancellation, Clients, RunRow, RunSink, Runner, RunnerConfig,
    SinkFuture,
};

/// The exact skip-text prefix (reference Part 2 §14).
const SKIP_PREFIX: &str = "live smoke is opt-in and spend-capped (spec §15)";
/// The hard spend cap in USD.
const CAP_USD: f64 = 1.00;
/// The per-call timeout.
const CALL_TIMEOUT: Duration = Duration::from_secs(90);

/// One smoke column: a provider model with its price.
#[derive(Clone)]
struct Column {
    provider: String,
    model_id: String,
    input_per_mtok: f64,
    output_per_mtok: f64,
}

impl Column {
    fn id(&self) -> String {
        format!("livesmoke-{}-{}", self.provider, self.model_id)
    }

    fn pricing_json(&self) -> String {
        format!(
            "{{\"input_per_mtok\":{},\"output_per_mtok\":{},\"currency\":\"USD\",\"as_of\":\"2026-07-01\",\"source\":\"livesmoke\"}}",
            self.input_per_mtok, self.output_per_mtok
        )
    }
}

/// The two default columns (reference Part 2 §14).
fn default_columns() -> Vec<Column> {
    vec![
        Column {
            provider: "anthropic".to_owned(),
            model_id: "claude-haiku-4-5".to_owned(),
            input_per_mtok: 1.00,
            output_per_mtok: 5.00,
        },
        Column {
            provider: "openai".to_owned(),
            model_id: "gpt-5.4-mini".to_owned(),
            input_per_mtok: 0.60,
            output_per_mtok: 2.40,
        },
    ]
}

/// `MAVERICK_LIVE_SMOKE_COLUMNS` (exactly two comma-separated
/// `provider:model_id:input_per_mtok:output_per_mtok` entries) or the defaults.
fn columns() -> Result<Vec<Column>, String> {
    let Ok(raw) = std::env::var("MAVERICK_LIVE_SMOKE_COLUMNS") else {
        return Ok(default_columns());
    };
    let parsed = raw
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(parse_column)
        .collect::<Result<Vec<Column>, String>>()?;
    if parsed.len() != 2 {
        return Err(format!(
            "MAVERICK_LIVE_SMOKE_COLUMNS needs exactly two entries, got {}",
            parsed.len()
        ));
    }
    Ok(parsed)
}

fn parse_column(entry: &str) -> Result<Column, String> {
    let parts: Vec<&str> = entry.split(':').collect();
    if parts.len() != 4 {
        return Err(format!(
            "a column entry is provider:model_id:input_per_mtok:output_per_mtok, got {entry:?}"
        ));
    }
    Ok(Column {
        provider: parts[0].to_owned(),
        model_id: parts[1].to_owned(),
        input_per_mtok: parts[2]
            .parse()
            .map_err(|error| format!("bad input price in {entry:?}: {error}"))?,
        output_per_mtok: parts[3]
            .parse()
            .map_err(|error| format!("bad output price in {entry:?}: {error}"))?,
    })
}

/// A synthetic smoke task (reference Part 2 §14). These are not corpus tasks.
struct SmokeTask {
    id: &'static str,
    prompt: &'static str,
    temperature: f32,
    max_tokens: i32,
}

fn tasks() -> Vec<SmokeTask> {
    vec![
        SmokeTask {
            id: "live-smoke-extraction",
            prompt: "Return only a JSON object with keys \"invoice_number\" (string) and \"total_usd\" (number), extracted from this text: Invoice INV-2041 was issued 2026-06-30 by Acme Corp for $1,180.50, due net-30.",
            temperature: 0.0,
            max_tokens: 200,
        },
        SmokeTask {
            id: "live-smoke-summarization",
            prompt: "Summarize the following in one sentence: The maintenance window moved from Saturday 02:00 UTC to Sunday 03:00 UTC because the database migration needs a second dry run; customer traffic is unaffected and the on-call rotation is unchanged.",
            temperature: 0.0,
            max_tokens: 150,
        },
    ]
}

/// Collects the runner's rows in completion order.
#[derive(Default)]
struct MemorySink {
    rows: Mutex<Vec<RunRow>>,
}

impl RunSink for MemorySink {
    fn insert_run<'a>(&'a self, row: &'a RunRow) -> SinkFuture<'a> {
        Box::pin(async move {
            self.rows.lock().expect("sink lock").push(row.clone());
            Ok(())
        })
    }
}

impl MemorySink {
    fn rows(&self) -> Vec<RunRow> {
        self.rows.lock().expect("sink lock").clone()
    }
}

/// The env var holding a provider's key, or `None` for an unknown provider.
fn key_var(provider: &str) -> Option<&'static str> {
    match provider {
        "anthropic" => Some("MAVERICK_ANTHROPIC_API_KEY"),
        "openai" => Some("MAVERICK_OPENAI_API_KEY"),
        "google" => Some("MAVERICK_GOOGLE_API_KEY"),
        "xai" => Some("MAVERICK_XAI_API_KEY"),
        _ => None,
    }
}

/// The providers used by `columns`, in first-seen order.
fn providers_used(columns: &[Column]) -> Vec<String> {
    let mut seen = Vec::new();
    for column in columns {
        if !seen.contains(&column.provider) {
            seen.push(column.provider.clone());
        }
    }
    seen
}

/// A real provider client for `provider` from its key env var. `Err` is the missing env var
/// name (never a key value).
fn real_client(provider: &str) -> Result<Arc<dyn ProviderClient>, String> {
    let var = key_var(provider).ok_or_else(|| format!("unknown provider {provider:?}"))?;
    let key = std::env::var(var).map_err(|_| var.to_owned())?;
    let client: Arc<dyn ProviderClient> = match provider {
        "anthropic" => Arc::new(AnthropicClient::new(key).map_err(|error| error.to_string())?),
        "openai" => Arc::new(OpenAiClient::openai(key).map_err(|error| error.to_string())?),
        "xai" => Arc::new(OpenAiClient::xai(key).map_err(|error| error.to_string())?),
        "google" => Arc::new(GoogleClient::new(key).map_err(|error| error.to_string())?),
        other => return Err(format!("unknown provider {other:?}")),
    };
    Ok(client)
}

/// `jobs.ProjectSpend(tasks, columns, 1)` must be at most the cap before any call.
fn project_and_gate(smoke_tasks: &[SmokeTask], columns: &[Column]) -> Result<f64, String> {
    let ids: Vec<String> = columns.iter().map(Column::id).collect();
    let pricings: Vec<String> = columns.iter().map(Column::pricing_json).collect();
    let spend_tasks: Vec<SpendTask<'_>> = smoke_tasks
        .iter()
        .map(|task| SpendTask {
            id: task.id,
            prompt: task.prompt,
            attachments: "",
            max_tokens: i64::from(task.max_tokens),
        })
        .collect();
    let spend_columns: Vec<SpendColumn<'_>> = ids
        .iter()
        .zip(&pricings)
        .map(|(id, pricing)| SpendColumn { id, pricing })
        .collect();
    let projection = project_spend(&spend_tasks, &spend_columns, 1)
        .map_err(|error| format!("spend projection failed: {error}"))?;
    if projection.projected_usd > CAP_USD {
        return Err(format!(
            "projected spend ${:.6} exceeds the ${CAP_USD:.2} cap",
            projection.projected_usd
        ));
    }
    Ok(projection.projected_usd)
}

fn column_by_id<'a>(columns: &'a [Column], id: &str) -> Option<&'a Column> {
    columns.iter().find(|column| column.id() == id)
}

/// Build the one-repeat batch, run it, and assert per-run: non-blank text, tokens in/out and
/// latency are positive, and the running cost never reaches the cap. One structured log line per
/// call, never with prompt text, output text, or keys. A provider failure is a test error but the
/// run continues (as Go's `t.Errorf`).
async fn drive(
    clients: Clients,
    columns: &[Column],
    smoke_tasks: &[SmokeTask],
) -> Result<(), String> {
    let batch = Batch {
        id: "bat_live_smoke".to_owned(),
        matrix_version: "livesmoke".to_owned(),
        repeats: 1,
        tasks: smoke_tasks
            .iter()
            .map(|task| BatchTask {
                id: task.id.to_owned(),
                prompt: task.prompt.to_owned(),
                attachments_json: "[]".to_owned(),
                temperature: task.temperature,
                max_tokens: task.max_tokens,
            })
            .collect(),
        columns: columns
            .iter()
            .map(|column| BatchColumn {
                id: column.id(),
                provider: column.provider.clone(),
                model_id: column.model_id.clone(),
                status: "active".to_owned(),
                settings_json: "{}".to_owned(),
                pricing_json: column.pricing_json(),
            })
            .collect(),
        done: HashSet::new(),
    };

    let sink = Arc::new(MemorySink::default());
    let config = RunnerConfig {
        repeats: 1,
        timeout: CALL_TIMEOUT,
        limits: HashMap::new(),
    };
    let runner = Runner::new(clients, sink.clone(), config);
    let summary = runner.run(&batch, &Cancellation::new()).await;

    let rows = sink.rows();
    if rows.is_empty() {
        return Err("the runner recorded no runs".to_owned());
    }

    let mut running_total = 0.0f64;
    let mut errors: Vec<String> = Vec::new();
    for row in &rows {
        let column = column_by_id(columns, &row.model_column_id);
        let provider = column.map_or("?", |column| column.provider.as_str());
        let model = column.map_or("?", |column| column.model_id.as_str());
        let cost_text = row.cost.clone().unwrap_or_else(|| "0".to_owned());
        running_total += cost_text.parse::<f64>().unwrap_or(0.0);
        // Structured line: no prompt, no output, no keys.
        eprintln!(
            "live-smoke call provider={provider} model={model} task={} status={} latency_ms={} tokens_in={} tokens_out={} cost={cost_text} running_total_usd={running_total:.6}",
            row.task_id,
            row.status,
            row.latency_ms.unwrap_or(0),
            row.tokens_in.unwrap_or(0),
            row.tokens_out.unwrap_or(0),
        );
        if running_total >= CAP_USD {
            return Err(format!(
                "actual spend reached ${running_total:.6}, at or above the ${CAP_USD:.2} cap"
            ));
        }
        if row.status == "ok" {
            let where_at = format!("task {} on {}", row.task_id, row.model_column_id);
            if row.output.as_deref().unwrap_or("").trim().is_empty() {
                errors.push(format!("blank output for {where_at}"));
            }
            if row.tokens_in.unwrap_or(0) <= 0 {
                errors.push(format!("tokens_in not positive for {where_at}"));
            }
            if row.tokens_out.unwrap_or(0) <= 0 {
                errors.push(format!("tokens_out not positive for {where_at}"));
            }
            if row.latency_ms.unwrap_or(0) <= 0 {
                errors.push(format!("latency not positive for {where_at}"));
            }
        } else {
            errors.push(format!(
                "provider call failed for task {} on {}: status={} error={}",
                row.task_id,
                row.model_column_id,
                row.status,
                row.error.as_deref().unwrap_or("")
            ));
        }
    }

    eprintln!(
        "live-smoke summary runs={} total_usd={:.6} by_status={:?}",
        summary.total, summary.cost_usd, summary.by_status
    );

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// A small server-side delay guarantees a positive round-trip latency.
fn ok_json(body: &str) -> ScriptedResponse {
    ScriptedResponse::json(200, body.as_bytes().to_vec()).with_delay(Duration::from_millis(3))
}

/// Two Anthropic Messages responses (one per task), in the shape the real client parses.
fn anthropic_script() -> Vec<ScriptedResponse> {
    vec![
        ok_json(
            "{\"content\":[{\"type\":\"text\",\"text\":\"invoice INV-2041 total 1180.50\"}],\"usage\":{\"input_tokens\":58,\"output_tokens\":21}}",
        ),
        ok_json(
            "{\"content\":[{\"type\":\"text\",\"text\":\"The maintenance window shifted a day for a second dry run with no customer or on-call impact.\"}],\"usage\":{\"input_tokens\":72,\"output_tokens\":26}}",
        ),
    ]
}

/// Two OpenAI chat-completions responses (one per task).
fn openai_script() -> Vec<ScriptedResponse> {
    vec![
        ok_json(
            "{\"choices\":[{\"message\":{\"content\":\"invoice INV-2041 total 1180.50\"}}],\"usage\":{\"prompt_tokens\":60,\"completion_tokens\":22}}",
        ),
        ok_json(
            "{\"choices\":[{\"message\":{\"content\":\"The maintenance window moved to Sunday for a second dry run; customers and on-call are unaffected.\"}}],\"usage\":{\"prompt_tokens\":74,\"completion_tokens\":25}}",
        ),
    ]
}

#[tokio::test]
#[ignore = "opt-in: run with MAVERICK_LIVE_SMOKE=1 and provider keys (scripts/live-smoke.sh)"]
async fn live_smoke() {
    if std::env::var("MAVERICK_LIVE_SMOKE").ok().as_deref() != Some("1") {
        eprintln!("{SKIP_PREFIX}: set MAVERICK_LIVE_SMOKE=1 to run");
        return;
    }
    let columns = columns().expect("live-smoke columns");
    let mut clients = Clients::new();
    let mut missing = Vec::new();
    for provider in providers_used(&columns) {
        match real_client(&provider) {
            Ok(client) => clients = clients.with(&provider, client),
            Err(var) => missing.push(var),
        }
    }
    if !missing.is_empty() {
        eprintln!(
            "{SKIP_PREFIX}: missing provider keys: {}",
            missing.join(", ")
        );
        return;
    }
    let smoke_tasks = tasks();
    let projected = project_and_gate(&smoke_tasks, &columns).expect("projected within cap");
    eprintln!("live-smoke projected_usd={projected:.6} cap_usd={CAP_USD:.2}");
    drive(clients, &columns, &smoke_tasks)
        .await
        .expect("live smoke passes");
}

#[tokio::test]
async fn live_smoke_flow_runs_against_the_scripted_server() {
    let columns = default_columns();
    let smoke_tasks = tasks();

    let anthropic = ScriptedTestServer::start(anthropic_script())
        .await
        .expect("anthropic mock");
    let openai = ScriptedTestServer::start(openai_script())
        .await
        .expect("openai mock");

    let clients = Clients::new()
        .with(
            "anthropic",
            Arc::new(
                AnthropicClient::new("fake-key")
                    .expect("anthropic client")
                    .with_base_url(anthropic.url()),
            ),
        )
        .with(
            "openai",
            Arc::new(
                OpenAiClient::openai("fake-key")
                    .expect("openai client")
                    .with_base_url(openai.url()),
            ),
        );

    // The spend gate runs the same way with fake keys.
    let projected = project_and_gate(&smoke_tasks, &columns).expect("projected within cap");
    assert!(projected <= CAP_USD, "projection {projected} over cap");

    drive(clients, &columns, &smoke_tasks)
        .await
        .expect("scripted live-smoke flow passes");

    // Two columns, two tasks, one repeat: two calls to each provider.
    assert_eq!(anthropic.calls().await.len(), 2, "anthropic calls");
    assert_eq!(openai.calls().await.len(), 2, "openai calls");
}
