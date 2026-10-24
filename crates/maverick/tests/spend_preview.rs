//! D52: `maverick spend preview` (bead 9.15), the hidden, read-only spend report on
//! real Postgres (testcontainers), plus its usage errors. The report numbers come from the
//! batch's own projection; `crates/maverick-jobs/tests/spend_preview.rs` checks those by
//! hand. Here one cold-start column runs one task: est_in ceil(4 / 4) + 600 = 601,
//! max_tokens 100, prices 4/20 per Mtok, 3 repeats:
//! (601 * 4 + 100 * 20) / 1e6 * 3 = 0.013212.

use std::io::{self, Write};
use std::sync::{Arc, LazyLock, Mutex};

use maverick_cli::{MAIN_USAGE, Output, SPEND_USAGE, run};
use maverick_store::catalog::{NewModelColumn, create_model_column};
use maverick_store::corpus::{TaskUpsert, upsert_corpus_version, upsert_task};
use maverick_store::{JsonRawValue, MIGRATOR};
use maverick_testdb::{Schema, database};

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

#[derive(Clone)]
struct Shared(Arc<Mutex<Vec<u8>>>);

impl Write for Shared {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// One CLI invocation on a blocking thread (`run` builds its own runtime).
async fn cli(args: &[&str], env: Vec<(String, String)>) -> (i32, String, String) {
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    tokio::task::spawn_blocking(move || {
        let out = Arc::new(Mutex::new(Vec::new()));
        let err = Arc::new(Mutex::new(Vec::new()));
        let lookup = |name: &str| {
            env.iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };
        let code = run(
            &args,
            &lookup,
            Output::new(Shared(out.clone())),
            Output::new(Shared(err.clone())),
        );
        let read = |buffer: &Arc<Mutex<Vec<u8>>>| {
            String::from_utf8(buffer.lock().expect("lock").clone()).expect("utf8")
        };
        (code, read(&out), read(&err))
    })
    .await
    .expect("join")
}

fn json(text: &str) -> Box<JsonRawValue> {
    JsonRawValue::from_string(text.to_owned()).expect("json")
}

#[tokio::test]
async fn preview_reports_the_cold_start_projection_and_the_refusal() {
    let Some(db) = database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    upsert_corpus_version(&pool, "v-cli", "", 1, true)
        .await
        .expect("version");
    let tags: Vec<String> = Vec::new();
    upsert_task(
        &pool,
        &TaskUpsert {
            id: "task_cli",
            capability: "factual_qa",
            domain_tags: &tags,
            difficulty_tier: 1,
            difficulty_knobs: &json("{}"),
            prompt: "abcd",
            attachments: &json("[]"),
            temperature: 0.0,
            max_tokens: 100,
            grader_specs: &json("[]"),
            dimension_weights: &json("{}"),
            pass_threshold: 0.7,
            smoke_member: false,
            corpus_version: "v-cli",
            active: true,
        },
    )
    .await
    .expect("task");
    create_model_column(
        &pool,
        &NewModelColumn {
            id: "col_cold",
            provider: "anthropic",
            model_id: "m-cold",
            endpoint: "direct",
            settings: &json("{}"),
            status: "cold_start",
            pricing: &json(r#"{"input_per_mtok":4,"output_per_mtok":20}"#),
            quality_source_column_id: None,
            cold_start_until: None,
            deprecation_date: None,
            execution_aliases: None,
            retention_mode: None,
            retention_verified_at: None,
            retention_valid_until: None,
            retention_source: None,
            execution_alias_retention: None,
            param_constraints: None,
        },
    )
    .await
    .expect("column");

    let env = vec![
        ("MAVERICK_ROLE".to_owned(), "origin".to_owned()),
        ("MAVERICK_DATABASE_URL".to_owned(), db.url().to_owned()),
        ("MAVERICK_CAPTURE_MASTER_KEY".to_owned(), "ab".repeat(32)),
        (
            "MAVERICK_INTERNAL_TOKENS".to_owned(),
            "rails:s3cret".to_owned(),
        ),
        (
            "MAVERICK_SPEND_NIGHTLY_CAP_USD".to_owned(),
            "0.01".to_owned(),
        ),
    ];
    let (code, out, err) = cli(&["spend", "preview", "--night", "2030-10-01"], env.clone()).await;
    assert_eq!((code, err.as_str()), (0, ""), "{out}");
    assert_eq!(
        out,
        "nightly_smoke for 2030-10-01: corpus v-cli, 3 repeats

GROUP       TASKS  COLUMNS  RUNS  PROJECTED_USD
smoke       0      0        0     0.0000
cold_start  1      1        3     0.0132

COLUMN    MODEL                    STATUS      GROUP       RUNS  PROJECTED_USD
col_cold  anthropic/m-cold/direct  cold_start  cold_start  3     0.0132

projected  $0.0132 for 3 runs
night      $0.0000 spent + $0.0132 projected = $0.0132 of the $0.01 nightly cap
week       $0.0000 spent + $0.0132 projected = $0.0132 of the $500.00 weekly cap
verdict    refuse: the batch would not run, and it would raise a critical spend_refusal
"
    );
    let (code, out, err) = cli(
        &[
            "spend",
            "preview",
            "--job",
            "weekly_sweep",
            "--night",
            "2030-10-01",
        ],
        env.clone(),
    )
    .await;
    assert_eq!((code, err.as_str()), (0, ""), "{out}");
    assert!(out.starts_with("weekly_sweep for 2030-10-01"), "{out}");
    assert!(
        out.contains("not checked: weekly_sweep has only the weekly cap"),
        "{out}"
    );
    assert!(out.contains("allow: the batch would run"), "{out}");

    let mut once = env;
    once.push(("MAVERICK_RUNNER_REPEATS".to_owned(), "1".to_owned()));
    let (code, out, err) = cli(&["spend", "preview", "--night", "2030-10-01"], once).await;
    assert_eq!((code, err.as_str()), (0, ""), "{out}");
    assert!(out.contains("corpus v-cli, 1 repeats"), "{out}");
    assert!(out.contains("projected  $0.0044 for 1 runs"), "{out}");
    assert!(out.contains("allow: the batch would run"), "{out}");

    let writes: i64 = sqlx::query_scalar("select (select count(*) from alerts) + (select count(*) from batches) + (select count(*) from runs)")
        .fetch_one(&pool)
        .await
        .expect("write count");
    assert_eq!(writes, 0, "the preview wrote no alert, batch, or run");
    db.close().await;
}

#[tokio::test]
async fn spend_is_hidden_and_rejects_bad_usage_before_the_config() {
    assert!(
        !MAIN_USAGE.contains("spend"),
        "Go's usage text is unchanged"
    );
    let (code, out, err) = cli(&["spend"], Vec::new()).await;
    assert_eq!((code, out.as_str(), err.as_str()), (2, "", SPEND_USAGE));
    let (code, _, err) = cli(&["spend", "project"], Vec::new()).await;
    assert_eq!(code, 2);
    assert_eq!(
        err,
        format!("maverick spend: unknown subcommand \"project\"\n\n{SPEND_USAGE}")
    );
    let (code, _, err) = cli(
        &["spend", "preview", "--job", "prediction_lock"],
        Vec::new(),
    )
    .await;
    assert_eq!(
        (code, err.as_str()),
        (
            2,
            "maverick spend preview: --job must be nightly_smoke or weekly_sweep, not \"prediction_lock\"\n"
        )
    );
    let (code, _, err) = cli(&["spend", "preview", "--night", "2030-13-01"], Vec::new()).await;
    assert_eq!(
        (code, err.as_str()),
        (
            2,
            "maverick spend preview: --night must be YYYY-MM-DD, not \"2030-13-01\"\n"
        )
    );
    for invalid in ["2030-1-01", "2030-02-30", "tomorrow"] {
        let (code, _, err) = cli(&["spend", "preview", "--night", invalid], Vec::new()).await;
        assert_eq!(code, 2, "{err}");
        assert!(err.starts_with("maverick spend preview: --night must be YYYY-MM-DD"));
    }
    let (code, _, err) = cli(&["spend", "preview", "extra"], Vec::new()).await;
    assert_eq!(code, 2);
    assert_eq!(
        err,
        "maverick spend preview: unexpected positional arguments\n"
    );
    for flag in ["--help", "--job", "--unknown"] {
        let (code, out, err) = cli(&["spend", "preview", flag], Vec::new()).await;
        assert_eq!((code, out.as_str()), (2, ""));
        assert!(err.contains("Usage of spend preview:"), "{err}");
    }
}
