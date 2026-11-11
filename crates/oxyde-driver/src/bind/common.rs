//! Shared helpers for PostgreSQL parameter binding

use crate::error::{DriverError, Result};
use sea_query::Value;

pub fn cast_u64_to_i64(value: u64, db: &str) -> Result<i64> {
    i64::try_from(value).map_err(|_| {
        DriverError::ExecutionError(format!("Parameter out of range for {db}: {value}"))
    })
}

pub fn unsupported_param(db: &str, value: &Value) -> DriverError {
    DriverError::ExecutionError(format!("Unsupported parameter type for {db}: {value:?}"))
}
