//! SELECT query building

use oxyde_codec::{JoinSpec, LockType as OxydeLockType, QueryIR};
use sea_query::ExprTrait;
use sea_query::{
    Alias, Asterisk, ColumnRef, Expr, Func, IntoColumnRef, IntoTableRef, LockType as SeaLockType,
    Order, PostgresQueryBuilder, Query, SelectStatement, UnionType, Value,
};

use crate::aggregate::build_aggregate_for_table;
use crate::error::{QueryError, Result};
use crate::filter::{build_filter_node_with_scope, ColumnScope};
use crate::utils::identifier::{grouped_projection, output_label, resolve_column};
use crate::utils::{compile_scalar, ColumnIdent, ExprTypes, TableIdent};
use crate::Dialect;

/// Build a `SelectStatement` from QueryIR (without serializing to string).
/// Used recursively for UNION sub-queries.
fn build_select_statement(ir: &QueryIR, dialect: Dialect) -> Result<SelectStatement> {
    let table = TableIdent(ir.table.clone());
    let mut query = Query::select();
    query.from(table.clone());

    let distinct = ir.distinct.unwrap_or(false);
    if distinct {
        query.distinct();
    }

    let annotations = ir.annotations.as_deref().unwrap_or(&[]);
    if ir.aggregates.is_some() && !annotations.is_empty() {
        return Err(QueryError::InvalidQuery(
            "Scalar annotations cannot be combined with aggregates".into(),
        ));
    }

    let group_by = ir.group_by.as_deref().unwrap_or_default();
    let grouped = ir.aggregates.is_some() || !group_by.is_empty();
    let joins = ir.joins.as_deref().unwrap_or_default();
    let exists = ir.exists.unwrap_or(false);
    // EXISTS can omit related columns unless they are part of the UNION
    // projection. Both UNION operands need to keep their normal row shape.
    let project_joins = ir.projects_join_columns()
        || (exists && ir.union_query.is_some() && !grouped && ir.project_joins != Some(false));
    // Reserve `{result_prefix}__*` for columns owned by the joined relation
    // across both raw projections and model hydration.
    for annotation in annotations.iter().filter(|_| !grouped) {
        if let Some(join) = joins.iter().find(|j| {
            annotation
                .alias
                .strip_prefix(j.result_prefix.as_str())
                .is_some_and(|rest| rest.starts_with("__"))
        }) {
            return Err(QueryError::InvalidQuery(format!(
                "Annotation alias '{}' collides with the result columns of join '{}'",
                annotation.alias, join.path
            )));
        }
    }

    let default_table = ir.joins.as_ref().map(|_| ir.table.as_str());
    let column_scope = ColumnScope {
        base_table: Some(&ir.table),
        table: default_table,
        joins,
    };
    // Column and output label of a grouped projection. The label is the name
    // the caller asked for, so a related key never shares a label with a base
    // column whose name looks like the relation path.
    let projected_column = |field: &str| -> (ColumnRef, String) {
        let column =
            resolve_column(Some(&ir.table), joins, field, false).column_ref(Some(&ir.table));
        (column, output_label(ir, field))
    };
    let annotation_index = |field: &str| annotations.iter().position(|a| a.alias == field);
    let is_aggregate_alias = |field: &str| {
        ir.aggregates
            .iter()
            .flatten()
            .any(|agg| agg.alias.as_deref() == Some(field))
    };

    if grouped {
        for field in ir.cols.iter().flatten() {
            if resolve_column(Some(&ir.table), joins, field, false)
                .join
                .is_some()
                && !group_by.contains(field)
                && !is_aggregate_alias(field)
            {
                return Err(QueryError::InvalidQuery(format!(
                    "Related field '{field}' must be in group_by() to be returned by an aggregate query"
                )));
            }
        }
    }

    // Add aggregates or columns
    if let Some(aggregates) = &ir.aggregates {
        let group_projection = grouped_projection(ir)
            .iter()
            .map(|field| projected_column(field))
            .collect::<Vec<_>>();
        for agg in aggregates {
            let agg_expr = build_aggregate_for_table(agg, Some(&ir.table), default_table, joins)?;
            if let Some(alias) = &agg.alias {
                if group_projection.iter().any(|(_, name)| name == alias) {
                    return Err(QueryError::InvalidQuery(format!(
                        "Aggregate alias '{alias}' duplicates a group_by() column; choose another name"
                    )));
                }
                query.expr_as(agg_expr, Alias::new(alias.clone()));
            } else {
                query.expr(agg_expr);
            }
        }
        for (column, name) in &group_projection {
            query.expr_as(Expr::col(column.clone()), Alias::new(name.clone()));
        }
    } else if let Some(cols) = &ir.cols {
        for col in cols {
            let (column, name) = projected_column(col);
            query.expr_as(Expr::col(column), Alias::new(name));
        }
    } else if !joins.is_empty() && !project_joins {
        query.column((table.clone(), Asterisk));
    } else {
        query.column(Asterisk);
    }

    // Scalar annotations follow the plain columns (or `*`). Columns are
    // qualified with the base table when joins are present, matching the
    // qualification of plain columns above.
    let expr_table = ir.joins.as_ref().map(|_| &table);
    for annotation in annotations {
        let expr = compile_scalar(&annotation.expr, expr_table, ExprTypes::of(ir))?;
        query.expr_as(expr, Alias::new(annotation.alias.clone()));
    }

    if let Some(filter_tree) = &ir.filter_tree {
        let expr = build_filter_node_with_scope(
            filter_tree,
            column_scope,
            ir.column_types.as_ref(),
            ir.column_db_types.as_ref(),
            None,
            dialect,
        )?;
        query.and_where(expr);
    }

    for field in group_by {
        let expr = if let Some(idx) = annotation_index(field) {
            // An ordinal avoids input-column shadowing and reuses any binds
            // in the selected expression. Raw IR with * has unknown width.
            match &ir.cols {
                Some(cols) => Expr::cust((cols.len() + idx + 1).to_string()),
                None => Expr::col(ColumnIdent(field.clone())),
            }
        } else {
            Expr::col(
                resolve_column(Some(&ir.table), joins, field, false).column_ref(default_table),
            )
        };
        query.add_group_by([expr]);
    }

    if let Some(having) = &ir.having {
        let expr = build_filter_node_with_scope(
            having,
            column_scope,
            ir.column_types.as_ref(),
            ir.column_db_types.as_ref(),
            ir.aggregates.as_deref(),
            dialect,
        )?;
        query.and_having(expr);
    }

    if let Some(joins) = &ir.joins {
        if project_joins {
            apply_select_joins(&mut query, joins, &table);
        } else {
            apply_joins_only(&mut query, joins, &table);
        }
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
    // A DISTINCT subquery can omit its ordering for EXISTS, where an
    // ordering expression might no longer be projected.
    if let Some(order_by) = ir.order_by.as_ref().filter(|_| !exists || !distinct) {
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
            let is_alias = annotation_index(field).is_some() || is_aggregate_alias(field);
            if ir.union_query.is_some() {
                if let Some(ordinal) = union_order_ordinal(ir, joins, field, project_joins) {
                    query.order_by_expr(Expr::cust(ordinal.to_string()), order);
                } else if ir.cols.is_some() || ir.aggregates.is_some() {
                    return Err(QueryError::InvalidQuery(
                        "UNION can only be ordered by a projected column".into(),
                    ));
                } else {
                    let label = if is_alias {
                        field.clone()
                    } else {
                        output_label(ir, field)
                    };
                    query.order_by(ColumnIdent(label), order);
                }
            } else if is_alias {
                // Output aliases are unqualified in ORDER BY.
                query.order_by(ColumnIdent(field.clone()), order);
            } else {
                // A bare ORDER BY name resolves to an output column first, and
                // output labels can name another column than this one.
                let table = (default_table.is_some() || ir.output_labels.is_some())
                    .then_some(ir.table.as_str());
                let column = resolve_column(Some(&ir.table), joins, field, false);
                query.order_by(column.column_ref(table), order);
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
        let lock = match lock_type {
            OxydeLockType::Update => SeaLockType::Update,
            OxydeLockType::Share => SeaLockType::Share,
        };
        if joins.is_empty() {
            query.lock(lock);
        } else {
            query.lock_with_tables(lock, [table]);
        }
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

        let joins = ir.joins.as_deref().unwrap_or_default();
        if let Some(filter_tree) = &ir.filter_tree {
            let expr = build_filter_node_with_scope(
                filter_tree,
                ColumnScope {
                    base_table: Some(&ir.table),
                    table: ir.joins.as_ref().map(|_| ir.table.as_str()),
                    joins,
                },
                ir.column_types.as_ref(),
                ir.column_db_types.as_ref(),
                None,
                dialect,
            )?;
            count_query.and_where(expr);
        }

        apply_joins_only(&mut count_query, joins, &table);

        return Ok(build_query_string(&count_query, dialect));
    }

    let exists = ir.exists.unwrap_or(false);
    let query = build_select_statement(ir, dialect)?;
    let (sql, values) = build_query_string(&query, dialect);

    if exists {
        return Ok((format!("SELECT EXISTS({sql})"), values));
    }

    Ok((sql, values))
}

/// Find the UNION output position so sorting survives duplicate or truncated labels.
fn union_order_ordinal(
    ir: &QueryIR,
    joins: &[JoinSpec],
    field: &str,
    project_joins: bool,
) -> Option<usize> {
    let (cols, offset) = if let Some(aggregates) = &ir.aggregates {
        if let Some(index) = aggregates
            .iter()
            .position(|agg| agg.alias.as_deref() == Some(field))
        {
            return Some(index + 1);
        }
        (grouped_projection(ir), aggregates.len())
    } else {
        (ir.cols.as_deref()?, 0)
    };
    let annotations = ir.annotations.as_deref().unwrap_or_default();
    if let Some(index) = annotations.iter().position(|a| a.alias == field) {
        return Some(cols.len() + index + 1);
    }
    let table = Some(ir.table.as_str());
    let column = resolve_column(table, joins, field, false);
    let reference = column.column_ref(table);
    if let Some(index) = cols
        .iter()
        .position(|col| resolve_column(table, joins, col, false).column_ref(table) == reference)
    {
        return Some(offset + index + 1);
    }

    let target = column.join.filter(|_| project_joins)?;
    // Join projections follow the base columns and scalar annotations.
    let mut offset = offset + cols.len() + annotations.len();
    for join in joins {
        if std::ptr::eq(join, target) {
            return join
                .columns
                .iter()
                .position(|candidate| candidate.column == column.column)
                .map(|index| offset + index + 1);
        }
        offset += join.columns.len();
    }
    None
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

/// Apply JOINs for filtering without projecting related columns.
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
