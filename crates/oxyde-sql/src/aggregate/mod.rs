//! Aggregate function building

use oxyde_codec::{Aggregate, AggregateOp, JoinSpec};
use sea_query::{Asterisk, Expr, Func, SimpleExpr};

use crate::error::{QueryError, Result};
use crate::utils::identifier::resolve_column;

/// Build aggregate expression from Aggregate specification
pub fn build_aggregate(agg: &Aggregate) -> Result<SimpleExpr> {
    build_aggregate_for_table(agg, None, None, &[])
}

pub(crate) fn build_aggregate_for_table(
    agg: &Aggregate,
    base_table: Option<&str>,
    table: Option<&str>,
    joins: &[JoinSpec],
) -> Result<SimpleExpr> {
    let column =
        |name: &str| Expr::col(resolve_column(base_table, joins, name, false).column_ref(table));
    let distinct = agg.distinct.unwrap_or(false);
    let expr =
        match &agg.op {
            AggregateOp::Count => {
                let col = match agg.field.as_deref() {
                    Some(field) if field != "*" => column(field),
                    _ if distinct => {
                        return Err(QueryError::InvalidQuery(
                            "COUNT(DISTINCT ...) requires a field".into(),
                        ));
                    }
                    _ => Expr::col(Asterisk),
                };
                if distinct {
                    Func::count_distinct(col).into()
                } else {
                    Func::count(col).into()
                }
            }
            AggregateOp::Sum => {
                let field = agg.field.as_ref().ok_or_else(|| {
                    QueryError::InvalidQuery("SUM aggregate requires field".into())
                })?;
                let col = column(field);
                if distinct {
                    Expr::cust_with_expr("SUM(DISTINCT $1)", col)
                } else {
                    Func::sum(col).into()
                }
            }
            AggregateOp::Avg => {
                let field = agg.field.as_ref().ok_or_else(|| {
                    QueryError::InvalidQuery("AVG aggregate requires field".into())
                })?;
                let col = column(field);
                if distinct {
                    Expr::cust_with_expr("AVG(DISTINCT $1)", col)
                } else {
                    Func::avg(col).into()
                }
            }
            AggregateOp::Max => {
                let field = agg.field.as_ref().ok_or_else(|| {
                    QueryError::InvalidQuery("MAX aggregate requires field".into())
                })?;
                // DISTINCT is meaningless for MAX
                Func::max(column(field)).into()
            }
            AggregateOp::Min => {
                let field = agg.field.as_ref().ok_or_else(|| {
                    QueryError::InvalidQuery("MIN aggregate requires field".into())
                })?;
                // DISTINCT is meaningless for MIN
                Func::min(column(field)).into()
            }
        };
    Ok(expr)
}
