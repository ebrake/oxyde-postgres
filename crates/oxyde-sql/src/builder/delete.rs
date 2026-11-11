//! DELETE query building

use oxyde_codec::QueryIR;
use sea_query::{PostgresQueryBuilder, Query, Value};

use crate::error::Result;
use crate::filter::build_filter_node;
use crate::utils::TableIdent;
use crate::Dialect;

/// Build DELETE query from QueryIR
pub fn build_delete(ir: &QueryIR, dialect: Dialect) -> Result<(String, Vec<Value>)> {
    let table = TableIdent(ir.table.clone());
    let mut query = Query::delete();
    query.from_table(table);

    // Add filters (no JOIN in DELETE, so no table qualification needed)
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
