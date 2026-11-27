//! PostgreSQL backend tests

#[cfg(test)]
mod tests {
    use crate::error::{require_server_version, DriverError};
    use crate::explain::postgres::{build_postgres_explain_sql, ExplainFormat, ExplainOptions};

    #[test]
    fn test_explain_sql_basic() {
        let options = ExplainOptions {
            analyze: false,
            format: ExplainFormat::Text,
            io: false,
        };
        let sql = build_postgres_explain_sql("SELECT * FROM users", options);
        assert!(sql.contains("EXPLAIN"));
        assert!(sql.contains("SELECT * FROM users"));
    }

    #[test]
    fn test_explain_sql_analyze() {
        let options = ExplainOptions {
            analyze: true,
            format: ExplainFormat::Text,
            io: false,
        };
        let sql = build_postgres_explain_sql("SELECT * FROM users", options);
        assert!(sql.contains("ANALYZE"));
    }

    #[test]
    fn test_explain_sql_json() {
        let options = ExplainOptions {
            analyze: false,
            format: ExplainFormat::Json,
            io: false,
        };
        let sql = build_postgres_explain_sql("SELECT * FROM users", options);
        assert!(sql.contains("FORMAT JSON"));
    }

    #[test]
    fn test_explain_io() {
        let options = ExplainOptions {
            analyze: true,
            format: ExplainFormat::Json,
            io: true,
        };
        assert_eq!(options.validate(), Ok(()));
        assert_eq!(
            build_postgres_explain_sql("SELECT * FROM users", options),
            "EXPLAIN (ANALYZE TRUE, IO TRUE, FORMAT JSON) SELECT * FROM users"
        );
        assert_eq!(
            ExplainOptions {
                analyze: false,
                ..options
            }
            .validate(),
            Err("EXPLAIN IO requires analyze=True")
        );
        let error = require_server_version(180_002, 190_000, "EXPLAIN IO").unwrap_err();
        assert!(matches!(
            error,
            DriverError::UnsupportedServerVersion { .. }
        ));
        assert_eq!(
            error.to_string(),
            "EXPLAIN IO requires PostgreSQL 19 or newer; server_version_num is 180002"
        );
        assert!(require_server_version(190_000, 190_000, "EXPLAIN IO").is_ok());
        assert_eq!(
            options.required_server_version(),
            Some((190_000, "EXPLAIN IO"))
        );
        assert_eq!(
            ExplainOptions {
                io: false,
                ..options
            }
            .required_server_version(),
            None
        );
    }
}
