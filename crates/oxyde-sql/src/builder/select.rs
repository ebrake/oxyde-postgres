//! SELECT query building

use oxyde_codec::{JoinSpec, LockType as OxydeLockType, QueryIR};
use sea_query::ExprTrait;
use sea_query::{
    Alias, Asterisk, Expr, Func, IntoColumnRef, IntoTableRef, LockType as SeaLockType, Order,
    PostgresQueryBuilder, Query, SelectStatement, UnionType, Value,
};

use crate::aggregate::build_aggregate;
use crate::error::Result;
use crate::filter::build_filter_node;
use crate::utils::{ColumnIdent, TableIdent};
use crate::Dialect;

/// Build a `SelectStatement` from QueryIR (without serializing to string).
/// Used recursively for UNION sub-queries.
fn build_select_statement(ir: &QueryIR, dialect: Dialect) -> Result<SelectStatement> {
    let table = TableIdent(ir.table.clone());
    let mut query = Query::select();
    query.from(table.clone());

    if ir.distinct.unwrap_or(false) {
        query.distinct();
    }

    // Add aggregates or columns
    if let Some(aggregates) = &ir.aggregates {
        for agg in aggregates {
            let agg_expr = build_aggregate(agg)?;
            if let Some(alias) = &agg.alias {
                query.expr_as(agg_expr, Alias::new(alias.clone()));
            } else {
                query.expr(agg_expr);
            }
        }
        if let Some(group_by) = &ir.group_by {
            for field in group_by {
                let column_ref = (table.clone(), ColumnIdent(field.clone())).into_column_ref();
                query.expr_as(Expr::col(column_ref), Alias::new(field.clone()));
            }
        }
    } else if let Some(cols) = &ir.cols {
        for col in cols {
            let column_ref = (table.clone(), ColumnIdent(col.clone())).into_column_ref();
            query.expr_as(Expr::col(column_ref), Alias::new(col.clone()));
        }
    } else {
        query.column(Asterisk);
    }

    let default_table = if ir.joins.is_some() {
        Some(ir.table.as_str())
    } else {
        None
    };

    if let Some(filter_tree) = &ir.filter_tree {
        let expr = build_filter_node(
            filter_tree,
            default_table,
            ir.column_types.as_ref(),
            ir.column_db_types.as_ref(),
            None,
            dialect,
        )?;
        query.and_where(expr);
    }

    if let Some(group_by) = &ir.group_by {
        for field in group_by {
            let col_expr = if ir.joins.is_some() {
                Expr::col((table.clone(), ColumnIdent(field.clone())).into_column_ref())
            } else {
                Expr::col(ColumnIdent(field.clone()))
            };
            query.add_group_by([col_expr]);
        }
    }

    if let Some(having) = &ir.having {
        let expr = build_filter_node(
            having,
            default_table,
            ir.column_types.as_ref(),
            ir.column_db_types.as_ref(),
            ir.aggregates.as_deref(),
            dialect,
        )?;
        query.and_having(expr);
    }

    if let Some(joins) = &ir.joins {
        apply_select_joins(&mut query, joins, &table);
    }

    // UNION via sea-query (recursive)
    if let Some(union_query_ir) = &ir.union_query {
        let union_stmt = build_select_statement(union_query_ir, dialect)?;
        let union_type = if ir.union_all.unwrap_or(false) {
            UnionType::All
        } else {
            UnionType::Distinct
        };
        query.union(union_type, union_stmt);
    }

    // ORDER BY, LIMIT, OFFSET — placed after UNION by sea-query
    if let Some(order_by) = &ir.order_by {
        for (field, direction) in order_by {
            if field == "?" {
                query.order_by_expr(Func::random().into(), Order::Asc);
                continue;
            }
            let order = match direction.to_uppercase().as_str() {
                "ASC" => Order::Asc,
                "DESC" => Order::Desc,
                _ => Order::Asc,
            };
            if ir.joins.is_some() {
                let col_ref = (table.clone(), ColumnIdent(field.clone())).into_column_ref();
                query.order_by(col_ref, order);
            } else {
                query.order_by(ColumnIdent(field.clone()), order);
            }
        }
    }

    if let Some(limit) = ir.limit {
        query.limit(u64::try_from(limit).unwrap_or(0));
    }

    if let Some(offset) = ir.offset {
        query.offset(u64::try_from(offset).unwrap_or(0));
    }

    if let Some(lock_type) = &ir.lock {
        match lock_type {
            OxydeLockType::Update => query.lock(SeaLockType::Update),
            OxydeLockType::Share => query.lock(SeaLockType::Share),
        };
    }

    Ok(query)
}

/// Helper to build SQL string from a `SelectStatement` for the given dialect.
fn build_query_string(query: &SelectStatement, dialect: Dialect) -> (String, Vec<Value>) {
    let (sql, values) = match dialect {
        Dialect::Postgres => query.build(PostgresQueryBuilder),
    };
    (sql, values.0)
}

/// Build SELECT query from QueryIR
pub fn build_select(ir: &QueryIR, dialect: Dialect) -> Result<(String, Vec<Value>)> {
    // COUNT(*) — separate minimal query
    if ir.count.unwrap_or(false) {
        let table = TableIdent(ir.table.clone());
        let mut count_query = Query::select();
        count_query.from(table.clone());
        count_query.expr_as(Func::count(Expr::col(Asterisk)), Alias::new("_count"));

        let default_table = if ir.joins.is_some() {
            Some(ir.table.as_str())
        } else {
            None
        };

        if let Some(filter_tree) = &ir.filter_tree {
            let expr = build_filter_node(
                filter_tree,
                default_table,
                ir.column_types.as_ref(),
                ir.column_db_types.as_ref(),
                None,
                dialect,
            )?;
            count_query.and_where(expr);
        }

        if let Some(joins) = &ir.joins {
            apply_joins_only(&mut count_query, joins, &table);
        }

        return Ok(build_query_string(&count_query, dialect));
    }

    let query = build_select_statement(ir, dialect)?;
    let (sql, values) = build_query_string(&query, dialect);

    if ir.exists.unwrap_or(false) {
        return Ok((format!("SELECT EXISTS({sql})"), values));
    }

    Ok((sql, values))
}

/// Apply LEFT JOINs and add joined columns to SELECT (with `prefix__field` aliases).
fn apply_select_joins(
    query: &mut sea_query::SelectStatement,
    joins: &[JoinSpec],
    base_table: &TableIdent,
) {
    for join in joins {
        let join_alias = Alias::new(join.alias.clone());
        let mut table_ref = TableIdent(join.table.clone()).into_table_ref();
        table_ref = table_ref.alias(join_alias.clone());
        let left_col = match &join.parent {
            Some(parent_alias) => (
                Alias::new(parent_alias.clone()),
                ColumnIdent(join.source_column.clone()),
            )
                .into_column_ref(),
            None => (base_table.clone(), ColumnIdent(join.source_column.clone())).into_column_ref(),
        };
        let right_col =
            (join_alias.clone(), ColumnIdent(join.target_column.clone())).into_column_ref();
        query.left_join(table_ref, Expr::col(left_col).equals(right_col));
        for column in &join.columns {
            let expr = Expr::col((join_alias.clone(), ColumnIdent(column.column.clone())));
            let alias = Alias::new(join.result_column(&column.field));
            query.expr_as(expr, alias);
        }
    }
}

/// Apply JOINs without adding columns to SELECT (for COUNT queries)
fn apply_joins_only(
    query: &mut sea_query::SelectStatement,
    joins: &[JoinSpec],
    base_table: &TableIdent,
) {
    for join in joins {
        let join_alias = Alias::new(join.alias.clone());
        let mut table_ref = TableIdent(join.table.clone()).into_table_ref();
        table_ref = table_ref.alias(join_alias.clone());
        let left_col = match &join.parent {
            Some(parent_alias) => (
                Alias::new(parent_alias.clone()),
                ColumnIdent(join.source_column.clone()),
            )
                .into_column_ref(),
            None => (base_table.clone(), ColumnIdent(join.source_column.clone())).into_column_ref(),
        };
        let right_col =
            (join_alias.clone(), ColumnIdent(join.target_column.clone())).into_column_ref();
        query.left_join(table_ref, Expr::col(left_col).equals(right_col));
        // Note: no columns added - only JOIN clause for filtering
    }
}
