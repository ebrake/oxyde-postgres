//! UPDATE query building

use oxyde_codec::QueryIR;
use sea_query::{PostgresQueryBuilder, Query, Value};

use crate::error::Result;
use crate::filter::build_filter_node;
use crate::utils::{assignment_expr, ColumnIdent, ExprTypes, TableIdent};
use crate::Dialect;

/// Build UPDATE query from QueryIR
pub fn build_update(ir: &QueryIR, dialect: Dialect) -> Result<(String, Vec<Value>)> {
    // Check for bulk update first
    if let Some(bulk) = &ir.bulk_update {
        return super::bulk::build_bulk_update(ir, bulk, dialect);
    }

    let table = TableIdent(ir.table.clone());
    let mut query = Query::update();
    query.table(table);

    if let Some(values) = &ir.values {
        for (col, val) in values {
            query.value(
                ColumnIdent(col.clone()),
                assignment_expr(val, col, ExprTypes::of(ir), dialect)?,
            );
        }
    }

    // Add filters (no JOIN in UPDATE, so no table qualification needed)
    if let Some(filter_tree) = &ir.filter_tree {
        let expr = build_filter_node(
            filter_tree,
            None,
            ir.column_types.as_ref(),
            ir.column_db_types.as_ref(),
            None,
            dialect,
        )?;
        query.and_where(expr);
    }

    if crate::emits_returning(ir, dialect) {
        query.returning_all();
    }

    let (sql, values) = match dialect {
        Dialect::Postgres => query.build(PostgresQueryBuilder),
    };

    Ok((sql, values.0))
}
