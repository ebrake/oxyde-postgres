use oxyde_codec::{
    Aggregate, AggregateOp, Filter, FilterNode, JoinColumn, JoinSpec, Operation, QueryIR,
    IR_PROTO_VERSION,
};
use oxyde_driver::{
    close_pool, execute_query_columnar, execute_query_columnar_dedup, execute_statement, init_pool,
    PoolSettings, RelationInfo,
};
use oxyde_sql::{build_sql, Dialect};
use std::collections::HashMap;
use uuid::Uuid;

fn decode_columnar(buf: &[u8]) -> (Vec<String>, Vec<Vec<rmpv::Value>>) {
    let val: rmpv::Value = rmp_serde::from_slice(buf).unwrap();
    let arr = val.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    let columns: Vec<String> = arr[0]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    let rows: Vec<Vec<rmpv::Value>> = arr[1]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row.as_array().unwrap().clone())
        .collect();
    (columns, rows)
}

#[tokio::test]
#[ignore = "requires OXYDE_TEST_POSTGRES_URL"]
async fn postgres_encodes_out_of_range_temporal_values_as_nil() {
    let pool_name = format!("temporal_limits_{}", Uuid::new_v4().simple());
    init_pool(
        &pool_name,
        &std::env::var("OXYDE_TEST_POSTGRES_URL").expect("set OXYDE_TEST_POSTGRES_URL"),
        PoolSettings::default(),
    )
    .await
    .unwrap();

    let sql = "SELECT 'infinity'::date AS infinite_date, \
               'infinity'::timestamp AS infinite_timestamp, \
               '294276-12-31'::date AS large_date";
    let (bytes, count) = execute_query_columnar(&pool_name, sql, &[], None)
        .await
        .unwrap();
    let (columns, rows) = decode_columnar(&bytes);
    assert_eq!(count, 1);
    assert_eq!(
        columns,
        ["infinite_date", "infinite_timestamp", "large_date"]
    );
    assert_eq!(rows, vec![vec![rmpv::Value::Nil; 3]]);

    close_pool(&pool_name).await.unwrap();
}

#[tokio::test]
#[ignore = "requires OXYDE_TEST_POSTGRES_URL"]
async fn postgres_dedup_uses_field_names_for_truncated_join_labels() {
    let pool_name = format!("long_labels_{}", Uuid::new_v4().simple());
    init_pool(
        &pool_name,
        &std::env::var("OXYDE_TEST_POSTGRES_URL").expect("set OXYDE_TEST_POSTGRES_URL"),
        PoolSettings::default(),
    )
    .await
    .unwrap();

    let prefix = "author__".repeat(9) + "company";
    let sql = format!(r#"SELECT 1 AS id, 2 AS "{prefix}__id", 'Acme' AS "{prefix}__name""#);
    let relations = [RelationInfo {
        prefix: prefix.clone(),
        pk_index: 0,
        field_names: vec!["id".into(), "name".into()],
        column_types: vec![None, None],
    }];
    let (bytes, _) = execute_query_columnar_dedup(&pool_name, &sql, &[], None, &relations)
        .await
        .unwrap();
    let result: rmpv::Value = rmp_serde::from_slice(&bytes).unwrap();
    let document = result.as_array().unwrap();
    let [(key, relation)] = document[2].as_map().unwrap().as_slice() else {
        panic!("expected exactly one relation");
    };
    assert_eq!(key.as_str(), Some(prefix.as_str()));
    let (_, columns) = relation
        .as_map()
        .unwrap()
        .iter()
        .find(|(key, _)| key.as_str() == Some("columns"))
        .unwrap();
    let field_names: Vec<_> = columns
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str())
        .collect();
    assert_eq!(field_names, [Some("id"), Some("name")]);

    close_pool(&pool_name).await.unwrap();
}

#[tokio::test]
#[ignore = "requires OXYDE_TEST_POSTGRES_URL"]
async fn postgres_end_to_end_pipeline() {
    let pool_name = format!("pipeline_{}", Uuid::new_v4().simple());
    let settings = PoolSettings {
        max_connections: Some(1),
        min_connections: Some(1),
        ..Default::default()
    };
    init_pool(
        &pool_name,
        &std::env::var("OXYDE_TEST_POSTGRES_URL").expect("set OXYDE_TEST_POSTGRES_URL"),
        settings,
    )
    .await
    .expect("init pool");

    execute_statement(
        &pool_name,
        "CREATE TEMP TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL)",
        &[],
    )
    .await
    .unwrap();

    let mut insert_values = HashMap::new();
    insert_values.insert("id".to_string(), rmpv::Value::Integer(1.into()));
    insert_values.insert(
        "name".to_string(),
        rmpv::Value::String("Ada Lovelace".into()),
    );
    let insert_ir = QueryIR {
        proto: IR_PROTO_VERSION,
        op: Operation::Insert,
        table: "users".into(),
        cols: None,
        column_types: None,
        column_db_types: None,
        bind_types: None,
        filter_tree: None,
        limit: None,
        offset: None,
        order_by: None,
        values: Some(insert_values),
        bulk_values: None,
        bulk_update: None,
        model: None,
        distinct: None,
        column_mappings: None,
        output_labels: None,
        project_joins: None,
        joins: None,
        aggregates: None,
        annotations: None,
        returning: None,
        group_by: None,
        having: None,
        exists: None,
        count: None,
        on_conflict: None,
        lock: None,
        union_query: None,
        union_all: None,
        sql: None,
        params: None,
        pk_column: None,
    };
    let (insert_sql, insert_params) = build_sql(&insert_ir, Dialect::Postgres).unwrap();
    execute_statement(&pool_name, &insert_sql, &insert_params)
        .await
        .unwrap();

    let select_ir = QueryIR {
        proto: IR_PROTO_VERSION,
        op: Operation::Select,
        table: "users".into(),
        cols: Some(vec!["id".into(), "name".into()]),
        column_types: None,
        column_db_types: None,
        bind_types: None,
        filter_tree: Some(FilterNode::Condition(Filter {
            field: "id".into(),
            operator: "=".into(),
            value: rmpv::Value::Integer(1.into()),
            column: None,
            escape: None,
        })),
        limit: None,
        offset: None,
        order_by: None,
        values: None,
        bulk_values: None,
        bulk_update: None,
        model: None,
        distinct: None,
        column_mappings: None,
        output_labels: None,
        project_joins: None,
        joins: None,
        aggregates: None,
        annotations: None,
        returning: None,
        group_by: None,
        having: None,
        exists: None,
        count: None,
        on_conflict: None,
        lock: None,
        union_query: None,
        union_all: None,
        sql: None,
        params: None,
        pk_column: None,
    };
    let (select_sql, select_params) = build_sql(&select_ir, Dialect::Postgres).unwrap();
    let (bytes, num_rows) = execute_query_columnar(&pool_name, &select_sql, &select_params, None)
        .await
        .unwrap();

    assert_eq!(num_rows, 1);
    let (columns, rows) = decode_columnar(&bytes);
    assert_eq!(columns, vec!["id", "name"]);
    assert_eq!(rows[0][1].as_str().unwrap(), "Ada Lovelace");

    close_pool(&pool_name).await.unwrap();
}

#[tokio::test]
#[ignore = "requires OXYDE_TEST_POSTGRES_URL"]
async fn postgres_joined_aggregate_pipeline() {
    let pool_name = format!("aggregate_pipeline_{}", Uuid::new_v4().simple());
    init_pool(
        &pool_name,
        &std::env::var("OXYDE_TEST_POSTGRES_URL").expect("set OXYDE_TEST_POSTGRES_URL"),
        PoolSettings {
            max_connections: Some(1),
            min_connections: Some(1),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    for statement in [
        "CREATE TEMP TABLE authors (id INTEGER PRIMARY KEY, name TEXT NOT NULL)",
        "CREATE TEMP TABLE posts (id INTEGER PRIMARY KEY, author_id INTEGER, views INTEGER)",
        "INSERT INTO authors VALUES (1, 'Alice'), (2, 'Bob')",
        "INSERT INTO posts VALUES (1, 1, 120), (2, 1, 35), (3, 2, 80)",
    ] {
        execute_statement(&pool_name, statement, &[]).await.unwrap();
    }

    let mut ir = QueryIR {
        table: "posts".into(),
        joins: Some(vec![JoinSpec {
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
        }]),
        filter_tree: Some(FilterNode::Condition(Filter {
            field: "name".into(),
            operator: "=".into(),
            value: "Alice".into(),
            column: Some("author.name".into()),
            escape: None,
        })),
        aggregates: Some(
            [
                (AggregateOp::Sum, "views", "total"),
                (AggregateOp::Avg, "views", "average"),
                (AggregateOp::Min, "views", "minimum"),
                (AggregateOp::Max, "views", "maximum"),
                (AggregateOp::Avg, "author.id", "average_author_id"),
            ]
            .into_iter()
            .map(|(op, field, alias)| Aggregate {
                op,
                field: Some(field.into()),
                alias: Some(alias.into()),
                distinct: None,
            })
            .collect(),
        ),
        ..Default::default()
    };

    let (sql, params) = build_sql(&ir, Dialect::Postgres).unwrap();
    let (bytes, num_rows) = execute_query_columnar(&pool_name, &sql, &params, None)
        .await
        .unwrap();
    assert_eq!(num_rows, 1);
    let (columns, rows) = decode_columnar(&bytes);
    assert_eq!(
        columns,
        [
            "total",
            "average",
            "minimum",
            "maximum",
            "average_author_id"
        ]
    );
    assert_eq!(rows[0][0].as_i64(), Some(155));
    assert_eq!(rows[0][1].as_str().unwrap().parse::<f64>().unwrap(), 77.5);
    assert_eq!(rows[0][2].as_i64(), Some(35));
    assert_eq!(rows[0][3].as_i64(), Some(120));
    assert_eq!(rows[0][4].as_str().unwrap().parse::<f64>().unwrap(), 1.0);

    ir.aggregates = Some(vec![Aggregate {
        op: AggregateOp::Count,
        field: Some("id".into()),
        alias: Some("n".into()),
        distinct: Some(true),
    }]);
    ir.group_by = Some(vec!["author_id".into()]);
    ir.having = Some(FilterNode::Condition(Filter {
        field: "n".into(),
        operator: ">".into(),
        value: 1.into(),
        column: None,
        escape: None,
    }));
    ir.order_by = Some(vec![("n".into(), "DESC".into())]);

    let (sql, params) = build_sql(&ir, Dialect::Postgres).unwrap();
    let (bytes, num_rows) = execute_query_columnar(&pool_name, &sql, &params, None)
        .await
        .unwrap();
    assert_eq!(num_rows, 1);
    let (columns, rows) = decode_columnar(&bytes);
    assert_eq!(columns, ["n", "author_id"]);
    assert_eq!(rows, vec![vec![rmpv::Value::from(2), rmpv::Value::from(1)]]);

    close_pool(&pool_name).await.unwrap();
}
