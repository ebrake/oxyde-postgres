//! Array lookups: `ARRAY_CONTAINS`, `ARRAY_CONTAINED_BY` and `ARRAY_OVERLAP`.
//!
//! Semantics shared by every dialect: the outer array is a set (order and
//! duplicates are ignored), elements compare as complete values, a `None`
//! operand element matches a null element, an SQL-NULL column yields NULL
//! and a non-array JSON value yields false. On PostgreSQL and MySQL, object
//! elements ignore key order and nested arrays keep theirs; SQLite rejects
//! operands holding objects or arrays.
//!
//! - PostgreSQL typed arrays use native `@>` / `<@` / `&&` against an operand
//!   cast to the column's array type. Native operators never match NULL
//!   elements, so operands containing NULL switch to an `unnest` form with
//!   `IS NOT DISTINCT FROM`.
//! - PostgreSQL JSON columns enumerate `jsonb_array_elements` and compare
//!   whole jsonb values.
//! - MySQL walks both arrays with `JSON_TABLE` and compares elements as JSON
//!   values. Its `JSON PATH '$'` columns turn JSON null elements into SQL
//!   NULL, so comparisons are null-safe.
//! - SQLite walks both arrays with `json_each` and compares elements by JSON
//!   type and value, with `integer` and `real` as one type (`1` matches `1.0`
//!   but not `true`). Typed decimals compare as `REAL`. JSONB values are read
//!   as the JSON they hold (`json_valid(.., 9)`, SQLite 3.45+; the bundled
//!   SQLite is newer).
//!
//! MySQL and SQLite store typed arrays as JSON. The operand is bound as a
//! `Value::Array` so the driver serializes it with the same encoder it uses
//! for stored values; every element is validated here first, because that
//! encoder (and PostgreSQL array binding) turns mismatched elements into NULL.

use oxyde_codec::ColumnTypeSpec;
use rust_decimal::Decimal;
use sea_query::{SimpleExpr, Value};
use uuid::Uuid;

use super::pg_array::{fits_pg_array_type, pg_column_array_type};
use super::render::{render, render_col};
use crate::error::{QueryError, Result};
use crate::utils::bind::{
    bind_value, coerce_element, element_array_type, parse_decimal_exact, string_value,
};
use crate::utils::value::rmpv_to_json;
use crate::Dialect;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArrayOp {
    Contains,
    ContainedBy,
    Overlap,
}

impl ArrayOp {
    pub(crate) fn from_operator(operator: &str) -> Option<Self> {
        match operator {
            "ARRAY_CONTAINS" => Some(Self::Contains),
            "ARRAY_CONTAINED_BY" => Some(Self::ContainedBy),
            "ARRAY_OVERLAP" => Some(Self::Overlap),
            _ => None,
        }
    }

    /// Lookup name.
    fn name(self) -> &'static str {
        match self {
            Self::Contains => "contains",
            Self::ContainedBy => "contained_by",
            Self::Overlap => "overlap",
        }
    }

    fn pg_operator(self) -> &'static str {
        match self {
            Self::Contains => "@>",
            Self::ContainedBy => "<@",
            Self::Overlap => "&&",
        }
    }
}

/// What the filtered column stores.
enum Target<'a> {
    /// Typed array column (`list[T]`): PostgreSQL `T[]`, JSON elsewhere.
    Typed(&'a ColumnTypeSpec),
    /// JSON/JSONB column holding a top-level array.
    Json,
}

fn classify(spec: &ColumnTypeSpec) -> Result<Target<'_>> {
    match spec {
        ColumnTypeSpec::Array { item } => match item.as_ref() {
            ColumnTypeSpec::Array { .. } => Err(unsupported(
                "array lookups do not support nested typed arrays; store them in a JSON column",
            )),
            ColumnTypeSpec::Unknown => Err(unsupported(
                "array lookups do not support this array's element type",
            )),
            item => Ok(Target::Typed(item)),
        },
        ColumnTypeSpec::Json | ColumnTypeSpec::JsonBinary => Ok(Target::Json),
        _ => Err(unsupported("array lookups require an array or JSON column")),
    }
}

/// Build the predicate for an array lookup on `col`.
pub(crate) fn build_array_filter(
    col: &SimpleExpr,
    op: ArrayOp,
    value: &rmpv::Value,
    spec: &ColumnTypeSpec,
    db_type: Option<&str>,
    dialect: Dialect,
) -> Result<SimpleExpr> {
    let rmpv::Value::Array(items) = value else {
        return Err(invalid(format!(
            "Array lookup '{}' requires a list or tuple value",
            op.name()
        )));
    };

    match classify(spec)? {
        Target::Typed(item) => {
            if matches!(item, ColumnTypeSpec::Blob) && dialect != Dialect::Postgres {
                // The JSON encoder stores bytes elements as null on these
                // backends, so there is nothing to compare against.
                return Err(unsupported(
                    "array lookups on bytes arrays are only supported on PostgreSQL",
                ));
            }
            let arr_type = element_array_type(item)
                .ok_or_else(|| unsupported("array lookups do not support this element type"))?;
            let mut elements = items
                .iter()
                .map(|v| typed_element(v, item))
                .collect::<Result<Vec<_>>>()?;
            if dialect == Dialect::Sqlite {
                for element in &elements {
                    if let Value::Json(Some(json)) = element {
                        check_sqlite_scalar(json)?;
                    }
                }
            }
            // Native operators never match NULL elements, so None operands
            // compare element by element.
            let has_null = items.iter().any(rmpv::Value::is_nil);
            let decimal = matches!(item, ColumnTypeSpec::Decimal { .. });

            if dialect == Dialect::Postgres {
                let array_type = pg_column_array_type(item, db_type)?;
                // An element the column type cannot hold equals no stored
                // element: `contains` matches no row, and the other lookups
                // leave it out.
                let count = elements.len();
                elements.retain(|e| fits_pg_array_type(e, &array_type));
                if elements.len() < count && op == ArrayOp::Contains {
                    let sql = "(CASE WHEN {col} IS NULL THEN NULL ELSE FALSE END)";
                    return Ok(render_col(sql, col, Dialect::Postgres));
                }
                let operand = Value::Array(arr_type, Some(Box::new(elements)));
                let json = matches!(item, ColumnTypeSpec::Json | ColumnTypeSpec::JsonBinary);
                return Ok(postgres_typed(
                    col,
                    op,
                    &operand,
                    &array_type,
                    has_null,
                    json,
                ));
            }
            let operand = Value::Array(arr_type, Some(Box::new(elements)));
            Ok(match dialect {
                Dialect::Mysql => mysql(col, op, &operand, decimal),
                _ => sqlite(col, op, &operand, decimal),
            })
        }
        Target::Json => {
            let elements = items
                .iter()
                .map(|v| {
                    rmpv_to_json(v).ok_or_else(|| {
                        invalid(format!(
                            "Array lookup element {v} cannot be represented as JSON"
                        ))
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            if dialect == Dialect::Sqlite {
                elements.iter().try_for_each(check_sqlite_scalar)?;
            }
            let operand = Value::Json(Some(Box::new(serde_json::Value::Array(elements))));

            Ok(match dialect {
                Dialect::Postgres => postgres_json(col, op, &operand, pg_json_needs_cast(db_type)),
                Dialect::Mysql => mysql(col, op, &operand, false),
                Dialect::Sqlite => sqlite(col, op, &operand, false),
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Operand validation
// ---------------------------------------------------------------------------

/// Strictly convert one operand element to the column's element type.
///
/// Unlike `bind_value`, nothing falls back to a string: an element that does
/// not convert is an error rather than a silent non-match (or NULL).
fn typed_element(value: &rmpv::Value, item: &ColumnTypeSpec) -> Result<Value> {
    use rmpv::Value as V;
    use ColumnTypeSpec as S;

    if value.is_nil() {
        return Ok(bind_value(value, item));
    }

    let converted = match (item, value) {
        (S::BigInteger | S::Timedelta, V::Integer(n)) => n.as_i64().map(|i| Value::BigInt(Some(i))),
        // Only integers a double holds exactly; anything else would round
        // into a different value. The round trip compares through i128, so
        // i64::MAX (which rounds up to 2^63) cannot saturate back to itself.
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_lossless
        )]
        (S::Double, V::Integer(n)) => n
            .as_i64()
            .map(i128::from)
            .or_else(|| n.as_u64().map(i128::from))
            .map(|i| (i as f64, i))
            .filter(|&(f, i)| f as i128 == i)
            .map(|(f, _)| Value::Double(Some(f))),
        (S::Double, V::F32(_) | V::F64(_)) => match bind_value(value, item) {
            Value::Double(Some(f)) if f.is_finite() => Some(Value::Double(Some(f))),
            _ => None,
        },
        (S::Boolean, V::Boolean(b)) => Some(Value::Bool(Some(*b))),
        (S::Text | S::String { .. }, V::String(s)) => s.as_str().map(string_value),
        (S::Enum { values, .. }, V::String(s)) => s
            .as_str()
            .filter(|s| values.iter().any(|allowed| allowed == s))
            .map(string_value),
        (S::Blob, V::Binary(b)) => Some(Value::Bytes(Some(Box::new(b.clone())))),
        (S::Uuid, V::String(s)) => s
            .as_str()
            .and_then(|s| Uuid::parse_str(s).ok())
            .map(|u| Value::Uuid(Some(Box::new(u)))),
        (S::Decimal { .. }, V::String(s)) => s
            .as_str()
            .and_then(parse_decimal_exact)
            .map(|d| Value::Decimal(Some(Box::new(d)))),
        (S::Decimal { .. }, V::Integer(n)) => n
            .as_i64()
            .map(Decimal::from)
            .or_else(|| n.as_u64().map(Decimal::from))
            .map(|d| Value::Decimal(Some(Box::new(d)))),
        (S::DateTime | S::DateTimeUtc | S::Date | S::Time, V::String(_)) => {
            temporal_element(bind_value(value, item), item)
        }
        // Strings are JSON strings, as stored, never parsed as JSON text.
        (S::Json | S::JsonBinary, _) => rmpv_to_json(value).map(|j| Value::Json(Some(Box::new(j)))),
        _ => None,
    };

    converted.ok_or_else(|| {
        invalid(format!(
            "Array lookup element {value} is not a valid {} value",
            kind_name(item)
        ))
    })
}

/// Keep only parses that produced the element's own temporal type; aware and
/// naive datetimes are normalized to the column's flavour exactly as stored
/// elements are (`coerce_element`).
fn temporal_element(bound: Value, item: &ColumnTypeSpec) -> Option<Value> {
    use ColumnTypeSpec as S;
    match (item, coerce_element(bound, item)) {
        (S::DateTime, v @ Value::ChronoDateTime(Some(_)))
        | (S::DateTimeUtc, v @ Value::ChronoDateTimeUtc(Some(_)))
        | (S::Date, v @ Value::ChronoDate(Some(_)))
        | (S::Time, v @ Value::ChronoTime(Some(_))) => Some(v),
        _ => None,
    }
}

fn kind_name(item: &ColumnTypeSpec) -> &'static str {
    use ColumnTypeSpec as S;
    match item {
        S::BigInteger => "integer",
        S::Timedelta => "timedelta (microseconds)",
        S::Double => "float",
        S::Boolean => "boolean",
        S::Text | S::String { .. } => "string",
        S::Blob => "bytes",
        S::DateTime | S::DateTimeUtc => "datetime",
        S::Date => "date",
        S::Time => "time",
        S::Uuid => "UUID",
        S::Decimal { .. } => "decimal",
        S::Json | S::JsonBinary => "JSON",
        S::Enum { .. } => "enum",
        S::Array { .. } => "array",
        S::Unknown => "untyped",
    }
}

// ---------------------------------------------------------------------------
// PostgreSQL
// ---------------------------------------------------------------------------

/// Canonical DDL renders both JSON kinds as JSONB; only an explicit
/// `db_type="JSON"` column is `json`, which has no equality operator.
fn pg_json_needs_cast(db_type: Option<&str>) -> bool {
    db_type.is_some_and(|d| d.trim().eq_ignore_ascii_case("JSON"))
}

const PG_LEFT: &str = r#""__oxyde_l"("__oxyde_v")"#;
const PG_RIGHT: &str = r#""__oxyde_r"("__oxyde_v")"#;
/// The stored value inside `pg_hold`.
const PG_HELD: &str = r#""__oxyde_h"."__oxyde_c""#;

/// Run `body` over the stored value `doc`, which `body` reads as `PG_HELD`.
///
/// `{col}` is unqualified outside joins, so read inside `body`'s subqueries
/// it could resolve to one of their aliases instead (a column named
/// `__oxyde_v`). The holder reads it once, as the only FROM item of a
/// subquery, where nothing but the outer query is in scope.
fn pg_hold(doc: &str, body: &str) -> String {
    format!(r#"(SELECT {body} FROM (SELECT {doc} AS "__oxyde_c") AS "__oxyde_h")"#)
}

fn postgres_typed(
    col: &SimpleExpr,
    op: ArrayOp,
    operand: &Value,
    array_type: &str,
    has_null: bool,
    json: bool,
) -> SimpleExpr {
    let arg = format!("{{arg}}::{array_type}");
    if !has_null {
        let sql = format!("({{col}} {} {arg})", op.pg_operator());
        return render(&sql, col, operand, Dialect::Postgres);
    }

    let left = format!("unnest({PG_HELD}) AS {PG_LEFT}");
    let right = format!("unnest({arg}) AS {PG_RIGHT}");
    // A JSON null element (written from "null" before strings were stored
    // as JSON strings) loads as None, so it matches an SQL NULL operand
    // element, and the other way round.
    let eq = if json {
        r#"COALESCE("__oxyde_l"."__oxyde_v", 'null'::jsonb) = COALESCE("__oxyde_r"."__oxyde_v", 'null'::jsonb)"#
    } else {
        r#""__oxyde_l"."__oxyde_v" IS NOT DISTINCT FROM "__oxyde_r"."__oxyde_v""#
    };
    let body = pg_hold("{col}", &set_predicate(op, &left, &right, eq));
    let sql = format!("(CASE WHEN {{col}} IS NULL THEN NULL ELSE {body} END)");
    render(&sql, col, operand, Dialect::Postgres)
}

fn postgres_json(col: &SimpleExpr, op: ArrayOp, operand: &Value, cast_column: bool) -> SimpleExpr {
    let doc = if cast_column {
        "({col})::jsonb"
    } else {
        "{col}"
    };
    let arg = "{arg}::jsonb";
    let left = format!("jsonb_array_elements({PG_HELD}) AS {PG_LEFT}");
    let right = format!("jsonb_array_elements({arg}) AS {PG_RIGHT}");
    let eq = r#""__oxyde_l"."__oxyde_v" = "__oxyde_r"."__oxyde_v""#;
    let body = pg_hold(doc, &set_predicate(op, &left, &right, eq));
    // CASE, not AND: PostgreSQL does not promise to evaluate the type check
    // first, and jsonb_array_elements raises on scalars and objects. The
    // NULL check reads the column itself: a `json` column is NULL exactly
    // when its jsonb cast is, so it need not be cast.
    let guarded = format!(
        "CASE WHEN {{col}} IS NULL THEN NULL \
         WHEN jsonb_typeof({doc}) = 'array' THEN {body} ELSE FALSE END"
    );
    let sql = if op == ArrayOp::Contains {
        // Exact containment implies jsonb `@>`, which is false for every
        // non-array, so it can lead the predicate and use a GIN index. That
        // binds the operand a second time; binding it once would move it
        // into a subquery, where the index cannot be used.
        format!("({doc} @> {arg} AND {guarded})")
    } else {
        format!("({guarded})")
    };
    render(&sql, col, operand, Dialect::Postgres)
}

// ---------------------------------------------------------------------------
// MySQL
// ---------------------------------------------------------------------------

/// Decimal lexemes the MySQL decimal comparison casts: at most 35 integer
/// and 30 fractional digits, which DECIMAL(65,30) holds exactly, and a
/// two-digit exponent, which keeps the `+ 0e0` range check finite.
const MYSQL_DECIMAL_LEXEME: &str =
    "^[-+]?([0-9]{1,35}([.][0-9]{0,30})?|[.][0-9]{1,30})([eE][-+]?[0-9]{1,2})?$";

fn mysql(col: &SimpleExpr, op: ArrayOp, operand: &Value, decimal: bool) -> SimpleExpr {
    // `?` placeholders bind one value each, so the operand is bound once in
    // a derived table and every other use reads its column. JSON_TABLE can
    // refer to tables before it in the same FROM clause.
    //
    // `{col}` is unqualified outside joins, so read inside these subqueries
    // it could resolve to one of their columns instead (a column named
    // `oxyde_v`). It is read once, by a holder that is the only FROM item of
    // a subquery, where nothing but the outer query is in scope; a
    // JSON_TABLE, since derived tables cannot refer to the outer query
    // before MySQL 8.0.14.
    let left = "JSON_TABLE(oxyde_h.oxyde_c, '$[*]' COLUMNS (oxyde_v JSON PATH '$')) AS oxyde_l";
    let right = "(SELECT CAST({arg} AS JSON) AS oxyde_j) AS oxyde_a \
         CROSS JOIN JSON_TABLE(oxyde_a.oxyde_j, '$[*]' COLUMNS (oxyde_v JSON PATH '$')) AS oxyde_r";
    // Elements, extracted once per row of each JSON_TABLE. The path always
    // exists, so SQL NULL here is a JSON null element; before MySQL 8.0.21,
    // JSON_TABLE yields a JSON null instead, which `null` also accepts.
    let l = "oxyde_l.oxyde_v";
    let r = "oxyde_r.oxyde_v";

    let eq = if decimal {
        // Typed decimals are stored as JSON strings that keep their scale
        // ("1.50"); compare them as DECIMAL, never as floating point.
        // Operands are exact rust_decimal values (at most 28 fractional
        // digits, magnitude below ~7.9e28; anything that would round is
        // rejected), which DECIMAL(65,30) holds exactly.
        //
        // The stored side can hold anything, and CAST turns text that is not
        // a number into 0 (a false match), or raises under strict mode in
        // UPDATE/DELETE, as it does on overflow. So, in CASE order (MySQL
        // evaluates CASE lazily): JSON nulls never reach the cast; anything
        // that is not a decimal lexeme DECIMAL(65,30) can hold is a
        // non-match; and an exponent lexeme must be 0, or within the
        // operand range, checked as a double (`+ 0e0`, which cannot fail on
        // a validated lexeme), since it could overflow the cast or underflow
        // into 0, and have at most 30 fractional digits once expanded
        // ("1.001e-28" has 31 and would round to 1e-28). Without an
        // exponent, the lexeme pattern already bounds both.
        let lt = format!("JSON_UNQUOTE({l})");
        // Fractional digits of the expanded value: digits up to the last
        // non-zero one, minus integer-part digits, minus the exponent.
        let mantissa = format!("REGEXP_REPLACE({lt}, '^[-+]|[eE].*$', '')");
        let expanded = format!(
            "(LENGTH(TRIM(TRAILING '0' FROM REPLACE({mantissa}, '.', ''))) \
             - LENGTH(SUBSTRING_INDEX({mantissa}, '.', 1)) \
             - CAST(REGEXP_SUBSTR({lt}, '[-+]?[0-9]+$') AS SIGNED))"
        );
        let null = |x: &str| format!("({x} IS NULL OR JSON_TYPE({x}) = 'NULL')");
        let (ln, rn) = (null(l), null(r));
        format!(
            "CASE WHEN {ln} OR {rn} THEN {ln} AND {rn} \
             WHEN JSON_TYPE({l}) NOT IN ('STRING', 'INTEGER', 'UNSIGNED INTEGER', 'DECIMAL', 'DOUBLE') \
             OR {lt} NOT REGEXP '{MYSQL_DECIMAL_LEXEME}' THEN FALSE \
             WHEN {lt} REGEXP '[eE]' \
             AND NOT ({lt} + 0e0 = 0 OR (ABS({lt} + 0e0) BETWEEN 1e-28 AND 1e29 \
             AND {expanded} <= 30)) THEN FALSE \
             ELSE CAST({lt} AS DECIMAL(65,30)) = CAST(JSON_UNQUOTE({r}) AS DECIMAL(65,30)) END"
        )
    } else {
        format!("{l} <=> {r}")
    };

    let body = format!(
        "(SELECT {} FROM JSON_TABLE({{col}}, '$' COLUMNS (oxyde_c JSON PATH '$')) AS oxyde_h)",
        set_predicate(op, left, right, &eq)
    );
    // JSON_TABLE yields no rows for a scalar, which would make the NOT EXISTS
    // forms true; non-arrays are answered before it runs.
    let guarded = format!(
        "CASE WHEN {{col}} IS NULL THEN NULL \
         WHEN JSON_TYPE({{col}}) = 'ARRAY' THEN {body} ELSE FALSE END"
    );
    // Exact containment implies JSON_CONTAINS, which is linear, so it rejects
    // most rows before the pairwise subqueries run; it binds the operand a
    // second time, outside the derived table. (JSON_OVERLAPS would do
    // the same for overlap but needs MySQL 8.0.17.) Decimals compare as
    // DECIMAL, which JSON containment does not, so they keep the bare form.
    let prefilter = match op {
        _ if decimal => None,
        ArrayOp::Contains => Some("JSON_CONTAINS({col}, CAST({arg} AS JSON))"),
        ArrayOp::ContainedBy => Some("JSON_CONTAINS(CAST({arg} AS JSON), {col})"),
        ArrayOp::Overlap => None,
    };
    let sql = match prefilter {
        Some(prefilter) => format!("({prefilter} AND {guarded})"),
        None => format!("({guarded})"),
    };
    render(&sql, col, operand, Dialect::Mysql)
}

// ---------------------------------------------------------------------------
// SQLite
// ---------------------------------------------------------------------------

/// The stored value inside `sqlite`'s holder subquery.
const SQLITE_HELD: &str = r#""oxyde_h"."oxyde_c""#;
/// `json_each` columns of a stored (`l`) and an operand (`r`) element.
const SQLITE_L_TYPE: &str = r#""oxyde_l"."type""#;
const SQLITE_L_VALUE: &str = r#""oxyde_l"."value""#;
const SQLITE_R_TYPE: &str = r#""oxyde_r"."type""#;
const SQLITE_R_VALUE: &str = r#""oxyde_r"."value""#;

fn sqlite(col: &SimpleExpr, op: ArrayOp, operand: &Value, decimal: bool) -> SimpleExpr {
    let left = format!(r#"json_each({SQLITE_HELD}) AS "oxyde_l""#);
    let right = r#"json_each({arg}) AS "oxyde_r""#;
    let (lt, lv, rt, rv) = (SQLITE_L_TYPE, SQLITE_L_VALUE, SQLITE_R_TYPE, SQLITE_R_VALUE);

    let eq = if decimal {
        // Typed decimals are stored as JSON strings that keep their scale
        // ("1.50"), so they compare as REAL. Other clients may have stored
        // anything, and CAST turns text that is not a number into 0, so only
        // JSON numbers and strings holding one are cast; nulls match nulls.
        format!(
            "CASE WHEN {lt} = 'null' OR {rt} = 'null' THEN {lt} = {rt} \
             WHEN CASE WHEN {lt} IN ('integer', 'real') THEN TRUE \
             WHEN {lt} = 'text' AND json_valid({lv}) THEN json_type({lv}) IN ('integer', 'real') \
             ELSE FALSE END \
             THEN CAST({lv} AS REAL) = CAST({rv} AS REAL) ELSE FALSE END"
        )
    } else {
        // `true` and `1` differ by type; `1` and `1.0` only by integer/real.
        format!(
            "({lt} = {rt} OR ({lt} IN ('integer', 'real') AND {rt} IN ('integer', 'real'))) \
             AND {lv} IS {rv}"
        )
    };

    // `{col}` is unqualified outside joins, so read inside the json_each
    // subqueries it could resolve to one of their columns instead (a column
    // named `value` or `type`). The holder reads it once, as the only FROM
    // item of a subquery, where nothing but the outer query is in scope.
    let body = format!(
        r#"(SELECT {} FROM (SELECT {{col}} AS "oxyde_c") AS "oxyde_h")"#,
        set_predicate(op, &left, right, &eq)
    );
    // SQLite does not validate JSON columns, and json_type() / json_each()
    // raise on text that is not JSON, so it is checked first; CASE, not
    // AND, fixes that order. Flags 9 accept RFC 8259 text (1) and strictly
    // valid JSONB blobs (8). json_each() would walk an object's members or
    // yield a scalar itself, so only arrays reach it.
    let sql = format!(
        "(CASE WHEN {{col}} IS NULL THEN NULL \
         WHEN json_valid({{col}}, 9) THEN \
         CASE WHEN json_type({{col}}) = 'array' THEN {body} ELSE FALSE END \
         ELSE FALSE END)"
    );
    render(&sql, col, operand, Dialect::Sqlite)
}

/// SQLite compares `json_each` scalars, so an object or array element in an
/// operand is an error rather than a filter that silently matches nothing.
fn check_sqlite_scalar(element: &serde_json::Value) -> Result<()> {
    if element.is_object() || element.is_array() {
        return Err(unsupported(
            "array lookups with object or array elements are only supported on \
             PostgreSQL and MySQL",
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Set predicate over two FROM items enumerating the stored (`left`) and
/// operand (`right`) elements; `eq` matches one pair.
fn set_predicate(op: ArrayOp, left: &str, right: &str, eq: &str) -> String {
    match op {
        ArrayOp::Contains => format!(
            "NOT EXISTS (SELECT 1 FROM {right} WHERE NOT EXISTS (SELECT 1 FROM {left} WHERE {eq}))"
        ),
        ArrayOp::ContainedBy => format!(
            "NOT EXISTS (SELECT 1 FROM {left} WHERE NOT EXISTS (SELECT 1 FROM {right} WHERE {eq}))"
        ),
        ArrayOp::Overlap => {
            format!("EXISTS (SELECT 1 FROM {left} WHERE EXISTS (SELECT 1 FROM {right} WHERE {eq}))")
        }
    }
}

fn invalid(message: String) -> QueryError {
    QueryError::InvalidQuery(message)
}

fn unsupported(message: &str) -> QueryError {
    QueryError::UnsupportedOperation(message.to_string())
}

#[cfg(test)]
mod tests {
    use sea_query::value::ArrayType;

    use super::*;

    #[test]
    fn nested_and_unknown_element_types_have_their_own_errors() {
        let nested = ColumnTypeSpec::Array {
            item: Box::new(ColumnTypeSpec::Array {
                item: Box::new(ColumnTypeSpec::BigInteger),
            }),
        };
        let err = classify(&nested).err().unwrap().to_string();
        assert!(err.contains("nested typed arrays"), "{err}");

        let unknown = ColumnTypeSpec::Array {
            item: Box::new(ColumnTypeSpec::Unknown),
        };
        let err = classify(&unknown).err().unwrap().to_string();
        assert!(err.contains("element type"), "{err}");
    }

    #[test]
    fn decimal_elements_beyond_rust_decimal_are_rejected() {
        let item = ColumnTypeSpec::Decimal {
            precision: None,
            scale: None,
        };
        assert!(typed_element(&rmpv::Value::String("2E+40".into()), &item).is_err());
    }

    #[test]
    fn decimal_elements_that_would_round_are_rejected() {
        let item = ColumnTypeSpec::Decimal {
            precision: None,
            scale: None,
        };
        for s in [
            "1.00000000000000000000000000001",
            "1.00000000000000000000000000001E+1",
        ] {
            assert!(
                typed_element(&rmpv::Value::String(s.into()), &item).is_err(),
                "{s}"
            );
        }
    }

    #[test]
    fn decimal_elements_with_trailing_zeros_are_exact() {
        let item = ColumnTypeSpec::Decimal {
            precision: None,
            scale: None,
        };
        let cases = [
            ("1.0000000000000000000000000000", "1"),
            ("1.000000000000000000000000000000", "1"),
            ("1.50000000000000000000000000000", "1.5"),
            ("1.5E-3", "0.0015"),
            ("1.500E-27", "0.0000000000000000000000000015"),
            ("-0", "0"),
            // Zero is exact beyond rust_decimal's ±28 exponent range.
            ("0E-30", "0"),
            ("0E+40", "0"),
        ];
        for (s, expected) in cases {
            let value = typed_element(&rmpv::Value::String(s.into()), &item).unwrap();
            let Value::Decimal(Some(d)) = value else {
                panic!("{s}: {value:?}");
            };
            assert_eq!(d.normalize(), expected.parse::<Decimal>().unwrap(), "{s}");
        }
    }

    #[test]
    fn decimal_elements_accept_exponents() {
        let item = ColumnTypeSpec::Decimal {
            precision: None,
            scale: None,
        };
        let value = typed_element(&rmpv::Value::String("1E+2".into()), &item).unwrap();
        assert_eq!(value, Value::Decimal(Some(Box::new(Decimal::from(100)))));
    }

    #[test]
    fn decimal_elements_accept_unsigned_integers() {
        let item = ColumnTypeSpec::Decimal {
            precision: None,
            scale: None,
        };
        for n in [1_u64 << 63, u64::MAX] {
            let value = typed_element(&rmpv::Value::Integer(n.into()), &item).unwrap();
            assert_eq!(
                value,
                Value::Decimal(Some(Box::new(Decimal::from(n)))),
                "{n}"
            );
        }
    }

    #[test]
    fn json_string_elements_match_how_they_were_inserted() {
        for item in [ColumnTypeSpec::Json, ColumnTypeSpec::JsonBinary] {
            let array = ColumnTypeSpec::Array {
                item: Box::new(item.clone()),
            };
            // Strings are JSON strings, never parsed as JSON text.
            let cases = [
                (r#""hello""#, serde_json::json!(r#""hello""#)),
                (r#"{"a": 1}"#, serde_json::json!(r#"{"a": 1}"#)),
                ("hello", serde_json::json!("hello")),
                ("123", serde_json::json!("123")),
                ("null", serde_json::json!("null")),
            ];
            for (text, expected) in cases {
                let expected = Value::Json(Some(Box::new(expected)));
                let operand = typed_element(&rmpv::Value::String(text.into()), &item).unwrap();
                assert_eq!(operand, expected, "{text}");
                let stored = bind_value(&rmpv::Value::Array(vec![text.into()]), &array);
                assert_eq!(
                    stored,
                    Value::Array(ArrayType::Json, Some(Box::new(vec![expected])))
                );
            }
            // Non-string elements convert as before.
            let map = rmpv::Value::Map(vec![("a".into(), 1.into())]);
            assert_eq!(
                typed_element(&map, &item).unwrap(),
                Value::Json(Some(Box::new(serde_json::json!({"a": 1}))))
            );
        }
    }

    #[test]
    fn mismatched_elements_are_rejected() {
        let cases = [
            (ColumnTypeSpec::BigInteger, rmpv::Value::String("1".into())),
            (ColumnTypeSpec::BigInteger, rmpv::Value::Boolean(true)),
            (ColumnTypeSpec::Boolean, rmpv::Value::Integer(1.into())),
            (
                ColumnTypeSpec::Uuid,
                rmpv::Value::String("not-a-uuid".into()),
            ),
            (
                ColumnTypeSpec::Date,
                rmpv::Value::String("2024-01-01T10:00:00Z".into()),
            ),
            (ColumnTypeSpec::Double, rmpv::Value::F64(f64::NAN)),
            // 2^53 + 1 would round to 2^53 as a double.
            (
                ColumnTypeSpec::Double,
                rmpv::Value::Integer(((1_i64 << 53) + 1).into()),
            ),
            (
                ColumnTypeSpec::Double,
                rmpv::Value::Integer((-(1_i64 << 53) - 1).into()),
            ),
            // Rounds up to 2^63, which i64 cannot hold.
            (
                ColumnTypeSpec::Double,
                rmpv::Value::Integer(i64::MAX.into()),
            ),
            (
                ColumnTypeSpec::Double,
                rmpv::Value::Integer(u64::MAX.into()),
            ),
            (
                ColumnTypeSpec::Enum {
                    name: "e".into(),
                    values: vec!["a".into()],
                },
                rmpv::Value::String("b".into()),
            ),
        ];
        for (item, value) in cases {
            assert!(
                typed_element(&value, &item).is_err(),
                "{value} should not convert to {item:?}"
            );
        }
    }

    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn float_elements_accept_exactly_representable_integers() {
        for n in [0_i64, 3, 1 << 53, -(1 << 53), 1 << 54, i64::MIN] {
            let value = typed_element(&rmpv::Value::Integer(n.into()), &ColumnTypeSpec::Double);
            assert_eq!(value.unwrap(), Value::Double(Some(n as f64)), "{n}");
        }
    }

    #[test]
    fn null_elements_bind_as_typed_nulls() {
        let value = typed_element(&rmpv::Value::Nil, &ColumnTypeSpec::Uuid).unwrap();
        assert_eq!(value, Value::Uuid(None));
    }
}
