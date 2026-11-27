//! EXPLAIN query functionality
//!
//! Uses a local query helper (not the main columnar path) because EXPLAIN
//! results are returned as `Vec<HashMap<String, serde_json::Value>>` for the
//! extract functions. This is a cold debug path — no hot-path allocation concern.

pub mod postgres;

pub use postgres::{
    build_postgres_explain_sql, extract_postgres_json_plan, extract_text_plan, ExplainFormat,
    ExplainOptions,
};

use std::collections::HashMap;

use sqlx::postgres::PgConnection;
use sqlx::{Column, Row};

use crate::bind::bind_postgres;
use crate::error::{require_server_version, DriverError, Result};
use crate::pool::{DatabaseBackend, DbPool};
use crate::registry;

/// Convert a single sqlx row to HashMap<String, serde_json::Value>.
/// Tries i64 → f64 → bool → String (covers all EXPLAIN output types).
fn row_to_map<R: Row>(row: &R) -> HashMap<String, serde_json::Value>
where
    for<'r> i64: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    for<'r> f64: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    for<'r> bool: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    for<'r> String: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    for<'r> serde_json::Value: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    for<'r> &'r str: sqlx::ColumnIndex<R>,
    <<R as Row>::Database as sqlx::Database>::Column: Column,
{
    let mut map = HashMap::new();
    for col in row.columns() {
        let name = col.name();
        // Try JSON first (Postgres EXPLAIN FORMAT JSON returns jsonb)
        let val = if let Ok(v) = row.try_get::<serde_json::Value, _>(name) {
            v
        } else if let Ok(v) = row.try_get::<i64, _>(name) {
            serde_json::Value::Number(serde_json::Number::from(v))
        } else if let Ok(v) = row.try_get::<f64, _>(name) {
            serde_json::Number::from_f64(v)
                .map_or(serde_json::Value::Null, serde_json::Value::Number)
        } else if let Ok(v) = row.try_get::<bool, _>(name) {
            serde_json::Value::Bool(v)
        } else if let Ok(v) = row.try_get::<String, _>(name) {
            serde_json::Value::String(v)
        } else {
            serde_json::Value::Null
        };
        map.insert(name.to_string(), val);
    }
    map
}

/// Read `server_version_num` for this connection.
///
/// Uses the version PostgreSQL reported at connection startup and only queries
/// the server when that value could not be parsed.
async fn connection_server_version(conn: &mut PgConnection) -> Result<i32> {
    if let Some(version) = conn
        .server_version_num()
        .and_then(|version| i32::try_from(version).ok())
    {
        return Ok(version);
    }
    sqlx::query_scalar::<_, i32>("SELECT current_setting('server_version_num')::int4")
        .fetch_one(conn)
        .await
        .map_err(|e| {
            DriverError::ExecutionError(format!("Cannot determine PostgreSQL server version: {e}"))
        })
}

/// Fetch EXPLAIN rows as Vec<HashMap> (cold debug path, uses pool directly).
///
/// Version requirements are checked on the connection that runs the
/// statement, so a reconnect or pool replacement cannot bypass them.
async fn fetch_explain_rows(
    pool_name: &str,
    sql: &str,
    params: &[sea_query::Value],
    required_version: Option<(i32, &'static str)>,
) -> Result<Vec<HashMap<String, serde_json::Value>>> {
    let handle = registry().get(pool_name).await?;
    match handle.clone_pool() {
        DbPool::Postgres(pool) => {
            let mut conn = pool.acquire().await.map_err(|e| {
                DriverError::ConnectionError(format!("Failed to acquire connection: {e}"))
            })?;
            if let Some((required, feature)) = required_version {
                let actual = connection_server_version(&mut conn).await?;
                require_server_version(actual, required, feature)?;
            }
            let query = bind_postgres(sqlx::query(sqlx::AssertSqlSafe(sql)), params)?;
            let rows = query
                .fetch_all(&mut *conn)
                .await
                .map_err(|e| DriverError::ExecutionError(format!("EXPLAIN failed: {e}")))?;
            Ok(rows.iter().map(row_to_map).collect())
        }
    }
}

pub async fn explain_query(
    pool_name: &str,
    sql: &str,
    params: &[sea_query::Value],
    options: ExplainOptions,
) -> Result<serde_json::Value> {
    let backend = crate::pool::api::pool_backend(pool_name).await?;
    let explain_sql = match backend {
        DatabaseBackend::Postgres => build_postgres_explain_sql(sql, options),
    };

    let rows = fetch_explain_rows(
        pool_name,
        &explain_sql,
        params,
        options.required_server_version(),
    )
    .await?;

    let payload = match backend {
        DatabaseBackend::Postgres => match options.format {
            ExplainFormat::Json => extract_postgres_json_plan(rows),
            ExplainFormat::Text => extract_text_plan(rows, "QUERY PLAN"),
        },
    };

    Ok(payload)
}
