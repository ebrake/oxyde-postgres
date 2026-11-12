//! Scalar expression compilation (`ScalarExpr` / `__expr__` IR nodes) and rmpv→JSON conversion.
//!
//! Value binding lives in `bind.rs` (`bind_value`, spec-driven). This module
//! compiles expression trees produced by Python's `F()` / `Coalesce` / `Greatest` and converts
//! rmpv maps/arrays into `serde_json::Value` for JSON column storage.

use std::collections::HashMap;

use sea_query::ExprTrait;
use sea_query::{Expr, Func, IntoColumnRef, SimpleExpr, Value};

use crate::error::{QueryError, Result};
use crate::utils::bind::{ensure_array_elements, typed_value_expr};
use crate::utils::identifier::{ColumnIdent, TableIdent};
use crate::Dialect;
use oxyde_codec::{ColumnTypeSpec, QueryIR, ScalarExpr};

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

/// Column name → type spec, as carried by `QueryIR.column_types`.
pub type ColumnTypes = HashMap<String, ColumnTypeSpec>;

/// What expression compilation knows about columns.
///
/// `specs` is `QueryIR.column_types`. `bind_only` names the entries of `specs`
/// that are only *binding hints* (an unrecognised `db_type` such as MONEY: see
/// `QueryIR::with_binding_types`). A hint says how to bind a value for the
/// column but not what type the column has, so it is used for binding
/// ([`binding`](Self::binding)) and never to type a sibling or target
/// ([`known`](Self::known)): `COALESCE("amount", NULL)` must stay a bare NULL.
#[derive(Clone, Copy, Default)]
pub struct ExprTypes<'a> {
    specs: Option<&'a ColumnTypes>,
    bind_only: Option<&'a ColumnTypes>,
}

impl<'a> From<Option<&'a ColumnTypes>> for ExprTypes<'a> {
    /// Plain column specs, none of them binding-only.
    fn from(specs: Option<&'a ColumnTypes>) -> Self {
        Self {
            specs,
            bind_only: None,
        }
    }
}

impl<'a> ExprTypes<'a> {
    /// The types of an IR as the SQL builders see it, i.e. after
    /// `QueryIR::with_binding_types` (whose `bind_types` are the hints).
    #[must_use]
    pub fn of(ir: &'a QueryIR) -> Self {
        Self {
            specs: ir.column_types.as_ref(),
            bind_only: ir.bind_types.as_ref(),
        }
    }

    /// The spec a value bound to `name` is converted by (`Unknown` = natively).
    #[must_use]
    pub fn binding(self, name: &str) -> &'a ColumnTypeSpec {
        self.specs
            .and_then(|specs| specs.get(name))
            .unwrap_or(&ColumnTypeSpec::Unknown)
    }

    /// The column's real type: neither missing, `Unknown` nor binding-only.
    fn known(self, name: &str) -> Option<&'a ColumnTypeSpec> {
        if self.bind_only.is_some_and(|hints| hints.contains_key(name)) {
            return None;
        }
        self.specs
            .and_then(|specs| specs.get(name))
            .filter(|spec| !matches!(spec, ColumnTypeSpec::Unknown))
    }
}

/// Build the SQL expression for a value assigned to `column` in
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
///   target column's type (if it has a real one) inherited by the literals
///   inside `coalesce`/`greatest` that [`retypes_to`] lets take it;
/// * everything else binds by the column's spec.
pub fn assignment_expr<'a>(
    value: &rmpv::Value,
    column: &str,
    types: impl Into<ExprTypes<'a>>,
    dialect: Dialect,
) -> Result<SimpleExpr> {
    let types = types.into();
    let spec = types.binding(column);
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
                (ColumnTypeSpec::Unknown, ColumnTypeSpec::Enum { .. }) => spec,
                (ColumnTypeSpec::Unknown, hint) => hint,
                _ => spec,
            };
            let bound = super::bind::bind_value(&literal, bind_spec);
            ensure_array_elements(&bound)?;
            Ok(typed_value_expr(bound, spec, dialect))
        }
        other => compile(&other, None, types, types.known(column)),
    }
}

/// Compile a [`ScalarExpr`] tree into a sea-query expression.
///
/// `table` qualifies column references (`"table"."col"`); pass `None` for
/// UPDATE/INSERT and for SELECTs without joins. `types` lets a literal next to
/// a typed column in `coalesce`/`greatest` take that column's type (PostgreSQL
/// cannot match `enum` with `text`); see [`retypes_to`].
pub fn compile_scalar<'a>(
    node: &'a ScalarExpr,
    table: Option<&TableIdent>,
    types: impl Into<ExprTypes<'a>>,
) -> Result<SimpleExpr> {
    compile(node, table, types.into(), None)
}

/// [`compile_scalar`] with an `inherited` type: the type an enclosing
/// `coalesce`/`greatest` (or the assignment target) imposes on the literals
/// found directly under it (see [`retypes_to`]: NULLs, enum strings, lists
/// under an array type). Arithmetic and `concat_ws` reset it.
fn compile<'a>(
    node: &'a ScalarExpr,
    table: Option<&TableIdent>,
    types: ExprTypes<'a>,
    inherited: Option<&'a ColumnTypeSpec>,
) -> Result<SimpleExpr> {
    match node {
        ScalarExpr::Value { value, value_type } => {
            // Optional per-literal ColumnTypeSpec hint (tagged dict), set by
            // Python for types that msgpack-encode as strings (Decimal, UUID,
            // datetime, ...). Absent or unparseable hint = native conversion.
            let own = literal_spec(value_type.as_ref());
            if let Some(hint) = inherited {
                if retypes_to(value, own, hint) {
                    let bound = super::bind::bind_value(value, hint);
                    // A list of the wrong element kind would bind as NULLs.
                    ensure_array_elements(&bound)?;
                    return Ok(typed_value_expr(bound, hint, Dialect::Postgres));
                }
            }
            // A NULL with no type to take is a bare SQL `NULL`, which
            // PostgreSQL types from context; a bound NULL would be text.
            if value.is_nil() && matches!(own, ColumnTypeSpec::Unknown) {
                return Ok(Expr::null());
            }
            Ok(Expr::val(super::bind::bind_value(value, own)))
        }
        ScalarExpr::Column { name } => Ok(match table {
            Some(t) => Expr::col((t.clone(), ColumnIdent(name.clone())).into_column_ref()),
            None => Expr::col(ColumnIdent(name.clone())),
        }),
        ScalarExpr::Op { op, lhs, rhs } => {
            let lhs = compile(lhs, table, types, None)?;
            let rhs = compile(rhs, table, types, None)?;
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
            let inner = compile(expr, table, types, None)?;
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
            let hint = if is_type_preserving(name) {
                sibling_hint(args, types).or(inherited)
            } else {
                None
            };
            let compiled = args
                .iter()
                .map(|a| compile(a, table, types, hint))
                .collect::<Result<Vec<_>>>()?;
            match name.as_str() {
                "coalesce" => Ok(Func::coalesce(compiled).into()),
                "greatest" => Ok(Func::greatest(compiled).into()),
                "concat_ws" => Ok(Func::cust("CONCAT_WS").args(compiled).into()),
                "lower" | "upper" => {
                    let [arg] = <[SimpleExpr; 1]>::try_from(compiled).map_err(|_| {
                        QueryError::InvalidQuery(format!(
                            "Function '{name}' takes exactly one argument"
                        ))
                    })?;
                    Ok(match name.as_str() {
                        "lower" => Func::lower(arg),
                        _ => Func::upper(arg),
                    }
                    .into())
                }
                other => Err(QueryError::InvalidQuery(format!(
                    "Unsupported scalar function '{other}'"
                ))),
            }
        }
    }
}

/// Register the result types of type-preserving annotations.
///
/// The driver decodes result columns by `column_types` and, without a spec,
/// by the runtime DB type, which cannot read enums or arrays and returns an
/// interval-as-BIGINT as raw microseconds. An alias therefore gets the spec of
/// the column its expression passes through unchanged (see [`result_spec`]).
/// Anything that can change the type (arithmetic, a literal of another type,
/// `concat_ws`, raw SQL) is left to native decoding: e.g.
/// `GREATEST(int_col, 1.5)` is NUMERIC, not the integer column's type.
///
/// `count()` / `exists()` build their own statement, whose only result column
/// is `_count` / `exists`: no annotation is selected, so none may be applied
/// (an alias that happens to share that name would mis-decode the result).
pub fn apply_annotation_result_types(ir: &mut QueryIR) {
    if ir.count.unwrap_or(false) || ir.exists.unwrap_or(false) {
        return;
    }
    let Some(annotations) = ir.annotations.as_deref() else {
        return;
    };
    // Decoding sees `column_types` as sent: binding-only hints are not in it.
    let types = ExprTypes::from(ir.column_types.as_ref());
    let inferred: Vec<(String, ColumnTypeSpec)> = annotations
        .iter()
        .filter_map(|a| result_spec(&a.expr, types).map(|s| (a.alias.clone(), s.clone())))
        .collect();
    if inferred.is_empty() {
        return;
    }
    ir.column_types
        .get_or_insert_with(Default::default)
        .extend(inferred);
}

/// The column type `node` evaluates to, when it is known.
///
/// * A bare column: its own type.
/// * A `coalesce`/`greatest` with an enum column among its arguments: that
///   enum. PostgreSQL cannot resolve an enum together with anything but
///   unknown-typed literals, so whatever else the arguments are (raw SQL,
///   arithmetic), the result is the enum or the query fails.
/// * Otherwise only when every argument keeps the sibling column's type
///   ([`preserves`]): `GREATEST(int_col, 1.5)` widens to NUMERIC.
fn result_spec<'a>(node: &'a ScalarExpr, types: ExprTypes<'a>) -> Option<&'a ColumnTypeSpec> {
    match node {
        ScalarExpr::Column { name } => types.known(name),
        ScalarExpr::Func { name, args } if is_type_preserving(name) => {
            sibling_hint(args, types).filter(|h| is_enum(h) || preserves(node, types, h))
        }
        _ => None,
    }
}

/// True when `node` yields a value of type `hint` inside a `coalesce`/`greatest`
/// that is typed `hint`: a column of that type, a nested function that
/// preserves it, or a literal that [`compile`] retypes to it.
fn preserves(node: &ScalarExpr, types: ExprTypes<'_>, hint: &ColumnTypeSpec) -> bool {
    match node {
        ScalarExpr::Column { name } => types.known(name) == Some(hint),
        ScalarExpr::Func { name, args } if is_type_preserving(name) => {
            args.iter().all(|a| preserves(a, types, hint))
        }
        ScalarExpr::Value { value, value_type } => {
            retypes_to(value, literal_spec(value_type.as_ref()), hint)
        }
        _ => false,
    }
}

/// The value-returning functions whose result is the type of their arguments;
/// only these hand a sibling's type to their literals.
fn is_type_preserving(function: &str) -> bool {
    matches!(function, "coalesce" | "greatest")
}

fn is_enum(spec: &ColumnTypeSpec) -> bool {
    matches!(spec, ColumnTypeSpec::Enum { .. })
}

/// A literal's own `ColumnTypeSpec` hint; `Unknown` when absent or unparseable
/// (decoded once, leniently, by the codec).
fn literal_spec(value_type: Option<&ColumnTypeSpec>) -> &ColumnTypeSpec {
    value_type.unwrap_or(&ColumnTypeSpec::Unknown)
}

/// Whether a literal takes the type `hint` imposed by its siblings or the
/// assignment target. A NULL takes any type (a text NULL next to a bigint
/// fails in PostgreSQL); a string only an enum's (`COALESCE(qty, 'x')` is left
/// to the database); a list only an array's. And only if the literal's own
/// hint leaves its type open: absent, plain string/text, or an enum (a Python
/// `Enum` member). Used both to bind literals and to decide whether an
/// expression's result type is preserved, so the two agree.
fn retypes_to(value: &rmpv::Value, own: &ColumnTypeSpec, hint: &ColumnTypeSpec) -> bool {
    let takes_hint = value.is_nil()
        || (value.is_str() && is_enum(hint))
        || (value.is_array() && matches!(hint, ColumnTypeSpec::Array { .. }));
    takes_hint
        && matches!(
            own,
            ColumnTypeSpec::Unknown
                | ColumnTypeSpec::Text
                | ColumnTypeSpec::String { .. }
                | ColumnTypeSpec::Enum { .. }
        )
}

/// The type the arguments of a `coalesce`/`greatest` impose on their literals:
/// the first enum column, else the first column with a known type (recursing
/// into nested `coalesce`/`greatest`). Literals and arithmetic contribute nothing.
fn sibling_hint<'a>(args: &'a [ScalarExpr], types: ExprTypes<'a>) -> Option<&'a ColumnTypeSpec> {
    let find = |accept: fn(&ColumnTypeSpec) -> bool| {
        args.iter().find_map(|a| sibling_where(a, types, accept))
    };
    find(is_enum).or_else(|| find(|_| true))
}

/// The first column spec accepted by `accept` that a `coalesce`/`greatest`
/// argument contributes: the column itself (when its type is known), or
/// (recursively) one inside a nested `coalesce`/`greatest`.
fn sibling_where<'a>(
    node: &'a ScalarExpr,
    types: ExprTypes<'a>,
    accept: fn(&ColumnTypeSpec) -> bool,
) -> Option<&'a ColumnTypeSpec> {
    match node {
        ScalarExpr::Column { name } => types.known(name).filter(|spec| accept(spec)),
        ScalarExpr::Func { name, args } if is_type_preserving(name) => {
            args.iter().find_map(|a| sibling_where(a, types, accept))
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
    fn test_apply_annotation_result_types_registers_enum_alias_and_skips_widened_greatest() {
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
        let (sql, _) = render(assign(&node, &spec, None).unwrap());
        assert_eq!(sql, r#"SELECT $1::"status_enum" AS "x""#);
        // A text-backed column (no enum spec) gets a plain parameter.
        let (plain, _) = render(assign(&node, &ColumnTypeSpec::Unknown, None).unwrap());
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

    /// `assignment_expr` for a target column holding `spec`, next to `others`.
    fn assign(
        value: &rmpv::Value,
        spec: &ColumnTypeSpec,
        others: Option<&ColumnTypes>,
    ) -> Result<SimpleExpr> {
        let mut types = others.cloned().unwrap_or_default();
        types.insert("target".into(), spec.clone());
        assignment_expr(value, "target", Some(&types), Dialect::Postgres)
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
        let (sql, _) = render(assign(&wrapper, &status_enum_spec(), None).unwrap());
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
        match assign(&wrapper, &ColumnTypeSpec::Unknown, None).unwrap() {
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
        let (sql, _) = render(assign(&wrapper, &ColumnTypeSpec::Unknown, None).unwrap());
        assert_eq!(sql, r#"SELECT $1 AS "x""#);
    }

    #[test]
    fn test_assignment_expr_rejects_bad_expression_node() {
        let bad = expr_wrapper(serde_json::json!({"type": "bogus"}));
        assert!(assign(&bad, &ColumnTypeSpec::Unknown, None).is_err());
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
        let (_, params) = render(assign(&wrapper, &ColumnTypeSpec::BigInteger, None).unwrap());
        assert!(matches!(params[0], Value::BigInt(None)), "{params:?}");
    }

    #[test]
    fn test_untyped_null_literal_is_a_bare_sql_null() {
        // No sibling column, no hint: a text-typed NULL parameter would make
        // `COALESCE(NULL, bigint)` / `bigint + NULL` fail. A bare NULL is
        // typed by PostgreSQL from context.
        let coalesce = se(serde_json::json!({
            "type": "func", "name": "coalesce", "args": [
                {"type": "value", "value": null},
                {"type": "value", "value": 1},
            ],
        }));
        let (sql, params) = render(compile_scalar(&coalesce, None, None).unwrap());
        assert_eq!(sql, r#"SELECT COALESCE(NULL, $1) AS "x""#);
        assert_eq!(params.len(), 1);

        let types = ColumnTypes::from([("qty".to_string(), ColumnTypeSpec::BigInteger)]);
        let arithmetic = se(serde_json::json!({
            "type": "func", "name": "coalesce", "args": [
                {"type": "op", "op": "add",
                 "lhs": {"type": "column", "name": "qty"},
                 "rhs": {"type": "value", "value": 1}},
                {"type": "value", "value": null},
            ],
        }));
        let (sql, params) = render(compile_scalar(&arithmetic, None, Some(&types)).unwrap());
        assert_eq!(sql, r#"SELECT COALESCE("qty" + $1, NULL) AS "x""#);
        assert_eq!(params.len(), 1);
    }

    fn array_types() -> ColumnTypes {
        ColumnTypes::from([
            (
                "nums".to_string(),
                ColumnTypeSpec::Array {
                    item: Box::new(ColumnTypeSpec::BigInteger),
                },
            ),
            (
                "tags".to_string(),
                ColumnTypeSpec::Array {
                    item: Box::new(ColumnTypeSpec::Text),
                },
            ),
            ("wait".to_string(), ColumnTypeSpec::Timedelta),
            ("qty".to_string(), ColumnTypeSpec::BigInteger),
        ])
    }

    #[test]
    fn test_array_literal_next_to_array_column_binds_as_that_array() {
        let node = se(serde_json::json!({
            "type": "func", "name": "coalesce", "args": [
                {"type": "column", "name": "tags"},
                {"type": "value", "value": []},
            ],
        }));
        let (_, params) = render(compile_scalar(&node, None, Some(&array_types())).unwrap());
        match &params[0] {
            Value::Array(sea_query::value::ArrayType::String, Some(items)) => {
                assert!(items.is_empty());
            }
            other => panic!("expected an empty text[] binding, got {other:?}"),
        }
    }

    #[test]
    fn test_assignment_array_literal_takes_target_array_spec() {
        let wrapper = expr_wrapper(serde_json::json!({
            "type": "func", "name": "coalesce", "args": [
                {"type": "column", "name": "tags"},
                {"type": "value", "value": []},
            ],
        }));
        let spec = ColumnTypeSpec::Array {
            item: Box::new(ColumnTypeSpec::Text),
        };
        let (_, params) = render(assign(&wrapper, &spec, Some(&array_types())).unwrap());
        assert!(
            matches!(
                params[0],
                Value::Array(sea_query::value::ArrayType::String, Some(_))
            ),
            "{params:?}"
        );
    }

    fn annotated_ir(annotations: Vec<(&str, serde_json::Value)>) -> oxyde_codec::QueryIR {
        let mut types = status_types();
        types.extend(array_types());
        oxyde_codec::QueryIR {
            table: "t".into(),
            column_types: Some(types),
            annotations: Some(
                annotations
                    .into_iter()
                    .map(|(alias, expr)| oxyde_codec::Annotation {
                        alias: alias.into(),
                        expr: se(expr),
                    })
                    .collect(),
            ),
            ..Default::default()
        }
    }

    #[test]
    fn test_annotation_result_types_preserve_known_column_types() {
        let mut ir = annotated_ir(vec![
            (
                "copy",
                serde_json::json!({"type": "column", "name": "nums"}),
            ),
            (
                "wait_copy",
                serde_json::json!({"type": "column", "name": "wait"}),
            ),
            (
                "nums_or_null",
                serde_json::json!({
                    "type": "func", "name": "coalesce", "args": [
                        {"type": "column", "name": "nums"},
                        {"type": "value", "value": null},
                    ],
                }),
            ),
            (
                "tags_or_empty",
                serde_json::json!({
                    "type": "func", "name": "coalesce", "args": [
                        {"type": "column", "name": "tags"},
                        {"type": "value", "value": []},
                    ],
                }),
            ),
        ]);
        apply_annotation_result_types(&mut ir);
        let types = ir.column_types.unwrap();
        assert!(matches!(
            types.get("copy"),
            Some(ColumnTypeSpec::Array { .. })
        ));
        assert_eq!(types.get("wait_copy"), Some(&ColumnTypeSpec::Timedelta));
        assert!(matches!(
            types.get("nums_or_null"),
            Some(ColumnTypeSpec::Array { .. })
        ));
        assert!(matches!(
            types.get("tags_or_empty"),
            Some(ColumnTypeSpec::Array { .. })
        ));
    }

    #[test]
    fn test_annotation_result_types_skip_expressions_that_change_the_type() {
        let mut ir = annotated_ir(vec![
            (
                "widened",
                serde_json::json!({
                    "type": "func", "name": "greatest", "args": [
                        {"type": "column", "name": "qty"},
                        {"type": "value", "value": 1.5},
                    ],
                }),
            ),
            (
                "defaulted",
                serde_json::json!({
                    "type": "func", "name": "coalesce", "args": [
                        {"type": "column", "name": "qty"},
                        {"type": "value", "value": 1},
                    ],
                }),
            ),
            (
                "sum",
                serde_json::json!({
                    "type": "op", "op": "add",
                    "lhs": {"type": "column", "name": "qty"},
                    "rhs": {"type": "value", "value": 1},
                }),
            ),
            (
                "joined",
                serde_json::json!({
                    "type": "func", "name": "concat_ws", "args": [
                        {"type": "value", "value": "-"},
                        {"type": "column", "name": "status"},
                    ],
                }),
            ),
        ]);
        apply_annotation_result_types(&mut ir);
        let types = ir.column_types.unwrap();
        for alias in ["widened", "defaulted", "sum", "joined"] {
            assert!(!types.contains_key(alias), "{alias} must decode natively");
        }
    }

    #[test]
    fn test_annotation_result_types_ignore_count_and_exists_shortcuts() {
        // The shortcut's own result column (`_count` / `exists`) shares the
        // namespace with annotation aliases and must keep its native decoding.
        for (count, exists) in [(Some(true), None), (None, Some(true))] {
            let mut ir = annotated_ir(vec![
                (
                    "_count",
                    serde_json::json!({"type": "column", "name": "status"}),
                ),
                (
                    "exists",
                    serde_json::json!({"type": "column", "name": "nums"}),
                ),
            ]);
            ir.count = count;
            ir.exists = exists;
            let before = ir.column_types.clone();
            apply_annotation_result_types(&mut ir);
            assert_eq!(ir.column_types, before);
        }
    }

    #[test]
    fn test_enum_alias_survives_non_preserving_sibling_arguments() {
        // An enum column among the arguments fixes the result type in
        // PostgreSQL, whatever else is there (raw SQL, arithmetic).
        let mut ir = annotated_ir(vec![
            (
                "raw",
                serde_json::json!({
                    "type": "func", "name": "coalesce", "args": [
                        {"type": "column", "name": "status"},
                        {"type": "raw", "sql": "'draft'"},
                    ],
                }),
            ),
            (
                "arith",
                serde_json::json!({
                    "type": "func", "name": "greatest", "args": [
                        {"type": "column", "name": "status"},
                        {"type": "op", "op": "add",
                         "lhs": {"type": "column", "name": "qty"},
                         "rhs": {"type": "value", "value": 1}},
                    ],
                }),
            ),
            (
                "widened",
                serde_json::json!({
                    "type": "func", "name": "greatest", "args": [
                        {"type": "column", "name": "qty"},
                        {"type": "raw", "sql": "1.5"},
                    ],
                }),
            ),
        ]);
        apply_annotation_result_types(&mut ir);
        let types = ir.column_types.unwrap();
        assert!(matches!(
            types.get("raw"),
            Some(ColumnTypeSpec::Enum { .. })
        ));
        assert!(matches!(
            types.get("arith"),
            Some(ColumnTypeSpec::Enum { .. })
        ));
        assert!(!types.contains_key("widened"));
    }

    #[test]
    fn test_list_literal_with_mismatched_elements_is_rejected() {
        // `"1"` and `2.5` do not fit a BIGINT[]; PostgreSQL array binding would
        // store them as NULL.
        let node = se(serde_json::json!({
            "type": "func", "name": "coalesce", "args": [
                {"type": "column", "name": "nums"},
                {"type": "value", "value": ["1", 2.5]},
            ],
        }));
        let err = compile_scalar(&node, None, Some(&array_types())).unwrap_err();
        assert!(err.to_string().contains("does not match"), "{err}");

        let fits = se(serde_json::json!({
            "type": "func", "name": "coalesce", "args": [
                {"type": "column", "name": "nums"},
                {"type": "value", "value": [1, 2]},
            ],
        }));
        assert!(compile_scalar(&fits, None, Some(&array_types())).is_ok());
    }

    #[test]
    fn test_assignment_bare_list_with_mismatched_elements_is_rejected() {
        let spec = ColumnTypeSpec::Array {
            item: Box::new(ColumnTypeSpec::BigInteger),
        };
        let bad = expr_wrapper(serde_json::json!({"type": "value", "value": ["1", 2.5]}));
        assert!(assign(&bad, &spec, None).is_err());
        let good = expr_wrapper(serde_json::json!({"type": "value", "value": [1, 2]}));
        assert!(assign(&good, &spec, None).is_ok());
    }

    #[test]
    fn test_binding_only_columns_type_neither_siblings_nor_targets() {
        // A MONEY column reaches the builders as a `bind_types` hint folded
        // into `column_types`: it binds a Decimal value, but must not make a
        // sibling NULL a NUMERIC parameter.
        let types = ColumnTypes::from([(
            "amount".to_string(),
            ColumnTypeSpec::Decimal {
                precision: None,
                scale: None,
            },
        )]);
        let ir = oxyde_codec::QueryIR {
            column_types: Some(types.clone()),
            bind_types: Some(types),
            ..Default::default()
        };
        let node = se(serde_json::json!({
            "type": "func", "name": "coalesce", "args": [
                {"type": "column", "name": "amount"},
                {"type": "value", "value": null},
            ],
        }));
        let (sql, params) = render(compile_scalar(&node, None, ExprTypes::of(&ir)).unwrap());
        assert_eq!(sql, r#"SELECT COALESCE("amount", NULL) AS "x""#);
        assert!(params.is_empty(), "{params:?}");

        // As an assignment target it imposes nothing either, yet a bare value
        // still binds by the hint.
        let wrapper = expr_wrapper(serde_json::json!({
            "type": "func", "name": "coalesce", "args": [
                {"type": "value", "value": null},
                {"type": "value", "value": 5},
            ],
        }));
        let (_, params) = render(
            assignment_expr(&wrapper, "amount", ExprTypes::of(&ir), Dialect::Postgres).unwrap(),
        );
        assert_eq!(params.len(), 1, "{params:?}");

        let bare = rmpv::Value::String("2.50".into());
        let (_, params) = render(
            assignment_expr(&bare, "amount", ExprTypes::of(&ir), Dialect::Postgres).unwrap(),
        );
        assert!(matches!(params[0], Value::Decimal(Some(_))), "{params:?}");
    }
}
