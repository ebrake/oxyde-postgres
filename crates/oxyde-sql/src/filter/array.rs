//! PostgreSQL array and JSON lookups: contains, contained_by, and overlap.
//!
//! The outer array is a set; order and duplicates are ignored. SQL NULL
//! columns yield NULL. Operand null elements compare explicitly, using
//! IS NOT DISTINCT FROM. Non-array JSON values yield false.
//!
//! Typed arrays use native @>, <@, and && with a cast to the column type.
//! Operands containing NULL use unnest. JSON columns compare whole values
//! through jsonb_array_elements; objects ignore key order and nested JSON
//! arrays retain their order. Operands are validated before binding.

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
    /// Typed PostgreSQL array column (`list[T]` stored as `T[]`).
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
            let arr_type = element_array_type(item)
                .ok_or_else(|| unsupported("array lookups do not support this element type"))?;
            let mut elements = items
                .iter()
                .map(|v| typed_element(v, item))
                .collect::<Result<Vec<_>>>()?;

            // Native operators never match NULL elements, so None operands
            // compare element by element.
            let has_null = items.iter().any(rmpv::Value::is_nil);

            {
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
                Ok(postgres_typed(
                    col,
                    op,
                    &operand,
                    &array_type,
                    has_null,
                    json,
                ))
            }
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

            let operand = Value::Json(Some(Box::new(serde_json::Value::Array(elements))));

            Ok(match dialect {
                Dialect::Postgres => postgres_json(col, op, &operand, pg_json_needs_cast(db_type)),
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
        (S::Blob, V::Binary(b)) => Some(Value::Bytes(Some(b.clone()))),
        (S::Uuid, V::String(s)) => s
            .as_str()
            .and_then(|s| Uuid::parse_str(s).ok())
            .map(|u| Value::Uuid(Some(u))),
        (S::Decimal { .. }, V::String(s)) => s
            .as_str()
            .and_then(parse_decimal_exact)
            .map(|d| Value::Decimal(Some(d))),
        (S::Decimal { .. }, V::Integer(n)) => n
            .as_i64()
            .map(Decimal::from)
            .or_else(|| n.as_u64().map(Decimal::from))
            .map(|d| Value::Decimal(Some(d))),
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
        assert_eq!(value, Value::Decimal(Some(Decimal::from(100))));
    }

    #[test]
    fn decimal_elements_accept_unsigned_integers() {
        let item = ColumnTypeSpec::Decimal {
            precision: None,
            scale: None,
        };
        for n in [1_u64 << 63, u64::MAX] {
            let value = typed_element(&rmpv::Value::Integer(n.into()), &item).unwrap();
            assert_eq!(value, Value::Decimal(Some(Decimal::from(n))), "{n}");
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
