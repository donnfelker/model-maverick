//! DIVERGENCE: D50. The upgrade matrix (spec §14.5 Tests; bead xq7v.17). Every dump in
//! `tests/data/upgrade-matrix/` is a release's database with a fixed populated scenario.
//! Each run restores it into an empty database, runs `maverick upgrade --yes`, and then
//! checks that the upgraded database still serves: the tenants' keys, the recommendation
//! history and its traces, the config, a declared recommend, and the internal tenant read.
//!
//! `<name>.replica.sql` dumps are a single-tenant replica (D49): the bootstrapped tenant
//! with a fixed app key. After `maverick upgrade --yes` on a replica, the key still serves,
//! `maverick key list` shows both keys, the tenant-admin routes still answer 409, and a
//! second bootstrap creates nothing.
//!
//! `write_the_dump_for_this_release` and `write_the_replica_dump_for_this_release`
//! (ignored) build the scenarios on this release and write their dumps. The release
//! pipeline runs them once per release; see the README in the data directory.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::Utc;
use maverick_api::{ApiState, AuthState, internal_router_with, provision, public_router_with};
use maverick_cli::version::VERSION;
use maverick_cli::{Output, run};
use maverick_domain::keys::{KEY_PREFIX, encode_base62, key_hash};
use maverick_store::catalog::{self, NewModelColumn};
use maverick_store::measurement::{self, AggregateUpsert};
use maverick_store::{JsonRawValue, corpus, tenancy};
use maverick_testdb::{Schema, TestDatabase};
use serde_json::{Value, json};
use sqlx::{Connection, PgConnection};
use tokio::net::TcpListener;

const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/upgrade-matrix");
const INTERNAL_TOKEN: &str = "ops:alpha-token";
const CORPUS: &str = "matrix-v1";
const COLUMNS: [(&str, &str, &str); 2] = [
    ("col_matrix_a", "anthropic", "matrix-model-a"),
    ("col_matrix_b", "openai", "matrix-model-b"),
];
const CAPABILITY: &str = "structured_extraction";

/// The scenario's tenants: id, name, and the byte their fixed key is made of.
const TENANTS: [(&str, &str, u8); 2] = [
    ("ten_matrix_one", "matrix-one", 1),
    ("ten_matrix_two", "matrix-two", 2),
];

/// The replica scenario: the bootstrapped tenant's name, and its fixed app key.
const REPLICA_TENANT: &str = "matrix-replica";
const REPLICA_KEY: (&str, u8) = ("matrix-app", 9);

/// A tenant's fixed API key: `mk_live_` and 43 base62 digits of 32 equal bytes.
fn key_of(byte: u8) -> String {
    format!("{KEY_PREFIX}{}", encode_base62(&[byte; 32]))
}

fn env(database_url: &str, role: &str) -> Vec<(String, String)> {
    [
        ("MAVERICK_ROLE", role),
        ("MAVERICK_DATABASE_URL", database_url),
        ("MAVERICK_INTERNAL_TOKENS", INTERNAL_TOKEN),
        ("MAVERICK_CAPTURE_MASTER_KEY", &"ab".repeat(32)),
        ("MAVERICK_RATELIMIT_RPM", "100000"),
        ("USER", "matrix"),
    ]
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .to_vec()
}

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

/// `maverick <args>` against `db` as `role`, on a blocking thread (the command runs its
/// own runtime). Returns the exit code, stdout, and stderr.
async fn cli(db: &TestDatabase, role: &str, args: &[&str]) -> (i32, String, String) {
    let env = env(db.url(), role);
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    tokio::task::spawn_blocking(move || {
        let out = Arc::new(Mutex::new(Vec::new()));
        let err = Arc::new(Mutex::new(Vec::new()));
        let code = run(
            &args,
            &|name| env.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()),
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

/// `maverick upgrade --yes --json` against `db` as `role`: the exit code and the report.
async fn upgrade(db: &TestDatabase, role: &str) -> (i32, Value) {
    let (code, stdout, stderr) = cli(db, role, &["upgrade", "--yes", "--json"]).await;
    let report =
        serde_json::from_str(stdout.trim()).unwrap_or_else(|e| panic!("{e}: {stdout}\n{stderr}"));
    (code, report)
}

/// The upgrade succeeded: upgraded or already current, and verified.
fn assert_upgraded(name: &str, code: i32, report: &Value) {
    assert_eq!(code, 0, "{name}: {report}");
    assert!(
        report["result"] == "upgraded" || report["result"] == "up_to_date",
        "{name}: {report}"
    );
    assert!(
        report["verify"].is_null() || report["verify"]["ok"] == true,
        "{name}: {report}"
    );
}

/// The engine's two listeners on `db`, served locally.
struct Engine {
    public: String,
    internal: String,
    http: reqwest::Client,
}

impl Engine {
    async fn start(db: &TestDatabase, role: &str) -> Engine {
        let env = env(db.url(), role);
        let config = maverick_config::load_with(|name| {
            env.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
        })
        .expect("config");
        let pool = db.pool().await;
        let auth = AuthState::from_config(pool.clone(), &config).expect("auth");
        let api = ApiState::new(pool, &config);
        Engine {
            public: serve(public_router_with(auth.clone(), api.clone())).await,
            internal: serve(internal_router_with(auth, api)).await,
            http: reqwest::Client::builder().no_proxy().build().expect("http"),
        }
    }

    async fn call(
        &self,
        base: &str,
        method: &str,
        path: &str,
        bearer: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut request = self
            .http
            .request(method.parse().expect("method"), format!("{base}{path}"))
            .bearer_auth(bearer);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.expect("request");
        let status = response.status().as_u16();
        let text = response.text().await.expect("body");
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }
}

async fn serve(router: axum::Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(listener, router).await.expect("serve") });
    format!("http://{addr}")
}

fn declared() -> Value {
    json!({"mode": "quality", "workload": {"capability": CAPABILITY, "difficulty": 3}})
}

/// The origin dumps (`<name>.sql`) or the replica dumps (`<name>.replica.sql`), sorted.
fn dumps(replica: bool) -> Vec<PathBuf> {
    let mut dumps: Vec<PathBuf> = std::fs::read_dir(DIR)
        .expect("the upgrade-matrix directory")
        .map(|entry| entry.expect("entry").path())
        .filter(|path| {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            name.ends_with(".sql") && name.ends_with(".replica.sql") == replica
        })
        .collect();
    dumps.sort();
    dumps
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_dump_upgrades_and_the_database_still_serves() {
    let dumps = dumps(false);
    assert!(!dumps.is_empty(), "no dump under {DIR}");
    for dump in dumps {
        let name = dump.file_name().unwrap().to_string_lossy().into_owned();
        let Some(db) = maverick_testdb::database(&Schema::empty()).await else {
            return;
        };
        db.restore(&std::fs::read_to_string(&dump).expect("read dump"))
            .await;

        let (code, report) = upgrade(&db, "origin").await;
        assert_upgraded(&name, code, &report);
        assert_still_serves(&db, &name).await;
        db.close().await;
    }
}

/// The upgraded database answers for the scenario's tenants with their old keys.
async fn assert_still_serves(db: &TestDatabase, name: &str) {
    let engine = Engine::start(db, "origin").await;
    for (tenant, tenant_name, byte) in TENANTS {
        let key = key_of(byte);
        let at = format!("{name} {tenant}");
        let (status, history) = engine
            .call(
                &engine.public,
                "GET",
                "/v1/history/recommendations",
                &key,
                None,
            )
            .await;
        assert_eq!(status, 200, "{at}: {history}");
        let items = history["recommendations"]
            .as_array()
            .unwrap_or_else(|| panic!("{at}: {history}"));
        assert!(
            !items.is_empty(),
            "{at}: the dump's recommendations are gone"
        );
        let id = items[0]["id"]
            .as_str()
            .unwrap_or_else(|| panic!("{at}: {history}"));
        let (status, trace) = engine
            .call(
                &engine.public,
                "GET",
                &format!("/v1/history/recommendations/{id}"),
                &key,
                None,
            )
            .await;
        assert_eq!(status, 200, "{at}: {trace}");
        assert_eq!(trace["id"], id, "{at}: {trace}");

        let (status, pools) = engine
            .call(&engine.public, "GET", "/v1/config/pools", &key, None)
            .await;
        assert_eq!(status, 200, "{at}: {pools}");
        let (status, recommended) = engine
            .call(
                &engine.public,
                "POST",
                "/v1/recommend",
                &key,
                Some(declared()),
            )
            .await;
        assert_eq!(status, 200, "{at}: {recommended}");
        assert!(
            !recommended["recommendations"]
                .as_array()
                .expect("chain")
                .is_empty(),
            "{at}: {recommended}"
        );

        let (status, summary) = engine
            .call(
                &engine.internal,
                "GET",
                &format!("/internal/v1/tenants/{tenant}"),
                INTERNAL_TOKEN,
                None,
            )
            .await;
        assert_eq!(status, 200, "{at}: {summary}");
        assert_eq!(summary["tenant"]["name"], tenant_name, "{at}: {summary}");
    }
    // The first tenant's named pool survived.
    let (_, pools) = engine
        .call(
            &engine.public,
            "GET",
            "/v1/config/pools",
            &key_of(TENANTS[0].2),
            None,
        )
        .await;
    assert!(pools.to_string().contains("\"team\""), "{name}: {pools}");
}

fn raw(text: &str) -> Box<JsonRawValue> {
    JsonRawValue::from_string(text.to_owned()).expect("json")
}

/// The fixed scenario on a database this release just upgraded.
async fn seed(db: &TestDatabase) {
    let pool = db.pool().await;
    corpus::upsert_corpus_version(&pool, CORPUS, "", 0, false)
        .await
        .expect("corpus");
    corpus::activate_corpus_version(&pool, CORPUS)
        .await
        .expect("activate");
    seed_catalog(&pool).await;
    seed_tenants(db, &pool).await;
}

/// Two active columns with measured aggregates.
async fn seed_catalog(pool: &sqlx::PgPool) {
    let empty = raw("{}");
    let pricing = raw(r#"{"input_per_mtok": 1.0, "output_per_mtok": 2.0}"#);
    for (id, provider, model) in COLUMNS {
        catalog::create_model_column(
            pool,
            &NewModelColumn {
                id,
                provider,
                model_id: model,
                endpoint: "direct",
                settings: &empty,
                status: "active",
                pricing: &pricing,
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
        for tier in 1..=5 {
            for (dimension, ewma) in [
                ("pass_rate", 0.9),
                ("quality", 0.9),
                ("cost_per_run_usd", 0.01),
                ("latency_p95_ms", 800.0),
            ] {
                measurement::upsert_aggregate(
                    pool,
                    &AggregateUpsert {
                        model_column_id: id,
                        capability: CAPABILITY,
                        difficulty_tier: tier,
                        dimension,
                        ewma,
                        variance: 0.0,
                        n: 20,
                        measured: true,
                        matrix_version: CORPUS,
                        as_of: Utc::now().date_naive(),
                    },
                )
                .await
                .expect("aggregate");
            }
        }
    }
}

/// The origin's tenants, keys, pools, constraints, and recommendation traces.
async fn seed_tenants(db: &TestDatabase, pool: &sqlx::PgPool) {
    let pool = pool.clone();
    let all: Vec<String> = COLUMNS.iter().map(|(id, _, _)| (*id).to_owned()).collect();
    for (tenant, name, byte) in TENANTS {
        tenancy::create_tenant(&pool, tenant, name, &[byte; 32])
            .await
            .expect("tenant");
        let key = key_of(byte);
        tenancy::create_api_key(
            &pool,
            &format!("key_{name}"),
            tenant,
            &key_hash(&key),
            &key[key.len() - 4..],
            "matrix",
        )
        .await
        .expect("key");
        tenancy::create_pool(
            &pool,
            &format!("pool_{name}"),
            tenant,
            "default",
            &all,
            true,
        )
        .await
        .expect("pool");
    }
    tenancy::create_pool(
        &pool,
        "pool_matrix_one_team",
        TENANTS[0].0,
        "team",
        &all[..1],
        false,
    )
    .await
    .expect("team pool");
    tenancy::upsert_tenant_constraints(&pool, TENANTS[1].0, None, Some(5000), false, false, false)
        .await
        .expect("constraints");

    // Recommendation traces, written by the real handler.
    let engine = Engine::start(db, "origin").await;
    for (_, _, byte) in TENANTS {
        for _ in 0..2 {
            let (status, body) = engine
                .call(
                    &engine.public,
                    "POST",
                    "/v1/recommend",
                    &key_of(byte),
                    Some(declared()),
                )
                .await;
            assert_eq!(status, 200, "{body}");
        }
    }
}

/// Builds the scenario on this release and writes its dump to the matrix directory, as
/// `MAVERICK_MATRIX_DUMP_NAME` (default: the release version) plus `.sql`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "run once per release; it writes a fixture file"]
async fn write_the_dump_for_this_release() {
    let db = maverick_testdb::database(&Schema::empty())
        .await
        .expect("a test database");
    let (code, report) = upgrade(&db, "origin").await;
    assert_eq!(code, 0, "{report}");
    seed(&db).await;
    assert_still_serves(&db, "fresh").await;
    let name = std::env::var("MAVERICK_MATRIX_DUMP_NAME").unwrap_or_else(|_| VERSION.to_owned());
    let path = Path::new(DIR).join(format!("{name}.sql"));
    std::fs::write(&path, db.dump().await).expect("write dump");
    eprintln!("wrote {}", path.display());
    db.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_replica_dump_upgrades_and_the_install_still_serves() {
    let dumps = dumps(true);
    assert!(!dumps.is_empty(), "no replica dump under {DIR}");
    for dump in dumps {
        let name = dump.file_name().unwrap().to_string_lossy().into_owned();
        let Some(db) = maverick_testdb::database(&Schema::empty()).await else {
            return;
        };
        db.restore(&std::fs::read_to_string(&dump).expect("read dump"))
            .await;
        let (code, report) = upgrade(&db, "replica").await;
        assert_upgraded(&name, code, &report);
        assert_replica_still_serves(&db, &name).await;
        db.close().await;
    }
}

/// The upgraded replica keeps its one tenant, its keys, and its single-tenant rules.
async fn assert_replica_still_serves(db: &TestDatabase, name: &str) {
    let pool = db.pool().await;
    let tenants = tenancy::list_tenants(&pool).await.expect("tenants");
    assert_eq!(tenants.len(), 1, "{name}: {tenants:?}");
    assert_eq!(tenants[0].name, REPLICA_TENANT, "{name}");
    let tenant = tenants[0].id.clone();

    let engine = Engine::start(db, "replica").await;
    let key = key_of(REPLICA_KEY.1);
    let (status, history) = engine
        .call(
            &engine.public,
            "GET",
            "/v1/history/recommendations",
            &key,
            None,
        )
        .await;
    assert_eq!(status, 200, "{name}: {history}");
    assert!(
        !history["recommendations"]
            .as_array()
            .expect("history")
            .is_empty(),
        "{name}: the dump's recommendation is gone"
    );
    let (status, recommended) = engine
        .call(
            &engine.public,
            "POST",
            "/v1/recommend",
            &key,
            Some(declared()),
        )
        .await;
    assert_eq!(status, 200, "{name}: {recommended}");
    assert!(
        !recommended["recommendations"]
            .as_array()
            .expect("chain")
            .is_empty(),
        "{name}: {recommended}"
    );

    // Both keys survive: the bootstrap's own and the fixed app key.
    let (code, stdout, stderr) = cli(db, "replica", &["key", "list"]).await;
    assert_eq!(code, 0, "{name}: {stderr}");
    let listed: Value = serde_json::from_str(stdout.trim()).expect("key list JSON");
    let mut names: Vec<&str> = listed
        .as_array()
        .expect("keys")
        .iter()
        .map(|key| key["name"].as_str().expect("name"))
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [provision::BOOTSTRAP_KEY_NAME, REPLICA_KEY.0],
        "{name}: {stdout}"
    );

    // Tenant administration is still refused.
    for (method, path, body) in [
        (
            "POST",
            "/internal/v1/tenants".to_owned(),
            Some(json!({"name": "second"})),
        ),
        (
            "POST",
            format!("/internal/v1/tenants/{tenant}/suspend"),
            None,
        ),
        (
            "POST",
            format!("/internal/v1/tenants/{tenant}/activate"),
            None,
        ),
        (
            "PUT",
            format!("/internal/v1/tenants/{tenant}/limits"),
            Some(json!({})),
        ),
    ] {
        let (status, body) = engine
            .call(&engine.internal, method, &path, INTERNAL_TOKEN, body)
            .await;
        assert_eq!(status, 409, "{name} {method} {path}: {body}");
        assert_eq!(
            body["error"]["code"], "single_tenant_install",
            "{name} {path}"
        );
    }

    // A later start bootstraps nothing.
    let mut conn = PgConnection::connect(db.url()).await.expect("connect");
    let again = provision::bootstrap(&mut conn, "again")
        .await
        .expect("bootstrap");
    assert!(
        again.is_none(),
        "{name}: a second bootstrap created {again:?}"
    );
    conn.close().await.expect("close");
    assert_eq!(
        tenancy::list_tenants(&pool).await.expect("tenants").len(),
        1
    );
}

/// The replica scenario on a database this release just upgraded: the catalog, the first
/// start's bootstrap, a fixed app key, and one recommendation trace.
async fn seed_replica(db: &TestDatabase) {
    let pool = db.pool().await;
    seed_catalog(&pool).await;
    let mut conn = PgConnection::connect(db.url()).await.expect("connect");
    let booted = provision::bootstrap(&mut conn, REPLICA_TENANT)
        .await
        .expect("bootstrap")
        .expect("an empty database bootstraps");
    conn.close().await.expect("close");
    let key = key_of(REPLICA_KEY.1);
    tenancy::create_api_key(
        &pool,
        "key_matrix_replica_app",
        &booted.tenant.id,
        &key_hash(&key),
        &key[key.len() - 4..],
        REPLICA_KEY.0,
    )
    .await
    .expect("app key");
    let engine = Engine::start(db, "replica").await;
    let (status, body) = engine
        .call(
            &engine.public,
            "POST",
            "/v1/recommend",
            &key,
            Some(declared()),
        )
        .await;
    assert_eq!(status, 200, "{body}");
}

/// Builds the replica scenario on this release and writes its dump, as
/// `MAVERICK_MATRIX_DUMP_NAME` (default: the release version) plus `.replica.sql`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "run once per release; it writes a fixture file"]
async fn write_the_replica_dump_for_this_release() {
    let db = maverick_testdb::database(&Schema::empty())
        .await
        .expect("a test database");
    let (code, report) = upgrade(&db, "replica").await;
    assert_eq!(code, 0, "{report}");
    seed_replica(&db).await;
    assert_replica_still_serves(&db, "fresh").await;
    let name = std::env::var("MAVERICK_MATRIX_DUMP_NAME").unwrap_or_else(|_| VERSION.to_owned());
    let path = Path::new(DIR).join(format!("{name}.replica.sql"));
    std::fs::write(&path, db.dump().await).expect("write dump");
    eprintln!("wrote {}", path.display());
    db.close().await;
}
