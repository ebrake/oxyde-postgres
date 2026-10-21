//! SQL templates over a filtered column.

use sea_query::{Expr, SimpleExpr, Value};

use crate::Dialect;

/// Render a template whose `{col}` markers stand for the filtered column.
///
/// PostgreSQL references it by number, so repeats render the same
/// expression; `?` dialects consume one value per placeholder, so the column
/// is pushed once per marker.
pub(super) fn render_col(template: &str, col: &SimpleExpr, dialect: Dialect) -> SimpleExpr {
    if dialect == Dialect::Postgres {
        return Expr::cust_with_exprs(template.replace("{col}", "$1"), [col.clone()]);
    }
    let parts: Vec<&str> = template.split("{col}").collect();
    let values = vec![col.clone(); parts.len() - 1];
    Expr::cust_with_exprs(parts.join("?"), values)
}

/// Render a template whose `{col}` / `{arg}` markers stand for the filtered
/// column and the bound `operand`, the same way as `render_col`; on `?`
/// dialects values are pushed in marker order.
pub(super) fn render(
    template: &str,
    col: &SimpleExpr,
    operand: &Value,
    dialect: Dialect,
) -> SimpleExpr {
    let arg = SimpleExpr::Value(operand.clone());
    if dialect == Dialect::Postgres {
        let sql = template.replace("{col}", "$1").replace("{arg}", "$2");
        return Expr::cust_with_exprs(sql, [col.clone(), arg]);
    }

    let mut sql = String::with_capacity(template.len());
    let mut values = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        sql.push_str(&rest[..start]);
        let tail = &rest[start..];
        if let Some(after) = tail.strip_prefix("{col}") {
            values.push(col.clone());
            sql.push('?');
            rest = after;
        } else if let Some(after) = tail.strip_prefix("{arg}") {
            values.push(arg.clone());
            sql.push('?');
            rest = after;
        } else {
            sql.push('{');
            rest = &tail[1..];
        }
    }
    sql.push_str(rest);
    Expr::cust_with_exprs(sql, values)
}
