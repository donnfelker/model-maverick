//! DIVERGENCE: D49. CLI import uses runtime files and fails closed without release keys.
use super::*;
use chrono::TimeZone;
use maverick_bundle::{Manifest, PublicMatrix, SchemaRange, trust::test_keys};
use maverick_testdb::Schema;
use std::{
    collections::BTreeMap,
    io,
    sync::{Arc, LazyLock, Mutex},
};
static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&maverick_store::MIGRATOR));
#[derive(Clone)]
struct Shared(Arc<Mutex<Vec<u8>>>);
impl Write for Shared {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
struct File(std::path::PathBuf);
impl Drop for File {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
fn fixture(day: u32, min: i64) -> File {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "mm-import-{}-{}.tar",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let bytes = maverick_bundle::build(
        Manifest {
            format: maverick_bundle::FORMAT.into(),
            matrix_version: format!("v{day}"),
            published_at: Utc.with_ymd_and_hms(2026, 10, day, 3, 0, 0).unwrap(),
            origin_release: "test".into(),
            schema: SchemaRange { min, max: i64::MAX },
            files: BTreeMap::new(),
        },
        &PublicMatrix::default(),
        &test_keys::signing(&[test_keys::CURRENT]),
    )
    .unwrap();
    std::fs::write(&path, bytes).unwrap();
    File(path)
}
fn invoke(
    url: &str,
    role: &str,
    path: &str,
    older: bool,
    test_trust: bool,
) -> (i32, String, String) {
    let env = [
        ("MAVERICK_ROLE", role.to_owned()),
        ("MAVERICK_DATABASE_URL", url.to_owned()),
        ("MAVERICK_INTERNAL_TOKENS", "ops:s".into()),
        ("MAVERICK_CAPTURE_MASTER_KEY", "ab".repeat(32)),
    ];
    let lookup = |name: &str| env.iter().find(|(k, _)| *k == name).map(|(_, v)| v.clone());
    let out = Arc::new(Mutex::new(Vec::new()));
    let err = Arc::new(Mutex::new(Vec::new()));
    let code = import_with_verifier(
        &lookup,
        path,
        older,
        Output::new(Shared(out.clone())),
        Output::new(Shared(err.clone())),
        |bytes, schema| {
            let trust = if test_trust {
                test_keys::trust(&[test_keys::CURRENT])
            } else {
                maverick_bundle::trust::release_keys()
            };
            maverick_bundle::verify(bytes, &trust, schema)
        },
    );
    let read = |b: Arc<Mutex<Vec<u8>>>| String::from_utf8(b.lock().unwrap().clone()).unwrap();
    (code, read(out), read(err))
}
async fn run(
    url: &str,
    role: &str,
    file: &File,
    older: bool,
    test_trust: bool,
) -> (i32, String, String) {
    let url = url.to_owned();
    let role = role.to_owned();
    let path = file.0.to_str().unwrap().to_owned();
    tokio::task::spawn_blocking(move || invoke(&url, &role, &path, older, test_trust))
        .await
        .unwrap()
}
#[test]
fn compiled_authentication_uses_test_keys_only_for_an_explicit_debug_harness() {
    let file = fixture(23, 11);
    let bytes = std::fs::read(&file.0).unwrap();
    assert_eq!(
        authenticate(&bytes, 12).is_ok(),
        cfg!(all(feature = "test-keys", debug_assertions))
    );
    assert!(maverick_bundle::verify(&bytes, &maverick_bundle::trust::release_keys(), 12).is_err());
}

#[tokio::test]
async fn cli_installs_reimports_refuses_older_and_allows_explicit_rollback() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let old = fixture(23, 11);
    let new = fixture(24, 11);
    let (code, out, err) = run(db.url(), "replica", &old, false, true).await;
    assert_eq!(
        (code, out.as_str()),
        (0, "matrix bundle imported\n"),
        "{err}"
    );
    assert_eq!(
        run(db.url(), "replica", &old, false, true).await.1,
        "matrix bundle already active\n"
    );
    assert_eq!(run(db.url(), "replica", &new, false, true).await.0, 0);
    let (code, out, err) = run(db.url(), "replica", &old, false, true).await;
    assert_eq!(code, 1);
    assert!(out.is_empty());
    assert!(err.contains("older than the active"));
    assert_eq!(run(db.url(), "replica", &old, true, true).await.0, 0);
    db.close().await;
}
#[tokio::test]
async fn cli_refuses_origin_unknown_signature_bad_schema_and_missing_file() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let valid = fixture(23, 11);
    let incompatible = fixture(24, 13);
    assert!(
        run(db.url(), "origin", &valid, false, true)
            .await
            .2
            .contains("requires MAVERICK_ROLE=replica")
    );
    let (code, out, err) = run(db.url(), "replica", &valid, false, false).await;
    assert_eq!(code, 1);
    assert!(out.is_empty());
    assert!(err.contains("no trusted signature"));
    assert_eq!(
        run(db.url(), "replica", &incompatible, false, true).await.0,
        1
    );
    std::fs::remove_file(&valid.0).unwrap();
    assert_eq!(run(db.url(), "replica", &valid, false, true).await.0, 1);
    let pool = db.pool().await;
    assert!(active_bundle(&pool).await.unwrap().is_none());
    pool.close().await;
    db.close().await;
}
