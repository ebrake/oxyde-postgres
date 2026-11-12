//! All SQL generation via sea-query: DML from QueryIR, DDL from MigrationOps.
//!
//! This is the single crate that renders SQL text. It converts QueryIR
//! structures (oxyde-codec) into PostgreSQL DML with bound parameters,
//! and MigrationOp lists into DDL statements. Consumers (oxyde-migrate,
//! oxyde-driver, oxyde-core-py) never touch sea-query's builders themselves.
//!
//! # Main Entry Points
//!
//! ```rust,ignore
//! use oxyde_sql::{build_sql, Dialect, Migration};
//!
//! let (sql, values) = build_sql(&ir, Dialect::Postgres)?;
//! let ddl: Vec<String> = migration.to_sql(Dialect::Postgres)?;
//! ```
//!
//! # Dialect Support
//!
//! - **PostgreSQL**: `$1, $2, $3` placeholders, RETURNING clause, FOR UPDATE/SHARE
//!
//! # Architecture
//!
//! The crate is organized into modules:
//! - `builder` - Main SQL builders for SELECT, INSERT, UPDATE, DELETE
//! - `filter` - WHERE clause from FilterNode tree (AND/OR/NOT, operators)
//! - `aggregate` - COUNT, SUM, AVG, MAX, MIN handling
//! - `ddl` - DDL rendering for migration operations (+ statement ordering)
//! - `spec_sql` - ColumnTypeSpec → SQL type string resolution
//! - `utils` - value binding, casts, identifier quoting
//! - `error` - QueryError types
//!
//! # Supported Features
//!
//! - SELECT with JOIN, GROUP BY, HAVING, ORDER BY, LIMIT, OFFSET
//! - INSERT single row and bulk insert
//! - UPDATE with SET, WHERE, and bulk CASE WHEN
//! - DELETE with WHERE
//! - Raw SQL pass-through
//! - Aggregate functions
//! - Row-level locking (FOR UPDATE/FOR SHARE)
//! - UNION / UNION ALL

use oxyde_codec::{Operation, QueryIR};
use sea_query::Value;

// Module declarations
pub mod aggregate;
pub mod builder;
pub mod ddl;
pub mod error;
pub mod filter;
pub mod spec_sql;
pub mod utils;

// Re-exports
pub use ddl::{Migration, MigrationOpExt};
pub use error::{QueryError, Result};
pub use spec_sql::resolve_spec_type;

/// Database dialect
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dialect {
    Postgres,
}

/// Whether the generated statement carries a full `RETURNING *` clause —
/// i.e. the caller asked for rows back (`ir.returning`).
///
/// Execution routing must use this predicate, not the SQL text: identifiers
/// may legally contain the word RETURNING, and INSERTs may later carry a
/// pk-only `RETURNING "id"` clause that does not mean "return full rows".
#[must_use]
pub fn emits_returning(ir: &QueryIR, dialect: Dialect) -> bool {
    ir.returning.unwrap_or(false) && matches!(dialect, Dialect::Postgres)
}

/// Build SQL from QueryIR
///
/// This is the main entry point for SQL generation. It dispatches to the
/// appropriate builder based on the operation type.
pub fn build_sql(ir: &QueryIR, dialect: Dialect) -> Result<(String, Vec<Value>)> {
    // Binding-only hints become column specs for SQL building (the folded copy
    // still names them in `bind_types`, so expression typing can tell them from
    // real specs); the original IR and its `column_types` stay the source for
    // decoding. The copy must not be folded again.
    let folded = ir.with_binding_types();
    let ir = folded.as_ref().unwrap_or(ir);
    match ir.op {
        Operation::Select => builder::build_select(ir, dialect),
        Operation::Insert => builder::build_insert(ir, dialect),
        Operation::Update => builder::build_update(ir, dialect),
        Operation::Delete => builder::build_delete(ir, dialect),
        Operation::Raw => {
            // Raw SQL - just return the SQL as-is with parameters
            let sql = ir
                .sql
                .as_ref()
                .ok_or_else(|| QueryError::InvalidQuery("Raw query missing sql field".into()))?
                .clone();

            // Convert params to sea_query Values
            let values = if let Some(params) = &ir.params {
                params.iter().map(utils::rmpv_to_value).collect()
            } else {
                Vec::new()
            };

            Ok((sql, values))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxyde_codec::{
        ColumnTypeSpec, ConflictAction, Filter, FilterNode, JoinColumn, JoinSpec, OnConflict,
        Operation, QueryIR,
    };
    use std::collections::HashMap;

    fn rmpv_int(v: i64) -> rmpv::Value {
        rmpv::Value::Integer(v.into())
    }

    fn rmpv_str(v: &str) -> rmpv::Value {
        rmpv::Value::String(v.into())
    }

    fn rmpv_null() -> rmpv::Value {
        rmpv::Value::Nil
    }

    fn rmpv_arr(vals: Vec<rmpv::Value>) -> rmpv::Value {
        rmpv::Value::Array(vals)
    }

    fn enum_spec() -> ColumnTypeSpec {
        ColumnTypeSpec::Enum {
            name: "post_status_enum".into(),
            values: vec!["draft".into(), "published".into()],
        }
    }

    /// Helper to create a simple condition filter node
    fn filter_cond(field: &str, operator: &str, value: rmpv::Value) -> FilterNode {
        FilterNode::Condition(Filter {
            field: field.into(),
            operator: operator.into(),
            value,
            column: None,
            escape: None,
        })
    }

    /// Helper to create a filter with column alias
    fn filter_with_column(
        field: &str,
        column: &str,
        operator: &str,
        value: rmpv::Value,
    ) -> FilterNode {
        FilterNode::Condition(Filter {
            field: field.into(),
            operator: operator.into(),
            value,
            column: Some(column.into()),
            escape: None,
        })
    }

    fn filter_with_escape(
        field: &str,
        operator: &str,
        value: rmpv::Value,
        escape: &str,
    ) -> FilterNode {
        FilterNode::Condition(Filter {
            field: field.into(),
            operator: operator.into(),
            value,
            column: None,
            escape: Some(escape.into()),
        })
    }

    #[test]
    fn test_select_query() {
        let ir = QueryIR {
            table: "users".into(),
            cols: Some(vec!["id".into(), "name".into()]),
            limit: Some(10),
            ..Default::default()
        };
        let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
        assert!(sql.contains("SELECT"));
        assert!(sql.contains("users"));
        assert!(sql.contains("LIMIT"));
    }

    #[test]
    fn test_select_with_params() {
        let ir = QueryIR {
            table: "users".into(),
            cols: Some(vec!["id".into()]),
            filter_tree: Some(filter_cond("id", "=", rmpv_int(42))),
            ..Default::default()
        };
        let (sql, params) = build_sql(&ir, Dialect::Postgres).unwrap();
        assert!(sql.contains("$1"));
        assert_eq!(params.len(), 1);
        match &params[0] {
            Value::BigInt(Some(v)) => assert_eq!(*v, 42),
            other => panic!("unexpected param value: {other:?}"),
        }
    }

    #[test]
    fn test_insert_enum_value_is_cast_on_postgres() {
        let ir = QueryIR {
            op: Operation::Insert,
            table: "posts".into(),
            values: Some(HashMap::from([("status".into(), rmpv_str("draft"))])),
            column_types: Some(HashMap::from([("status".into(), enum_spec())])),
            ..Default::default()
        };
        let (sql, params) = build_sql(&ir, Dialect::Postgres).unwrap();
        assert!(sql.contains("$1::\"post_status_enum\""), "{sql}");
        assert_eq!(params.len(), 1);
    }

    #[test]
    fn test_filter_enum_in_values_are_cast_on_postgres() {
        let ir = QueryIR {
            table: "posts".into(),
            cols: Some(vec!["id".into()]),
            filter_tree: Some(filter_cond(
                "status",
                "IN",
                rmpv_arr(vec![rmpv_str("draft"), rmpv_str("published")]),
            )),
            column_types: Some(HashMap::from([("status".into(), enum_spec())])),
            ..Default::default()
        };
        let (sql, params) = build_sql(&ir, Dialect::Postgres).unwrap();
        assert!(sql.contains("$1::\"post_status_enum\""), "{sql}");
        assert!(sql.contains("$2::\"post_status_enum\""), "{sql}");
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn test_qualified_filter_enum_value_is_cast_on_postgres() {
        let ir = QueryIR {
            table: "posts".into(),
            cols: Some(vec!["id".into()]),
            filter_tree: Some(filter_with_column(
                "status",
                "author.status",
                "=",
                rmpv_str("draft"),
            )),
            column_types: Some(HashMap::from([("author.status".into(), enum_spec())])),
            ..Default::default()
        };
        let (sql, params) = build_sql(&ir, Dialect::Postgres).unwrap();
        assert!(sql.contains("$1::\"post_status_enum\""), "{sql}");
        assert_eq!(params.len(), 1);
    }

    #[test]
    fn test_qualified_column_type_wins_over_same_named_base_column() {
        // The base model's `status` is text; the joined `author.status` is an
        // enum. The query-level map holds both, keyed by SQL column reference.
        let ir = QueryIR {
            table: "posts".into(),
            cols: Some(vec!["id".into()]),
            filter_tree: Some(filter_with_column(
                "status",
                "author.status",
                "=",
                rmpv_str("draft"),
            )),
            column_types: Some(HashMap::from([
                ("status".into(), ColumnTypeSpec::Text),
                ("author.status".into(), enum_spec()),
            ])),
            ..Default::default()
        };
        let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
        assert!(sql.contains("$1::\"post_status_enum\""), "{sql}");
    }

    #[test]
    fn test_qualified_column_without_a_spec_ignores_the_base_column() {
        // The base model's `name` is JSON; the joined `author.name` has no
        // spec (e.g. db_type="CITEXT"), so LIKE must not unwrap it as JSON.
        let ir = QueryIR {
            table: "posts".into(),
            cols: Some(vec!["id".into()]),
            filter_tree: Some(filter_with_column(
                "name",
                "author.name",
                "LIKE",
                rmpv_str("a%"),
            )),
            column_types: Some(HashMap::from([("name".into(), ColumnTypeSpec::Json)])),
            ..Default::default()
        };
        let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
        assert!(sql.contains(r#""author"."name" LIKE $1"#), "{sql}");
        assert!(!sql.contains("#>>"), "{sql}");
    }

    #[test]
    fn test_select_ilike_lookup() {
        let ir = QueryIR {
            table: "articles".into(),
            cols: Some(vec!["id".into()]),
            filter_tree: Some(filter_cond("title", "ILIKE", rmpv_str("%rust%"))),
            ..Default::default()
        };
        let (sql, params) = build_sql(&ir, Dialect::Postgres).unwrap();
        assert!(sql.to_uppercase().contains("LIKE"));
        assert!(!sql.contains("ESCAPE"));
        assert_eq!(params.len(), 1);
        match &params[0] {
            Value::String(Some(val)) => assert_eq!(val.as_str(), "%rust%"),
            other => panic!("unexpected parameter value {other:?}"),
        }
    }

    #[test]
    fn test_select_between_lookup() {
        let ir = QueryIR {
            table: "numbers".into(),
            cols: Some(vec!["value".into()]),
            filter_tree: Some(filter_cond(
                "value",
                "BETWEEN",
                rmpv_arr(vec![rmpv_int(1), rmpv_int(5)]),
            )),
            ..Default::default()
        };
        let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
        assert!(sql.to_uppercase().contains("BETWEEN"));
        assert!(sql.contains("$1") && sql.contains("$2"));
    }

    #[test]
    fn test_uuid_comparison_and_range_bindings() {
        let lower = "10000000-0000-4000-8000-000000000001";
        let upper = "90000000-0000-4000-8000-000000000003";
        let expected = [lower, upper];

        for dialect in [Dialect::Postgres] {
            for operator in [">", ">=", "<", "<=", "BETWEEN"] {
                let filter = if operator == "BETWEEN" {
                    filter_with_column(
                        "id",
                        "item.uuid_id",
                        operator,
                        rmpv_arr(vec![rmpv_str(lower), rmpv_str(upper)]),
                    )
                } else {
                    filter_with_column("id", "item.uuid_id", operator, rmpv_str(lower))
                };
                let ir = QueryIR {
                    table: "items".into(),
                    cols: Some(vec!["id".into()]),
                    column_types: Some(HashMap::from([(
                        "item.uuid_id".into(),
                        ColumnTypeSpec::Uuid,
                    )])),
                    filter_tree: Some(filter),
                    ..Default::default()
                };

                let (sql, params) = build_sql(&ir, dialect).unwrap();
                assert!(sql.contains(operator), "{dialect:?}: {sql}");
                assert_eq!(params.len(), if operator == "BETWEEN" { 2 } else { 1 });
                for (actual, value) in params.iter().zip(expected) {
                    match actual {
                        Value::Uuid(Some(uuid)) => assert_eq!(uuid.to_string(), value),
                        other => panic!("{dialect:?} {operator}: expected UUID, got {other:?}"),
                    }
                }
            }
        }
    }

    #[test]
    fn test_uuid_between_rejects_wrong_endpoint_count() {
        let ir = QueryIR {
            table: "items".into(),
            column_types: Some(HashMap::from([("id".into(), ColumnTypeSpec::Uuid)])),
            filter_tree: Some(filter_cond(
                "id",
                "BETWEEN",
                rmpv_arr(vec![rmpv_str("10000000-0000-4000-8000-000000000001")]),
            )),
            ..Default::default()
        };
        assert!(matches!(
            build_sql(&ir, Dialect::Postgres),
            Err(QueryError::InvalidQuery(_))
        ));
    }

    #[test]
    fn test_select_is_null_lookup() {
        let ir = QueryIR {
            table: "entries".into(),
            cols: Some(vec!["id".into()]),
            filter_tree: Some(filter_cond("deleted_at", "IS NULL", rmpv_null())),
            ..Default::default()
        };
        let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
        assert!(sql.to_uppercase().contains("IS NULL"));
    }

    #[test]
    fn test_cols_are_db_columns() {
        // Python now sends db_columns directly in cols (no column_mappings needed)
        let ir = QueryIR {
            table: "posts".into(),
            cols: Some(vec!["title_text".into()]),
            ..Default::default()
        };
        let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
        assert!(sql.contains("\"title_text\""), "unexpected SQL: {sql}");
    }

    #[test]
    fn test_join_spec_adds_left_join() {
        let join = JoinSpec {
            path: "author".into(),
            alias: "author".into(),
            parent: None,
            table: "authors".into(),
            source_column: "author_id".into(),
            target_column: "id".into(),
            result_prefix: "author".into(),
            columns: vec![
                JoinColumn {
                    field: "id".into(),
                    column: "id".into(),
                },
                JoinColumn {
                    field: "name".into(),
                    column: "name".into(),
                },
            ],
        };
        let ir = QueryIR {
            table: "posts".into(),
            cols: Some(vec!["title".into()]),
            joins: Some(vec![join]),
            ..Default::default()
        };
        let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
        let upper = sql.to_uppercase();
        assert!(upper.contains("LEFT JOIN"), "{}", sql);
        assert!(sql.contains("\"posts\".\"author_id\""), "{}", sql);
        assert!(sql.contains("\"author\".\"id\""), "{}", sql);
        assert!(sql.contains("author__id"), "{}", sql);
        assert!(sql.contains("author__name"), "{}", sql);
    }

    fn annotation(alias: &str, json: serde_json::Value) -> oxyde_codec::Annotation {
        oxyde_codec::Annotation {
            alias: alias.into(),
            expr: serde_json::from_value(json).unwrap(),
        }
    }

    fn eff_min() -> oxyde_codec::Annotation {
        annotation(
            "eff_min",
            serde_json::json!({
                "type": "func", "name": "greatest", "args": [
                    {"type": "func", "name": "coalesce", "args": [
                        {"type": "column", "name": "id"},
                        {"type": "value", "value": 1},
                    ]},
                    {"type": "value", "value": 1},
                ],
            }),
        )
    }

    fn author_join() -> JoinSpec {
        JoinSpec {
            path: "author".into(),
            alias: "author".into(),
            parent: None,
            table: "authors".into(),
            source_column: "author_id".into(),
            target_column: "id".into(),
            result_prefix: "author".into(),
            columns: vec![JoinColumn {
                field: "id".into(),
                column: "id".into(),
            }],
        }
    }

    #[test]
    fn test_select_annotation_follows_cols() {
        let ir = QueryIR {
            table: "posts".into(),
            cols: Some(vec!["id".into()]),
            annotations: Some(vec![eff_min()]),
            ..Default::default()
        };
        let (sql, params) = build_sql(&ir, Dialect::Postgres).unwrap();
        assert_eq!(
            sql,
            r#"SELECT "posts"."id" AS "id", GREATEST(COALESCE("id", $1), $2) AS "eff_min" FROM "posts""#
        );
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn test_select_annotation_lower_upper() {
        let name = || serde_json::json!({"type": "column", "name": "name"});
        let ir = QueryIR {
            table: "posts".into(),
            cols: Some(vec!["id".into()]),
            annotations: Some(vec![
                annotation(
                    "lo",
                    serde_json::json!({"type": "func", "name": "lower", "args": [name()]}),
                ),
                annotation(
                    "up",
                    serde_json::json!({"type": "func", "name": "upper", "args": [
                        {"type": "func", "name": "coalesce", "args": [
                            name(), {"type": "value", "value": "x"}]}]}),
                ),
            ]),
            ..Default::default()
        };
        let (sql, params) = build_sql(&ir, Dialect::Postgres).unwrap();
        assert_eq!(
            sql,
            r#"SELECT "posts"."id" AS "id", LOWER("name") AS "lo", UPPER(COALESCE("name", $1)) AS "up" FROM "posts""#
        );
        assert_eq!(params.len(), 1);
    }

    #[test]
    fn test_lower_upper_take_exactly_one_argument() {
        for (function, args) in [
            ("lower", serde_json::json!([])),
            (
                "upper",
                serde_json::json!([
                    {"type": "column", "name": "a"},
                    {"type": "column", "name": "b"},
                ]),
            ),
        ] {
            let ir = QueryIR {
                table: "posts".into(),
                cols: Some(vec!["id".into()]),
                annotations: Some(vec![annotation(
                    "x",
                    serde_json::json!({"type": "func", "name": function, "args": args}),
                )]),
                ..Default::default()
            };
            let error = build_sql(&ir, Dialect::Postgres).unwrap_err().to_string();
            assert!(
                error.contains("exactly one argument") || error.contains("at least one argument"),
                "{function}: {error}"
            );
        }
    }

    #[test]
    fn test_select_annotation_only_with_empty_cols() {
        let ir = QueryIR {
            table: "posts".into(),
            cols: Some(vec![]),
            annotations: Some(vec![eff_min()]),
            ..Default::default()
        };
        let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
        assert!(
            sql.starts_with(r#"SELECT GREATEST(COALESCE("id", $1), $2) AS "eff_min" FROM"#),
            "{sql}"
        );
    }

    #[test]
    fn test_annotation_alias_in_join_result_namespace_is_rejected() {
        let ir = QueryIR {
            table: "posts".into(),
            cols: Some(vec!["id".into()]),
            annotations: Some(vec![annotation(
                "author__x",
                serde_json::json!({"type": "value", "value": 1}),
            )]),
            joins: Some(vec![author_join()]),
            ..Default::default()
        };
        let err = build_sql(&ir, Dialect::Postgres).unwrap_err().to_string();
        assert!(err.contains("author__x"), "{err}");
        // Without a matching join the alias is fine.
        let ok = QueryIR { joins: None, ..ir };
        assert!(build_sql(&ok, Dialect::Postgres).is_ok());
    }

    #[test]
    fn test_select_annotation_after_star() {
        let ir = QueryIR {
            table: "posts".into(),
            annotations: Some(vec![eff_min()]),
            ..Default::default()
        };
        let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
        assert!(sql.starts_with("SELECT *, GREATEST("), "{sql}");
    }

    #[test]
    fn test_select_annotation_qualifies_columns_under_joins() {
        let ir = QueryIR {
            table: "posts".into(),
            cols: Some(vec!["id".into()]),
            annotations: Some(vec![eff_min()]),
            joins: Some(vec![author_join()]),
            ..Default::default()
        };
        let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
        assert!(sql.contains(r#"COALESCE("posts"."id", $1)"#), "{sql}");
    }

    #[test]
    fn test_order_by_annotation_alias_is_unqualified_under_joins() {
        let ir = QueryIR {
            table: "posts".into(),
            cols: Some(vec!["id".into()]),
            annotations: Some(vec![eff_min()]),
            joins: Some(vec![author_join()]),
            order_by: Some(vec![
                ("eff_min".into(), "DESC".into()),
                ("id".into(), "ASC".into()),
            ]),
            ..Default::default()
        };
        let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
        assert!(
            sql.contains(r#"ORDER BY "eff_min" DESC, "posts"."id" ASC"#),
            "{sql}"
        );
    }

    #[test]
    fn test_annotations_with_aggregates_is_an_error() {
        let ir = QueryIR {
            table: "posts".into(),
            aggregates: Some(vec![oxyde_codec::Aggregate {
                op: oxyde_codec::AggregateOp::Sum,
                field: Some("id".into()),
                alias: Some("total".into()),
                distinct: None,
            }]),
            annotations: Some(vec![eff_min()]),
            ..Default::default()
        };
        assert!(build_sql(&ir, Dialect::Postgres).is_err());
    }

    #[test]
    fn test_on_conflict_update_requires_values() {
        let ir = QueryIR {
            op: Operation::Insert,
            table: "users".into(),
            values: Some(HashMap::from([(
                "email".into(),
                rmpv_str("test@example.com"),
            )])),
            on_conflict: Some(OnConflict {
                columns: vec!["email".into()],
                action: ConflictAction::Update,
                update_values: None, // No values - should fail
            }),
            ..Default::default()
        };
        let result = build_sql(&ir, Dialect::Postgres);
        assert!(
            result.is_err(),
            "on_conflict Update without values should fail"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("update_values"),
            "error should mention update_values: {err}"
        );
    }

    #[test]
    fn test_ilike_uses_column_alias() {
        let ir = QueryIR {
            table: "products".into(),
            cols: Some(vec!["id".into(), "name".into()]),
            filter_tree: Some(filter_with_column(
                "name",
                "product_name",
                "ILIKE",
                rmpv_str("%test%"),
            )),
            ..Default::default()
        };
        let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
        assert!(
            sql.contains("product_name") || sql.contains("\"product_name\""),
            "ILIKE should use column alias 'product_name', got: {sql}"
        );
    }

    #[test]
    fn test_union_order_by_after_union() {
        let union_ir = QueryIR {
            table: "posts".into(),
            cols: Some(vec!["id".into(), "title".into()]),
            filter_tree: Some(filter_cond("published", "=", rmpv_int(1))),
            ..Default::default()
        };
        let ir = QueryIR {
            table: "posts".into(),
            cols: Some(vec!["id".into(), "title".into()]),
            filter_tree: Some(filter_cond("author_id", "=", rmpv_int(1))),
            union_query: Some(Box::new(union_ir)),
            order_by: Some(vec![("id".into(), "ASC".into())]),
            ..Default::default()
        };

        {
            let dialect = Dialect::Postgres;
            let (sql, _) = build_sql(&ir, dialect).unwrap();
            let union_pos = sql.find("UNION").expect("should contain UNION");
            let order_pos = sql.find("ORDER BY").expect("should contain ORDER BY");
            assert!(
                order_pos > union_pos,
                "{dialect:?}: ORDER BY should come after UNION.\nSQL: {sql}"
            );
        }
    }

    #[test]
    fn test_union_with_postgres_placeholders() {
        let union_ir = QueryIR {
            table: "posts".into(),
            cols: Some(vec!["id".into()]),
            filter_tree: Some(filter_cond("status", "=", rmpv_str("draft"))),
            ..Default::default()
        };
        let ir = QueryIR {
            table: "posts".into(),
            cols: Some(vec!["id".into()]),
            filter_tree: Some(filter_cond("status", "=", rmpv_str("active"))),
            union_query: Some(Box::new(union_ir)),
            ..Default::default()
        };
        let (sql, params) = build_sql(&ir, Dialect::Postgres).unwrap();
        assert!(sql.contains("$1"), "should have $1: {sql}");
        assert!(sql.contains("$2"), "should have $2: {sql}");
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn test_count_distinct() {
        let ir = QueryIR {
            table: "users".into(),
            aggregates: Some(vec![oxyde_codec::Aggregate {
                op: oxyde_codec::AggregateOp::Count,
                field: Some("email".into()),
                alias: Some("unique_emails".into()),
                distinct: Some(true),
            }]),
            ..Default::default()
        };
        let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
        assert!(
            sql.contains("COUNT(DISTINCT"),
            "should have COUNT(DISTINCT: {sql}"
        );
        assert!(sql.contains("unique_emails"), "should have alias: {sql}");
    }

    /// Guard against predicate/builder desync. The contract:
    /// - `RETURNING *` appears iff `emits_returning` says so;
    ///   `RETURNING "<pk>"` (the driver reads generated PKs from rows);
    #[test]
    fn test_emits_returning_matches_builder_output() {
        let ops: Vec<QueryIR> = vec![
            QueryIR {
                op: Operation::Insert,
                table: "t".into(),
                values: Some(HashMap::from([("name".into(), rmpv_str("a"))])),
                ..Default::default()
            },
            QueryIR {
                op: Operation::Update,
                table: "t".into(),
                values: Some(HashMap::from([("name".into(), rmpv_str("a"))])),
                filter_tree: Some(filter_cond("id", "=", rmpv_int(1))),
                ..Default::default()
            },
            QueryIR {
                op: Operation::Delete,
                table: "t".into(),
                filter_tree: Some(filter_cond("id", "=", rmpv_int(1))),
                ..Default::default()
            },
        ];

        for base in ops {
            for returning in [None, Some(false), Some(true)] {
                {
                    let dialect = Dialect::Postgres;
                    let mut ir = base.clone();
                    ir.returning = returning;
                    let (sql, _) = build_sql(&ir, dialect).unwrap();
                    let full = emits_returning(&ir, dialect);
                    assert_eq!(
                        full,
                        sql.contains("RETURNING *"),
                        "predicate/builder desync: op={:?} returning={:?} dialect={:?} sql={}",
                        ir.op,
                        returning,
                        dialect,
                        sql
                    );
                    let pk_only_insert =
                        ir.op == Operation::Insert && matches!(dialect, Dialect::Postgres);
                    assert_eq!(
                        full || pk_only_insert,
                        sql.contains("RETURNING"),
                        "pk-RETURNING desync: op={:?} returning={:?} dialect={:?} sql={}",
                        ir.op,
                        returning,
                        dialect,
                        sql
                    );
                }
            }
        }
    }

    #[test]
    fn test_count_without_distinct() {
        let ir = QueryIR {
            table: "users".into(),
            aggregates: Some(vec![oxyde_codec::Aggregate {
                op: oxyde_codec::AggregateOp::Count,
                field: Some("email".into()),
                alias: Some("total".into()),
                distinct: None,
            }]),
            ..Default::default()
        };
        let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
        assert!(!sql.contains("DISTINCT"), "should NOT have DISTINCT: {sql}");
    }

    #[test]
    fn postgres_string_lookups_preserve_escape() {
        let ir = QueryIR {
            table: "articles".into(),
            filter_tree: Some(filter_with_escape(
                "title",
                "LIKE",
                rmpv_str(r"%snake\_case%"),
                "\\",
            )),
            ..Default::default()
        };
        let (sql, params) = build_sql(&ir, Dialect::Postgres).unwrap();
        assert!(sql.contains("LIKE") && sql.contains("ESCAPE"), "{sql}");
        assert_eq!(params.len(), 1);
    }

    // ── bind_types: binding-only hints for columns without a column spec ──

    fn spec_json(json: serde_json::Value) -> ColumnTypeSpec {
        serde_json::from_value(json).unwrap()
    }

    fn money_update(column_types: Option<HashMap<String, ColumnTypeSpec>>) -> QueryIR {
        QueryIR {
            op: Operation::Update,
            table: "prices".into(),
            values: Some(HashMap::from([("amount".into(), rmpv_str("2.50"))])),
            filter_tree: Some(filter_cond("id", "=", rmpv_int(1))),
            column_types,
            bind_types: Some(HashMap::from([(
                "amount".into(),
                spec_json(serde_json::json!({"kind": "decimal"})),
            )])),
            ..Default::default()
        }
    }

    #[test]
    fn test_bind_types_bind_a_column_without_a_spec() {
        // A MONEY column has no column spec, so a Decimal (msgpack-encoded as a
        // string) would bind as text; the hint recovers the Decimal.
        let (_, params) = build_sql(&money_update(None), Dialect::Postgres).unwrap();
        assert!(
            matches!(&params[0], Value::Decimal(Some(_))),
            "expected Decimal, got {params:?}"
        );
    }

    #[test]
    fn test_bind_types_apply_to_filters() {
        let ir = QueryIR {
            table: "prices".into(),
            cols: Some(vec!["id".into()]),
            filter_tree: Some(filter_cond("amount", "=", rmpv_str("2.50"))),
            bind_types: Some(HashMap::from([(
                "amount".into(),
                spec_json(serde_json::json!({"kind": "decimal"})),
            )])),
            ..Default::default()
        };
        let (_, params) = build_sql(&ir, Dialect::Postgres).unwrap();
        assert!(
            matches!(&params[0], Value::Decimal(Some(_))),
            "expected Decimal, got {params:?}"
        );
    }

    #[test]
    fn test_column_types_win_over_bind_types() {
        let ir = money_update(Some(HashMap::from([(
            "amount".into(),
            spec_json(serde_json::json!({"kind": "text"})),
        )])));
        let (_, params) = build_sql(&ir, Dialect::Postgres).unwrap();
        assert!(
            matches!(&params[0], Value::String(Some(_))),
            "expected String, got {params:?}"
        );
    }

    #[test]
    fn test_bind_types_reach_union_subqueries() {
        let sub = QueryIR {
            table: "prices".into(),
            cols: Some(vec!["id".into()]),
            filter_tree: Some(filter_cond("amount", "=", rmpv_str("2.50"))),
            bind_types: Some(HashMap::from([(
                "amount".into(),
                spec_json(serde_json::json!({"kind": "decimal"})),
            )])),
            ..Default::default()
        };
        let ir = QueryIR {
            table: "prices".into(),
            cols: Some(vec!["id".into()]),
            union_query: Some(Box::new(sub)),
            ..Default::default()
        };
        let (_, params) = build_sql(&ir, Dialect::Postgres).unwrap();
        assert!(
            matches!(&params[0], Value::Decimal(Some(_))),
            "expected Decimal, got {params:?}"
        );
    }

    // A MONEY column reaches the builders as a binding-only hint, never as a
    // real column spec: it must not type a sibling NULL (a NUMERIC parameter
    // next to a MONEY column fails in PostgreSQL).

    #[test]
    fn test_bind_types_do_not_type_a_sibling_null_in_annotations() {
        let ir = QueryIR {
            table: "prices".into(),
            annotations: Some(vec![annotation(
                "a",
                serde_json::json!({
                    "type": "func", "name": "coalesce", "args": [
                        {"type": "column", "name": "amount"},
                        {"type": "value", "value": null},
                    ],
                }),
            )]),
            bind_types: Some(HashMap::from([(
                "amount".into(),
                spec_json(serde_json::json!({"kind": "decimal"})),
            )])),
            ..Default::default()
        };
        let (sql, params) = build_sql(&ir, Dialect::Postgres).unwrap();
        assert!(sql.contains(r#"COALESCE("amount", NULL)"#), "{sql}");
        assert!(params.is_empty(), "{params:?}");
    }

    #[test]
    fn test_bind_types_do_not_type_a_null_in_an_update_expression() {
        let wrapper = rmpv::ext::to_value(serde_json::json!({"__expr__": {
            "type": "func", "name": "coalesce", "args": [
                {"type": "column", "name": "amount"},
                {"type": "value", "value": null},
            ],
        }}))
        .unwrap();
        let ir = QueryIR {
            op: Operation::Update,
            table: "prices".into(),
            values: Some(HashMap::from([("amount".into(), wrapper)])),
            filter_tree: Some(filter_cond("id", "=", rmpv_int(1))),
            bind_types: Some(HashMap::from([(
                "amount".into(),
                spec_json(serde_json::json!({"kind": "decimal"})),
            )])),
            ..Default::default()
        };
        let (sql, params) = build_sql(&ir, Dialect::Postgres).unwrap();
        assert!(sql.contains(r#"COALESCE("amount", NULL)"#), "{sql}");
        assert!(
            !params.iter().any(|p| matches!(p, Value::Decimal(_))),
            "{params:?}"
        );
    }

    #[test]
    fn test_column_types_still_type_a_sibling_null_despite_bind_types() {
        // The same column as a real spec (a hint for it is ignored) does.
        let decimal = spec_json(serde_json::json!({"kind": "decimal"}));
        let ir = QueryIR {
            table: "prices".into(),
            annotations: Some(vec![annotation(
                "a",
                serde_json::json!({
                    "type": "func", "name": "coalesce", "args": [
                        {"type": "column", "name": "amount"},
                        {"type": "value", "value": null},
                    ],
                }),
            )]),
            column_types: Some(HashMap::from([("amount".into(), decimal.clone())])),
            bind_types: Some(HashMap::from([("amount".into(), decimal)])),
            ..Default::default()
        };
        let (_, params) = build_sql(&ir, Dialect::Postgres).unwrap();
        assert!(matches!(&params[0], Value::Decimal(None)), "{params:?}");
    }
}

#[cfg(test)]
mod postgres_regressions {
    use super::*;
    #[test]
    fn postgres_is_the_only_dialect() {
        assert_eq!(
            serde_json::to_string(&Dialect::Postgres).unwrap(),
            "\"postgres\""
        );
        for name in ["mysql", "sqlite", "mariadb", "unknown"] {
            assert!(serde_json::from_str::<Dialect>(&format!("\"{name}\"")).is_err());
        }
    }
}
