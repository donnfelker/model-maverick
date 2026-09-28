//! Self-test of the helper against a real server: Docker, or `MAVERICK_TEST_DATABASE_URL`.

use std::sync::LazyLock;

use maverick_testdb::{Schema, TestDatabase};
use sqlx::migrate::Migrator;
use sqlx::{Connection, PgConnection};

static MIGRATOR: Migrator = sqlx::migrate!("tests/migrations");
static WIDGETS: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

/// Connects to the server-level database of `db`, where `pg_database` can be read after
/// `db` itself is gone.
async fn database_exists(url: &str, name: &str) -> bool {
    let mut conn = PgConnection::connect(url).await.expect("connect");
    let exists = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)")
        .bind(name)
        .fetch_one(&mut conn)
        .await
        .expect("query pg_database");
    conn.close().await.expect("close");
    exists
}

async fn widget_count(conn: &mut PgConnection) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM widgets")
        .fetch_one(conn)
        .await
        .expect("count widgets")
}

/// A URL for the `postgres` maintenance database on the same server as `db`.
fn maintenance_url(db: &TestDatabase) -> String {
    let (base, query) = db.url().split_once('?').unwrap_or((db.url(), ""));
    let server = base.rsplit_once('/').expect("url has a path").0;
    if query.is_empty() {
        format!("{server}/postgres")
    } else {
        format!("{server}/postgres?{query}")
    }
}

#[tokio::test]
async fn a_test_database_answers_queries() {
    let Some(db) = maverick_testdb::database(&Schema::empty()).await else {
        return;
    };
    assert!(db.name().starts_with("mvt_"));
    let pool = db.pool().await;
    let (current, sum): (String, i32) = sqlx::query_as("SELECT current_database()::text, 1 + 1")
        .fetch_one(&pool)
        .await
        .expect("trivial query");
    assert_eq!(current, db.name());
    assert_eq!(sum, 2);
    pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn the_schema_is_applied_and_each_test_has_its_own_rows() {
    let Some(first) = maverick_testdb::database(&WIDGETS).await else {
        return;
    };
    let second = maverick_testdb::database(&WIDGETS)
        .await
        .expect("server already running");
    assert_ne!(first.name(), second.name());

    let mut a = first.connect().await;
    let mut b = second.connect().await;
    sqlx::query("INSERT INTO widgets (id, weight) VALUES (2, 2.5)")
        .execute(&mut a)
        .await
        .expect("insert");
    assert_eq!(widget_count(&mut a).await, 2);
    assert_eq!(widget_count(&mut b).await, 1);

    // The migration history comes along with the template.
    let applied: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations")
        .fetch_one(&mut b)
        .await
        .expect("sqlx history");
    assert_eq!(applied, 1);

    // Real Postgres: `real` is float32, and Go widened it to float64 on read.
    let weight: f64 = sqlx::query_scalar("SELECT weight::float8 FROM widgets WHERE id = 1")
        .fetch_one(&mut b)
        .await
        .expect("weight");
    assert_eq!(weight, f64::from(0.1_f32));

    a.close().await.expect("close a");
    b.close().await.expect("close b");
    first.close().await;
    second.close().await;
}

#[tokio::test]
async fn close_drops_the_database_even_with_open_connections() {
    let Some(db) = maverick_testdb::database(&Schema::empty()).await else {
        return;
    };
    let maintenance = maintenance_url(&db);
    let name = db.name().to_owned();
    let _pool = db.pool().await;
    assert!(database_exists(&maintenance, &name).await);
    db.close().await;
    assert!(!database_exists(&maintenance, &name).await);
}

#[tokio::test]
async fn dropping_the_handle_drops_the_database() {
    let Some(db) = maverick_testdb::database(&Schema::empty()).await else {
        return;
    };
    let maintenance = maintenance_url(&db);
    let name = db.name().to_owned();
    drop(db);
    assert!(!database_exists(&maintenance, &name).await);
}

#[tokio::test]
async fn a_worker_can_reconnect_through_the_url() {
    let Some(db) = maverick_testdb::database(&Schema::sql("CREATE TABLE jobs (id int)")).await
    else {
        return;
    };
    let pool = db.pool().await;
    sqlx::query("INSERT INTO jobs VALUES (7)")
        .execute(&pool)
        .await
        .expect("insert");
    pool.close().await;

    let mut restarted = PgConnection::connect(db.url()).await.expect("reconnect");
    let id: i32 = sqlx::query_scalar("SELECT id FROM jobs")
        .fetch_one(&mut restarted)
        .await
        .expect("read after reconnect");
    assert_eq!(id, 7);
    restarted.close().await.expect("close");
    db.close().await;
}
