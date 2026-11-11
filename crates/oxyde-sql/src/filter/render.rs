//! PostgreSQL SQL templates over a filtered column.
use crate::Dialect;
use sea_query::{Expr, SimpleExpr, Value};

pub(super) fn render_col(template: &str, col: &SimpleExpr, _dialect: Dialect) -> SimpleExpr {
    Expr::cust_with_exprs(template.replace("{col}", "$1"), [col.clone()])
}
pub(super) fn render(
    template: &str,
    col: &SimpleExpr,
    operand: &Value,
    _dialect: Dialect,
) -> SimpleExpr {
    let sql = template.replace("{col}", "$1").replace("{arg}", "$2");
    Expr::cust_with_exprs(sql, [col.clone(), SimpleExpr::Value(operand.clone())])
}
