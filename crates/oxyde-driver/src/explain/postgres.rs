//! PostgreSQL EXPLAIN functionality

use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExplainFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy)]
pub struct ExplainOptions {
    pub analyze: bool,
    pub format: ExplainFormat,
    pub io: bool,
}

/// `server_version_num` of the first PostgreSQL release with `EXPLAIN (IO)`.
pub const EXPLAIN_IO_MIN_SERVER_VERSION: i32 = 190_000;

impl ExplainOptions {
    pub fn validate(self) -> Result<(), &'static str> {
        if self.io && !self.analyze {
            return Err("EXPLAIN IO requires analyze=True");
        }
        Ok(())
    }

    /// Minimum `server_version_num` and feature name these options require.
    #[must_use]
    pub fn required_server_version(self) -> Option<(i32, &'static str)> {
        self.io
            .then_some((EXPLAIN_IO_MIN_SERVER_VERSION, "EXPLAIN IO"))
    }
}

pub fn build_postgres_explain_sql(sql: &str, options: ExplainOptions) -> String {
    let mut clauses: Vec<String> = Vec::new();
    if options.analyze {
        clauses.push("ANALYZE TRUE".into());
    }
    if options.io {
        clauses.push("IO TRUE".into());
    }
    if matches!(options.format, ExplainFormat::Json) {
        clauses.push("FORMAT JSON".into());
    }
    if clauses.is_empty() {
        format!("EXPLAIN {sql}")
    } else {
        format!("EXPLAIN ({}) {}", clauses.join(", "), sql)
    }
}

pub fn extract_postgres_json_plan(
    rows: Vec<HashMap<String, serde_json::Value>>,
) -> serde_json::Value {
    rows.into_iter()
        .find_map(|row| row.get("QUERY PLAN").cloned())
        .unwrap_or(serde_json::Value::Null)
}

pub fn extract_text_plan(
    rows: Vec<HashMap<String, serde_json::Value>>,
    column: &str,
) -> serde_json::Value {
    let lines = rows
        .into_iter()
        .filter_map(|row| row.get(column).cloned())
        .map(|value| match value {
            serde_json::Value::String(s) => serde_json::Value::String(s),
            other => serde_json::Value::String(other.to_string()),
        })
        .collect();
    serde_json::Value::Array(lines)
}
