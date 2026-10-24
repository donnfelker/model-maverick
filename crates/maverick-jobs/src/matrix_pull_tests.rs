//! DIVERGENCE: D49. Postgres and a loopback-only static mirror.
use super::*;
use axum::{
    Router,
    body::Body,
    extract::{OriginalUri, State},
    http::{Response, StatusCode},
};
use chrono::{TimeZone, Utc};
use maverick_bundle::{Manifest, PublicMatrix, SchemaRange, trust::test_keys};
use maverick_store::MIGRATOR;
use maverick_testdb::Schema;
use std::{
    collections::BTreeMap,
    sync::{Arc, LazyLock, Mutex},
};

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

#[test]
fn cdn_redirects_are_limited_to_three_https_targets_without_credentials() {
    for (url, hops, allowed) in [
        ("https://cdn.example/archive?public-token=1", 1, true),
        ("https://cdn.example/archive", 3, true),
        ("https://cdn.example/archive", 4, false),
        ("http://cdn.example/archive", 1, false),
        ("https://user:secret@cdn.example/archive", 1, false),
        ("ftp://cdn.example/archive", 1, false),
    ] {
        assert_eq!(
            safe_redirect(&reqwest::Url::parse(url).unwrap(), hops),
            allowed
        );
    }
}
#[derive(Default)]
struct Mirror {
    files: BTreeMap<String, Vec<u8>>,
    requests: Vec<String>,
}
struct Server {
    root: String,
    state: Arc<Mutex<Mirror>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    async fn new() -> Self {
        let state = Arc::new(Mutex::new(Mirror::default()));
        let app = Router::new()
            .fallback(
                async |State(state): State<Arc<Mutex<Mirror>>>, OriginalUri(uri): OriginalUri| {
                    let mut state = state.lock().unwrap();
                    state.requests.push(uri.path().to_owned());
                    match state.files.get(uri.path()) {
                        Some(bytes) => Response::builder()
                            .status(StatusCode::OK)
                            .body(Body::from(bytes.clone()))
                            .unwrap(),
                        None => Response::builder()
                            .status(StatusCode::NOT_FOUND)
                            .body(Body::empty())
                            .unwrap(),
                    }
                },
            )
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = format!("http://{}/bundles", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { root, state, task }
    }
    fn index(&self, entries: &[IndexEntry]) {
        self.state.lock().unwrap().files.insert(
            "/bundles/index.json".into(),
            serde_json::to_vec(entries).unwrap(),
        );
    }
    fn add(&self, day: u32, min: i64, kid: &str) -> IndexEntry {
        let published_at = Utc.with_ymd_and_hms(2026, 10, day, 3, 0, 0).unwrap();
        let bytes = maverick_bundle::build(
            Manifest {
                format: FORMAT.into(),
                matrix_version: format!("v{day}"),
                published_at,
                origin_release: "test".into(),
                schema: SchemaRange { min, max: i64::MAX },
                files: BTreeMap::new(),
            },
            &PublicMatrix::default(),
            &test_keys::signing(&[kid]),
        )
        .unwrap();
        let entry = IndexEntry {
            matrix_version: format!("v{day}"),
            published_at,
            format: FORMAT.into(),
            file: maverick_bundle::filename(published_at),
            size: bytes.len() as u64,
            sha256: maverick_bundle::sha256(&bytes),
        };
        self.state
            .lock()
            .unwrap()
            .files
            .insert(format!("/bundles/{}", entry.file), bytes);
        entry
    }
    async fn pull(&self, pool: &PgPool) -> Result<ImportOutcome, PullError> {
        pull_verified(pool, &MatrixPullConfig::new(&self.root), |bytes, schema| {
            maverick_bundle::verify(bytes, &test_keys::trust(&[test_keys::CURRENT]), schema)
        })
        .await
    }
}

#[tokio::test]
async fn newest_readable_bundle_is_authenticated_and_reimport_is_a_noop() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let mirror = Server::new().await;
    let old = mirror.add(21, 11, test_keys::CURRENT);
    let new = mirror.add(23, 11, test_keys::CURRENT);
    let mut future = new.clone();
    future.format = "mmbundle2".into();
    future.published_at += chrono::Duration::days(1);
    mirror.index(&[old, future, new.clone()]);
    assert_eq!(mirror.pull(&pool).await.unwrap(), ImportOutcome::Imported);
    let active = maverick_store::matrix::active_bundle(&pool)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(active.matrix_version, "v23");
    assert_eq!(active.published_at, new.published_at);
    assert_eq!(
        mirror.pull(&pool).await.unwrap(),
        ImportOutcome::AlreadyActive
    );
    assert_eq!(
        maverick_store::matrix::active_bundle(&pool)
            .await
            .unwrap()
            .unwrap(),
        active
    );
    assert_eq!(
        mirror.state.lock().unwrap().requests,
        vec![
            "/bundles/index.json".to_owned(),
            format!("/bundles/{}", new.file),
            "/bundles/index.json".into()
        ]
    );
    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn an_edited_or_stale_index_never_rolls_back_the_replica() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let mirror = Server::new().await;
    let old = mirror.add(21, 11, test_keys::CURRENT);
    let new = mirror.add(23, 11, test_keys::CURRENT);
    mirror.index(std::slice::from_ref(&new));
    mirror.pull(&pool).await.unwrap();
    mirror.index(std::slice::from_ref(&old));
    assert!(matches!(
        mirror.pull(&pool).await,
        Err(PullError::Import(ImportError::Older))
    ));
    let mut forged = old.clone();
    forged.published_at += chrono::Duration::days(5);
    forged.file = maverick_bundle::filename(forged.published_at);
    let bytes = mirror.state.lock().unwrap().files[&format!("/bundles/{}", old.file)].clone();
    mirror
        .state
        .lock()
        .unwrap()
        .files
        .insert(format!("/bundles/{}", forged.file), bytes);
    mirror.index(&[forged]);
    assert!(matches!(mirror.pull(&pool).await, Err(PullError::Mismatch)));
    assert_eq!(
        maverick_store::matrix::active_bundle(&pool)
            .await
            .unwrap()
            .unwrap()
            .published_at,
        new.published_at
    );
    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn hash_signature_schema_identity_and_size_refusals_preserve_the_database() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let mirror = Server::new().await;
    let new = mirror.add(23, 11, test_keys::CURRENT);
    mirror.index(&[new]);
    mirror.pull(&pool).await.unwrap();
    let active = maverick_store::matrix::active_bundle(&pool).await.unwrap();
    let candidate = mirror.add(24, 11, test_keys::CURRENT);
    let mut bad = candidate.clone();
    bad.sha256 = "0".repeat(64);
    mirror.index(&[bad]);
    assert!(matches!(mirror.pull(&pool).await, Err(PullError::Mismatch)));
    let bad = mirror.add(25, 11, test_keys::UNKNOWN);
    mirror.index(&[bad]);
    assert!(matches!(
        mirror.pull(&pool).await,
        Err(PullError::Bundle(_))
    ));
    let bad = mirror.add(26, 13, test_keys::CURRENT);
    mirror.index(&[bad]);
    assert!(matches!(
        mirror.pull(&pool).await,
        Err(PullError::Bundle(_))
    ));
    let mut bad = candidate.clone();
    bad.matrix_version = "forged-version".into();
    mirror.index(&[bad]);
    assert!(matches!(mirror.pull(&pool).await, Err(PullError::Mismatch)));
    let mut bad = candidate.clone();
    bad.size += 1;
    mirror.index(&[bad]);
    assert!(matches!(mirror.pull(&pool).await, Err(PullError::Mismatch)));
    let mut bad = candidate.clone();
    bad.size = MAX_BUNDLE_BYTES as u64 + 1;
    mirror.index(&[bad]);
    assert!(matches!(mirror.pull(&pool).await, Err(PullError::Size)));
    let mut bad = candidate.clone();
    bad.file = "../outside.tar".into();
    mirror.index(&[bad]);
    assert!(matches!(mirror.pull(&pool).await, Err(PullError::Mismatch)));
    let mut bad = candidate;
    bad.format = "future".into();
    mirror.index(&[bad]);
    assert!(matches!(mirror.pull(&pool).await, Err(PullError::Format)));
    assert_eq!(
        maverick_store::matrix::active_bundle(&pool).await.unwrap(),
        active
    );
    let count: i64 = sqlx::query_scalar("select count(*) from matrix_imports")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn production_trust_rejects_test_signatures_even_with_the_test_feature_compiled() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    let mirror = Server::new().await;
    let entry = mirror.add(23, 11, test_keys::CURRENT);
    mirror.index(&[entry]);
    assert!(matches!(
        matrix_pull(&pool, &MatrixPullConfig::new(&mirror.root)).await,
        Err(PullError::Bundle(_))
    ));
    assert!(
        maverick_store::matrix::active_bundle(&pool)
            .await
            .unwrap()
            .is_none()
    );
    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn transport_and_json_errors_are_bounded_and_never_echo_the_remote_body() {
    let mirror = Server::new().await;
    let client = reqwest::Client::new();
    let base = location(&mirror.root).unwrap();
    assert!(matches!(
        download(&client, base.join("missing").unwrap(), 10).await,
        Err(PullError::Http)
    ));
    mirror
        .state
        .lock()
        .unwrap()
        .files
        .insert("/bundles/large".into(), vec![b'x'; 100]);
    assert!(matches!(
        download(&client, base.join("large").unwrap(), 10).await,
        Err(PullError::Size)
    ));
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    mirror
        .state
        .lock()
        .unwrap()
        .files
        .insert("/bundles/index.json".into(), b"remote-body-canary".to_vec());
    let error = mirror.pull(&pool).await.unwrap_err();
    assert!(matches!(error, PullError::Index));
    assert!(!format!("{error:?} {error}").contains("canary"));
    for url in [
        "file:///tmp/index.json",
        "https://user:secret@host/path",
        "https://host/path?secret=1",
        "https://host/path#secret",
        "not a URL",
    ] {
        assert!(location(url).is_err());
        assert!(!format!("{:?}", MatrixPullConfig::new(url)).contains("secret"));
    }
    pool.close().await;
    db.close().await;
}
