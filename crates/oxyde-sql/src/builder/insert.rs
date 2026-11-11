//! INSERT query building

use oxyde_codec::{ColumnTypeSpec, ConflictAction, QueryIR};
use sea_query::{PostgresQueryBuilder, Query, SimpleExpr, Value};

use crate::error::{QueryError, Result};
use crate::utils::{bind_value, rmpv_to_simple_expr, typed_value_expr, ColumnIdent, TableIdent};
use crate::Dialect;

/// Build INSERT query from QueryIR
pub fn build_insert(ir: &QueryIR, dialect: Dialect) -> Result<(String, Vec<Value>)> {
    let table = TableIdent(ir.table.clone());
    let mut query = Query::insert();
    query.into_table(table);

    // Helper to get the column spec; Unknown = native conversion
    let get_spec = |col: &str| -> &ColumnTypeSpec {
        ir.column_types
            .as_ref()
            .and_then(|ct| ct.get(col))
            .unwrap_or(&ColumnTypeSpec::Unknown)
    };

    // Single row insert
    if let Some(values) = &ir.values {
        let mut columns = Vec::new();
        let mut vals: Vec<SimpleExpr> = Vec::new();

        for (col, val) in values {
            columns.push(ColumnIdent(col.clone()));
            if let Some(expr) = rmpv_to_simple_expr(val)? {
                vals.push(expr);
            } else {
                let spec = get_spec(col);
                vals.push(typed_value_expr(bind_value(val, spec), spec, dialect));
            }
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
                if let Some(expr) = rmpv_to_simple_expr(val)? {
                    vals.push(expr);
                } else {
                    let spec = get_spec(&col.0);
                    vals.push(typed_value_expr(bind_value(val, spec), spec, dialect));
                }
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
                            if let Some(expr) = rmpv_to_simple_expr(val)? {
                                conflict.value(ColumnIdent(col.clone()), expr);
                            } else {
                                let spec = get_spec(col);
                                conflict.value(
                                    ColumnIdent(col.clone()),
                                    typed_value_expr(bind_value(val, spec), spec, dialect),
                                );
                            }
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
