//! Query and statement execution (pool and transaction paths).

use oxyde_codec::ColumnTypeSpec;
use sea_query::Value;
use std::collections::HashMap;
use std::time::Instant;
use tracing::debug;

use crate::bind::bind_postgres;
use crate::convert::encoder::{encode_stream_mutation_returning, RelationInfo, ResultOptions};
use crate::convert::postgres::PgEncoder;
use crate::error::{DriverError, Result};
use crate::execute::traits::{ConnExec, PoolExec};
use crate::pool::DbPool;
use crate::transaction::DbTx;
use crate::version::check_connection_version;
use crate::{registry, transaction_registry};

/// Check if profiling is enabled via OXYDE_PROFILE env var
fn is_profiling_enabled() -> bool {
    std::env::var("OXYDE_PROFILE").is_ok_and(|v| v == "1")
}

/// Execute a SELECT query and return pre-encoded msgpack bytes + row count.
pub async fn execute_query_columnar<'a>(
    pool_name: &str,
    sql: &str,
    params: &[Value],
    col_types: Option<&HashMap<String, ColumnTypeSpec>>,
    options: impl Into<ResultOptions<'a>>,
) -> Result<(Vec<u8>, usize)> {
    debug!(
        "Executing columnar query on '{}': {} ({} params)",
        pool_name,
        sql,
        params.len()
    );

    let profile = is_profiling_enabled();
    let handle = registry().get(pool_name).await?;
    let pool = handle.clone_pool();

    let start = Instant::now();
    let results = pool
        .query_columnar(sql, params, col_types, options.into())
        .await?;
    let elapsed_us = start.elapsed().as_micros();

    if profile {
        eprintln!(
            "[OXYDE_PROFILE] execute_query_columnar ({}, {} rows): total={} µs",
            pool.backend_name(),
            results.1,
            elapsed_us
        );
    }
    Ok(results)
}

/// Execute a SELECT with JOIN dedup encoding.
pub async fn execute_query_columnar_dedup<'a>(
    pool_name: &str,
    sql: &str,
    params: &[Value],
    col_types: Option<&HashMap<String, ColumnTypeSpec>>,
    relations: &[RelationInfo],
    options: impl Into<ResultOptions<'a>>,
) -> Result<(Vec<u8>, usize)> {
    debug!(
        "Executing columnar dedup query on '{}': {} ({} params, {} relations)",
        pool_name,
        sql,
        params.len(),
        relations.len()
    );

    let handle = registry().get(pool_name).await?;
    let pool = handle.clone_pool();
    pool.query_columnar_dedup(sql, params, col_types, relations, options.into())
        .await
}

/// Execute a mutation with RETURNING clause, return pre-encoded msgpack map.
///
/// `required_version` is checked on the connection that runs the statement,
/// so a pool that reconnected to another server cannot bypass it.
pub async fn execute_mutation_returning(
    pool_name: &str,
    sql: &str,
    params: &[Value],
    col_types: Option<&HashMap<String, ColumnTypeSpec>>,
    required_version: Option<(i32, &'static str)>,
) -> Result<Vec<u8>> {
    debug!(
        "Executing mutation+returning on '{}': {} ({} params)",
        pool_name,
        sql,
        params.len()
    );

    let handle = registry().get(pool_name).await?;
    let pool = handle.clone_pool();
    let Some(required) = required_version else {
        return pool.query_mutation_returning(sql, params, col_types).await;
    };
    match pool {
        DbPool::Postgres(pool) => {
            let mut conn = pool
                .acquire()
                .await
                .map_err(|e| DriverError::db("Failed to acquire connection", e))?;
            check_connection_version(&mut conn, &[required]).await?;
            let query = bind_postgres(sqlx::query(sqlx::AssertSqlSafe(sql)), params)?;
            encode_stream_mutation_returning::<PgEncoder, _>(query.fetch(&mut *conn), col_types)
                .await
                .map_err(|e| DriverError::db("Query failed", e))
        }
    }
}

/// Execute a statement (INSERT/UPDATE/DELETE without RETURNING) and return affected row count.
pub async fn execute_statement(pool_name: &str, sql: &str, params: &[Value]) -> Result<u64> {
    debug!(
        "Executing statement on '{}': {} ({} params)",
        pool_name,
        sql,
        params.len()
    );

    let handle = registry().get(pool_name).await?;
    let pool = handle.clone_pool();
    let affected = pool.execute(sql, params).await?;

    debug!(
        "Statement on '{}' ({}) affected {} rows",
        pool_name,
        pool.backend_name(),
        affected
    );
    Ok(affected)
}

/// Execute SELECT query in transaction returning pre-encoded msgpack bytes + row count.
pub async fn execute_query_columnar_in_transaction<'a>(
    tx_id: u64,
    sql: &str,
    params: &[Value],
    col_types: Option<&HashMap<String, ColumnTypeSpec>>,
    options: impl Into<ResultOptions<'a>>,
) -> Result<(Vec<u8>, usize)> {
    let registry = transaction_registry();
    let arc = registry
        .get(tx_id)
        .await
        .ok_or(DriverError::TransactionNotFound(tx_id))?;
    let mut tx = arc.lock().await;
    if !tx.is_active() {
        return Err(DriverError::TransactionClosed(tx_id));
    }
    tx.update_activity();

    let conn = tx
        .tx
        .as_mut()
        .ok_or(DriverError::TransactionClosed(tx_id))?;
    conn.query_columnar(sql, params, col_types, options.into())
        .await
}

/// Execute SELECT with JOIN dedup in transaction.
pub async fn execute_query_columnar_dedup_in_transaction<'a>(
    tx_id: u64,
    sql: &str,
    params: &[Value],
    col_types: Option<&HashMap<String, ColumnTypeSpec>>,
    relations: &[RelationInfo],
    options: impl Into<ResultOptions<'a>>,
) -> Result<(Vec<u8>, usize)> {
    let registry = transaction_registry();
    let arc = registry
        .get(tx_id)
        .await
        .ok_or(DriverError::TransactionNotFound(tx_id))?;
    let mut tx = arc.lock().await;
    if !tx.is_active() {
        return Err(DriverError::TransactionClosed(tx_id));
    }
    tx.update_activity();

    let conn = tx
        .tx
        .as_mut()
        .ok_or(DriverError::TransactionClosed(tx_id))?;
    conn.query_columnar_dedup(sql, params, col_types, relations, options.into())
        .await
}

/// Execute mutation+RETURNING in transaction, return pre-encoded msgpack map.
///
/// `required_version` is checked on the transaction's connection before any
/// SQL is sent, so a failed check leaves the transaction usable.
pub async fn execute_mutation_returning_in_transaction(
    tx_id: u64,
    sql: &str,
    params: &[Value],
    col_types: Option<&HashMap<String, ColumnTypeSpec>>,
    required_version: Option<(i32, &'static str)>,
) -> Result<Vec<u8>> {
    let registry = transaction_registry();
    let arc = registry
        .get(tx_id)
        .await
        .ok_or(DriverError::TransactionNotFound(tx_id))?;
    let mut tx = arc.lock().await;
    if !tx.is_active() {
        return Err(DriverError::TransactionClosed(tx_id));
    }
    tx.update_activity();

    let conn = tx
        .tx
        .as_mut()
        .ok_or(DriverError::TransactionClosed(tx_id))?;
    if let Some(required) = required_version {
        match conn {
            DbTx::Postgres(pg) => check_connection_version(pg, &[required]).await?,
        }
    }
    conn.query_mutation_returning(sql, params, col_types).await
}

/// Execute a statement within a transaction, return affected row count.
pub async fn execute_statement_in_transaction(
    tx_id: u64,
    sql: &str,
    params: &[Value],
) -> Result<u64> {
    let registry = transaction_registry();
    let arc = registry
        .get(tx_id)
        .await
        .ok_or(DriverError::TransactionNotFound(tx_id))?;
    let mut tx = arc.lock().await;
    if !tx.is_active() {
        return Err(DriverError::TransactionClosed(tx_id));
    }
    tx.update_activity();

    let conn = tx
        .tx
        .as_mut()
        .ok_or(DriverError::TransactionClosed(tx_id))?;
    conn.execute(sql, params).await
}
