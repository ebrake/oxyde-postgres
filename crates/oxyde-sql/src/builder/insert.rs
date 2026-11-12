//! INSERT query building

use oxyde_codec::{ConflictAction, QueryIR};
use sea_query::{PostgresQueryBuilder, Query, SimpleExpr, Value};

use crate::error::{QueryError, Result};
use crate::utils::{assignment_expr, ColumnIdent, ExprTypes, TableIdent};
use crate::Dialect;

/// Build INSERT query from QueryIR
pub fn build_insert(ir: &QueryIR, dialect: Dialect) -> Result<(String, Vec<Value>)> {
    let table = TableIdent(ir.table.clone());
    let mut query = Query::insert();
    query.into_table(table);

    let types = ExprTypes::of(ir);

    // Single row insert
    if let Some(values) = &ir.values {
        let mut columns = Vec::new();
        let mut vals: Vec<SimpleExpr> = Vec::new();

        for (col, val) in values {
            columns.push(ColumnIdent(col.clone()));
            vals.push(assignment_expr(val, col, types, dialect)?);
        }

        query.columns(columns);
        query.values(vals)?;
    }
    // Bulk insert
    else if let Some(bulk_values) = &ir.bulk_values {
        if bulk_values.is_empty() {
            return Err(QueryError::InvalidQuery(
                "Bulk insert requires at least one row".into(),
            ));
        }

        // Extract columns from first row
        let first_row = &bulk_values[0];
        let mut columns: Vec<ColumnIdent> =
            first_row.keys().map(|k| ColumnIdent(k.clone())).collect();
        columns.sort_by(|a, b| a.0.cmp(&b.0));

        query.columns(columns.clone());

        // Add each row
        for row in bulk_values {
            let mut vals: Vec<SimpleExpr> = Vec::new();
            for col in &columns {
                let val = row.get(&col.0).ok_or_else(|| {
                    QueryError::InvalidQuery(format!(
                        "Missing column '{}' in bulk insert row",
                        col.0
                    ))
                })?;
                vals.push(assignment_expr(val, &col.0, types, dialect)?);
            }
            query.values(vals)?;
        }
    }

    // Add ON CONFLICT clause (UPSERT)
    if let Some(on_conflict) = &ir.on_conflict {
        match dialect {
            Dialect::Postgres => {
                let target_cols: Vec<ColumnIdent> = on_conflict
                    .columns
                    .iter()
                    .map(|c| ColumnIdent(c.clone()))
                    .collect();

                match on_conflict.action {
                    ConflictAction::Nothing => {
                        query.on_conflict(
                            sea_query::OnConflict::columns(target_cols)
                                .do_nothing()
                                .to_owned(),
                        );
                    }
                    ConflictAction::Update => {
                        // Require update_values for Update action
                        let update_vals = on_conflict.update_values.as_ref().ok_or_else(|| {
                            QueryError::InvalidQuery(
                                "ON CONFLICT UPDATE requires update_values".into(),
                            )
                        })?;

                        if update_vals.is_empty() {
                            return Err(QueryError::InvalidQuery(
                                "ON CONFLICT UPDATE requires at least one value to update".into(),
                            ));
                        }

                        let mut conflict = sea_query::OnConflict::columns(target_cols);
                        for (col, val) in update_vals {
                            conflict.value(
                                ColumnIdent(col.clone()),
                                assignment_expr(val, col, types, dialect)?,
                            );
                        }

                        query.on_conflict(conflict.clone());
                    }
                }
            }
        }
    }

    // them, otherwise pk-only so the driver reads generated PKs from rows
    // instead of appending to SQL text. Routing (emits_returning) stays
    // false for the pk-only form by design.
    if crate::emits_returning(ir, dialect) {
        query.returning_all();
    } else {
        let pk = ir.pk_column.as_deref().unwrap_or("id");
        query.returning_col(ColumnIdent(pk.to_string()));
    }

    let (sql, values) = match dialect {
        Dialect::Postgres => query.build(PostgresQueryBuilder),
    };

    Ok((sql, values.0))
}
