//! Server version checks on the connection that runs a statement.
//!
//! A pool can reconnect to a different server after it opens, so a version
//! recorded at pool startup cannot gate version-specific SQL.

use sqlx::postgres::PgConnection;

use crate::error::{require_server_version, DriverError, Result};

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

/// Check each `(server_version_num, feature)` requirement against this connection.
///
/// Does nothing when `required` is empty.
pub(crate) async fn check_connection_version(
    conn: &mut PgConnection,
    required: &[(i32, &'static str)],
) -> Result<()> {
    if required.is_empty() {
        return Ok(());
    }
    let actual = connection_server_version(conn).await?;
    for &(version, feature) in required {
        require_server_version(actual, version, feature)?;
    }
    Ok(())
}
