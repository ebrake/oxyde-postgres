//! Pool lifecycle API: init, close, query backend.

use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use std::path::PathBuf;
use std::str::FromStr;
use tracing::info;

use crate::error::{DriverError, Result};
use crate::pool::{DatabaseBackend, DbPool, PoolHandle};
use crate::settings::PoolSettings;
use crate::{registry, transaction_registry};

/// Initialize a named connection pool. Fails if pool with this name already exists.
pub async fn init_pool(name: &str, url: &str, settings: PoolSettings) -> Result<()> {
    init_pool_inner(name, url, settings, false).await
}

/// Initialize a named connection pool, replacing existing pool if present.
pub async fn init_pool_overwrite(name: &str, url: &str, settings: PoolSettings) -> Result<()> {
    init_pool_inner(name, url, settings, true).await
}

async fn init_pool_inner(
    name: &str,
    url: &str,
    settings: PoolSettings,
    overwrite: bool,
) -> Result<()> {
    info!("Initializing pool '{}' (overwrite={})", name, overwrite);

    validate_settings(&settings)?;

    let Some(backend) = backend_from_url(url) else {
        return Err(DriverError::ConnectionError(
            "Unsupported database URL scheme; expected postgres:// or postgresql://".to_string(),
        ));
    };

    let pool = match backend {
        DatabaseBackend::Postgres => {
            let connect_opts = build_pg_connect_options(url, &settings)?;
            let pool_opts = apply_common_settings_pg(PgPoolOptions::new(), &settings);
            pool_opts
                .connect_with(connect_opts)
                .await
                .map(DbPool::Postgres)
                .map_err(|e| DriverError::ConnectionError(format!("Failed to connect: {e}")))?
        }
    };

    let DbPool::Postgres(pg_pool) = &pool;
    let version =
        sqlx::query_scalar::<_, i32>("SELECT current_setting('server_version_num')::int4")
            .fetch_one(pg_pool)
            .await;
    let version_result = match version {
        Ok(version) => validate_server_version(version),
        Err(error) => Err(DriverError::ConnectionError(format!(
            "Cannot determine PostgreSQL server version: {error}"
        ))),
    };
    if let Err(error) = version_result {
        pg_pool.close().await;
        return Err(error);
    }
    let handle = PoolHandle::new(backend, pool);

    if overwrite {
        // Close old pool if exists
        if let Some(old_handle) = registry().insert_or_replace(name.to_string(), handle).await {
            info!("Closing old pool '{}' during overwrite", name);
            old_handle.close().await;
        }
    } else if let Err(err) = registry().insert(name.to_string(), handle.clone()).await {
        handle.close().await;
        return Err(err);
    }

    // Update transaction registry settings for this pool
    transaction_registry()
        .update_settings(name, &settings)
        .await;

    info!("Pool '{}' initialised successfully", name);
    Ok(())
}

/// Close and remove a named connection pool.
pub async fn close_pool(name: &str) -> Result<()> {
    info!("Closing pool '{}'", name);
    let handle = registry().remove(name).await?;
    handle.close().await;
    Ok(())
}

/// Close all connection pools, rolling back any active transactions first.
pub async fn close_all_pools() -> Result<()> {
    info!("Closing all pools");

    // First, rollback all active transactions
    let rolled_back = transaction_registry().rollback_all().await;
    if rolled_back > 0 {
        info!(
            "Rolled back {} active transactions during shutdown",
            rolled_back
        );
    }

    // Then close all connection pools
    let handles = registry().take_all().await;
    for (name, handle) in handles {
        info!("Closing pool '{}'", name);
        handle.close().await;
    }
    Ok(())
}

/// Get the database backend type for a named pool.
pub async fn pool_backend(name: &str) -> Result<DatabaseBackend> {
    registry().get(name).await.map(|handle| handle.backend)
}

fn backend_from_url(url: &str) -> Option<DatabaseBackend> {
    if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        Some(DatabaseBackend::Postgres)
    } else {
        None
    }
}

fn validate_server_version(version: i32) -> Result<()> {
    if version < 180_000 {
        return Err(DriverError::ConnectionError(format!(
            "PostgreSQL 18 or newer is required; server_version_num is {version}"
        )));
    }
    Ok(())
}

fn validate_settings(settings: &PoolSettings) -> Result<()> {
    if let (Some(min), Some(max)) = (settings.min_connections, settings.max_connections) {
        if min > max {
            return Err(DriverError::InvalidPoolSettings(
                "min_connections cannot exceed max_connections".into(),
            ));
        }
    }

    Ok(())
}

fn apply_common_settings_pg(mut options: PgPoolOptions, settings: &PoolSettings) -> PgPoolOptions {
    if let Some(max) = settings.max_connections {
        options = options.max_connections(max);
    }
    if let Some(min) = settings.min_connections {
        options = options.min_connections(min);
    }
    if let Some(timeout) = settings.acquire_timeout {
        options = options.acquire_timeout(timeout);
    }
    if let Some(timeout) = settings.idle_timeout {
        options = options.idle_timeout(timeout);
    }
    if let Some(lifetime) = settings.max_lifetime {
        options = options.max_lifetime(lifetime);
    }
    if let Some(test_before) = settings.test_before_acquire {
        options = options.test_before_acquire(test_before);
    }
    options
}

fn build_pg_connect_options(url: &str, settings: &PoolSettings) -> Result<PgConnectOptions> {
    let mut opts = PgConnectOptions::from_str(url)
        .map_err(|e| DriverError::ConnectionError(format!("Invalid PostgreSQL URL: {e}")))?;

    if let Some(mode) = &settings.ssl_mode {
        opts = opts.ssl_mode(parse_pg_ssl_mode(mode)?);
    }
    if let Some(path) = &settings.ssl_root_cert {
        opts = opts.ssl_root_cert(PathBuf::from(path));
    }
    if let Some(path) = &settings.ssl_client_cert {
        opts = opts.ssl_client_cert(PathBuf::from(path));
    }
    if let Some(path) = &settings.ssl_client_key {
        opts = opts.ssl_client_key(PathBuf::from(path));
    }
    if let Some(name) = &settings.pg_application_name {
        opts = opts.application_name(name);
    }
    if let Some(cap) = settings.pg_statement_cache_capacity {
        opts = opts.statement_cache_capacity(cap as usize);
    }

    Ok(opts)
}

fn parse_pg_ssl_mode(mode: &str) -> Result<PgSslMode> {
    match mode.to_lowercase().replace('-', "_").as_str() {
        "disable" => Ok(PgSslMode::Disable),
        "allow" => Ok(PgSslMode::Allow),
        "prefer" => Ok(PgSslMode::Prefer),
        "require" => Ok(PgSslMode::Require),
        "verify_ca" => Ok(PgSslMode::VerifyCa),
        "verify_full" => Ok(PgSslMode::VerifyFull),
        _ => Err(DriverError::InvalidPoolSettings(format!(
            "Invalid PostgreSQL SSL mode: '{mode}'. \
             Expected: disable, allow, prefer, require, verify-ca, verify-full"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn minimum_server_version_is_postgres_18() {
        for version in [120_000, 160_015, 170_999, 179_999] {
            assert!(validate_server_version(version)
                .unwrap_err()
                .to_string()
                .contains("PostgreSQL 18"));
        }
        for version in [180_000, 180_001, 190_000] {
            assert!(validate_server_version(version).is_ok());
        }
    }
    #[test]
    fn only_exact_postgres_url_schemes_are_supported() {
        for url in ["postgres://localhost/db", "postgresql://localhost/db"] {
            assert_eq!(backend_from_url(url), Some(DatabaseBackend::Postgres));
        }
        for url in [
            "sqlite::memory:",
            "mysql://localhost/db",
            "postgresjunk://localhost/db",
            "postgresqlx://localhost/db",
            "postgres",
        ] {
            assert_eq!(backend_from_url(url), None);
        }
    }
}
