//! Scalar expression compilation (`ScalarExpr` / `__expr__` IR nodes) and rmpv→JSON conversion.
//!
//! Value binding lives in `bind.rs` (`bind_value`, spec-driven). This module
//! compiles expression trees produced by Python's `F()` / `Coalesce` / `Greatest` and converts
//! rmpv maps/arrays into `serde_json::Value` for JSON column storage.

use std::collections::HashMap;

use sea_query::ExprTrait;
use sea_query::{Expr, Func, IntoColumnRef, SimpleExpr, Value};

use crate::error::{QueryError, Result};
use crate::utils::bind::typed_value_expr;
use crate::utils::identifier::{ColumnIdent, TableIdent};
use crate::Dialect;
use oxyde_codec::{ColumnTypeSpec, ScalarExpr};

/// Convert rmpv::Value to serde_json::Value for JSON column storage.
/// Returns `None` for values that cannot be represented in JSON (NaN, Ext).
pub(crate) fn rmpv_to_json(value: &rmpv::Value) -> Option<serde_json::Value> {
    Some(match value {
        rmpv::Value::Nil => serde_json::Value::Null,
        rmpv::Value::Boolean(b) => serde_json::Value::Bool(*b),
        rmpv::Value::Integer(n) => {
            let num = n
                .as_i64()
                .map(serde_json::Number::from)
                .or_else(|| n.as_u64().map(serde_json::Number::from))?;
            serde_json::Value::Number(num)
        }
        rmpv::Value::F32(f) => {
            serde_json::Value::Number(serde_json::Number::from_f64(f64::from(*f))?)
        }
        rmpv::Value::F64(f) => serde_json::Value::Number(serde_json::Number::from_f64(*f)?),
        rmpv::Value::String(s) => {
            serde_json::Value::String(s.as_str().unwrap_or_default().to_string())
        }
        rmpv::Value::Binary(b) => serde_json::Value::Array(
            b.iter()
                .map(|&byte| serde_json::Value::Number(byte.into()))
                .collect(),
        ),
        rmpv::Value::Array(arr) => {
            let items: Option<Vec<_>> = arr.iter().map(rmpv_to_json).collect();
            serde_json::Value::Array(items?)
        }
        rmpv::Value::Map(pairs) => {
            let mut map = serde_json::Map::new();
            for (k, v) in pairs {
                let key = k.as_str().map_or_else(|| k.to_string(), String::from);
                map.insert(key, rmpv_to_json(v)?);
            }
            serde_json::Value::Object(map)
        }
        rmpv::Value::Ext(_, _) => return None,
    })
}

/// Decode a `{"__expr__": <node>}` wrapper. `Ok(None)` when `value` is not one.
fn decode_expr_wrapper(value: &rmpv::Value) -> Result<Option<ScalarExpr>> {
    let Some(expr) = map_get(value, "__expr__") else {
        return Ok(None);
    };
    rmpv::ext::from_value(expr.clone())
        .map(Some)
        .map_err(|e| QueryError::InvalidQuery(format!("Invalid expression node: {e}")))
}

/// Build the SQL expression for a value assigned to column `spec` in
/// INSERT / UPDATE / ON CONFLICT:
///
/// * `{"__expr__": <literal node>}` (a bare `Value(...)`) is a plain literal
///   for the target column. It is *cast* by the column's spec like any raw
///   value (an enum column gets its `::enum` cast, a text-backed enum does
///   not). It is *bound* by the column's spec too, except when that spec is
///   `Unknown` (e.g. MONEY, CITEXT): then the literal's own type hint (Decimal,
///   UUID, ...) decides the binding. An enum hint never applies there, since
///   it names a Python enum, not the column's type;
/// * any other `{"__expr__": ...}` compiles as a scalar expression, with the
///   target column's enum type (if any) inherited by its string/NULL literals
///   inside `coalesce`/`greatest`;
/// * everything else binds by the column's spec.
pub fn assignment_expr(
    value: &rmpv::Value,
    spec: &ColumnTypeSpec,
    column_types: Option<&ColumnTypes>,
    dialect: Dialect,
) -> Result<SimpleExpr> {
    let Some(node) = decode_expr_wrapper(value)? else {
        return Ok(typed_value_expr(
            super::bind::bind_value(value, spec),
            spec,
            dialect,
        ));
    };
    match node {
        ScalarExpr::Value {
            value: literal,
            value_type,
        } => {
            let bind_spec = match (spec, literal_spec(value_type.as_ref())) {
                (ColumnTypeSpec::Unknown, ColumnTypeSpec::Enum { .. }) => ColumnTypeSpec::Unknown,
                (ColumnTypeSpec::Unknown, hint) => hint,
                _ => spec.clone(),
            };
            Ok(typed_value_expr(
                super::bind::bind_value(&literal, &bind_spec),
                spec,
                dialect,
            ))
        }
        other => compile(
            &other,
            None,
            column_types,
            (!matches!(spec, ColumnTypeSpec::Unknown)).then_some(spec),
        ),
    }
}

/// Column name → type spec, as carried by `QueryIR.column_types`.
pub type ColumnTypes = HashMap<String, ColumnTypeSpec>;

/// Compile a [`ScalarExpr`] tree into a sea-query expression.
///
/// `table` qualifies column references (`"table"."col"`); pass `None` for
/// UPDATE/INSERT and for SELECTs without joins. `column_types` lets a string
/// or NULL literal next to an enum column in `coalesce`/`greatest` bind as
/// that enum (PostgreSQL cannot match `enum` with `text`).
pub fn compile_scalar(
    node: &ScalarExpr,
    table: Option<&TableIdent>,
    column_types: Option<&ColumnTypes>,
) -> Result<SimpleExpr> {
    compile(node, table, column_types, None)
}

/// [`compile_scalar`] with an `inherited` type: the type an enclosing
/// `coalesce`/`greatest` (or the assignment target) imposes on NULL literals
/// found directly under it, and on string literals when it is an enum.
/// Arithmetic and `concat_ws` reset it.
fn compile<'a>(
    node: &'a ScalarExpr,
    table: Option<&TableIdent>,
    column_types: Option<&'a ColumnTypes>,
    inherited: Option<&'a ColumnTypeSpec>,
) -> Result<SimpleExpr> {
    match node {
        ScalarExpr::Value { value, value_type } => {
            if let Some(hint) = inherited {
                // A NULL takes the sibling's type whatever it is (a text NULL
                // next to a bigint fails in PostgreSQL); only an enum retypes
                // a string, so `COALESCE(qty, 'x')` is left to the database.
                let retype = value.is_nil()
                    || (value.is_str() && matches!(hint, ColumnTypeSpec::Enum { .. }));
                if retype && is_retypable_literal(value_type.as_ref()) {
                    return Ok(typed_value_expr(
                        super::bind::bind_value(value, hint),
                        hint,
                        Dialect::Postgres,
                    ));
                }
            }
            // Optional per-literal ColumnTypeSpec hint (tagged dict), set by
            // Python for types that msgpack-encode as strings (Decimal, UUID,
            // datetime, ...). Absent or unparseable hint = native conversion.
            let spec = literal_spec(value_type.as_ref());
            Ok(Expr::val(super::bind::bind_value(value, &spec)))
        }
        ScalarExpr::Column { name } => Ok(match table {
            Some(t) => Expr::col((t.clone(), ColumnIdent(name.clone())).into_column_ref()),
            None => Expr::col(ColumnIdent(name.clone())),
        }),
        ScalarExpr::Op { op, lhs, rhs } => {
            let lhs = compile(lhs, table, column_types, None)?;
            let rhs = compile(rhs, table, column_types, None)?;
            Ok(match op.as_str() {
                "add" => Expr::expr(lhs).add(rhs),
                "sub" => Expr::expr(lhs).sub(rhs),
                "mul" => Expr::expr(lhs).mul(rhs),
                "div" => Expr::expr(lhs).div(rhs),
                other => {
                    return Err(QueryError::InvalidQuery(format!(
                        "Unsupported arithmetic operator '{other}'"
                    )))
                }
            })
        }
        ScalarExpr::Neg { expr } => {
            let inner = compile(expr, table, column_types, None)?;
            Ok(Expr::val(Value::BigInt(Some(0))).sub(inner))
        }
        ScalarExpr::Raw { sql } => {
            if sql.trim().is_empty() {
                return Err(QueryError::InvalidQuery("Raw SQL must not be empty".into()));
            }
            Ok(Expr::cust(sql.clone()))
        }
        ScalarExpr::Func { name, args } => {
            if args.is_empty() {
                return Err(QueryError::InvalidQuery(format!(
                    "Function '{name}' requires at least one argument"
                )));
            }
            // Only value-returning functions take their literals' type from a
            // sibling column (or an enclosing function / assignment target);
            // `concat_ws`'s separator must stay text.
            let hint = if matches!(name.as_str(), "coalesce" | "greatest") {
                args.iter()
                    .find_map(|a| sibling_enum_spec(a, column_types))
                    .or_else(|| args.iter().find_map(|a| sibling_spec(a, column_types)))
                    .or(inherited)
            } else {
                None
            };
            let compiled = args
                .iter()
                .map(|a| compile(a, table, column_types, hint))
                .collect::<Result<Vec<_>>>()?;
            match name.as_str() {
                "coalesce" => Ok(Func::coalesce(compiled).into()),
                "greatest" => Ok(Func::greatest(compiled).into()),
                "concat_ws" => Ok(Func::cust("CONCAT_WS").args(compiled).into()),
                other => Err(QueryError::InvalidQuery(format!(
                    "Unsupported scalar function '{other}'"
                ))),
            }
        }
    }
}

/// Register the result type of enum-valued `coalesce`/`greatest` annotations.
///
/// The driver decodes result columns by `column_types`, and cannot classify
/// enum values on its own, so an annotation such as `Coalesce("status", ...)`
/// needs its alias mapped to the enum spec. Other result types (e.g. NUMERIC
/// from `GREATEST(int_col, 1.5)`) are deliberately left to native decoding:
/// the function's type can differ from its first column's.
pub fn apply_annotation_result_types(ir: &mut oxyde_codec::QueryIR) {
    let Some(annotations) = ir.annotations.as_deref() else {
        return;
    };
    let inferred: Vec<(String, ColumnTypeSpec)> = annotations
        .iter()
        .filter_map(|a| {
            sibling_enum_spec(&a.expr, ir.column_types.as_ref())
                .map(|spec| (a.alias.clone(), spec.clone()))
        })
        .collect();
    if inferred.is_empty() {
        return;
    }
    ir.column_types
        .get_or_insert_with(Default::default)
        .extend(inferred);
}

/// Leniently decode a literal's `value_type` hint (`Unknown` when absent or unparseable).
fn literal_spec(value_type: Option<&rmpv::Value>) -> ColumnTypeSpec {
    value_type
        .and_then(|v| rmpv::ext::from_value::<ColumnTypeSpec>(v.clone()).ok())
        .unwrap_or(ColumnTypeSpec::Unknown)
}

/// True when a string literal may be re-typed from a sibling enum column: its
/// hint is absent, plain string/text, or an enum (a Python `Enum` member).
fn is_retypable_literal(value_type: Option<&rmpv::Value>) -> bool {
    matches!(
        literal_spec(value_type),
        ColumnTypeSpec::Unknown
            | ColumnTypeSpec::Text
            | ColumnTypeSpec::String { .. }
            | ColumnTypeSpec::Enum { .. }
    )
}

/// The type a `coalesce`/`greatest` argument contributes to its siblings'
/// NULL literals: a column with a known type, or (recursively) the first such
/// column inside a nested `coalesce`/`greatest`.
fn sibling_spec<'a>(
    node: &'a ScalarExpr,
    column_types: Option<&'a ColumnTypes>,
) -> Option<&'a ColumnTypeSpec> {
    match node {
        ScalarExpr::Column { name } => column_types.and_then(|ct| ct.get(name)),
        ScalarExpr::Func { name, args } if matches!(name.as_str(), "coalesce" | "greatest") => {
            args.iter().find_map(|a| sibling_spec(a, column_types))
        }
        _ => None,
    }
}

/// The enum spec a `coalesce`/`greatest` argument contributes to its siblings:
/// an enum-typed column, or (recursively) the first such column inside a nested
/// `coalesce`/`greatest`. Literals and arithmetic contribute nothing.
fn sibling_enum_spec<'a>(
    node: &'a ScalarExpr,
    column_types: Option<&'a ColumnTypes>,
) -> Option<&'a ColumnTypeSpec> {
    match node {
        ScalarExpr::Column { name } => column_types
            .and_then(|ct| ct.get(name))
            .filter(|spec| matches!(spec, ColumnTypeSpec::Enum { .. })),
        ScalarExpr::Func { name, args } if matches!(name.as_str(), "coalesce" | "greatest") => {
            args.iter().find_map(|a| sibling_enum_spec(a, column_types))
        }
        _ => None,
    }
}

// ── rmpv map helpers ──────────────────────────────────────────────────

/// Get a value from an rmpv Map by string key.
fn map_get<'a>(map: &'a rmpv::Value, key: &str) -> Option<&'a rmpv::Value> {
    match map {
        rmpv::Value::Map(pairs) => pairs.iter().find_map(|(k, v)| {
            if k.as_str() == Some(key) {
                Some(v)
            } else {
                None
            }
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use std::str::FromStr;

    #[test]
    fn test_rmpv_to_json_nested() {
        let val = rmpv::Value::Map(vec![
            (
                rmpv::Value::String("items".into()),
                rmpv::Value::Array(vec![
                    rmpv::Value::Integer(1.into()),
                    rmpv::Value::Boolean(true),
                    rmpv::Value::Nil,
                ]),
            ),
            (
                rmpv::Value::String("count".into()),
                rmpv::Value::Integer(3.into()),
            ),
        ]);
        let json = rmpv_to_json(&val).unwrap();
        assert_eq!(json["count"], 3);
        assert_eq!(json["items"][0], 1);
        assert_eq!(json["items"][1], true);
        assert!(serde_json::Value::is_null(&json["items"][2]));
    }

    // ── compile_scalar: value_type spec on literal nodes ──────────────

    fn compile_node(node: &rmpv::Value) -> Result<SimpleExpr> {
        let parsed: ScalarExpr = rmpv::ext::from_value(node.clone())
            .map_err(|e| QueryError::InvalidQuery(e.to_string()))?;
        compile_scalar(&parsed, None, None)
    }

    /// Build a `{"type": "value", "value": <val>, "value_type": {"kind": <kind>}}`
    /// rmpv map — the shape Python sends for F-expression literals.
    /// If `kind` is None, the key is omitted (no hint = native conversion).
    fn build_value_node(value: rmpv::Value, kind: Option<&str>) -> rmpv::Value {
        let mut pairs = vec![
            (
                rmpv::Value::String("type".into()),
                rmpv::Value::String("value".into()),
            ),
            (rmpv::Value::String("value".into()), value),
        ];
        if let Some(k) = kind {
            pairs.push((
                rmpv::Value::String("value_type".into()),
                rmpv::Value::Map(vec![(
                    rmpv::Value::String("kind".into()),
                    rmpv::Value::String(k.into()),
                )]),
            ));
        }
        rmpv::Value::Map(pairs)
    }

    #[test]
    fn test_compile_scalar_value_with_decimal_hint() {
        let node = build_value_node(rmpv::Value::String("2.50".into()), Some("decimal"));
        let expr = compile_node(&node).unwrap();
        match expr {
            SimpleExpr::Value(Value::Decimal(Some(d))) => {
                assert_eq!(d, Decimal::from_str("2.50").unwrap());
            }
            other => panic!("expected Value::Decimal, got {other:?}"),
        }
    }

    #[test]
    fn test_compile_scalar_value_without_hint_stays_string() {
        // No value_type → native conversion: Decimal-looking strings bind
        // as String, exactly like any untyped string parameter.
        let node = build_value_node(rmpv::Value::String("2.50".into()), None);
        let expr = compile_node(&node).unwrap();
        match expr {
            SimpleExpr::Value(Value::String(Some(s))) => {
                assert_eq!(s.as_str(), "2.50");
            }
            other => panic!("expected Value::String without hint, got {other:?}"),
        }
    }

    #[test]
    fn test_compile_scalar_value_with_uuid_hint() {
        let uuid_str = "550e8400-e29b-41d4-a716-446655440000";
        let node = build_value_node(rmpv::Value::String(uuid_str.into()), Some("uuid"));
        let expr = compile_node(&node).unwrap();
        match expr {
            SimpleExpr::Value(Value::Uuid(Some(u))) => {
                assert_eq!(u.to_string(), uuid_str);
            }
            other => panic!("expected Value::Uuid, got {other:?}"),
        }
    }

    #[test]
    fn test_compile_scalar_op_with_decimal_inside() {
        // Nested: (column "price") + (value "2.50" with decimal spec)
        // The literal deep inside the op tree must still bind as Decimal.
        let lhs = rmpv::Value::Map(vec![
            (
                rmpv::Value::String("type".into()),
                rmpv::Value::String("column".into()),
            ),
            (
                rmpv::Value::String("name".into()),
                rmpv::Value::String("price".into()),
            ),
        ]);
        let rhs = build_value_node(rmpv::Value::String("2.50".into()), Some("decimal"));
        let op_node = rmpv::Value::Map(vec![
            (
                rmpv::Value::String("type".into()),
                rmpv::Value::String("op".into()),
            ),
            (
                rmpv::Value::String("op".into()),
                rmpv::Value::String("add".into()),
            ),
            (rmpv::Value::String("lhs".into()), lhs),
            (rmpv::Value::String("rhs".into()), rhs),
        ]);
        let expr = compile_node(&op_node).unwrap();
        let SimpleExpr::Binary(_, _, rhs_box) = expr else {
            panic!("expected SimpleExpr::Binary, got {expr:?}");
        };
        match *rhs_box {
            SimpleExpr::Value(Value::Decimal(Some(d))) => {
                assert_eq!(d, Decimal::from_str("2.50").unwrap());
            }
            other => panic!("expected nested Value::Decimal on RHS, got {other:?}"),
        }
    }

    fn se(json: serde_json::Value) -> ScalarExpr {
        serde_json::from_value(json).unwrap()
    }

    fn render(expr: SimpleExpr) -> (String, Vec<Value>) {
        use sea_query::{PostgresQueryBuilder, Query};
        let (sql, values) = Query::select()
            .expr_as(expr, sea_query::Alias::new("x"))
            .build(PostgresQueryBuilder);
        (sql, values.0)
    }

    #[test]
    fn test_compile_scalar_legacy_shapes_unchanged() {
        // column / op / neg: the shapes `F()` has always produced.
        let node = se(serde_json::json!({
            "type": "op", "op": "add",
            "lhs": {"type": "column", "name": "a"},
            "rhs": {"type": "neg", "expr": {"type": "value", "value": 3}},
        }));
        let (sql, params) = render(compile_scalar(&node, None, None).unwrap());
        assert_eq!(sql, r#"SELECT "a" + ($1 - $2) AS "x""#);
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn test_compile_scalar_func_coalesce_greatest() {
        let node = se(serde_json::json!({
            "type": "func", "name": "greatest", "args": [
                {"type": "func", "name": "coalesce", "args": [
                    {"type": "column", "name": "minimum_quantity"},
                    {"type": "value", "value": 1},
                ]},
                {"type": "value", "value": 1},
            ],
        }));
        let (sql, params) = render(compile_scalar(&node, None, None).unwrap());
        assert_eq!(
            sql,
            r#"SELECT GREATEST(COALESCE("minimum_quantity", $1), $2) AS "x""#
        );
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn test_compile_scalar_func_rejects_empty_args_and_unknown_name() {
        let empty = se(serde_json::json!({"type": "func", "name": "coalesce", "args": []}));
        assert!(compile_scalar(&empty, None, None).is_err());
        let unknown = se(serde_json::json!({
            "type": "func", "name": "nope", "args": [{"type": "value", "value": 1}],
        }));
        assert!(compile_scalar(&unknown, None, None).is_err());
    }

    #[test]
    fn test_compile_scalar_qualifies_columns_with_table() {
        let node = se(serde_json::json!({
            "type": "func", "name": "coalesce", "args": [
                {"type": "column", "name": "id"},
                {"type": "value", "value": 0},
            ],
        }));
        let table = TableIdent("t".into());
        let (sql, _) = render(compile_scalar(&node, Some(&table), None).unwrap());
        assert_eq!(sql, r#"SELECT COALESCE("t"."id", $1) AS "x""#);
    }

    #[test]
    fn test_compile_scalar_bad_value_type_falls_back_to_unknown() {
        let node = se(serde_json::json!({
            "type": "value", "value": "2.50", "value_type": {"kind": "no_such_kind"},
        }));
        match compile_scalar(&node, None, None).unwrap() {
            SimpleExpr::Value(Value::String(Some(s))) => assert_eq!(s.as_str(), "2.50"),
            other => panic!("expected untyped string binding, got {other:?}"),
        }
    }

    #[test]
    fn test_compile_scalar_string_literal_next_to_enum_column_is_cast() {
        let node = se(serde_json::json!({
            "type": "func", "name": "coalesce", "args": [
                {"type": "column", "name": "status"},
                {"type": "value", "value": "draft", "value_type": {"kind": "string"}},
            ],
        }));
        let types = ColumnTypes::from([(
            "status".to_string(),
            ColumnTypeSpec::Enum {
                name: "status_enum".into(),
                values: vec!["draft".into()],
            },
        )]);
        let (sql, _) = render(compile_scalar(&node, None, Some(&types)).unwrap());
        assert_eq!(
            sql,
            r#"SELECT COALESCE("status", $1::"status_enum") AS "x""#
        );
        // Without the column type the literal stays a plain parameter.
        let (plain, _) = render(compile_scalar(&node, None, None).unwrap());
        assert_eq!(plain, r#"SELECT COALESCE("status", $1) AS "x""#);
    }

    fn status_types() -> ColumnTypes {
        ColumnTypes::from([(
            "status".to_string(),
            ColumnTypeSpec::Enum {
                name: "status_enum".into(),
                values: vec!["draft".into(), "live".into()],
            },
        )])
    }

    #[test]
    fn test_compile_scalar_enum_hinted_literal_takes_sibling_column_type() {
        // The literal's own enum hint names a different type; the sibling
        // column's type wins (the column may be a text-backed enum field).
        let node = se(serde_json::json!({
            "type": "func", "name": "coalesce", "args": [
                {"type": "column", "name": "status"},
                {"type": "value", "value": "draft", "value_type": {
                    "kind": "enum", "name": "other_enum", "values": ["draft"],
                }},
            ],
        }));
        let (sql, _) = render(compile_scalar(&node, None, Some(&status_types())).unwrap());
        assert_eq!(
            sql,
            r#"SELECT COALESCE("status", $1::"status_enum") AS "x""#
        );
    }

    #[test]
    fn test_compile_scalar_enum_hinted_literal_without_sibling_stays_uncast() {
        let node = se(serde_json::json!({
            "type": "func", "name": "coalesce", "args": [
                {"type": "column", "name": "id"},
                {"type": "value", "value": "draft", "value_type": {
                    "kind": "enum", "name": "status_enum", "values": ["draft"],
                }},
            ],
        }));
        let (sql, _) = render(compile_scalar(&node, None, Some(&status_types())).unwrap());
        assert_eq!(sql, r#"SELECT COALESCE("id", $1) AS "x""#);
    }

    #[test]
    fn test_compile_scalar_nested_functions_cast_outer_literal() {
        let node = se(serde_json::json!({
            "type": "func", "name": "greatest", "args": [
                {"type": "func", "name": "coalesce", "args": [
                    {"type": "column", "name": "status"},
                    {"type": "value", "value": "draft"},
                ]},
                {"type": "value", "value": "live"},
            ],
        }));
        let (sql, _) = render(compile_scalar(&node, None, Some(&status_types())).unwrap());
        assert_eq!(
            sql,
            r#"SELECT GREATEST(COALESCE("status", $1::"status_enum"), $2::"status_enum") AS "x""#
        );
    }

    #[test]
    fn test_compile_scalar_concat_ws_keeps_separator_text_next_to_enum() {
        let node = se(serde_json::json!({
            "type": "func", "name": "concat_ws", "args": [
                {"type": "value", "value": " ", "value_type": {"kind": "string"}},
                {"type": "column", "name": "status"},
                {"type": "column", "name": "id"},
            ],
        }));
        let types = ColumnTypes::from([(
            "status".to_string(),
            ColumnTypeSpec::Enum {
                name: "status_enum".into(),
                values: vec![],
            },
        )]);
        let (sql, _) = render(compile_scalar(&node, None, Some(&types)).unwrap());
        assert_eq!(sql, r#"SELECT CONCAT_WS($1, "status", "id") AS "x""#);
    }

    #[test]
    fn test_compile_scalar_raw_is_verbatim_and_rejects_empty() {
        let node = se(serde_json::json!({"type": "raw", "sql": "LENGTH(name)"}));
        let (sql, params) = render(compile_scalar(&node, None, None).unwrap());
        assert_eq!(sql, r#"SELECT LENGTH(name) AS "x""#);
        assert_eq!(params.len(), 0);
        let empty = se(serde_json::json!({"type": "raw", "sql": "  "}));
        assert!(compile_scalar(&empty, None, None).is_err());
    }

    #[test]
    fn test_apply_annotation_result_types_registers_enum_aliases_only() {
        let mut ir = oxyde_codec::QueryIR {
            table: "t".into(),
            column_types: Some(status_types()),
            annotations: Some(vec![
                oxyde_codec::Annotation {
                    alias: "st".into(),
                    expr: se(serde_json::json!({
                        "type": "func", "name": "coalesce", "args": [
                            {"type": "column", "name": "status"},
                            {"type": "value", "value": "draft"},
                        ],
                    })),
                },
                oxyde_codec::Annotation {
                    alias: "n".into(),
                    expr: se(serde_json::json!({
                        "type": "func", "name": "greatest", "args": [
                            {"type": "column", "name": "stock"},
                            {"type": "value", "value": 1.5},
                        ],
                    })),
                },
            ]),
            ..Default::default()
        };
        apply_annotation_result_types(&mut ir);
        let types = ir.column_types.unwrap();
        assert!(matches!(types.get("st"), Some(ColumnTypeSpec::Enum { .. })));
        assert!(!types.contains_key("n"));
    }

    #[test]
    fn test_assignment_expr_binds_root_literal_by_column_spec() {
        let node = rmpv::Value::Map(vec![(
            rmpv::Value::String("__expr__".into()),
            build_value_node(rmpv::Value::String("draft".into()), Some("string")),
        )]);
        let spec = ColumnTypeSpec::Enum {
            name: "status_enum".into(),
            values: vec!["draft".into()],
        };
        let (sql, _) = render(assignment_expr(&node, &spec, None, Dialect::Postgres).unwrap());
        assert_eq!(sql, r#"SELECT $1::"status_enum" AS "x""#);
        // A text-backed column (no enum spec) gets a plain parameter.
        let (plain, _) = render(
            assignment_expr(&node, &ColumnTypeSpec::Unknown, None, Dialect::Postgres).unwrap(),
        );
        assert_eq!(plain, r#"SELECT $1 AS "x""#);
    }

    fn status_enum_spec() -> ColumnTypeSpec {
        ColumnTypeSpec::Enum {
            name: "status_enum".into(),
            values: vec!["draft".into(), "live".into()],
        }
    }

    fn expr_wrapper(node: serde_json::Value) -> rmpv::Value {
        let node: rmpv::Value = rmpv::ext::to_value(node).unwrap();
        rmpv::Value::Map(vec![(rmpv::Value::String("__expr__".into()), node)])
    }

    #[test]
    fn test_nested_coalesce_literals_take_enum_from_outer_sibling() {
        // The inner COALESCE has no enum column of its own; its literals must
        // inherit the outer GREATEST's enum column.
        let node = se(serde_json::json!({
            "type": "func", "name": "greatest", "args": [
                {"type": "func", "name": "coalesce", "args": [
                    {"type": "value", "value": "draft"},
                    {"type": "value", "value": "live"},
                ]},
                {"type": "column", "name": "status"},
            ],
        }));
        let (sql, _) = render(compile_scalar(&node, None, Some(&status_types())).unwrap());
        assert_eq!(
            sql,
            r#"SELECT GREATEST(COALESCE($1::"status_enum", $2::"status_enum"), "status") AS "x""#
        );
    }

    #[test]
    fn test_null_literal_next_to_enum_column_is_cast() {
        let node = se(serde_json::json!({
            "type": "func", "name": "coalesce", "args": [
                {"type": "column", "name": "status"},
                {"type": "value", "value": null},
            ],
        }));
        let (sql, _) = render(compile_scalar(&node, None, Some(&status_types())).unwrap());
        assert_eq!(
            sql,
            r#"SELECT COALESCE("status", $1::"status_enum") AS "x""#
        );
    }

    #[test]
    fn test_assignment_coalesce_of_literals_takes_target_enum_spec() {
        let wrapper = expr_wrapper(serde_json::json!({
            "type": "func", "name": "coalesce", "args": [
                {"type": "value", "value": null},
                {"type": "value", "value": "draft"},
            ],
        }));
        let (sql, _) = render(
            assignment_expr(&wrapper, &status_enum_spec(), None, Dialect::Postgres).unwrap(),
        );
        assert_eq!(
            sql,
            r#"SELECT COALESCE($1::"status_enum", $2::"status_enum") AS "x""#
        );
    }

    #[test]
    fn test_assignment_root_value_uses_hint_on_unknown_column() {
        let wrapper = expr_wrapper(serde_json::json!({
            "type": "value", "value": "2.50", "value_type": {"kind": "decimal"},
        }));
        match assignment_expr(&wrapper, &ColumnTypeSpec::Unknown, None, Dialect::Postgres).unwrap()
        {
            SimpleExpr::Value(Value::Decimal(Some(d))) => {
                assert_eq!(d, Decimal::from_str("2.50").unwrap());
            }
            other => panic!("expected Value::Decimal, got {other:?}"),
        }
    }

    #[test]
    fn test_assignment_root_enum_hint_never_casts_unknown_column() {
        // A Python-enum hint must not turn into `::"python_enum"` on a column
        // whose own spec is Unknown (e.g. CITEXT): cast follows the column.
        let wrapper = expr_wrapper(serde_json::json!({
            "type": "value", "value": "draft", "value_type": {
                "kind": "enum", "name": "status_enum", "values": ["draft"],
            },
        }));
        let (sql, _) = render(
            assignment_expr(&wrapper, &ColumnTypeSpec::Unknown, None, Dialect::Postgres).unwrap(),
        );
        assert_eq!(sql, r#"SELECT $1 AS "x""#);
    }

    #[test]
    fn test_assignment_expr_rejects_bad_expression_node() {
        let bad = expr_wrapper(serde_json::json!({"type": "bogus"}));
        assert!(assignment_expr(&bad, &ColumnTypeSpec::Unknown, None, Dialect::Postgres).is_err());
    }

    #[test]
    fn test_null_literal_next_to_typed_column_takes_its_type() {
        // `COALESCE(bigint, text-NULL)` fails in PostgreSQL: the NULL must be
        // bound with the sibling column's type. String literals stay untouched.
        let types = ColumnTypes::from([("qty".to_string(), ColumnTypeSpec::BigInteger)]);
        let node = se(serde_json::json!({
            "type": "func", "name": "coalesce", "args": [
                {"type": "column", "name": "qty"},
                {"type": "value", "value": null},
            ],
        }));
        let (_, params) = render(compile_scalar(&node, None, Some(&types)).unwrap());
        assert!(matches!(params[0], Value::BigInt(None)), "{params:?}");

        let text = se(serde_json::json!({
            "type": "func", "name": "coalesce", "args": [
                {"type": "column", "name": "qty"},
                {"type": "value", "value": "x"},
            ],
        }));
        let (_, params) = render(compile_scalar(&text, None, Some(&types)).unwrap());
        assert!(matches!(params[0], Value::String(Some(_))), "{params:?}");
    }

    #[test]
    fn test_assignment_null_literal_takes_target_column_type() {
        let wrapper = expr_wrapper(serde_json::json!({
            "type": "func", "name": "coalesce", "args": [
                {"type": "value", "value": null},
                {"type": "value", "value": 5},
            ],
        }));
        let (_, params) = render(
            assignment_expr(
                &wrapper,
                &ColumnTypeSpec::BigInteger,
                None,
                Dialect::Postgres,
            )
            .unwrap(),
        );
        assert!(matches!(params[0], Value::BigInt(None)), "{params:?}");
    }
}
