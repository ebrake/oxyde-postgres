//! Error types for the driver

use thiserror::Error;

#[derive(Debug, Error)]
pub enum DriverError {
    #[error("Database error: {0}")]
    DatabaseError(String),

    #[error("Connection error: {0}")]
    ConnectionError(String),

    #[error("{feature} requires PostgreSQL {major} or newer; server_version_num is {actual}", major = .required / 10_000)]
    UnsupportedServerVersion {
        feature: &'static str,
        required: i32,
        actual: i32,
    },

    #[error("Pool '{0}' already exists")]
    PoolAlreadyExists(String),

    #[error("Pool '{0}' not found")]
    PoolNotFound(String),

    #[error("Invalid listener channels: {0}")]
    InvalidListenerChannels(String),

    #[error("{0}")]
    ListenerFailed(String),

    #[error("Invalid pool settings: {0}")]
    InvalidPoolSettings(String),

    #[error("Query execution error: {0}")]
    ExecutionError(String),

    /// A failed database call with the original `sqlx::Error` preserved,
    /// so constraint violations classify via [`DriverError::db_kind`]
    /// instead of message-text matching. Display mirrors the historical
    /// `ExecutionError` wording.
    #[error("Query execution error: {context}: {source}")]
    Db {
        context: String,
        #[source]
        source: sqlx::Error,
    },

    #[error("Transaction '{0}' not found")]
    TransactionNotFound(u64),

    #[error("Transaction '{0}' already completed")]
    TransactionClosed(u64),
}

/// Cross-dialect constraint classification (mirror of `sqlx::error::ErrorKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbErrorKind {
    UniqueViolation,
    ForeignKeyViolation,
    NotNullViolation,
    CheckViolation,
    Other,
}

impl DriverError {
    /// Build a [`DriverError::Db`] with human context, keeping the source.
    pub(crate) fn db(context: impl Into<String>, source: sqlx::Error) -> Self {
        Self::Db {
            context: context.into(),
            source,
        }
    }

    /// Whether the error came from a connection or pool failure outside the server.
    #[must_use]
    pub fn is_connection_failure(&self) -> bool {
        matches!(
            self,
            Self::ConnectionError(_)
                | Self::Db {
                    source: sqlx::Error::Io(_)
                        | sqlx::Error::Tls(_)
                        | sqlx::Error::Protocol(_)
                        | sqlx::Error::PoolTimedOut
                        | sqlx::Error::PoolClosed
                        | sqlx::Error::WorkerCrashed,
                    ..
                }
        )
    }

    /// Constraint classification of the underlying database error, if any.
    #[must_use]
    pub fn db_kind(&self) -> Option<DbErrorKind> {
        let Self::Db { source, .. } = self else {
            return None;
        };
        let db = source.as_database_error()?;
        Some(match db.kind() {
            sqlx::error::ErrorKind::UniqueViolation => DbErrorKind::UniqueViolation,
            sqlx::error::ErrorKind::ForeignKeyViolation => DbErrorKind::ForeignKeyViolation,
            sqlx::error::ErrorKind::NotNullViolation => DbErrorKind::NotNullViolation,
            sqlx::error::ErrorKind::CheckViolation => DbErrorKind::CheckViolation,
            _ => DbErrorKind::Other,
        })
    }

    /// SQLSTATE (or dialect-native code) of the underlying error, if any.
    #[must_use]
    pub fn sqlstate(&self) -> Option<String> {
        let Self::Db { source, .. } = self else {
            return None;
        };
        Some(source.as_database_error()?.code()?.into_owned())
    }
}

pub type Result<T> = std::result::Result<T, DriverError>;

pub fn require_server_version(actual: i32, required: i32, feature: &'static str) -> Result<()> {
    if actual < required {
        return Err(DriverError::UnsupportedServerVersion {
            feature,
            required,
            actual,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::DriverError;

    #[test]
    fn classifies_connection_and_pool_failures() {
        for source in [
            sqlx::Error::Io(std::io::ErrorKind::ConnectionReset.into()),
            sqlx::Error::Tls("TLS error".into()),
            sqlx::Error::Protocol("protocol error".into()),
            sqlx::Error::PoolTimedOut,
            sqlx::Error::PoolClosed,
            sqlx::Error::WorkerCrashed,
        ] {
            assert!(DriverError::db("query", source).is_connection_failure());
        }
        assert!(DriverError::ConnectionError("connection failed".into()).is_connection_failure());

        for source in [
            sqlx::Error::Encode("bad value".into()),
            sqlx::Error::Decode("bad value".into()),
            sqlx::Error::Configuration("invalid setting".into()),
        ] {
            assert!(!DriverError::db("query", source).is_connection_failure());
        }
        assert!(!DriverError::PoolNotFound("default".into()).is_connection_failure());
    }
}
