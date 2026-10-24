//! DIVERGENCE: D49. Export holds a coherent snapshot and serializes publishers.
use maverick_store::{MIGRATOR, matrix};
use maverick_testdb::Schema;
use std::sync::LazyLock;
static SCHEMA: LazyLock<Schema> = LazyLock::new(|| Schema::sqlx(&MIGRATOR));

#[tokio::test]
async fn export_snapshot_is_consistent_and_the_next_publisher_waits() {
    let Some(db) = maverick_testdb::database(&SCHEMA).await else {
        return;
    };
    let pool = db.pool().await;
    sqlx::query("insert into model_columns (id,provider,model_id,endpoint) values ('public','openai','before','direct')").execute(&pool).await.unwrap();
    let night = "2026-10-24".parse().unwrap();
    let (mut tx, first) = matrix::export_snapshot(&pool, "v1", night).await.unwrap();
    assert_eq!(first.catalog[0].model_id, "before");
    sqlx::query("update model_columns set model_id='after' where id='public'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        matrix::public_matrix(&mut tx, "v1", night)
            .await
            .unwrap()
            .catalog[0]
            .model_id,
        "before"
    );
    let other = pool.clone();
    let (started, ready) = tokio::sync::oneshot::channel();
    let mut waiter = tokio::spawn(async move {
        started.send(()).unwrap();
        matrix::export_snapshot(&other, "v1", night).await.unwrap()
    });
    ready.await.unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut waiter)
            .await
            .is_err()
    );
    drop(tx);
    let (second_tx, second) = tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.catalog[0].model_id, "after");
    drop(second_tx);
    pool.close().await;
    db.close().await;
}
