//! The `maverick corpus` subcommand bodies (bead 3.10; reference Part 1 §7.5).
//!
//! `load`, `activate`, and `status` drive the store; `calibrate` prints the tier calibration
//! report (a report, never a gate). The dispatcher and flag parsing live in [`crate::cli`];
//! this module holds the bodies, the pure tier-findings math, and the tabwriter.
//!
//! Streams follow §7.2: human text and the `corpus load` validation output go to stderr/stdout
//! as plain text, while config and database-connect failures go through the JSON logger to
//! stdout.

use std::io::Write as _;
use std::path::Path;

use maverick_domain::graders::ValidationRegistry;
use maverick_domain::tasks;
use maverick_store::catalog::{ModelColumn, list_model_columns};
use maverick_store::corpus::{activate_corpus, list_corpus_versions, store_corpus};
use maverick_store::measurement::{Aggregate, list_latest_aggregates_by_dimension};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

use crate::cli::{block_on, load_config};
use crate::logging::{Output, with_logger};

const EXIT_SUCCESS: i32 = 0;
const EXIT_FAILURE: i32 = 1;

/// The strictly-decreasing threshold for a tier-trend outlier, in pass-rate points.
const OUTLIER_THRESHOLD: f64 = 15.0;

type Lookup<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Open an eager pool. An unreachable database fails here: log `connecting to database` (JSON,
/// stdout) and return `None`.
async fn open_pool(url: &str) -> Option<PgPool> {
    match PgPoolOptions::new().max_connections(2).connect(url).await {
        Ok(pool) => Some(pool),
        Err(error) => {
            tracing::error!(error = %error, "connecting to database");
            None
        }
    }
}

/// `corpus load --version <v> --dir <path> [--description <text>] [--strict]`.
pub(crate) fn load(
    version: &str,
    dir: &str,
    description: &str,
    strict: bool,
    lookup: Lookup<'_>,
    stdout: Output,
    mut stderr: Output,
) -> i32 {
    // Validation runs before config and the database, so it works with no env set.
    let corpus = match tasks::load(Path::new(dir)) {
        Ok(corpus) => corpus,
        Err(error) => {
            let _ = writeln!(stderr, "maverick corpus load: {error}");
            return EXIT_FAILURE;
        }
    };
    let report = tasks::validate(&corpus, &ValidationRegistry::v1());
    let mut errors = report.errors;
    let mut warnings = report.warnings;
    if strict {
        errors.append(&mut warnings);
    }
    for warning in &warnings {
        let _ = writeln!(stderr, "warning: {warning}");
    }
    if !errors.is_empty() {
        for error in &errors {
            let _ = writeln!(stderr, "error: {error}");
        }
        let _ = writeln!(
            stderr,
            "maverick corpus load: rejected {dir}: {} violation(s)",
            errors.len()
        );
        return EXIT_FAILURE;
    }

    let task_count = corpus.tasks.len();
    let mut plain_out = stdout.clone();
    with_logger(stdout, || {
        let Ok(config) = load_config(lookup) else {
            return EXIT_FAILURE;
        };
        let outcome = block_on(async {
            let Some(pool) = open_pool(&config.database_url).await else {
                return Err(None);
            };
            store_corpus(&pool, version, description, &corpus)
                .await
                .map_err(|error| Some(error.to_string()))
        });
        match outcome {
            Ok(Ok(())) => {
                let _ = writeln!(
                    plain_out,
                    "loaded corpus version {version}: {task_count} tasks (inactive; run `maverick corpus activate --version {version}`)"
                );
                EXIT_SUCCESS
            }
            Ok(Err(Some(message))) => {
                let _ = writeln!(stderr, "maverick corpus load: {message}");
                EXIT_FAILURE
            }
            // Connect failure (already logged) or the runtime failed to build.
            Ok(Err(None)) | Err(_) => EXIT_FAILURE,
        }
    })
}

/// `corpus activate --version <v>`.
pub(crate) fn activate(
    version: &str,
    lookup: Lookup<'_>,
    stdout: Output,
    mut stderr: Output,
) -> i32 {
    let mut plain_out = stdout.clone();
    with_logger(stdout, || {
        let Ok(config) = load_config(lookup) else {
            return EXIT_FAILURE;
        };
        block_on(async {
            let Some(pool) = open_pool(&config.database_url).await else {
                return EXIT_FAILURE;
            };
            match activate_corpus(&pool, version).await {
                Ok(()) => {
                    let _ = writeln!(plain_out, "corpus version {version} is now active");
                    EXIT_SUCCESS
                }
                Err(error) => {
                    let _ = writeln!(stderr, "maverick corpus activate: {error}");
                    EXIT_FAILURE
                }
            }
        })
        .unwrap_or(EXIT_FAILURE)
    })
}

/// `corpus status`: list versions and the active flag through the tabwriter.
pub(crate) fn status(lookup: Lookup<'_>, stdout: Output, mut stderr: Output) -> i32 {
    let mut plain_out = stdout.clone();
    with_logger(stdout, || {
        let Ok(config) = load_config(lookup) else {
            return EXIT_FAILURE;
        };
        block_on(async {
            let Some(pool) = open_pool(&config.database_url).await else {
                return EXIT_FAILURE;
            };
            match list_corpus_versions(&pool).await {
                Ok(versions) => {
                    let mut rows = vec![vec![
                        "VERSION".to_owned(),
                        "TASKS".to_owned(),
                        "ACTIVE".to_owned(),
                        "CREATED".to_owned(),
                    ]];
                    for version in &versions {
                        rows.push(vec![
                            version.version.clone(),
                            version.task_count.to_string(),
                            version.active.to_string(),
                            version.created_at.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
                        ]);
                    }
                    let _ = plain_out.write_all(tabwrite(&rows, "").as_bytes());
                    EXIT_SUCCESS
                }
                Err(error) => {
                    let _ = writeln!(stderr, "maverick corpus status: {error}");
                    EXIT_FAILURE
                }
            }
        })
        .unwrap_or(EXIT_FAILURE)
    })
}

/// `corpus calibrate`: the tier calibration report. Exit 0 whenever the database is reachable.
pub(crate) fn calibrate(lookup: Lookup<'_>, stdout: Output, mut stderr: Output) -> i32 {
    let mut plain_out = stdout.clone();
    with_logger(stdout, || {
        let Ok(config) = load_config(lookup) else {
            return EXIT_FAILURE;
        };
        block_on(async {
            let Some(pool) = open_pool(&config.database_url).await else {
                return EXIT_FAILURE;
            };
            let aggregates = match list_latest_aggregates_by_dimension(&pool, "pass_rate").await {
                Ok(aggregates) => aggregates,
                Err(error) => {
                    let _ = writeln!(stderr, "maverick corpus calibrate: {error}");
                    return EXIT_FAILURE;
                }
            };
            let columns: Vec<ColumnInfo> = match list_model_columns(&pool).await {
                Ok(columns) => columns.iter().map(ColumnInfo::from).collect(),
                Err(error) => {
                    let _ = writeln!(stderr, "maverick corpus calibrate: {error}");
                    return EXIT_FAILURE;
                }
            };
            let _ = plain_out.write_all(render_report(&aggregates, &columns).as_bytes());
            EXIT_SUCCESS
        })
        .unwrap_or(EXIT_FAILURE)
    })
}

// ---------------------------------------------------------------------------------------
// The calibration report and its pure math.

/// One measured cell of a (capability, column) series.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Cell {
    tier: i16,
    /// The pass rate in points: `float64(float32 ewma) * 100`.
    pts: f64,
}

/// A finding's kind. The wire strings are `INVERSION` and `OUTLIER`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Inversion,
    Outlier,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Inversion => "INVERSION",
            Kind::Outlier => "OUTLIER",
        }
    }
}

/// One calibration finding.
#[derive(Debug, Clone, PartialEq)]
struct Finding {
    kind: Kind,
    tier: i16,
    detail: String,
}

/// The findings for one series (tiers ascending, measured only): monotonicity inversions and
/// tier-trend outliers past the 15-point threshold. All inversions, then all outliers, then a
/// stable sort by tier (an inversion sorts before an outlier at the same tier).
fn tier_findings(series: &[Cell]) -> Vec<Finding> {
    let mut findings = Vec::new();
    for window in series.windows(2) {
        let (easy, hard) = (window[0], window[1]);
        if hard.pts > easy.pts {
            let difference = hard.pts - easy.pts;
            findings.push(Finding {
                kind: Kind::Inversion,
                tier: hard.tier,
                detail: format!(
                    "tier {} pass rate {:.1} > tier {} pass rate {:.1} (+{difference:.1} points; expected monotone decreasing)",
                    hard.tier, hard.pts, easy.tier, easy.pts
                ),
            });
        }
    }
    for i in 1..series.len().saturating_sub(1) {
        let (lo, mid, hi) = (series[i - 1], series[i], series[i + 1]);
        let span = f64::from(hi.tier - lo.tier);
        let expected = lo.pts + (hi.pts - lo.pts) * f64::from(mid.tier - lo.tier) / span;
        let deviation = mid.pts - expected;
        if deviation.abs() > OUTLIER_THRESHOLD {
            findings.push(Finding {
                kind: Kind::Outlier,
                tier: mid.tier,
                detail: format!(
                    "tier {} pass rate {:.1} is {deviation:+.1} points off the tier trend (expected {expected:.1} from tiers {} and {}, threshold {OUTLIER_THRESHOLD:.1})",
                    mid.tier, mid.pts, lo.tier, hi.tier
                ),
            });
        }
    }
    findings.sort_by_key(|finding| finding.tier);
    findings
}

/// One (capability, column) measured series, ready to report and to check for findings.
struct Series {
    capability: String,
    display: String,
    rows: Vec<Aggregate>,
    cells: Vec<Cell>,
}

/// The column fields the report shows. Decoupled from the full store row so the report is
/// testable without constructing a `ModelColumn`.
struct ColumnInfo {
    id: String,
    provider: String,
    model_id: String,
    endpoint: String,
}

impl From<&ModelColumn> for ColumnInfo {
    fn from(column: &ModelColumn) -> Self {
        ColumnInfo {
            id: column.id.clone(),
            provider: column.provider.clone(),
            model_id: column.model_id.clone(),
            endpoint: column.endpoint.clone(),
        }
    }
}

/// The column display name `provider/model_id@endpoint (id)`, or the bare id when the column
/// row is missing.
fn display_name(id: &str, columns: &[ColumnInfo]) -> String {
    columns.iter().find(|column| column.id == id).map_or_else(
        || id.to_owned(),
        |column| {
            format!(
                "{}/{}@{} ({})",
                column.provider, column.model_id, column.endpoint, column.id
            )
        },
    )
}

/// Build the ordered, measured (capability, column) series from the latest aggregates.
fn build_series(aggregates: &[Aggregate], columns: &[ColumnInfo]) -> Vec<Series> {
    use std::collections::BTreeMap;
    // Group by (capability, column) keeping the measured rows.
    let mut groups: BTreeMap<(String, String), Vec<Aggregate>> = BTreeMap::new();
    for aggregate in aggregates {
        if aggregate.measured {
            groups
                .entry((
                    aggregate.capability.clone(),
                    aggregate.model_column_id.clone(),
                ))
                .or_default()
                .push(aggregate.clone());
        }
    }
    let mut series: Vec<Series> = groups
        .into_iter()
        .map(|((capability, column_id), mut rows)| {
            rows.sort_by_key(|row| row.difficulty_tier);
            let cells = rows
                .iter()
                .map(|row| Cell {
                    tier: row.difficulty_tier,
                    pts: f64::from(row.ewma) * 100.0,
                })
                .collect();
            Series {
                capability,
                display: display_name(&column_id, columns),
                rows,
                cells,
            }
        })
        .collect();
    // Order by capability, then display name (byte order).
    series.sort_by(|a, b| {
        a.capability
            .cmp(&b.capability)
            .then_with(|| a.display.cmp(&b.display))
    });
    series
}

/// The full report text (reference §7.5).
fn render_report(aggregates: &[Aggregate], columns: &[ColumnInfo]) -> String {
    let mut out = String::new();
    out.push_str("tier calibration report (capability-matrix design 5.3)\n");
    let series = build_series(aggregates, columns);
    if series.is_empty() {
        out.push_str("no data: no measured pass_rate aggregates\n");
        return out;
    }
    out.push_str("latest pass_rate aggregates per cell, measured cells only\n");

    let mut current_capability: Option<&str> = None;
    for entry in &series {
        if current_capability != Some(entry.capability.as_str()) {
            out.push('\n');
            out.push_str(&format!("capability: {}\n", entry.capability));
            current_capability = Some(entry.capability.as_str());
        }
        out.push('\n');
        out.push_str(&format!("  column {}\n", entry.display));
        let mut rows = vec![vec![
            "TIER".to_owned(),
            "PASS%".to_owned(),
            "N".to_owned(),
            "AS_OF".to_owned(),
        ]];
        for (cell, row) in entry.cells.iter().zip(&entry.rows) {
            rows.push(vec![
                cell.tier.to_string(),
                format!("{:.1}", cell.pts),
                row.n.to_string(),
                row.as_of.format("%Y-%m-%d").to_string(),
            ]);
        }
        out.push_str(&tabwrite(&rows, "    "));
    }

    let all: Vec<(usize, Finding)> = series
        .iter()
        .enumerate()
        .flat_map(|(index, entry)| {
            tier_findings(&entry.cells)
                .into_iter()
                .map(move |finding| (index, finding))
        })
        .collect();
    let inversions = all
        .iter()
        .filter(|(_, f)| f.kind == Kind::Inversion)
        .count();
    let outliers = all.iter().filter(|(_, f)| f.kind == Kind::Outlier).count();

    out.push('\n');
    out.push_str("findings:\n");
    if all.is_empty() {
        out.push_str("  none: every measured column is monotone within the 15.0-point threshold\n");
        out.push('\n');
        out.push_str(&format!(
            "0 finding(s) across {} measured (capability, column) series\n",
            series.len()
        ));
    } else {
        for (index, finding) in &all {
            let entry = &series[*index];
            out.push_str(&format!(
                "  {} {} {}: {}\n",
                finding.kind.label(),
                entry.capability,
                entry.display,
                finding.detail
            ));
        }
        out.push('\n');
        out.push_str(&format!(
            "{} finding(s): {inversions} inversion(s), {outliers} outlier(s) across {} measured (capability, column) series\n",
            all.len(),
            series.len()
        ));
    }
    out
}

/// A minimal Go `text/tabwriter` (minwidth 2, padding 2, space pad, no flags): each column is
/// `max(2, longest_cell + 2)` wide, cells are left-aligned, and the last cell of a row is not
/// padded. `indent` prefixes every line.
fn tabwrite(rows: &[Vec<String>], indent: &str) -> String {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![0usize; columns];
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(cell.chars().count());
        }
    }
    let mut out = String::new();
    for row in rows {
        out.push_str(indent);
        let last = row.len().saturating_sub(1);
        for (index, cell) in row.iter().enumerate() {
            if index == last {
                out.push_str(cell);
            } else {
                let width = (widths[index] + 2).max(2);
                out.push_str(cell);
                for _ in 0..width - cell.chars().count() {
                    out.push(' ');
                }
            }
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn calibrate_fixture() -> Value {
        let raw = include_str!("../../../parity/fixtures/store/calibrate.json");
        serde_json::from_str(raw).expect("parse calibrate.json")
    }

    // parity: store/main.TestTierFindingsInterpolatesAcrossTierGaps
    // parity: store/main.TestTierFindingsFlagsInversionAcrossTierGaps
    // parity: store/main.TestTierFindingsSingleTierAndEqualRatesAreClean
    // parity: store/main.TestCraftedGridDeviations
    #[test]
    fn tier_findings_match_the_fixture() {
        let fixture = calibrate_fixture();
        for case in fixture["tier_findings"].as_array().expect("tier_findings") {
            let id = case["id"].as_str().expect("id");
            let cells: Vec<Cell> = case["series"]
                .as_array()
                .expect("series")
                .iter()
                .map(|point| Cell {
                    tier: i16::try_from(point["tier"].as_i64().expect("tier")).expect("tier fits"),
                    pts: point["ewma_stored_float32_as_float64"]
                        .as_f64()
                        .expect("ewma")
                        * 100.0,
                })
                .collect();
            let got = tier_findings(&cells);
            let want = case["expected_findings"].as_array().expect("expected");
            assert_eq!(got.len(), want.len(), "{id}: finding count");
            for (finding, expected) in got.iter().zip(want) {
                assert_eq!(finding.kind.label(), expected["kind"], "{id}: kind");
                assert_eq!(
                    i64::from(finding.tier),
                    expected["tier"].as_i64().expect("tier"),
                    "{id}: tier"
                );
                assert_eq!(
                    finding.detail,
                    expected["detail"].as_str().expect("detail"),
                    "{id}"
                );
            }
        }
    }

    #[test]
    fn tabwriter_pads_columns_and_leaves_the_last_cell() {
        let rows = vec![
            vec![
                "TIER".to_owned(),
                "PASS%".to_owned(),
                "N".to_owned(),
                "AS_OF".to_owned(),
            ],
            vec![
                "1".to_owned(),
                "95.0".to_owned(),
                "5".to_owned(),
                "2026-07-05".to_owned(),
            ],
        ];
        let out = tabwrite(&rows, "    ");
        assert_eq!(
            out,
            "    TIER  PASS%  N  AS_OF\n    1     95.0   5  2026-07-05\n"
        );
    }

    fn aggregate(column: &str, capability: &str, tier: i16, pass: i32) -> Aggregate {
        Aggregate {
            model_column_id: column.to_owned(),
            capability: capability.to_owned(),
            difficulty_tier: tier,
            dimension: "pass_rate".to_owned(),
            ewma: (f64::from(pass) / 100.0) as f32,
            variance: 0.0,
            n: 5,
            measured: true,
            matrix_version: "2026-07-01".to_owned(),
            as_of: chrono::NaiveDate::from_ymd_opt(2026, 7, 5).expect("date"),
        }
    }

    fn column(id: &str, model: &str) -> ColumnInfo {
        ColumnInfo {
            id: id.to_owned(),
            provider: "anthropic".to_owned(),
            model_id: model.to_owned(),
            endpoint: "direct".to_owned(),
        }
    }

    fn series(column: &str, capability: &str, passes: &[i32]) -> Vec<Aggregate> {
        (1i16..)
            .zip(passes)
            .map(|(tier, &pass)| aggregate(column, capability, tier, pass))
            .collect()
    }

    #[test]
    fn render_report_matches_the_fixture() {
        let fixture = calibrate_fixture();

        // Empty: no measured aggregates.
        assert_eq!(
            render_report(&[], &[]),
            fixture["report_empty"].as_str().expect("report_empty")
        );

        // Clean single column: monotone, no findings.
        let clean = series("col_mono", "structured_extraction", &[95, 90, 80, 70, 60]);
        assert_eq!(
            render_report(&clean, &[column("col_mono", "cal-mono")]),
            fixture["report_clean_single_column"]
                .as_str()
                .expect("report_clean")
        );

        // Crafted grid: an outlier, an inversion, and a clean column, across two capabilities.
        let mut crafted = series("col_outlier", "factual_qa", &[95, 80, 45, 42, 40]);
        crafted.extend(series(
            "col_inverted",
            "structured_extraction",
            &[90, 70, 78, 65, 55],
        ));
        crafted.extend(series(
            "col_mono",
            "structured_extraction",
            &[95, 90, 80, 70, 60],
        ));
        let columns = [
            column("col_outlier", "cal-outlier"),
            column("col_inverted", "cal-inverted"),
            column("col_mono", "cal-mono"),
        ];
        assert_eq!(
            render_report(&crafted, &columns),
            fixture["report_crafted_grid"]["stdout"]
                .as_str()
                .expect("report_crafted")
        );
    }
}
