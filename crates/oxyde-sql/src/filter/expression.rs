//! Filter expression building for WHERE clauses

use std::collections::HashMap;

use oxyde_codec::{Aggregate, AggregateOp, ColumnTypeSpec, Filter, FilterNode, JoinSpec};
use sea_query::ExprTrait;
use sea_query::{Expr, Func, LikeExpr, SimpleExpr, Value};

use super::array::{build_array_filter, ArrayOp};
use super::pg_array::{out_of_range_element, pg_array_type};
use super::render::render_col;
use crate::aggregate::build_aggregate_for_table;
use crate::error::{QueryError, Result};
use crate::utils::bind::ensure_array_elements;
use crate::utils::identifier::resolve_column;
use crate::utils::{bind_value, typed_value_expr};
use crate::Dialect;

/// Type of an aggregate's result, for binding the values a HAVING condition
/// compares it with: derived from the column it aggregates, never from a
/// column that shares its alias.
///
/// MIN / MAX have the column's own type, `db_type` override included. SUM /
/// AVG of an integer are exact numerics (`numeric` on PostgreSQL), so a
/// `Decimal` value binds as a decimal; integers and floats bind as they are.
fn aggregate_type<'a>(
    agg: &Aggregate,
    scope: ColumnScope<'_>,
    col_types: Option<&HashMap<String, ColumnTypeSpec>>,
    db_types: Option<&'a HashMap<String, String>>,
) -> (ColumnTypeSpec, Option<&'a str>) {
    let field = resolve_column(
        scope.base_table,
        scope.joins,
        agg.field.as_deref().unwrap_or("*"),
        false,
    );
    let field_spec = field.lookup(col_types).unwrap_or(&ColumnTypeSpec::Unknown);
    match agg.op {
        AggregateOp::Count => (ColumnTypeSpec::BigInteger, None),
        AggregateOp::Min | AggregateOp::Max => (
            field_spec.clone(),
            field.lookup(db_types).map(String::as_str),
        ),
        AggregateOp::Sum | AggregateOp::Avg => {
            let spec = match field_spec {
                ColumnTypeSpec::Double => ColumnTypeSpec::Double,
                ColumnTypeSpec::BigInteger | ColumnTypeSpec::Decimal { .. } => {
                    ColumnTypeSpec::Decimal {
                        precision: None,
                        scale: None,
                    }
                }
                ColumnTypeSpec::Timedelta => ColumnTypeSpec::Timedelta,
                _ => ColumnTypeSpec::Unknown,
            };
            (spec, None)
        }
    }
}

/// PostgreSQL array type that a filter's array values are cast to: the
/// column's `db_type` override, since an `INTEGER[]` column has no `=` with
/// the `bigint[]` the value binds as. An override that is not an array type
/// leaves values as they bind.
fn pg_array_cast(
    spec: &ColumnTypeSpec,
    db_type: Option<&str>,
    dialect: Dialect,
) -> Option<Result<String>> {
    match (dialect, spec, db_type) {
        (Dialect::Postgres, ColumnTypeSpec::Array { .. }, Some(db_type))
            if db_type.trim().ends_with("[]") =>
        {
            Some(pg_array_type(db_type))
        }
        _ => None,
    }
}

/// `typed_value_expr`, plus the `pg_array_cast` of an array value.
///
/// `=`, `!=` and `IN` answer values the array type cannot hold themselves;
/// an ordering comparison against one raises rather than fail the cast.
/// On PostgreSQL, an array element of another type than the column's (a
/// float in a `list[Decimal]`) raises, as it would bind as NULL. JSON arrays
/// so the members of a union element type (`list[str | int]`) all match.
fn filter_value_expr(
    value: Value,
    spec: &ColumnTypeSpec,
    array_cast: Option<&Result<String>>,
    dialect: Dialect,
) -> Result<SimpleExpr> {
    ensure_array_elements(&value)?;
    match (array_cast, &value) {
        (Some(Ok(array_type)), Value::Array(..)) => {
            if let Some(shown) = out_of_range_element(&value, array_type) {
                return Err(QueryError::InvalidQuery(format!(
                    "Array element {shown} is out of range for {array_type}"
                )));
            }
            Ok(Expr::cust_with_values(
                format!("$1::{array_type}"),
                vec![value],
            ))
        }
        // An override that cannot be read only fails a filter that needs it.
        (Some(Err(err)), Value::Array(..)) => Err(err.clone()),
        _ => Ok(typed_value_expr(value, spec, dialect)),
    }
}

/// Whether an array value has an element the column's array type cannot
/// hold, so it equals no stored value.
fn out_of_range(value: &Value, array_cast: Option<&Result<String>>) -> bool {
    match array_cast {
        Some(Ok(array_type)) => out_of_range_element(value, array_type).is_some(),
        _ => false,
    }
}

/// `=` (`equal`) or `!=` against a value that equals no stored value: NULL
/// for a NULL column, as the comparison would be, and a constant otherwise.
fn never_equal(col: &Expr, equal: bool, dialect: Dialect) -> SimpleExpr {
    let template = if equal {
        "(CASE WHEN {col} IS NULL THEN NULL ELSE FALSE END)"
    } else {
        "(CASE WHEN {col} IS NULL THEN NULL ELSE TRUE END)"
    };
    render_col(template, col, dialect)
}

/// Left side of LIKE / ILIKE. A `str` field stored as JSON matches the
/// string it reads back as, not its quoted JSON text.
///
/// The document is not always a string: a `str` value that is JSON text
/// (`'123'`, `'null'`) is stored as that JSON value. A JSON null matches
/// nothing on every backend; other documents match as each database prints
/// them.
fn like_target(col: Expr, spec: &ColumnTypeSpec, dialect: Dialect) -> Expr {
    if !matches!(spec, ColumnTypeSpec::Json | ColumnTypeSpec::JsonBinary) {
        return col;
    }
    let template = match dialect {
        Dialect::Postgres => "{col} #>> '{}'",
        // JSON_UNQUOTE() prints a JSON null as 'null'; `#>>` and
        // json_extract() give SQL NULL.

        // on text that is not JSON, so it is checked first; such text
        // matches as stored. Flags 9 accept RFC 8259 text (1) and strictly
        // valid JSONB blobs (8).
    };
    Expr::expr(render_col(template, &col, dialect))
}

fn like_expr(filter: &Filter, pattern: String) -> Result<LikeExpr> {
    let expr = LikeExpr::new(pattern);
    let Some(escape) = filter.escape.as_deref() else {
        return Ok(expr);
    };

    let mut chars = escape.chars();
    match (chars.next(), chars.next()) {
        (Some(ch), None) => Ok(expr.escape(ch)),
        _ => Err(QueryError::InvalidQuery(
            "LIKE escape requires a single character".into(),
        )),
    }
}

/// Build filter clause from FilterNode tree.
///
/// default_table: if provided, unqualified columns will be prefixed with this table name
/// col_types: type hints for typed value conversion in filter conditions
/// db_types: `db_type` overrides, keyed like `col_types`
/// aggregates: if provided (HAVING), alias references are substituted with aggregate expressions
pub fn build_filter_node(
    node: &FilterNode,
    default_table: Option<&str>,
    col_types: Option<&HashMap<String, ColumnTypeSpec>>,
    db_types: Option<&HashMap<String, String>>,
    aggregates: Option<&[Aggregate]>,
    dialect: Dialect,
) -> Result<SimpleExpr> {
    build_filter_node_with_scope(
        node,
        ColumnScope {
            base_table: default_table,
            table: default_table,
            joins: &[],
        },
        col_types,
        db_types,
        aggregates,
        dialect,
    )
}

#[derive(Clone, Copy)]
pub(crate) struct ColumnScope<'a> {
    pub base_table: Option<&'a str>,
    pub table: Option<&'a str>,
    pub joins: &'a [JoinSpec],
}

impl<'a> ColumnScope<'a> {
    pub(crate) fn unqualified(base_table: &'a str) -> Self {
        Self {
            base_table: Some(base_table),
            table: None,
            joins: &[],
        }
    }
}

pub(crate) fn build_filter_node_with_scope(
    node: &FilterNode,
    column_scope: ColumnScope<'_>,
    col_types: Option<&HashMap<String, ColumnTypeSpec>>,
    db_types: Option<&HashMap<String, String>>,
    aggregates: Option<&[Aggregate]>,
    dialect: Dialect,
) -> Result<SimpleExpr> {
    match node {
        FilterNode::Condition(filter) => apply_filter(
            filter,
            column_scope,
            col_types,
            db_types,
            aggregates,
            dialect,
        ),
        FilterNode::And { conditions } => {
            if conditions.is_empty() {
                return Err(QueryError::InvalidQuery(
                    "AND node must have at least one condition".into(),
                ));
            }
            let first = build_filter_node_with_scope(
                &conditions[0],
                column_scope,
                col_types,
                db_types,
                aggregates,
                dialect,
            )?;
            let mut result = first;
            for cond in &conditions[1..] {
                let next = build_filter_node_with_scope(
                    cond,
                    column_scope,
                    col_types,
                    db_types,
                    aggregates,
                    dialect,
                )?;
                result = result.and(next);
            }
            Ok(result)
        }
        FilterNode::Or { conditions } => {
            if conditions.is_empty() {
                return Err(QueryError::InvalidQuery(
                    "OR node must have at least one condition".into(),
                ));
            }
            let first = build_filter_node_with_scope(
                &conditions[0],
                column_scope,
                col_types,
                db_types,
                aggregates,
                dialect,
            )?;
            let mut result = first;
            for cond in &conditions[1..] {
                let next = build_filter_node_with_scope(
                    cond,
                    column_scope,
                    col_types,
                    db_types,
                    aggregates,
                    dialect,
                )?;
                result = result.or(next);
            }
            Ok(result)
        }
        FilterNode::Not { condition } => {
            let inner = build_filter_node_with_scope(
                condition,
                column_scope,
                col_types,
                db_types,
                aggregates,
                dialect,
            )?;
            Ok(inner.not())
        }
    }
}

/// Apply filter to expression.
/// If the aggregate scope contains a matching alias,
/// the aggregate expression (e.g. SUM("views")) is used instead of a column ref.
fn apply_filter(
    filter: &Filter,
    column_scope: ColumnScope<'_>,
    col_types: Option<&HashMap<String, ColumnTypeSpec>>,
    db_types: Option<&HashMap<String, String>>,
    aggregates: Option<&[Aggregate]>,
    dialect: Dialect,
) -> Result<SimpleExpr> {
    let col_name = filter.column.as_ref().unwrap_or(&filter.field);
    let column = resolve_column(column_scope.base_table, column_scope.joins, col_name, true);
    let col_spec = column.lookup(col_types).unwrap_or(&ColumnTypeSpec::Unknown);
    let col_db_type = column.lookup(db_types).map(String::as_str);

    // HAVING: substitute aggregate alias with aggregate expression
    let agg_match =
        aggregates.and_then(|aggs| aggs.iter().find(|a| a.alias.as_deref() == Some(col_name)));

    if let Some(op) = ArrayOp::from_operator(&filter.operator) {
        if agg_match.is_some() {
            return Err(QueryError::InvalidQuery(
                "Array lookups cannot filter on aggregate aliases".into(),
            ));
        }
        if matches!(col_spec, ColumnTypeSpec::Unknown) {
            return Err(QueryError::InvalidQuery(format!(
                "Array lookup on '{col_name}' needs its column type, which this query does not provide"
            )));
        }
        return build_array_filter(
            &Expr::col(column.column_ref(column_scope.table)),
            op,
            &filter.value,
            col_spec,
            col_db_type,
            dialect,
        );
    }

    // An aggregate is typed by its result, not by the column of the same name.
    let agg_spec;
    let (col, spec, db_type) = if let Some(agg) = agg_match {
        let (agg_type, agg_db_type) = aggregate_type(agg, column_scope, col_types, db_types);
        agg_spec = agg_type;
        (
            Expr::expr(build_aggregate_for_table(
                agg,
                column_scope.base_table,
                column_scope.table,
                column_scope.joins,
            )?),
            &agg_spec,
            agg_db_type,
        )
    } else {
        (
            Expr::col(column.column_ref(column_scope.table)),
            col_spec,
            col_db_type,
        )
    };

    // Resolved once: an IN list would otherwise reparse db_type per value.
    let array_cast = pg_array_cast(spec, db_type, dialect);
    let array_cast = array_cast.as_ref();
    let typed = |value: Value| filter_value_expr(value, spec, array_cast, dialect);
    let val = bind_value(&filter.value, spec);

    let expr = match filter.operator.as_str() {
        // A value the column's array type cannot hold equals no stored value.
        "=" if out_of_range(&val, array_cast) => never_equal(&col, true, dialect),
        "!=" if out_of_range(&val, array_cast) => never_equal(&col, false, dialect),
        "=" => col.eq(typed(val)?),
        "!=" => col.ne(typed(val)?),
        ">" => col.gt(typed(val)?),
        ">=" => col.gte(typed(val)?),
        "<" => col.lt(typed(val)?),
        "<=" => col.lte(typed(val)?),
        "LIKE" => {
            let text = filter.value.as_str().ok_or_else(|| {
                QueryError::InvalidQuery("LIKE operator requires string value".into())
            })?;
            like_target(col, spec, dialect).like(like_expr(filter, text.to_string())?)
        }
        "ILIKE" => {
            let text = filter.value.as_str().ok_or_else(|| {
                QueryError::InvalidQuery("ILIKE operator requires string value".into())
            })?;
            let lowered = text.to_lowercase();
            let lower_col = Func::lower(like_target(col, spec, dialect));
            Expr::expr(lower_col).like(like_expr(filter, lowered)?)
        }
        "IN" => {
            if let rmpv::Value::Array(arr) = &filter.value {
                // Values the column's array type cannot hold match nothing.
                let values: Vec<Value> = arr
                    .iter()
                    .map(|v| bind_value(v, spec))
                    .filter(|v| !out_of_range(v, array_cast))
                    .collect();
                if values.is_empty() && !arr.is_empty() {
                    never_equal(&col, true, dialect)
                } else {
                    let values = values.into_iter().map(typed).collect::<Result<Vec<_>>>()?;
                    col.is_in(values)
                }
            } else {
                return Err(QueryError::InvalidQuery(
                    "IN operator requires array value".to_string(),
                ));
            }
        }
        "BETWEEN" => {
            if let rmpv::Value::Array(arr) = &filter.value {
                if arr.len() != 2 {
                    return Err(QueryError::InvalidQuery(
                        "BETWEEN operator requires exactly two values".to_string(),
                    ));
                }
                let start = typed(bind_value(&arr[0], spec))?;
                let end = typed(bind_value(&arr[1], spec))?;
                col.between(start, end)
            } else {
                return Err(QueryError::InvalidQuery(
                    "BETWEEN operator requires array value".to_string(),
                ));
            }
        }
        "IS NULL" => col.is_null(),
        "IS NOT NULL" => col.is_not_null(),
        op => {
            return Err(QueryError::UnsupportedOperation(format!(
                "Unsupported operator: {op}",
            )))
        }
    };

    Ok(expr)
}
