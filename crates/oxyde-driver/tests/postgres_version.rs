//! Run with disposable PostgreSQL 18+ and pre-18 servers; see Test CI.
use oxyde_driver::{close_pool, init_pool, init_pool_overwrite, pool_backend, PoolSettings};

fn settings() -> PoolSettings {
    PoolSettings {
        max_connections: Some(1),
        min_connections: Some(1),
        ..Default::default()
    }
}

#[tokio::test]
#[ignore = "requires OXYDE_TEST_POSTGRES_URL and OXYDE_TEST_OLD_POSTGRES_URL"]
async fn rejects_old_server_without_registering_pool() {
    let url = std::env::var("OXYDE_TEST_OLD_POSTGRES_URL").unwrap();
    let name = format!("old_{}", uuid::Uuid::new_v4().simple());
    let error = init_pool(&name, &url, settings()).await.unwrap_err();
    assert!(
        error.to_string().contains("PostgreSQL 18 or newer"),
        "{error}"
    );
    assert!(pool_backend(&name).await.is_err());
}

#[tokio::test]
#[ignore = "requires OXYDE_TEST_POSTGRES_URL and OXYDE_TEST_OLD_POSTGRES_URL"]
async fn rejected_overwrite_preserves_existing_pool() {
    let url = std::env::var("OXYDE_TEST_POSTGRES_URL").unwrap();
    let old_url = std::env::var("OXYDE_TEST_OLD_POSTGRES_URL").unwrap();
    let name = format!("overwrite_{}", uuid::Uuid::new_v4().simple());
    init_pool(&name, &url, settings()).await.unwrap();
    let error = init_pool_overwrite(&name, &old_url, settings())
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("PostgreSQL 18 or newer"),
        "{error}"
    );
    assert!(pool_backend(&name).await.is_ok());
    close_pool(&name).await.unwrap();
}
