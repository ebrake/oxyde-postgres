//! INSERT query building

use std::collections::{HashMap, HashSet};

use oxyde_codec::{ConflictAction, QueryIR};
use sea_query::{Alias, Expr, PostgresQueryBuilder, Query, SimpleExpr, TableRef, Value};

use crate::error::{QueryError, Result};
use crate::utils::{
    assignment_expr, assignment_expr_for_table, ColumnIdent, ExprTypes, TableIdent,
};
use crate::Dialect;

/// Alias for the INSERT target of an `ON CONFLICT DO UPDATE`.
///
/// Aliasing hides the table name, so a table named `excluded` cannot shadow
/// PostgreSQL's proposed row in the generated `"excluded"."col"` assignments.
const TARGET_ALIAS: &str = "__oxyde_target";

/// Alias for the pre-update row in `RETURNING WITH (OLD AS ...)`.
const OLD_ALIAS: &str = "__oxyde_old";

/// Build INSERT query from QueryIR
pub fn build_insert(ir: &QueryIR, dialect: Dialect) -> Result<(String, Vec<Value>)> {
    let table = TableIdent(ir.table.clone());
    let mut query = Query::insert();
    let upsert = ir
        .on_conflict
        .as_ref()
        .is_some_and(|conflict| matches!(conflict.action, ConflictAction::Update));
    if upsert {
        query.into_table(TableRef::from(table).alias(Alias::new(TARGET_ALIAS)));
    } else {
        query.into_table(table);
    }

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
                        let update_columns = on_conflict.update_columns.as_deref().unwrap_or(&[]);
                        let update_values = on_conflict.update_values.as_ref();
                        if update_columns.is_empty() && update_values.is_none_or(HashMap::is_empty)
                        {
                            return Err(QueryError::InvalidQuery(
                                "ON CONFLICT UPDATE requires nonempty update_columns or update_values"
                                    .into(),
                            ));
                        }

                        let mut seen = HashSet::new();
                        for col in update_columns {
                            if update_values.is_some_and(|values| values.contains_key(col)) {
                                return Err(QueryError::InvalidQuery(format!(
                                    "ON CONFLICT UPDATE has multiple assignments for column '{col}'"
                                )));
                            }
                        }
                        let mut conflict = sea_query::OnConflict::columns(target_cols);
                        conflict.update_columns(
                            update_columns
                                .iter()
                                .filter(|col| seen.insert(*col))
                                .cloned()
                                .map(ColumnIdent),
                        );
                        if let Some(values) = update_values {
                            let target = TableIdent(TARGET_ALIAS.into());
                            for (col, val) in values {
                                conflict.value(
                                    ColumnIdent(col.clone()),
                                    assignment_expr_for_table(
                                        val,
                                        col,
                                        types,
                                        dialect,
                                        Some(&target),
                                    )?,
                                );
                            }
                        }

                        query.on_conflict(conflict);
                    }
                }
            }
        }
    }

    // Return full rows when requested, otherwise return generated PKs only.
    // Routing (emits_returning) stays false for the pk-only form by design.
    if crate::emits_returning(ir, dialect) {
        let inserted_alias = ir
            .on_conflict
            .as_ref()
            .filter(|conflict| matches!(conflict.action, ConflictAction::Update))
            .and_then(|conflict| conflict.inserted_alias.as_deref());
        if let Some(inserted_alias) = inserted_alias {
            let pk = ir.pk_column.as_deref().ok_or_else(|| {
                QueryError::InvalidQuery("ON CONFLICT insert outcome requires pk_column".into())
            })?;
            let pk = pk.replace('"', "\"\"");
            let inserted_alias = inserted_alias.replace('"', "\"\"");
            // The target is aliased (see TARGET_ALIAS), so its real name is out of
            // scope here and cannot collide with OLD_ALIAS.
            let returning = format!(
                r#"WITH (OLD AS "{OLD_ALIAS}") *, ("{OLD_ALIAS}"."{pk}" IS NULL) AS "{inserted_alias}""#
            );
            query.returning(Query::returning().exprs([Expr::cust(returning)]));
        } else {
            query.returning_all();
        }
    } else {
        let pk = ir.pk_column.as_deref().unwrap_or("id");
        query.returning_col(ColumnIdent(pk.to_string()));
    }

    let (sql, values) = match dialect {
        Dialect::Postgres => query.build(PostgresQueryBuilder),
    };

    Ok((sql, values.0))
}
