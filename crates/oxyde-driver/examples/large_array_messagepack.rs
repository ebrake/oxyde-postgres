//! Measure warm PostgreSQL fetch and MessagePack encoding in a release build.
//!
//! Set `OXYDE_TEST_POSTGRES_URL` and run
//! `cargo run --release -p oxyde-driver --example large_array_messagepack`.

use std::hint::black_box;
use std::time::Instant;

use oxyde_driver::{close_pool, execute_query_columnar, init_pool, PoolSettings};

const POOL: &str = "large_array_messagepack";
const BIGINTS: &str = "WITH input AS MATERIALIZED (
    SELECT array_agg(n::bigint ORDER BY n) AS items FROM generate_series(1, 1000) AS n
) SELECT items FROM input CROSS JOIN generate_series(1, 1000)";
const FLOATS: &str = "WITH input AS MATERIALIZED (
    SELECT array_agg(n::float8 / 8 ORDER BY n) AS items FROM generate_series(1, 1000) AS n
) SELECT items FROM input CROSS JOIN generate_series(1, 1000)";

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let url = std::env::var("OXYDE_TEST_POSTGRES_URL").expect("set OXYDE_TEST_POSTGRES_URL");
    let settings = PoolSettings {
        max_connections: Some(1),
        ..PoolSettings::default()
    };
    init_pool(POOL, &url, settings).await.unwrap();

    for (label, sql) in [("BIGINT[]", BIGINTS), ("FLOAT8[]", FLOATS)] {
        execute_query_columnar(POOL, sql, &[], None, true)
            .await
            .unwrap();
        for _ in 0..3 {
            let start = Instant::now();
            let (bytes, rows) = execute_query_columnar(POOL, sql, &[], None, true)
                .await
                .unwrap();
            let micros = start.elapsed().as_micros();
            assert_eq!(rows, 1000);
            assert!(bytes.len() > 1_000_000, "{label} payload is too small");
            println!("{label} {micros} {}", black_box(bytes).len());
        }
    }

    close_pool(POOL).await.unwrap();
}
