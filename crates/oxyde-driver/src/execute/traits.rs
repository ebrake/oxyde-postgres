//! Backend-agnostic query execution traits.
//!
//! This module provides traits that abstract over the three database backends

use async_trait::async_trait;
use oxyde_codec::ColumnTypeSpec;
use sea_query::Value;
use std::collections::HashMap;

use crate::bind::bind_postgres;
use crate::convert::encoder::{encode_stream, encode_stream_mutation_returning, RelationInfo};
use crate::convert::postgres::PgEncoder;
use crate::error::{DriverError, Result};
use crate::pool::DbPool;
use crate::transaction::DbTx;

/// Wrap a database error, preserving the source for classification.
fn exec_err(e: sqlx::Error) -> DriverError {
    DriverError::db("Query failed", e)
}

fn stmt_err(e: sqlx::Error) -> DriverError {
    DriverError::db("Statement failed", e)
}

/// Trait for executing queries on a database pool.
/// Uses `&self` because sqlx pools are internally reference-counted.
#[async_trait]
pub trait PoolExec {
    /// Execute a SELECT query and return pre-encoded msgpack bytes + row count.
    async fn query_columnar(
        &self,
        sql: &str,
        params: &[Value],
        col_types: Option<&HashMap<String, ColumnTypeSpec>>,
    ) -> Result<(Vec<u8>, usize)>;

    /// Execute a SELECT with JOIN dedup encoding.
    async fn query_columnar_dedup(
        &self,
        sql: &str,
        params: &[Value],
        col_types: Option<&HashMap<String, ColumnTypeSpec>>,
        relations: &[RelationInfo],
    ) -> Result<(Vec<u8>, usize)>;

    /// Execute a mutation with RETURNING clause, return pre-encoded msgpack map.
    async fn query_mutation_returning(
        &self,
        sql: &str,
        params: &[Value],
        col_types: Option<&HashMap<String, ColumnTypeSpec>>,
    ) -> Result<Vec<u8>>;

    /// Execute a statement (INSERT/UPDATE/DELETE) and return affected rows
    async fn execute(&self, sql: &str, params: &[Value]) -> Result<u64>;

    /// Backend name for logging/profiling
    fn backend_name(&self) -> &'static str;
}

/// Trait for executing queries on a database connection (in transaction).
/// Uses `&mut self` because sqlx connections require mutable access.
#[async_trait]
pub trait ConnExec {
    /// Execute a SELECT query and return pre-encoded msgpack bytes + row count.
    async fn query_columnar(
        &mut self,
        sql: &str,
        params: &[Value],
        col_types: Option<&HashMap<String, ColumnTypeSpec>>,
    ) -> Result<(Vec<u8>, usize)>;

    /// Execute a SELECT with JOIN dedup encoding.
    async fn query_columnar_dedup(
        &mut self,
        sql: &str,
        params: &[Value],
        col_types: Option<&HashMap<String, ColumnTypeSpec>>,
        relations: &[RelationInfo],
    ) -> Result<(Vec<u8>, usize)>;

    /// Execute a mutation with RETURNING clause, return pre-encoded msgpack map.
    async fn query_mutation_returning(
        &mut self,
        sql: &str,
        params: &[Value],
        col_types: Option<&HashMap<String, ColumnTypeSpec>>,
    ) -> Result<Vec<u8>>;

    /// Execute a statement and return affected rows
    async fn execute(&mut self, sql: &str, params: &[Value]) -> Result<u64>;
}

// =============================================================================
// DbPool implementation
// =============================================================================

#[async_trait]
impl PoolExec for DbPool {
    async fn query_columnar(
        &self,
        sql: &str,
        params: &[Value],
        col_types: Option<&HashMap<String, ColumnTypeSpec>>,
    ) -> Result<(Vec<u8>, usize)> {
        match self {
            DbPool::Postgres(pool) => {
                let query = bind_postgres(sqlx::query(sqlx::AssertSqlSafe(sql)), params)?;
                let stream = query.fetch(pool);
                encode_stream::<PgEncoder, _>(stream, col_types, None)
                    .await
                    .map_err(exec_err)
            }
        }
    }

    async fn query_columnar_dedup(
        &self,
        sql: &str,
        params: &[Value],
        col_types: Option<&HashMap<String, ColumnTypeSpec>>,
        relations: &[RelationInfo],
    ) -> Result<(Vec<u8>, usize)> {
        match self {
            DbPool::Postgres(pool) => {
                let query = bind_postgres(sqlx::query(sqlx::AssertSqlSafe(sql)), params)?;
                let stream = query.fetch(pool);
                encode_stream::<PgEncoder, _>(stream, col_types, Some(relations))
                    .await
                    .map_err(exec_err)
            }
        }
    }

    async fn query_mutation_returning(
        &self,
        sql: &str,
        params: &[Value],
        col_types: Option<&HashMap<String, ColumnTypeSpec>>,
    ) -> Result<Vec<u8>> {
        match self {
            DbPool::Postgres(pool) => {
                let query = bind_postgres(sqlx::query(sqlx::AssertSqlSafe(sql)), params)?;
                let stream = query.fetch(pool);
                encode_stream_mutation_returning::<PgEncoder, _>(stream, col_types)
                    .await
                    .map_err(exec_err)
            }
        }
    }

    async fn execute(&self, sql: &str, params: &[Value]) -> Result<u64> {
        match self {
            DbPool::Postgres(pool) => {
                let query = bind_postgres(sqlx::query(sqlx::AssertSqlSafe(sql)), params)?;
                let result = query.execute(pool).await.map_err(stmt_err)?;
                Ok(result.rows_affected())
            }
        }
    }

    fn backend_name(&self) -> &'static str {
        match self {
            DbPool::Postgres(_) => "Postgres",
        }
    }
}

// =============================================================================
// DbTx implementation (for transactions)
// =============================================================================

#[async_trait]
impl ConnExec for DbTx {
    async fn query_columnar(
        &mut self,
        sql: &str,
        params: &[Value],
        col_types: Option<&HashMap<String, ColumnTypeSpec>>,
    ) -> Result<(Vec<u8>, usize)> {
        match self {
            DbTx::Postgres(tx) => {
                let query = bind_postgres(sqlx::query(sqlx::AssertSqlSafe(sql)), params)?;
                let stream = query.fetch(&mut **tx);
                encode_stream::<PgEncoder, _>(stream, col_types, None)
                    .await
                    .map_err(exec_err)
            }
        }
    }

    async fn query_columnar_dedup(
        &mut self,
        sql: &str,
        params: &[Value],
        col_types: Option<&HashMap<String, ColumnTypeSpec>>,
        relations: &[RelationInfo],
    ) -> Result<(Vec<u8>, usize)> {
        match self {
            DbTx::Postgres(tx) => {
                let query = bind_postgres(sqlx::query(sqlx::AssertSqlSafe(sql)), params)?;
                let stream = query.fetch(&mut **tx);
                encode_stream::<PgEncoder, _>(stream, col_types, Some(relations))
                    .await
                    .map_err(exec_err)
            }
        }
    }

    async fn query_mutation_returning(
        &mut self,
        sql: &str,
        params: &[Value],
        col_types: Option<&HashMap<String, ColumnTypeSpec>>,
    ) -> Result<Vec<u8>> {
        match self {
            DbTx::Postgres(tx) => {
                let query = bind_postgres(sqlx::query(sqlx::AssertSqlSafe(sql)), params)?;
                let stream = query.fetch(&mut **tx);
                encode_stream_mutation_returning::<PgEncoder, _>(stream, col_types)
                    .await
                    .map_err(exec_err)
            }
        }
    }

    async fn execute(&mut self, sql: &str, params: &[Value]) -> Result<u64> {
        match self {
            DbTx::Postgres(tx) => {
                let query = bind_postgres(sqlx::query(sqlx::AssertSqlSafe(sql)), params)?;
                let result = query.execute(&mut **tx).await.map_err(stmt_err)?;
                Ok(result.rows_affected())
            }
        }
    }
}
