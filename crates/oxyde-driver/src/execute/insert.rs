//! INSERT RETURNING execution (pool and transaction paths).

use sea_query::Value;
use sqlx::postgres::PgRow;
use tracing::debug;

use crate::bind::bind_postgres;
use crate::convert::{encoder::CellEncoder, postgres::PgEncoder};
use crate::error::{DriverError, Result};
use crate::pool::DbPool;
use crate::transaction::DbTx;
use crate::{registry, transaction_registry};

/// Decode primary keys with the same PostgreSQL type handling as row queries.
/// Keep an entry for each returned row, even if its key cannot be decoded.
fn extract_pks(rows: &[PgRow], pk_col: &str) -> Vec<rmpv::Value> {
    let Some(first) = rows.first() else {
        return Vec::new();
    };
    let columns = PgEncoder::extract_columns(first, None);
    let Some((index, column)) = columns
        .iter()
        .enumerate()
        .find(|(_, column)| column.name == pk_col)
    else {
        return vec![rmpv::Value::Nil; rows.len()];
    };

    let mut buffer = Vec::new();
    rows.iter()
        .map(|row| {
            buffer.clear();
            PgEncoder::encode_cell(&mut buffer, row, index, column);
            rmpv::decode::read_value(&mut buffer.as_slice()).unwrap_or(rmpv::Value::Nil)
        })
        .collect()
}

/// Execute INSERT RETURNING and return a key slot for every returned row.
pub async fn execute_insert_returning(
    pool_name: &str,
    sql: &str,
    params: &[Value],
    pk_col: &str,
) -> Result<Vec<rmpv::Value>> {
    debug!(
        "Executing INSERT RETURNING on '{}': {} ({} params, pk={})",
        pool_name,
        sql,
        params.len(),
        pk_col
    );

    let handle = registry().get(pool_name).await?;
    match handle.clone_pool() {
        DbPool::Postgres(pool) => {
            let query = bind_postgres(sqlx::query(sqlx::AssertSqlSafe(sql)), params)?;
            let rows = query
                .fetch_all(&pool)
                .await
                .map_err(|e| DriverError::db("INSERT RETURNING failed", e))?;

            let ids = extract_pks(&rows, pk_col);

            debug!(
                "INSERT on '{}' (Postgres) returned {} IDs",
                pool_name,
                ids.len()
            );
            Ok(ids)
        }
    }
}

/// Execute INSERT within a transaction and return generated IDs.
/// Same contract as [`execute_insert_returning`]: SQL arrives with pk-only
pub async fn execute_insert_returning_in_transaction(
    tx_id: u64,
    sql: &str,
    params: &[Value],
    pk_col: &str,
) -> Result<Vec<rmpv::Value>> {
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

    match conn {
        DbTx::Postgres(tx) => {
            let query = bind_postgres(sqlx::query(sqlx::AssertSqlSafe(sql)), params)?;
            let rows = query
                .fetch_all(&mut **tx)
                .await
                .map_err(|e| DriverError::db("INSERT RETURNING failed", e))?;

            let ids = extract_pks(&rows, pk_col);

            debug!(
                "INSERT in transaction {} (Postgres) returned {} IDs",
                tx_id,
                ids.len()
            );
            Ok(ids)
        }
    }
}
