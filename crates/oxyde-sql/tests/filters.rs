//! SQL generation for filters on array and JSON columns on PostgreSQL, and
//! for HAVING conditions on aggregates.

use std::collections::HashMap;

use oxyde_codec::{
    Aggregate, AggregateOp, Annotation, ColumnTypeSpec, Filter, FilterNode, JoinColumn, JoinSpec,
    LockType, Operation, QueryIR, ScalarExpr,
};
use oxyde_sql::{build_sql, Dialect, QueryError};
use rust_decimal::Decimal;
use sea_query::Value;

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

fn filter(operator: &str, value: rmpv::Value) -> FilterNode {
    FilterNode::Condition(Filter {
        field: "tags".into(),
        operator: operator.into(),
        value,
        column: None,
        escape: None,
    })
}

/// One condition on `posts.tags`, whose spec is `spec`.
struct Condition {
    operator: &'static str,
    value: rmpv::Value,
    spec: ColumnTypeSpec,
    db_type: Option<&'static str>,
}

impl Condition {
    fn new(operator: &'static str, value: rmpv::Value, spec: ColumnTypeSpec) -> Self {
        Self {
            operator,
            value,
            spec,
            db_type: None,
        }
    }

    fn ir(&self) -> QueryIR {
        QueryIR {
            table: "posts".into(),
            cols: Some(vec!["id".into()]),
            column_types: Some(HashMap::from([("tags".into(), self.spec.clone())])),
            column_db_types: self
                .db_type
                .map(|db_type| HashMap::from([("tags".into(), db_type.into())])),
            filter_tree: Some(filter(self.operator, self.value.clone())),
            ..Default::default()
        }
    }

    fn build(&self, dialect: Dialect) -> (String, Vec<Value>) {
        build_sql(&self.ir(), dialect).unwrap()
    }

    fn error(&self, dialect: Dialect) -> String {
        build_sql(&self.ir(), dialect).unwrap_err().to_string()
    }
}

fn array_override(
    operator: &'static str,
    value: rmpv::Value,
    item: ColumnTypeSpec,
    db_type: &'static str,
) -> Condition {
    let mut condition = Condition::new(operator, value, array_of(item));
    condition.db_type = Some(db_type);
    condition
}

fn int_array_override(operator: &'static str, value: rmpv::Value) -> Condition {
    array_override(operator, value, ColumnTypeSpec::BigInteger, "INTEGER[]")
}

#[test]
fn postgres_casts_array_values_to_the_override_for_every_operator() {
    let ints = || arr(vec![1.into(), 2.into()]);
    for (operator, sql_op) in [("=", "="), ("!=", "<>"), ("<", "<"), (">=", ">=")] {
        let (sql, params) = int_array_override(operator, ints()).build(Dialect::Postgres);
        assert!(
            sql.ends_with(&format!(r#""tags" {sql_op} ($1::INTEGER[])"#)),
            "{sql}"
        );
        assert_eq!(params.len(), 1);
    }
    let (sql, params) =
        int_array_override("IN", arr(vec![ints(), arr(vec![3.into()])])).build(Dialect::Postgres);
    assert!(
        sql.ends_with(r#""tags" IN ($1::INTEGER[], $2::INTEGER[])"#),
        "{sql}"
    );
    assert_eq!(params.len(), 2);
}

#[test]
fn values_the_array_type_cannot_hold_equal_nothing() {
    let too_big = || arr(vec![1.into(), (1_i64 << 31).into()]);

    // `=` is false and `!=` true, NULL for a NULL column either way.
    let (sql, params) = int_array_override("=", too_big()).build(Dialect::Postgres);
    assert!(
        sql.ends_with(r#"(CASE WHEN "tags" IS NULL THEN NULL ELSE FALSE END)"#),
        "{sql}"
    );
    assert_eq!(params, [] as [sea_query::Value; 0]);
    let (sql, _) = int_array_override("!=", too_big()).build(Dialect::Postgres);
    assert!(
        sql.ends_with(r#"(CASE WHEN "tags" IS NULL THEN NULL ELSE TRUE END)"#),
        "{sql}"
    );

    // IN drops them, and matches nothing when none are left.
    let (sql, params) = int_array_override("IN", arr(vec![arr(vec![1.into()]), too_big()]))
        .build(Dialect::Postgres);
    assert!(sql.ends_with(r#""tags" IN ($1::INTEGER[])"#), "{sql}");
    assert_eq!(params.len(), 1);
    let (sql, _) = int_array_override("IN", arr(vec![too_big()])).build(Dialect::Postgres);
    assert!(sql.ends_with("ELSE FALSE END)"), "{sql}");

    // An ordering against one has no clear answer.
    for operator in ["<", ">="] {
        let err = int_array_override(operator, too_big()).error(Dialect::Postgres);
        assert!(
            err.contains("2147483648 is out of range for INTEGER[]"),
            "{err}"
        );
    }

    // NaN and the infinities fit REAL; 1e300 does not.
    let real = |x: f64| {
        array_override(
            "=",
            arr(vec![rmpv::Value::F64(x)]),
            ColumnTypeSpec::Double,
            "REAL[]",
        )
    };
    for x in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let (sql, _) = real(x).build(Dialect::Postgres);
        assert!(sql.ends_with(r#""tags" = ($1::REAL[])"#), "{x}: {sql}");
    }
    let (sql, _) = real(1e300).build(Dialect::Postgres);
    assert!(sql.ends_with("ELSE FALSE END)"), "{sql}");
}

#[test]
fn array_elements_of_another_type_are_an_error_on_postgres() {
    // PostgreSQL would bind each of these elements as NULL, and NULL
    // elements compare equal there, so the filter would match rows holding
    // given, exactly as a saved value, so a union element type
    // (`list[str | int]`, whose spec is its first member's) still matches.
    let decimal = || ColumnTypeSpec::Decimal {
        precision: None,
        scale: None,
    };
    let beyond_i64 = || arr(vec![rmpv::Value::from(1_u64 << 63)]);
    let cases = [
        (arr(vec![rmpv::Value::F64(1.5)]), decimal()),
        (arr(vec![rmpv::Value::F64(1.5)]), ColumnTypeSpec::BigInteger),
        (arr(vec![s("2.5")]), ColumnTypeSpec::Double),
        (arr(vec![true.into()]), ColumnTypeSpec::BigInteger),
        (beyond_i64(), ColumnTypeSpec::BigInteger),
        (arr(vec![s("a"), 1.into()]), ColumnTypeSpec::Text),
    ];
    for (value, item) in cases {
        for (operator, value) in [
            ("=", value.clone()),
            ("!=", value.clone()),
            (">", value.clone()),
            ("IN", arr(vec![value.clone()])),
            ("BETWEEN", arr(vec![value.clone(), value.clone()])),
        ] {
            let condition = Condition::new(operator, value, array_of(item.clone()));
            let err = condition.error(Dialect::Postgres);
            assert!(
                err.contains("does not match the array's element type"),
                "{item:?} {operator}: {err}"
            );
        }
    }

    // An integer beyond 64 bits is not just out of range for INTEGER[]:
    // it does not bind as an integer at all.
    let err = int_array_override("=", beyond_i64()).error(Dialect::Postgres);
    assert!(err.contains("9223372036854775808"), "{err}");

    // NULL elements fit any array, and integers convert to the element type.
    for value in [arr(vec![rmpv::Value::Nil, 1.into()]), arr(vec![2.into()])] {
        for item in [decimal(), ColumnTypeSpec::Double] {
            {
                let dialect = Dialect::Postgres;
                Condition::new("=", value.clone(), array_of(item.clone())).build(dialect);
            }
        }
    }
}

#[test]
fn unreadable_array_overrides_are_an_error() {
    let ints = || arr(vec![1.into()]);
    for db_type in ["INTEGER[][]", "VARCHAR(10[]"] {
        let mut condition = int_array_override("=", ints());
        condition.db_type = Some(db_type);
        let err = condition.error(Dialect::Postgres);
        assert!(err.contains(db_type), "{err}");
    }
    let json = array_override("=", arr(vec![s("a")]), ColumnTypeSpec::Json, "JSON[]");
    assert!(json.error(Dialect::Postgres).contains("use jsonb[]"));

    // Only filters that cast a value need the override.
    let mut isnull = int_array_override("IS NULL", rmpv::Value::Nil);
    isnull.db_type = Some("VARCHAR(10[]");
    let (sql, _) = isnull.build(Dialect::Postgres);
    assert!(sql.ends_with(r#""tags" IS NULL"#), "{sql}");
}

#[test]
fn array_values_without_an_array_override_are_not_cast() {
    let ints = || arr(vec![1.into()]);
    let (sql, _) =
        Condition::new("=", ints(), array_of(ColumnTypeSpec::BigInteger)).build(Dialect::Postgres);
    assert!(sql.ends_with(r#""tags" = $1"#), "{sql}");
    let mut condition = int_array_override("=", ints());
    condition.db_type = Some("JSONB");
    let (sql, _) = condition.build(Dialect::Postgres);
    assert!(sql.ends_with(r#""tags" = $1"#), "{sql}");
}

#[test]
fn joined_columns_use_their_own_db_type() {
    // The base `tags` has no override; the joined `a.tags` is SMALLINT[].
    let mut author = author_join();
    author.alias = "a".into();
    author.columns.push(JoinColumn {
        field: "tags".into(),
        column: "tags".into(),
    });
    let ir = QueryIR {
        table: "posts".into(),
        joins: Some(vec![author]),
        cols: Some(vec!["id".into()]),
        column_types: Some(HashMap::from([
            ("tags".into(), array_of(ColumnTypeSpec::BigInteger)),
            ("a.tags".into(), array_of(ColumnTypeSpec::BigInteger)),
        ])),
        column_db_types: Some(HashMap::from([("a.tags".into(), "SMALLINT[]".into())])),
        filter_tree: Some(FilterNode::And {
            conditions: vec![
                filter("=", arr(vec![1.into()])),
                FilterNode::Condition(Filter {
                    field: "tags".into(),
                    operator: "=".into(),
                    value: arr(vec![1.into()]),
                    column: Some("a.tags".into()),
                    escape: None,
                }),
            ],
        }),
        ..Default::default()
    };
    let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
    assert!(
        sql.contains(r#""posts"."tags" = $1 AND "a"."tags" = ($2::SMALLINT[])"#),
        "{sql}"
    );
}

#[test]
fn having_uses_the_aggregate_not_a_same_named_column() {
    // `tags` is both a JSON array column with an override and the alias of
    // MAX("name"): the HAVING condition is on the aggregate.
    for (operator, expected) in [
        ("LIKE", r#"MAX("name") LIKE $1"#),
        ("ILIKE", r#"LOWER(MAX("name")) LIKE $1"#),
        ("=", r#"MAX("name") = $1"#),
    ] {
        let ir = QueryIR {
            table: "posts".into(),
            cols: Some(vec!["status".into()]),
            column_types: Some(HashMap::from([(
                "tags".into(),
                array_of(ColumnTypeSpec::Json),
            )])),
            column_db_types: Some(HashMap::from([("tags".into(), "JSONB[]".into())])),
            aggregates: Some(vec![Aggregate {
                op: AggregateOp::Max,
                field: Some("name".into()),
                alias: Some("tags".into()),
                distinct: None,
            }]),
            group_by: Some(vec!["status".into()]),
            having: Some(filter(operator, s("x"))),
            ..Default::default()
        };
        let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
        assert!(sql.contains(&format!("HAVING {expected}")), "{sql}");
    }
}

fn aggregate(op: AggregateOp, field: &str, alias: &str) -> Aggregate {
    Aggregate {
        op,
        field: Some(field.into()),
        alias: Some(alias.into()),
        distinct: None,
    }
}

/// `posts` grouped by `status`, with `HAVING <alias of agg> <operator> value`.
fn having_ir(
    agg: Aggregate,
    operator: &str,
    value: rmpv::Value,
    column_types: &[(&str, ColumnTypeSpec)],
) -> QueryIR {
    QueryIR {
        table: "posts".into(),
        cols: Some(vec!["status".into()]),
        column_types: Some(
            column_types
                .iter()
                .map(|(column, spec)| ((*column).into(), spec.clone()))
                .collect(),
        ),
        group_by: Some(vec!["status".into()]),
        having: Some(FilterNode::Condition(Filter {
            field: agg.alias.clone().unwrap(),
            operator: operator.into(),
            value,
            column: None,
            escape: None,
        })),
        aggregates: Some(vec![agg]),
        ..Default::default()
    }
}

fn author_join() -> JoinSpec {
    JoinSpec {
        path: "author".into(),
        alias: "author".into(),
        parent: None,
        table: "authors".into(),
        source_column: "author_id".into(),
        target_column: "id".into(),
        pk_column: None,
        result_prefix: "author".into(),
        columns: vec![JoinColumn {
            field: "id".into(),
            column: "id".into(),
        }],
    }
}

#[test]
fn joined_grouped_aggregate_omits_related_columns() {
    let mut ir = having_ir(aggregate(AggregateOp::Count, "id", "n"), ">", 1.into(), &[]);
    ir.group_by = Some(vec!["author_id".into()]);
    ir.aggregates.as_mut().unwrap()[0].distinct = Some(true);
    let mut author = author_join();
    author.columns.push(JoinColumn {
        field: "name".into(),
        column: "name".into(),
    });
    ir.joins = Some(vec![author]);
    ir.filter_tree = Some(FilterNode::Condition(Filter {
        field: "name".into(),
        operator: "=".into(),
        value: s("Alice"),
        column: Some("author.name".into()),
        escape: None,
    }));
    ir.order_by = Some(vec![("n".into(), "DESC".into())]);

    let (sql, params) = build_sql(&ir, Dialect::Postgres).unwrap();
    assert_eq!(
        sql,
        concat!(
            r#"SELECT COUNT(DISTINCT "posts"."id") AS "n", "posts"."author_id" AS "author_id" "#,
            r#"FROM "posts" LEFT JOIN "authors" AS "author" ON "posts"."author_id" = "author"."id" "#,
            r#"WHERE "author"."name" = $1 GROUP BY "posts"."author_id" "#,
            r#"HAVING COUNT(DISTINCT "posts"."id") > $2 ORDER BY "n" DESC"#,
        )
    );
    assert_eq!(params, vec![Value::from("Alice"), Value::from(1_i64)]);
}

#[test]
fn aggregate_groups_and_orders_by_related_fields() {
    let mut author = author_join();
    author.columns.extend([
        JoinColumn {
            field: "name".into(),
            column: "display_name".into(),
        },
        JoinColumn {
            field: "reputation".into(),
            column: "score".into(),
        },
    ]);
    let mut ir = having_ir(
        aggregate(AggregateOp::Sum, "author.score", "total"),
        ">",
        1.into(),
        &[],
    );
    ir.joins = Some(vec![author]);
    ir.group_by = Some(vec!["author.display_name".into()]);
    ir.cols = Some(vec!["author.display_name".into(), "total".into()]);
    ir.order_by = Some(vec![("author.display_name".into(), "ASC".into())]);

    let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
    assert_eq!(
        sql,
        concat!(
            r#"SELECT SUM("author"."score") AS "total", "author"."display_name" AS "author__name" "#,
            r#"FROM "posts" LEFT JOIN "authors" AS "author" ON "posts"."author_id" = "author"."id" "#,
            r#"GROUP BY "author"."display_name" HAVING SUM("author"."score") > $1 "#,
            r#"ORDER BY "author"."display_name" ASC"#,
        )
    );

    ir.aggregates.as_mut().unwrap()[0].alias = Some("author__name".into());
    assert!(matches!(
        build_sql(&ir, Dialect::Postgres),
        Err(QueryError::InvalidQuery(message))
            if message.contains("author__name") && message.contains("duplicates")
    ));

    ir.group_by = None;
    assert!(matches!(
        build_sql(&ir, Dialect::Postgres),
        Err(QueryError::InvalidQuery(message))
            if message.contains("author.display_name") && message.contains("group_by()")
    ));
}

#[test]
fn grouped_columns_keep_base_and_related_names_distinct() {
    let mut author = author_join();
    author.columns.push(JoinColumn {
        field: "name".into(),
        column: "name".into(),
    });

    for (field, column) in [
        ("author__name", r#""posts"."author__name""#),
        ("author.name", r#""author"."name""#),
    ] {
        let ir = QueryIR {
            table: "posts".into(),
            joins: Some(vec![author.clone()]),
            cols: Some(vec![field.into()]),
            group_by: Some(vec![field.into()]),
            order_by: Some(vec![(field.into(), "ASC".into())]),
            ..Default::default()
        };
        let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
        assert_eq!(
            sql,
            format!(
                concat!(
                    r#"SELECT {} AS "author__name" FROM "posts" "#,
                    r#"LEFT JOIN "authors" AS "author" ON "posts"."author_id" = "author"."id" "#,
                    r#"GROUP BY {} ORDER BY {} ASC"#,
                ),
                column, column, column,
            ),
        );

        let mut union = ir.clone();
        union.order_by = Some(vec![(field.into(), "DESC".into())]);
        union.union_query = Some(Box::new(ir));
        union.union_all = Some(true);
        let (sql, _) = build_sql(&union, Dialect::Postgres).unwrap();
        assert!(sql.ends_with("ORDER BY 1 DESC"), "{sql}");
    }
}

#[test]
fn union_orders_join_columns_by_position_when_labels_collide_or_are_long() {
    let long_prefix = "regional_reporting_group_for_review".repeat(2);
    for prefix in ["author", long_prefix.as_str()] {
        let mut author = author_join();
        author.result_prefix = prefix.into();
        author.columns.push(JoinColumn {
            field: "name".into(),
            column: "full_name".into(),
        });
        let mut editor = author_join();
        editor.path = "editor".into();
        editor.alias = "editor".into();
        editor.result_prefix = "editor".into();
        editor.source_column = "editor_id".into();
        let branch = QueryIR {
            table: "posts".into(),
            cols: Some(vec!["id".into(), "author__name".into()]),
            annotations: Some(vec![Annotation {
                alias: "rank".into(),
                expr: ScalarExpr::Value {
                    value: 2.into(),
                    value_type: None,
                },
            }]),
            joins: Some(vec![editor, author]),
            ..Default::default()
        };
        let ir = QueryIR {
            union_query: Some(Box::new(branch.clone())),
            order_by: Some(vec![
                ("author.full_name".into(), "DESC".into()),
                ("posts.author__name".into(), "ASC".into()),
                ("rank".into(), "ASC".into()),
            ]),
            ..branch
        };
        let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
        assert!(sql.contains(&format!(r#"AS "{prefix}__name""#)), "{sql}");
        assert!(sql.ends_with("ORDER BY 6 DESC, 2 ASC, 3 ASC"), "{sql}");
    }
}

#[test]
fn union_rejects_an_unprojected_join_column_with_a_matching_output_label() {
    let mut author = author_join();
    author.columns.push(JoinColumn {
        field: "name".into(),
        column: "name".into(),
    });
    let branch = QueryIR {
        table: "posts".into(),
        cols: Some(vec!["author__name".into()]),
        joins: Some(vec![author]),
        project_joins: Some(false),
        ..Default::default()
    };
    let ir = QueryIR {
        union_query: Some(Box::new(branch.clone())),
        order_by: Some(vec![("author.name".into(), "ASC".into())]),
        ..branch
    };
    assert!(matches!(
        build_sql(&ir, Dialect::Postgres),
        Err(QueryError::InvalidQuery(message))
            if message == "UNION can only be ordered by a projected column"
    ));
}

#[test]
fn nested_related_field_resolves_to_its_own_join() {
    let company = JoinSpec {
        path: "author__company".into(),
        alias: "author_company".into(),
        parent: Some("author".into()),
        table: "companies".into(),
        source_column: "company_id".into(),
        target_column: "id".into(),
        pk_column: None,
        result_prefix: "author__company".into(),
        columns: vec![JoinColumn {
            field: "name".into(),
            column: "company_name".into(),
        }],
    };
    let mut ir = QueryIR {
        table: "posts".into(),
        cols: Some(vec!["id".into()]),
        joins: Some(vec![author_join(), company]),
        order_by: Some(vec![("author_company.company_name".into(), "DESC".into())]),
        ..Default::default()
    };
    let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
    assert!(
        sql.ends_with(r#"ORDER BY "author_company"."company_name" DESC"#),
        "{sql}"
    );

    ir.aggregates = Some(vec![aggregate(AggregateOp::Count, "id", "n")]);
    ir.group_by = Some(vec!["author_company.company_name".into()]);
    let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
    assert!(
        sql.contains(r#""author_company"."company_name" AS "author__company__name""#),
        "{sql}"
    );
}

#[test]
fn having_related_min_max_keep_the_joined_column_cast() {
    let mut author = author_join();
    author.columns.push(JoinColumn {
        field: "tags".into(),
        column: "tag_ids".into(),
    });
    let mut ir = having_ir(
        aggregate(AggregateOp::Max, "author.tag_ids", "top_tags"),
        "=",
        arr(vec![1.into(), 2.into()]),
        &[("author.tag_ids", array_of(ColumnTypeSpec::BigInteger))],
    );
    ir.joins = Some(vec![author]);
    ir.column_db_types = Some(HashMap::from([(
        "author.tag_ids".into(),
        "SMALLINT[]".into(),
    )]));

    let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
    assert!(
        sql.contains(r#"HAVING MAX("author"."tag_ids") = ($1::SMALLINT[])"#),
        "{sql}"
    );
}

#[test]
fn aggregate_qualification_preserves_asterisks_and_mapped_names() {
    let mut ir = QueryIR {
        table: "posts".into(),
        aggregates: Some(vec![
            aggregate(AggregateOp::Count, "*", "n"),
            aggregate(AggregateOp::Sum, "mapped_views", "total"),
        ]),
        ..Default::default()
    };
    let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
    assert_eq!(
        sql,
        r#"SELECT COUNT(*) AS "n", SUM("mapped_views") AS "total" FROM "posts""#,
    );

    ir.joins = Some(vec![author_join()]);
    let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
    assert!(
        sql.starts_with(r#"SELECT COUNT(*) AS "n", SUM("posts"."mapped_views") AS "total" FROM "#),
        "{sql}"
    );

    for (input, expected) in [
        ("unknown.id", r#"SUM("posts"."unknown.id") AS "total""#),
        ("author.missing", r#"SUM("author"."missing") AS "total""#),
    ] {
        ir.aggregates.as_mut().unwrap()[1].field = Some(input.into());
        let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
        assert!(sql.contains(expected), "{sql}");
    }
}

#[test]
fn dotted_mapped_columns_stay_one_identifier() {
    let mut ir = QueryIR {
        table: "ledger".into(),
        aggregates: Some(vec![aggregate(AggregateOp::Sum, "legacy.amount", "total")]),
        group_by: Some(vec!["legacy.amount".into()]),
        cols: Some(vec!["legacy.amount".into(), "total".into()]),
        order_by: Some(vec![("legacy.amount".into(), "ASC".into())]),
        ..Default::default()
    };
    let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
    assert_eq!(
        sql,
        concat!(
            r#"SELECT SUM("legacy.amount") AS "total", "ledger"."legacy.amount" AS "legacy.amount" "#,
            r#"FROM "ledger" GROUP BY "legacy.amount" ORDER BY "legacy.amount" ASC"#,
        )
    );

    // An unrelated join qualifies the literal with the base table.
    let mut author = author_join();
    author.source_column = "author_id".into();
    ir.joins = Some(vec![author]);
    ir.table = "ledger".into();
    let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
    assert!(
        sql.starts_with(
            r#"SELECT SUM("ledger"."legacy.amount") AS "total", "ledger"."legacy.amount" AS "legacy.amount" "#
        ),
        "{sql}"
    );
    assert!(
        sql.ends_with(r#"GROUP BY "ledger"."legacy.amount" ORDER BY "ledger"."legacy.amount" ASC"#),
        "{sql}"
    );

    // A grouped selection of the literal is a base column, not an ungrouped
    // related field.
    ir.aggregates = Some(vec![aggregate(AggregateOp::Count, "*", "n")]);
    ir.group_by = Some(vec!["id".into()]);
    ir.cols = Some(vec!["legacy.amount".into(), "n".into()]);
    ir.order_by = None;
    build_sql(&ir, Dialect::Postgres).unwrap();
}

#[test]
fn output_labels_name_grouped_columns() {
    let supplier = JoinSpec {
        path: "supplier".into(),
        alias: "supplier".into(),
        parent: None,
        table: "suppliers".into(),
        source_column: "supplier_id".into(),
        target_column: "id".into(),
        pk_column: None,
        result_prefix: "supplier".into(),
        columns: vec![JoinColumn {
            field: "name".into(),
            column: "name".into(),
        }],
    };
    let ir = QueryIR {
        table: "products".into(),
        joins: Some(vec![supplier]),
        aggregates: Some(vec![aggregate(AggregateOp::Count, "id", "n")]),
        group_by: Some(vec!["supplier__name".into(), "supplier.name".into()]),
        cols: Some(vec![
            "supplier__name".into(),
            "supplier.name".into(),
            "n".into(),
        ]),
        output_labels: Some(HashMap::from([
            ("supplier__name".into(), "cached_supplier".into()),
            ("supplier.name".into(), "supplier__name".into()),
        ])),
        ..Default::default()
    };
    let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
    assert_eq!(
        sql,
        concat!(
            r#"SELECT COUNT("products"."id") AS "n", "products"."supplier__name" AS "cached_supplier", "#,
            r#""supplier"."name" AS "supplier__name" FROM "products" "#,
            r#"LEFT JOIN "suppliers" AS "supplier" ON "products"."supplier_id" = "supplier"."id" "#,
            r#"GROUP BY "products"."supplier__name", "supplier"."name""#,
        )
    );

    // An aggregate alias may not reuse a label.
    let mut clash = ir.clone();
    clash.aggregates.as_mut().unwrap()[0].alias = Some("cached_supplier".into());
    assert!(matches!(
        build_sql(&clash, Dialect::Postgres),
        Err(QueryError::InvalidQuery(message)) if message.contains("cached_supplier")
    ));

    let union = QueryIR {
        union_query: Some(Box::new(ir.clone())),
        order_by: Some(vec![
            ("supplier.name".into(), "ASC".into()),
            ("supplier__name".into(), "DESC".into()),
            ("n".into(), "ASC".into()),
        ]),
        ..ir
    };
    let (sql, _) = build_sql(&union, Dialect::Postgres).unwrap();
    assert!(sql.ends_with("ORDER BY 3 ASC, 2 DESC, 1 ASC"), "{sql}");
}

#[test]
fn labeled_grouping_and_ordering_use_columns_not_labels() {
    // Field `a` is stored in column "b"; field `b` in column "b_col". The
    // output label "b" names field `b`, so a bare "b" in GROUP BY or ORDER BY
    // must not be mistaken for it.
    let ir = QueryIR {
        table: "items".into(),
        aggregates: Some(vec![aggregate(AggregateOp::Count, "*", "n")]),
        group_by: Some(vec!["b".into(), "b_col".into()]),
        output_labels: Some(HashMap::from([
            ("b".into(), "a".into()),
            ("b_col".into(), "b".into()),
        ])),
        order_by: Some(vec![("b".into(), "ASC".into())]),
        ..Default::default()
    };
    let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
    assert_eq!(
        sql,
        concat!(
            r#"SELECT COUNT(*) AS "n", "items"."b" AS "a", "items"."b_col" AS "b" FROM "items" "#,
            r#"GROUP BY "b", "b_col" ORDER BY "items"."b" ASC"#,
        )
    );
}

#[test]
fn count_star_requires_a_field_for_distinct() {
    let distinct_results = [None, Some("*")].map(|field| {
        let mut ir = QueryIR {
            table: "posts".into(),
            aggregates: Some(vec![Aggregate {
                op: AggregateOp::Count,
                field: field.map(str::to_owned),
                alias: Some("n".into()),
                distinct: None,
            }]),
            ..Default::default()
        };
        let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
        assert_eq!(sql, r#"SELECT COUNT(*) AS "n" FROM "posts""#);
        ir.aggregates.as_mut().unwrap()[0].distinct = Some(true);
        build_sql(&ir, Dialect::Postgres)
    });
    assert!(
        distinct_results.iter().all(|result| matches!(
            result,
            Err(QueryError::InvalidQuery(message))
                if message == "COUNT(DISTINCT ...) requires a field"
        )),
        "{distinct_results:?}"
    );
}

#[test]
fn aggregate_input_qualification_matches_in_select_and_having() {
    let mut ir = having_ir(
        aggregate(AggregateOp::Sum, "reputation", "total"),
        ">",
        1.into(),
        &[],
    );
    let mut projection_free_join = author_join();
    projection_free_join.columns.clear();
    for join in [projection_free_join, author_join()] {
        ir.joins = Some(vec![join]);
        let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
        assert!(
            sql.starts_with(r#"SELECT SUM("posts"."reputation") AS "total""#),
            "{sql}"
        );
        assert!(
            sql.contains(r#"HAVING SUM("posts"."reputation") > "#),
            "{sql}"
        );
    }
}

#[test]
fn aggregate_alias_cannot_duplicate_a_grouped_column() {
    for joins in [None, Some(vec![author_join()])] {
        let ir = QueryIR {
            table: "posts".into(),
            aggregates: Some(vec![aggregate(AggregateOp::Count, "*", "author_id")]),
            group_by: Some(vec!["author_id".into()]),
            order_by: Some(vec![("author_id".into(), "ASC".into())]),
            joins,
            ..Default::default()
        };
        assert!(matches!(
            build_sql(&ir, Dialect::Postgres),
            Err(QueryError::InvalidQuery(message))
                if message.contains("author_id") && message.contains("group_by")
        ));
    }
}

#[test]
fn exists_keeps_joins_without_projecting_related_columns() {
    let mut ir = QueryIR {
        table: "posts".into(),
        cols: Some(vec!["id".into()]),
        joins: Some(vec![author_join()]),
        ..Default::default()
    };
    let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
    assert!(sql.contains(r#""author"."id" AS "author__id""#), "{sql}");
    ir.exists = Some(true);
    ir.distinct = Some(true);
    ir.order_by = Some(vec![("author.id".into(), "ASC".into())]);
    let (exists_sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
    assert_eq!(
        exists_sql,
        concat!(
            r#"SELECT EXISTS(SELECT DISTINCT "posts"."id" AS "id" FROM "posts" "#,
            r#"LEFT JOIN "authors" AS "author" ON "posts"."author_id" = "author"."id")"#,
        )
    );

    ir.distinct = None;
    ir.lock = Some(LockType::Update);
    let (locked_sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
    assert!(
        locked_sql.ends_with(r#"ORDER BY "author"."id" ASC FOR UPDATE OF "posts")"#),
        "{locked_sql}"
    );
}

fn status_enum() -> ColumnTypeSpec {
    ColumnTypeSpec::Enum {
        name: "post_status_enum".into(),
        values: vec!["draft".into(), "published".into()],
    }
}

fn decimal(value: i64, scale: u32) -> Value {
    Value::Decimal(Some(Decimal::new(value, scale)))
}

#[test]
fn having_binds_values_by_the_aggregate_result_type() {
    let column_types = [
        (
            "price",
            ColumnTypeSpec::Decimal {
                precision: Some(10),
                scale: Some(2),
            },
        ),
        ("other_status", status_enum()),
    ];
    // Whether the alias is new, the aggregated column, or another column
    // with a type of its own, MAX("price") is a decimal.
    for alias in ["top", "price", "other_status"] {
        let ir = having_ir(
            aggregate(AggregateOp::Max, "price", alias),
            ">",
            s("5.5"),
            &column_types,
        );
        let (sql, params) = build_sql(&ir, Dialect::Postgres).unwrap();
        assert!(
            sql.contains(r#"HAVING MAX("price") > $1"#),
            "{alias}: {sql}"
        );
        assert_eq!(params, vec![decimal(55, 1)], "{alias}");
    }
}

#[test]
fn having_min_max_keep_the_column_casts() {
    let ir = having_ir(
        aggregate(AggregateOp::Min, "status", "first_status"),
        "=",
        s("draft"),
        &[("status", status_enum())],
    );
    let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
    assert!(
        sql.contains(r#"HAVING MIN("status") = ($1::"post_status_enum")"#),
        "{sql}"
    );

    let mut ir = having_ir(
        aggregate(AggregateOp::Max, "tags", "top_tags"),
        "=",
        arr(vec![1.into(), 2.into()]),
        &[("tags", array_of(ColumnTypeSpec::BigInteger))],
    );
    ir.column_db_types = Some(HashMap::from([("tags".into(), "INTEGER[]".into())]));
    let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
    assert!(
        sql.contains(r#"HAVING MAX("tags") = ($1::INTEGER[])"#),
        "{sql}"
    );
}

#[test]
fn having_sum_avg_of_integers_compare_as_exact_numerics() {
    let column_types = [("views", ColumnTypeSpec::BigInteger)];
    for op in [AggregateOp::Sum, AggregateOp::Avg] {
        let ir = having_ir(
            aggregate(op.clone(), "views", "v"),
            ">",
            s("2.5"),
            &column_types,
        );
        let (_, params) = build_sql(&ir, Dialect::Postgres).unwrap();
        assert_eq!(params, vec![decimal(25, 1)], "{op:?}");

        let ir = having_ir(
            aggregate(op.clone(), "views", "v"),
            ">",
            3.into(),
            &column_types,
        );
        let (_, params) = build_sql(&ir, Dialect::Postgres).unwrap();
        assert_eq!(params, vec![Value::BigInt(Some(3))], "{op:?}");
    }

    let ir = having_ir(
        aggregate(AggregateOp::Count, "id", "n"),
        ">=",
        3.into(),
        &column_types,
    );
    let (_, params) = build_sql(&ir, Dialect::Postgres).unwrap();
    assert_eq!(params, vec![Value::BigInt(Some(3))]);
}

#[test]
fn having_uses_qualified_base_type_hints_for_aggregate_inputs() {
    let mut ir = having_ir(
        aggregate(AggregateOp::Max, "posts.tags", "top_tags"),
        "=",
        arr(vec![1.into(), 2.into()]),
        &[("posts.tags", array_of(ColumnTypeSpec::BigInteger))],
    );
    ir.column_db_types = Some(HashMap::from([("posts.tags".into(), "INTEGER[]".into())]));
    let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
    assert!(
        sql.ends_with(r#"HAVING MAX("posts"."tags") = ($1::INTEGER[])"#),
        "{sql}"
    );

    let ir = having_ir(
        aggregate(AggregateOp::Sum, "posts.x", "total"),
        ">",
        s("2.5"),
        &[("posts.x", ColumnTypeSpec::BigInteger)],
    );
    let (_, params) = build_sql(&ir, Dialect::Postgres).unwrap();
    assert_eq!(params, vec![decimal(25, 1)]);
}

#[test]
fn left_join_locks_only_the_base_table() {
    for (lock, clause) in [
        (LockType::Update, "FOR UPDATE"),
        (LockType::Share, "FOR SHARE"),
    ] {
        let mut ir = QueryIR {
            table: "posts".into(),
            cols: Some(vec!["title".into()]),
            lock: Some(lock),
            ..Default::default()
        };
        let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
        assert!(sql.ends_with(clause), "{sql}");

        ir.joins = Some(vec![author_join()]);
        let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
        assert!(sql.ends_with(&format!(r#"{clause} OF "posts""#)), "{sql}");
    }
}

#[test]
fn join_projection_can_be_disabled() {
    let ir = QueryIR {
        table: "posts".into(),
        cols: Some(vec!["title".into()]),
        joins: Some(vec![author_join()]),
        project_joins: Some(false),
        distinct: Some(true),
        ..Default::default()
    };
    let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
    assert_eq!(
        sql,
        concat!(
            r#"SELECT DISTINCT "posts"."title" AS "title" FROM "posts" "#,
            r#"LEFT JOIN "authors" AS "author" ON "posts"."author_id" = "author"."id""#,
        )
    );
}

#[test]
fn filters_resolve_joined_columns_outside_the_projection() {
    let ir = QueryIR {
        table: "posts".into(),
        cols: Some(vec!["id".into()]),
        joins: Some(vec![author_join()]),
        filter_tree: Some(FilterNode::Condition(Filter {
            field: "status".into(),
            column: Some("author.status".into()),
            operator: "=".into(),
            value: "active".into(),
            escape: None,
        })),
        ..Default::default()
    };
    let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
    assert!(sql.contains(r#""author"."id" AS "author__id""#), "{sql}");
    assert!(sql.ends_with(r#"WHERE "author"."status" = $1"#), "{sql}");
}

#[test]
fn filters_keep_dotted_base_columns_literal() {
    let mut ir = QueryIR {
        table: "posts".into(),
        cols: Some(vec!["id".into()]),
        filter_tree: Some(FilterNode::Condition(Filter {
            field: "rating".into(),
            column: Some("posts.supplier.rating".into()),
            operator: "=".into(),
            value: 2.into(),
            escape: None,
        })),
        ..Default::default()
    };
    let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
    assert!(sql.ends_with(r#"WHERE "supplier.rating" = $1"#), "{sql}");

    ir.joins = Some(vec![author_join()]);
    let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
    assert!(
        sql.ends_with(r#"WHERE "posts"."supplier.rating" = $1"#),
        "{sql}"
    );

    ir.count = Some(true);
    let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
    assert!(
        sql.ends_with(r#"WHERE "posts"."supplier.rating" = $1"#),
        "{sql}"
    );
}

#[test]
fn qualified_filters_render_table_and_column() {
    let mut ir = QueryIR {
        table: "posts".into(),
        cols: Some(vec!["id".into()]),
        filter_tree: Some(FilterNode::Condition(Filter {
            field: "posts.id".into(),
            column: None,
            operator: "=".into(),
            value: 2.into(),
            escape: None,
        })),
        ..Default::default()
    };
    for op in [Operation::Select, Operation::Update, Operation::Delete] {
        ir.op = op;
        if ir.op == Operation::Update {
            ir.values = Some(HashMap::from([("views".into(), 5.into())]));
        }
        let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
        assert!(sql.contains(r#"WHERE "posts"."id" = $"#), "{sql}");
    }

    ir.op = Operation::Select;
    ir.joins = Some(vec![author_join()]);
    let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
    assert!(sql.contains(r#"WHERE "posts"."id" = $"#), "{sql}");

    let Some(FilterNode::Condition(filter)) = ir.filter_tree.as_mut() else {
        unreachable!();
    };
    filter.field = "authors.id".into();
    let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
    assert!(sql.contains(r#"WHERE "authors"."id" = $"#), "{sql}");
}

#[test]
fn group_by_scalar_annotation_uses_its_select_ordinal() {
    let ir = QueryIR {
        table: "posts".into(),
        cols: Some(vec!["id".into()]),
        joins: Some(vec![author_join()]),
        annotations: Some(vec![Annotation {
            alias: "views".into(),
            expr: ScalarExpr::Value {
                value: 2.into(),
                value_type: None,
            },
        }]),
        group_by: Some(vec!["id".into(), "views".into()]),
        ..Default::default()
    };
    let (sql, params) = build_sql(&ir, Dialect::Postgres).unwrap();
    assert!(sql.ends_with(r#"GROUP BY "posts"."id", 2"#), "{sql}");
    assert_eq!(params.len(), 1);
}

#[test]
fn qualified_group_key_with_same_name_as_scalar_annotation_stays_a_column() {
    let ir = QueryIR {
        table: "posts".into(),
        cols: Some(vec!["posts.bucket".into()]),
        output_labels: Some(HashMap::from([("posts.bucket".into(), "title".into())])),
        annotations: Some(vec![Annotation {
            alias: "bucket".into(),
            expr: ScalarExpr::Value {
                value: s("draft"),
                value_type: None,
            },
        }]),
        group_by: Some(vec!["posts.bucket".into(), "bucket".into()]),
        ..Default::default()
    };
    let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
    assert!(sql.contains(r#""posts"."bucket" AS "title""#), "{sql}");
    assert!(sql.ends_with(r#"GROUP BY "posts"."bucket", 2"#), "{sql}");
}

#[test]
fn qualified_base_filter_uses_bare_or_qualified_column_type_and_db_type() {
    let mut ir = int_array_override("=", arr(vec![1.into()])).ir();
    let Some(FilterNode::Condition(filter)) = ir.filter_tree.as_mut() else {
        unreachable!();
    };
    filter.column = Some("posts.tags".into());
    for key in ["tags", "posts.tags"] {
        ir.column_types = Some(HashMap::from([(
            key.into(),
            array_of(ColumnTypeSpec::BigInteger),
        )]));
        ir.column_db_types = Some(HashMap::from([(key.into(), "INTEGER[]".into())]));
        let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
        assert!(
            sql.ends_with(r#"WHERE "posts"."tags" = ($1::INTEGER[])"#),
            "{sql}"
        );
    }
}

#[test]
fn qualified_base_filter_prefers_bare_enum_hint_and_keeps_dotted_columns_literal() {
    for (field, hints, expected_column) in [
        (
            "posts.status",
            HashMap::from([("posts.status".into(), status_enum())]),
            r#""posts"."status""#,
        ),
        (
            "posts.status",
            HashMap::from([
                ("status".into(), status_enum()),
                ("posts.status".into(), ColumnTypeSpec::Text),
            ]),
            r#""posts"."status""#,
        ),
        (
            "posts.a.b",
            HashMap::from([("a.b".into(), status_enum())]),
            r#""a.b""#,
        ),
    ] {
        let ir = QueryIR {
            table: "posts".into(),
            cols: Some(vec!["id".into()]),
            column_types: Some(hints),
            filter_tree: Some(FilterNode::Condition(Filter {
                field: field.into(),
                column: None,
                operator: "=".into(),
                value: s("draft"),
                escape: None,
            })),
            ..Default::default()
        };
        let (sql, _) = build_sql(&ir, Dialect::Postgres).unwrap();
        assert!(
            sql.ends_with(&format!(
                r#"WHERE {expected_column} = ($1::"post_status_enum")"#
            )),
            "{sql}"
        );
    }
}
