//! Pool handle wrapper

use sqlx::postgres::PgPool;

/// Supported database backend type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DatabaseBackend {
    Postgres,
}

/// PostgreSQL connection pool.
#[derive(Clone)]
pub enum DbPool {
    Postgres(PgPool),
}

/// Named pool handle: backend type + connection pool.
#[derive(Clone)]
pub struct PoolHandle {
    pub(crate) backend: DatabaseBackend,
    pub(crate) pool: DbPool,
}

impl PoolHandle {
    pub fn new(backend: DatabaseBackend, pool: DbPool) -> Self {
        Self { backend, pool }
    }

    pub async fn close(&self) {
        match &self.pool {
            DbPool::Postgres(pool) => pool.close().await,
        }
    }

    pub fn clone_pool(&self) -> DbPool {
        self.pool.clone()
    }
}
