//! The black-box API replay (bead 5.15): the parity gate for the whole API. See
//! `replay/mod.rs` and `parity/blackbox/README.md`.
//!
//! `MAVERICK_REPLAY_REPORT=<file>` writes every difference, explained or not, as JSON.

#[path = "replay/mod.rs"]
mod harness;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use maverick_store::MIGRATOR;
use maverick_store::health::GRAPHILE_WORKER_SCHEMA;
use maverick_testdb::Schema;
use sqlx::postgres::PgPoolOptions;

/// The engine migrations and graphile_worker's, as a deployment has them: `/health/detail`
/// reads the queue (D35).
static SCHEMA: LazyLock<Schema> = LazyLock::new(|| {
    Schema::sqlx(&MIGRATOR).then(Schema::custom("graphile_worker 0.13", |url| async move {
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await?;
        graphile_worker::WorkerUtils::new(pool.clone(), GRAPHILE_WORKER_SCHEMA)
            .migrate()
            .await?;
        pool.close().await;
        Ok(())
    }))
});

/// Replays the 188 Go tests the manifest points at `parity/blackbox/` (all but
/// `TestPhase2ExitEndToEndRecommend`, which builds its own database; bead 5.16's
/// `crates/maverick/tests/e2e.rs` covers it).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_go_recording_replays_with_no_unexplained_diffs() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let report = harness::run(&pool).await;
    let mut summary = format!(
        "{} exchanges, {} matched, {} explained differences, {} unexplained, {} seed errors, {} notes\n",
        report.exchanges,
        report.matched,
        report.explained.len(),
        report.diffs.len(),
        report.seed_errors.len(),
        report.notes.len(),
    );
    let mut by_row: BTreeMap<&str, usize> = BTreeMap::new();
    for (_, rows) in &report.explained {
        *by_row
            .entry(rows.split(':').next().unwrap_or_default())
            .or_default() += 1;
    }
    for (rows, count) in &by_row {
        summary.push_str(&format!("explained by {rows}: {count}\n"));
    }
    for error in report.seed_errors.iter().take(20) {
        summary.push_str(&format!("seed error: {error}\n"));
    }
    for note in report.notes.iter().take(20) {
        summary.push_str(&format!("note: {note}\n"));
    }
    for diff in report.diffs.iter().take(30) {
        summary.push_str(&format!(
            "\n{} #{} {} {} [{}]\n  expected: {}\n  actual:   {}\n",
            diff.test,
            diff.seq,
            diff.method,
            diff.path,
            diff.part,
            truncate(&diff.expected),
            truncate(&diff.actual),
        ));
    }
    eprintln!("{summary}");
    assert!(
        report.diffs.is_empty() && report.seed_errors.is_empty(),
        "unexplained replay differences:\n{summary}"
    );
}

/// Every Go test the manifest points at the recording is in the replayed script, and the
/// script holds the whole recording (README §1: 619 exchanges, 1,498 statements).
#[test]
fn every_blackbox_manifest_row_is_replayed() {
    let events = harness::script::load();
    let exchanges = events
        .iter()
        .filter(|e| matches!(e, harness::script::Event::Http(_)))
        .count();
    assert_eq!((exchanges, events.len() - exchanges), (619, 1498));
    let replayed: BTreeSet<&str> = events
        .iter()
        .map(|e| e.test().split('/').next().unwrap())
        .collect();
    let path = parity_support::parity_path("manifest/api.json").expect("parity dir");
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).expect("api manifest")).unwrap();
    let pointed: BTreeSet<&str> = manifest["tests"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row.get("blackbox").is_some())
        .map(|row| row["go_test"].as_str().unwrap().trim_start_matches("api."))
        .collect();
    assert_eq!(replayed, pointed);
    assert!(pointed.contains(harness::OWN_DATABASE));
}

fn truncate(text: &str) -> String {
    if text.len() > 600 {
        format!(
            "{}... ({} bytes)",
            &text[..(0..=600)
                .rev()
                .find(|&i| text.is_char_boundary(i))
                .unwrap_or(0)],
            text.len()
        )
    } else {
        text.to_owned()
    }
}
