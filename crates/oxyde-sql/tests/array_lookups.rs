//! SQL generation for array lookups (ARRAY_CONTAINS / ARRAY_CONTAINED_BY /
//! ARRAY_OVERLAP) on PostgreSQL.

use std::collections::HashMap;

use oxyde_codec::{ColumnTypeSpec, Filter, FilterNode, JoinColumn, JoinSpec, QueryIR};
use oxyde_sql::{build_sql, Dialect, QueryError};
use sea_query::value::ArrayType;
use sea_query::Value;

const OPS: [&str; 3] = ["ARRAY_CONTAINS", "ARRAY_CONTAINED_BY", "ARRAY_OVERLAP"];

fn array_of(item: ColumnTypeSpec) -> ColumnTypeSpec {
    ColumnTypeSpec::Array {
        item: Box::new(item),
    }
}

fn s(v: &str) -> rmpv::Value {
    rmpv::Value::String(v.into())
}

fn arr(items: Vec<rmpv::Value>) -> rmpv::Value {
    rmpv::Value::Array(items)
}

struct Lookup {
    column: Option<&'static str>,
    operator: &'static str,
    value: rmpv::Value,
    /// Spec of the filtered column, keyed in `column_types` by its name.
    spec: ColumnTypeSpec,
    db_type: Option<&'static str>,
    /// Other `column_types` entries.
    table_types: HashMap<String, ColumnTypeSpec>,
}

impl Lookup {
    fn new(operator: &'static str, value: rmpv::Value, spec: ColumnTypeSpec) -> Self {
        Self {
            column: None,
            operator,
            value,
            spec,
            db_type: None,
            table_types: HashMap::new(),
        }
    }

    fn ir(&self) -> QueryIR {
        let column = self.column.unwrap_or("tags");
        let mut column_types = self.table_types.clone();
        column_types.insert(column.into(), self.spec.clone());
        QueryIR {
            table: "posts".into(),
            cols: Some(vec!["id".into()]),
            column_types: Some(column_types),
            column_db_types: self
                .db_type
                .map(|db_type| HashMap::from([(column.into(), db_type.into())])),
            filter_tree: Some(FilterNode::Condition(Filter {
                field: "tags".into(),
                operator: self.operator.into(),
                value: self.value.clone(),
                column: self.column.map(Into::into),
                escape: None,
            })),
            ..Default::default()
        }
    }

    fn build(&self, dialect: Dialect) -> (String, Vec<Value>) {
        build_sql(&self.ir(), dialect).unwrap()
    }
}

#[test]
fn postgres_typed_arrays_use_native_operators_with_cast_operands() {
    for (op, sql_op) in OPS.iter().zip(["@>", "<@", "&&"]) {
        let (sql, params) = Lookup::new(op, arr(vec![s("rust")]), array_of(ColumnTypeSpec::Text))
            .build(Dialect::Postgres);
        assert!(
            sql.ends_with(&format!(r#"WHERE ("tags" {sql_op} $1::TEXT[])"#)),
            "{sql}"
        );
        assert_eq!(
            params,
            [Value::Array(
                ArrayType::String,
                Some(Box::new(vec![Value::String(Some(Box::new("rust".into())))]))
            )]
        );
    }
}

#[test]
fn postgres_elements_the_column_type_cannot_hold_match_nothing() {
    let lookup = |op: &'static str| {
        let value = arr(vec![1.into(), 100_000.into()]);
        let mut lookup = Lookup::new(op, value, array_of(ColumnTypeSpec::BigInteger));
        lookup.db_type = Some("SMALLINT[]");
        lookup
    };
    // No row holds 100000, so `contains` matches nothing (NULL stays NULL).
    let (sql, params) = lookup("ARRAY_CONTAINS").build(Dialect::Postgres);
    assert!(
        sql.ends_with(r#"(CASE WHEN "tags" IS NULL THEN NULL ELSE FALSE END)"#),
        "{sql}"
    );
    assert!(params.is_empty());
    // The other lookups leave it out.
    for (op, sql_op) in [("ARRAY_CONTAINED_BY", "<@"), ("ARRAY_OVERLAP", "&&")] {
        let (sql, params) = lookup(op).build(Dialect::Postgres);
        assert!(
            sql.contains(&format!(r#"("tags" {sql_op} $1::SMALLINT[])"#)),
            "{op}: {sql}"
        );
        assert!(
            matches!(&params[..], [Value::Array(_, Some(elements))] if elements.len() == 1),
            "{op}: {params:?}"
        );
    }
}

#[test]
fn postgres_casts_follow_the_real_column_type_without_modifiers() {
    let cases = [
        (array_of(ColumnTypeSpec::BigInteger), None, "BIGINT[]"),
        (
            array_of(ColumnTypeSpec::BigInteger),
            Some("INTEGER[]"),
            "INTEGER[]",
        ),
        (
            array_of(ColumnTypeSpec::String { length: Some(20) }),
            None,
            "VARCHAR[]",
        ),
        (
            array_of(ColumnTypeSpec::String { length: Some(20) }),
            Some("VARCHAR(20)[]"),
            "VARCHAR[]",
        ),
        (
            array_of(ColumnTypeSpec::String { length: Some(3) }),
            Some("CHAR(3)[]"),
            "BPCHAR[]",
        ),
        (
            array_of(ColumnTypeSpec::Double),
            Some("FLOAT(24)[]"),
            "REAL[]",
        ),
        (
            array_of(ColumnTypeSpec::Double),
            Some("FLOAT(53)[]"),
            "DOUBLE PRECISION[]",
        ),
        (
            array_of(ColumnTypeSpec::Decimal {
                precision: Some(10),
                scale: Some(2),
            }),
            Some("NUMERIC(10,2)[]"),
            "NUMERIC[]",
        ),
        (array_of(ColumnTypeSpec::DateTimeUtc), None, "TIMESTAMPTZ[]"),
        (array_of(ColumnTypeSpec::Uuid), None, "UUID[]"),
        (array_of(ColumnTypeSpec::JsonBinary), None, "JSONB[]"),
        (
            array_of(ColumnTypeSpec::Enum {
                name: "app.post_status".into(),
                values: vec!["draft".into()],
            }),
            None,
            r#""app"."post_status"[]"#,
        ),
    ];
    for (spec, db_type, cast) in cases {
        let mut lookup = Lookup::new("ARRAY_CONTAINS", arr(vec![]), spec);
        lookup.db_type = db_type;
        let (sql, _) = lookup.build(Dialect::Postgres);
        assert!(
            sql.ends_with(&format!(r#"("tags" @> $1::{cast})"#)),
            "{sql}"
        );
    }
}

#[test]
fn postgres_null_operand_elements_switch_to_null_aware_unnest() {
    let (sql, params) = Lookup::new(
        "ARRAY_CONTAINS",
        arr(vec![s("a"), rmpv::Value::Nil]),
        array_of(ColumnTypeSpec::Text),
    )
    .build(Dialect::Postgres);
    assert!(
        sql.contains(r#"CASE WHEN "tags" IS NULL THEN NULL"#),
        "{sql}"
    );
    assert!(sql.contains("unnest($1::TEXT[])"), "{sql}");
    // The column is read once, by a holder outside the unnest subqueries,
    // so a column named like their aliases cannot be shadowed.
    assert!(
        sql.contains(r#"FROM (SELECT "tags" AS "__oxyde_c") AS "__oxyde_h")"#),
        "{sql}"
    );
    assert!(sql.contains(r#"unnest("__oxyde_h"."__oxyde_c")"#), "{sql}");
    assert_eq!(sql.matches(r#""tags""#).count(), 2, "{sql}");
    assert!(sql.contains("IS NOT DISTINCT FROM"), "{sql}");
    assert!(!sql.contains("@>"), "{sql}");
    assert_eq!(params.len(), 1);
}

#[test]
fn postgres_json_null_operand_elements_switch_to_null_aware_unnest() {
    // None is stored as an SQL NULL element, and rows written before
    // strings were stored as JSON strings may hold jsonb nulls (from
    // "null"); both load as None, so a None operand must match either.
    let spec = array_of(ColumnTypeSpec::JsonBinary);
    for (op, sql_op) in OPS.iter().zip(["@>", "<@", "&&"]) {
        let (sql, _) =
            Lookup::new(op, arr(vec![rmpv::Value::Nil]), spec.clone()).build(Dialect::Postgres);
        assert!(sql.contains("unnest($1::JSONB[])"), "{op}: {sql}");
        assert!(sql.contains("COALESCE"), "{op}: {sql}");
        assert!(!sql.contains(sql_op), "{op}: {sql}");

        // "null" is a string.
        let (sql, _) = Lookup::new(op, arr(vec![s("null")]), spec.clone()).build(Dialect::Postgres);
        assert!(sql.contains(sql_op), "{op}: {sql}");
        assert!(!sql.contains("unnest"), "{op}: {sql}");
    }
}

#[test]
fn postgres_json_columns_compare_whole_elements_behind_a_type_guard() {
    let spec = ColumnTypeSpec::JsonBinary;
    let (sql, params) =
        Lookup::new("ARRAY_CONTAINS", arr(vec![1.into()]), spec.clone()).build(Dialect::Postgres);
    // GIN-usable containment prefilter, then the exact check.
    assert!(sql.contains(r#"("tags" @> $1::jsonb AND CASE"#), "{sql}");
    assert!(
        sql.contains(r#"WHEN jsonb_typeof("tags") = 'array' THEN"#),
        "{sql}"
    );
    assert!(sql.contains("jsonb_array_elements($2::jsonb)"), "{sql}");
    assert!(
        sql.contains(r#"jsonb_array_elements("__oxyde_h"."__oxyde_c")"#),
        "{sql}"
    );
    assert!(
        sql.contains(r#"FROM (SELECT "tags" AS "__oxyde_c") AS "__oxyde_h")"#),
        "{sql}"
    );
    assert!(sql.contains("ELSE FALSE END"), "{sql}");
    assert_eq!(params.len(), 2);
    assert!(matches!(&params[0], Value::Json(Some(j)) if j.to_string() == "[1]"));

    for op in ["ARRAY_CONTAINED_BY", "ARRAY_OVERLAP"] {
        let (sql, _) = Lookup::new(op, arr(vec![1.into()]), spec.clone()).build(Dialect::Postgres);
        assert!(!sql.contains("@>"), "{sql}");
        assert!(sql.contains("jsonb_array_elements"), "{sql}");
    }
}

#[test]
fn postgres_json_db_type_casts_the_column_to_jsonb() {
    let mut lookup = Lookup::new("ARRAY_OVERLAP", arr(vec![1.into()]), ColumnTypeSpec::Json);
    lookup.db_type = Some("JSON");
    let (sql, _) = lookup.build(Dialect::Postgres);
    assert!(
        sql.contains(r#"CASE WHEN "tags" IS NULL THEN NULL"#),
        "{sql}"
    );
    assert!(sql.contains(r#"jsonb_typeof(("tags")::jsonb)"#), "{sql}");
    assert!(
        sql.contains(r#"(SELECT ("tags")::jsonb AS "__oxyde_c") AS "__oxyde_h""#),
        "{sql}"
    );
}

#[test]
fn qualified_column_type_wins_over_same_named_base_column() {
    // A joined model's `tags` is an array; the base model's `tags` is text.
    let mut lookup = Lookup::new(
        "ARRAY_CONTAINS",
        arr(vec![s("x")]),
        array_of(ColumnTypeSpec::Text),
    );
    lookup.column = Some("author.tags");
    lookup.table_types = HashMap::from([("tags".into(), ColumnTypeSpec::Text)]);
    let (sql, _) = lookup.build(Dialect::Postgres);
    assert!(sql.contains(r#"("author"."tags" @> $1::TEXT[])"#), "{sql}");
}

#[test]
fn qualified_column_never_takes_the_base_columns_type() {
    // The query has no spec for `sched.tags` (an UPDATE carries no joins);
    // the base model's same-named `tags` array must not stand in for it.
    let mut ir = Lookup::new(
        "ARRAY_CONTAINS",
        arr(vec![s("x")]),
        array_of(ColumnTypeSpec::Text),
    )
    .ir();
    if let Some(FilterNode::Condition(filter)) = ir.filter_tree.as_mut() {
        filter.column = Some("sched.tags".into());
    }
    {
        let dialect = Dialect::Postgres;
        let err = build_sql(&ir, dialect).unwrap_err().to_string();
        assert!(
            err.contains("'sched.tags' needs its column type"),
            "{dialect:?}: {err}"
        );
    }
}

#[test]
fn joined_select_qualifies_the_array_column() {
    let mut lookup = Lookup::new(
        "ARRAY_CONTAINS",
        arr(vec![s("x")]),
        array_of(ColumnTypeSpec::Text),
    );
    lookup.column = Some("tags");
    let mut ir = lookup.ir();
    ir.joins = Some(vec![JoinSpec {
        path: "author".into(),
        alias: "author".into(),
        parent: None,
        table: "authors".into(),
        source_column: "author_id".into(),
        target_column: "id".into(),
        result_prefix: "author".into(),
        columns: vec![JoinColumn {
            field: "name".into(),
            column: "name".into(),
        }],
    }]);
    let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
    assert!(sql.contains(r#"("posts"."tags" @> $1::TEXT[])"#), "{sql}");
}

#[test]
fn invalid_operands_are_rejected() {
    let text_array = array_of(ColumnTypeSpec::Text);
    let int_array = array_of(ColumnTypeSpec::BigInteger);
    let cases = [
        Lookup::new("ARRAY_CONTAINS", s("rust"), text_array.clone()),
        Lookup::new("ARRAY_CONTAINS", rmpv::Value::Nil, text_array.clone()),
        Lookup::new("ARRAY_CONTAINS", arr(vec![1.into()]), text_array),
        Lookup::new("ARRAY_OVERLAP", arr(vec![s("1")]), int_array.clone()),
        Lookup::new(
            "ARRAY_OVERLAP",
            arr(vec![rmpv::Value::Boolean(true)]),
            int_array,
        ),
        Lookup::new(
            "ARRAY_CONTAINS",
            arr(vec![rmpv::Value::Ext(1, vec![0])]),
            ColumnTypeSpec::Json,
        ),
    ];
    for lookup in cases {
        {
            let dialect = Dialect::Postgres;
            let err = build_sql(&lookup.ir(), dialect).unwrap_err();
            assert!(matches!(err, QueryError::InvalidQuery(_)), "{err}");
        }
    }
}

#[test]
fn array_lookups_cannot_target_aggregates() {
    let mut ir = Lookup::new(
        "ARRAY_CONTAINS",
        arr(vec![]),
        array_of(ColumnTypeSpec::Text),
    )
    .ir();
    ir.having = ir.filter_tree.take();
    ir.group_by = Some(vec!["id".into()]);
    ir.aggregates = Some(vec![oxyde_codec::Aggregate {
        op: oxyde_codec::AggregateOp::Count,
        field: Some("id".into()),
        alias: Some("tags".into()),
        distinct: None,
    }]);
    let err = build_sql(&ir, Dialect::Postgres).unwrap_err().to_string();
    assert!(err.contains("aggregate"), "{err}");
}

#[test]
fn negation_wraps_the_whole_predicate() {
    let mut ir = Lookup::new(
        "ARRAY_CONTAINS",
        arr(vec![s("x")]),
        array_of(ColumnTypeSpec::Text),
    )
    .ir();
    ir.filter_tree = Some(FilterNode::Not {
        condition: Box::new(ir.filter_tree.take().unwrap()),
    });
    let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
    assert!(sql.contains(r#"NOT (("tags" @> $1::TEXT[]))"#), "{sql}");
}
