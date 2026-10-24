//! DIVERGENCE: D49. The bundle format fixtures (self-hosted plan §7.7 "Bundle
//! compatibility"; bead xq7v.12 item 4). `tests/data/<format>/` holds bundles signed with
//! the fixed test keys. Each one stays forever and must keep loading on every later
//! release: `verify` accepts it against the test trust set, and it decodes to exactly what
//! its `.expected.json` recorded when it was written. One fixture is signed only by the
//! standby key, which also loads. A release build trusts no test key, so it refuses them
//! all. The README in `tests/data/` has the rules.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, TimeZone, Utc};
use maverick_bundle::trust::{release_keys, test_keys};
use maverick_bundle::{
    Aggregate, Calibration, CatalogColumn, FORMAT, Manifest, PublicMatrix, SchemaRange, build,
    inspect, verify,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// What a fixture decoded to when it was written.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Expected {
    matrix_version: String,
    published_at: DateTime<Utc>,
    signing_kids: Vec<String>,
    matrix: PublicMatrix,
}

fn data_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data")
}

/// Every fixture archive, by format directory, sorted.
fn fixtures() -> Vec<PathBuf> {
    let mut found = Vec::new();
    for format in std::fs::read_dir(data_dir()).expect("tests/data") {
        let format = format.expect("entry").path();
        if !format.is_dir() {
            continue;
        }
        for file in std::fs::read_dir(&format).expect("format dir") {
            let file = file.expect("entry").path();
            if file.extension().is_some_and(|ext| ext == "tar") {
                found.push(file);
            }
        }
    }
    found.sort();
    found
}

fn every_test_key() -> BTreeMap<String, ed25519_dalek::VerifyingKey> {
    test_keys::trust(&[test_keys::CURRENT, test_keys::STANDBY, test_keys::RETIRING])
}

#[test]
fn every_format_fixture_still_loads_and_decodes_as_written() {
    let fixtures = fixtures();
    assert!(
        fixtures.len() >= 2,
        "the format fixtures are missing: {fixtures:?}"
    );
    for path in &fixtures {
        let name = path.display();
        let bytes = std::fs::read(path).expect("read fixture");
        let schema = inspect(&bytes)
            .unwrap_or_else(|e| panic!("{name}: {e}"))
            .manifest
            .schema
            .min;
        let bundle =
            verify(&bytes, &every_test_key(), schema).unwrap_or_else(|e| panic!("{name}: {e}"));
        let expected: Expected = serde_json::from_slice(
            &std::fs::read(path.with_extension("expected.json")).expect("the .expected.json"),
        )
        .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(
            Expected {
                matrix_version: bundle.manifest.matrix_version,
                published_at: bundle.manifest.published_at,
                signing_kids: bundle.signing_kids,
                matrix: bundle.matrix,
            },
            expected,
            "{name}"
        );
        // A release trusts no test key.
        assert!(verify(&bytes, &release_keys(), schema).is_err(), "{name}");
    }
}

/// Plan §7.7: a fixture signed only by the test standby key also loads.
#[test]
fn a_standby_only_fixture_is_present_and_loads() {
    let standby_only: Vec<_> = fixtures()
        .into_iter()
        .filter(|path| {
            let bytes = std::fs::read(path).expect("read fixture");
            let schema = inspect(&bytes).expect("inspect").manifest.schema.min;
            verify(&bytes, &every_test_key(), schema)
                .expect("verify")
                .signing_kids
                == [test_keys::STANDBY]
        })
        .collect();
    assert!(!standby_only.is_empty(), "no standby-only fixture");
    // It loads on a release that trusts only the standby key, as after a leak.
    for path in standby_only {
        let bytes = std::fs::read(&path).expect("read fixture");
        let schema = inspect(&bytes).expect("inspect").manifest.schema.min;
        verify(&bytes, &test_keys::trust(&[test_keys::STANDBY]), schema).expect("standby");
    }
}

fn column(id: &str, provider: &str, model: &str) -> CatalogColumn {
    CatalogColumn {
        id: id.to_owned(),
        provider: provider.to_owned(),
        model_id: model.to_owned(),
        endpoint: "direct".to_owned(),
        settings: json!({}),
        status: "active".to_owned(),
        pricing: json!({"input_per_mtok": 3.0, "output_per_mtok": 15.0, "currency": "USD",
                        "as_of": "2026-10-01", "source": "list"}),
        quality_source_column_id: None,
        cold_start_until: None,
        deprecation_date: None,
        execution_aliases: json!({}),
        retention_mode: "standard".to_owned(),
        retention_verified_at: None,
        retention_valid_until: None,
        retention_source: None,
        execution_alias_retention: json!({}),
        param_constraints: json!({}),
    }
}

/// The public matrix every mmbundle1 fixture carries: two columns, a few aggregate cells,
/// one calibration row, and one check per capability. Global data only.
fn fixture_matrix() -> PublicMatrix {
    let mut aggregates = Vec::new();
    for (column, ewma) in [("col_fixture_a", 0.91_f32), ("col_fixture_b", 0.74)] {
        for (dimension, value) in [
            ("pass_rate", ewma),
            ("quality", ewma),
            ("latency_p95_ms", 850.0),
        ] {
            aggregates.push(Aggregate {
                model_column_id: column.to_owned(),
                capability: "structured_extraction".to_owned(),
                difficulty_tier: 2,
                dimension: dimension.to_owned(),
                ewma: value,
                variance: 0.0,
                n: 20,
                measured: true,
                matrix_version: "fixture-v1".to_owned(),
                as_of: "2026-10-23".parse().expect("date"),
            });
        }
    }
    PublicMatrix {
        catalog: vec![
            column("col_fixture_a", "anthropic", "fixture-model-a"),
            column("col_fixture_b", "openai", "fixture-model-b"),
        ],
        aggregates,
        calibration: vec![Calibration {
            batch_id: "batch_fixture_1".to_owned(),
            capability: "structured_extraction".to_owned(),
            brier_score: 0.125,
            overconfidence: 0.0625,
            n: 40,
            scored_at: Utc.with_ymd_and_hms(2026, 10, 23, 4, 0, 0).unwrap(),
        }],
        checks: BTreeMap::from([(
            "structured_extraction".to_owned(),
            vec![json!({"type": "json_schema", "id": "fixture-check"})],
        )]),
    }
}

/// Writes one fixture and its `.expected.json`; refuses to replace a file.
fn write_fixture(dir: &Path, name: &str, published_at: DateTime<Utc>, signers: &[&str]) {
    let path = dir.join(format!("{name}.tar"));
    assert!(
        !path.exists(),
        "{} exists: a fixture is never edited",
        path.display()
    );
    let manifest = Manifest {
        format: FORMAT.to_owned(),
        matrix_version: "fixture-v1".to_owned(),
        published_at,
        origin_release: "0.1.0".to_owned(),
        schema: SchemaRange {
            min: 11,
            max: i64::MAX,
        },
        files: BTreeMap::new(),
    };
    let bytes = build(manifest, &fixture_matrix(), &test_keys::signing(signers)).expect("build");
    let bundle = verify(&bytes, &every_test_key(), 11).expect("the new fixture verifies");
    let expected = Expected {
        matrix_version: bundle.manifest.matrix_version,
        published_at: bundle.manifest.published_at,
        signing_kids: bundle.signing_kids,
        matrix: bundle.matrix,
    };
    std::fs::write(&path, &bytes).expect("write fixture");
    let mut json = serde_json::to_string_pretty(&expected).expect("expected JSON");
    json.push('\n');
    std::fs::write(path.with_extension("expected.json"), json).expect("write expected");
    eprintln!("wrote {}", path.display());
}

/// Writes this format's fixtures. Run it once when a format is added (README in
/// `tests/data/`); it never replaces an existing fixture.
#[test]
#[ignore = "writes fixture files; run once per new bundle format"]
fn write_the_format_fixtures() {
    let dir = data_dir().join(FORMAT);
    std::fs::create_dir_all(&dir).expect("format dir");
    write_fixture(
        &dir,
        "current-and-standby",
        Utc.with_ymd_and_hms(2026, 10, 24, 3, 0, 0).unwrap(),
        &[test_keys::CURRENT, test_keys::STANDBY],
    );
    write_fixture(
        &dir,
        "standby-only",
        Utc.with_ymd_and_hms(2026, 10, 24, 4, 0, 0).unwrap(),
        &[test_keys::STANDBY],
    );
}
