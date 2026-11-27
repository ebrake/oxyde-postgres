//! PostgreSQL CellEncoder implementation.
//!
//! Decodes PostgreSQL cells using the column spec or runtime type.

use std::borrow::Cow;

use bigdecimal::{BigDecimal, RoundingMode, ToPrimitive};
use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use oxyde_codec::ColumnTypeSpec;
use sqlx::{
    error::BoxDynError,
    postgres::{types::Oid, PgHasArrayType, PgRow, PgTypeInfo, PgTypeKind, PgValueRef, Postgres},
    Column, Decode, Row, Type, TypeInfo,
};

use super::cell::{numeric_cell, ArrayValues, Cell, PgChronoTimeTz, PgNumeric, PgTemporal};
use super::encoder::{CellEncoder, ColumnMeta, ResultEncoding};
use super::postgres_types::{PgOid8, OID, OID8, OID8_ARRAY, OID_ARRAY};

pub struct PgEncoder;

// Catalog OIDs let common signed integers skip the size-probing fallback.
const INT2: Oid = Oid(21);
const INT4: Oid = Oid(23);
const INT8: Oid = Oid(20);

#[derive(Debug, Clone)]
#[repr(transparent)]
struct PgEnumText(String);

impl Type<Postgres> for PgEnumText {
    fn type_info() -> PgTypeInfo {
        <String as Type<Postgres>>::type_info()
    }

    fn compatible(ty: &PgTypeInfo) -> bool {
        <String as Type<Postgres>>::compatible(ty) || matches!(ty.kind(), PgTypeKind::Enum(_))
    }
}

impl PgHasArrayType for PgEnumText {
    fn array_type_info() -> PgTypeInfo {
        <String as PgHasArrayType>::array_type_info()
    }

    fn array_compatible(ty: &PgTypeInfo) -> bool {
        matches!(ty.kind(), PgTypeKind::Array(element) if Self::compatible(element))
    }
}

impl<'r> Decode<'r, Postgres> for PgEnumText {
    fn decode(value: PgValueRef<'r>) -> Result<Self, BoxDynError> {
        Ok(Self(value.as_str()?.to_string()))
    }
}

impl CellEncoder for PgEncoder {
    type Row = PgRow;

    fn result_kind(col: &ColumnMeta) -> Option<&'static str> {
        match (col.result_encoding, col.db_type.as_str()) {
            (Some(ResultEncoding::Wire), _) => return None,
            (Some(ResultEncoding::Runtime), ty) => return pg_result_kind(None, ty),
            (Some(ResultEncoding::DurationMicros), "NUMERIC") => return Some("timedelta"),
            (Some(ResultEncoding::Decimal), "NUMERIC") => {
                return Some("decimal");
            }
            (Some(ResultEncoding::Float | ResultEncoding::IntegerSum), "NUMERIC") => return None,
            _ => {}
        }
        pg_result_kind(col.spec.as_ref(), &col.db_type)
    }

    #[inline]
    fn decode_cell(row: &PgRow, idx: usize, col: &ColumnMeta) -> (Cell, Option<&'static str>) {
        if col.result_encoding == Some(ResultEncoding::Runtime) {
            return (decode_by_db_type(row, idx, &col.db_type), None);
        }
        if let Some(encoding) = col.result_encoding.filter(|encoding| {
            col.db_type == "NUMERIC" && !matches!(encoding, ResultEncoding::Wire)
        }) {
            return decode_aggregate_numeric(row, idx, encoding);
        }
        let cell = col
            .spec
            .as_ref()
            .and_then(|spec| decode_by_spec(row, idx, spec))
            .unwrap_or_else(|| decode_by_db_type(row, idx, &col.db_type));
        (cell, None)
    }
}

#[inline]
fn decode_by_spec(row: &PgRow, idx: usize, spec: &ColumnTypeSpec) -> Option<Cell> {
    Some(match spec {
        ColumnTypeSpec::BigInteger => decode_pg_int(row, idx),
        ColumnTypeSpec::Text | ColumnTypeSpec::String { .. } => fallback_str(row, idx),
        ColumnTypeSpec::Enum { .. } => {
            try_cell(row, idx, |v: PgEnumText| Cell::Str(v.0)).unwrap_or(Cell::Null)
        }
        ColumnTypeSpec::Double => decode_or_text(row, idx, Cell::Float),
        ColumnTypeSpec::Boolean => decode_or_text(row, idx, Cell::Bool),
        ColumnTypeSpec::Blob => decode_or_text(row, idx, Cell::Bytes),
        ColumnTypeSpec::DateTime if row.columns()[idx].type_info().name() != "TIMESTAMPTZ" => {
            decode_or_text::<PgTemporal<NaiveDateTime>>(row, idx, Cell::from)
        }
        ColumnTypeSpec::DateTime | ColumnTypeSpec::DateTimeUtc => {
            decode_or_text::<PgTemporal<DateTime<Utc>>>(row, idx, Cell::from)
        }
        ColumnTypeSpec::Date => decode_or_text::<PgTemporal<NaiveDate>>(row, idx, Cell::from),
        ColumnTypeSpec::Time => {
            decode_pg_time(row, idx, row.columns()[idx].type_info().name() == "TIMETZ")
        }
        ColumnTypeSpec::Uuid => decode_or_text(row, idx, Cell::Uuid),
        ColumnTypeSpec::Decimal { .. } => decode_or_text(row, idx, |value| numeric_cell(&value)),
        ColumnTypeSpec::Timedelta => try_cell(row, idx, Cell::Duration).unwrap_or(Cell::Null),
        ColumnTypeSpec::Json | ColumnTypeSpec::JsonBinary => decode_or_text(row, idx, Cell::Json),
        ColumnTypeSpec::Array { item } => decode_pg_array(row, idx, item),
        ColumnTypeSpec::Unknown => return None,
    })
}

fn decode_by_db_type(row: &PgRow, idx: usize, db_type: &str) -> Cell {
    // oid8's display name depends on search_path; dispatch it by catalog OID.
    match row.columns()[idx].type_info().oid() {
        Some(OID8) => return decode_pg_int(row, idx),
        Some(OID8_ARRAY) => return decode_pg_array(row, idx, &ColumnTypeSpec::BigInteger),
        _ => {}
    }
    if let Some(item) = pg_array_item(db_type) {
        return decode_pg_array(row, idx, &item);
    }
    match db_type {
        "BOOL" | "BOOLEAN" => decode_or_text(row, idx, Cell::Bool),
        "OID" => decode_pg_int(row, idx),
        name if name.contains("INT") => decode_pg_int(row, idx),
        "FLOAT4" => decode_or_text(row, idx, |v: f32| Cell::Float(f64::from(v))),
        name if name.contains("FLOAT") || name.contains("DOUBLE") || name.contains("REAL") => {
            decode_or_text(row, idx, Cell::Float)
        }
        "NUMERIC" | "DECIMAL" => decode_or_text(row, idx, |value| numeric_cell(&value)),
        "UUID" => decode_or_text(row, idx, Cell::Uuid),
        "JSON" | "JSONB" => decode_or_text(row, idx, Cell::Json),
        "TIMESTAMPTZ" => decode_or_text::<PgTemporal<DateTime<Utc>>>(row, idx, Cell::from),
        "TIMESTAMP" => decode_or_text::<PgTemporal<NaiveDateTime>>(row, idx, Cell::from),
        "DATE" => decode_or_text::<PgTemporal<NaiveDate>>(row, idx, Cell::from),
        "TIME" | "TIMETZ" => decode_pg_time(row, idx, db_type == "TIMETZ"),
        "BYTEA" => decode_or_text(row, idx, Cell::Bytes),
        _ => fallback_str(row, idx),
    }
}

fn pg_array_item(db_type: &str) -> Option<ColumnTypeSpec> {
    use ColumnTypeSpec as Spec;

    Some(match db_type {
        "UUID[]" => Spec::Uuid,
        "NUMERIC[]" => Spec::Decimal {
            precision: None,
            scale: None,
        },
        "TIMESTAMP[]" => Spec::DateTime,
        "TIMESTAMPTZ[]" => Spec::DateTimeUtc,
        "DATE[]" => Spec::Date,
        "TIME[]" | "TIMETZ[]" => Spec::Time,
        "INT2[]" | "INT4[]" | "INT8[]" | "OID[]" => Spec::BigInteger,
        "FLOAT4[]" | "FLOAT8[]" => Spec::Double,
        "BOOL[]" => Spec::Boolean,
        "TEXT[]" | "VARCHAR[]" => Spec::Text,
        _ => return None,
    })
}

/// Tag only the runtime types that the corresponding encoder arm can decode.
fn pg_result_kind(spec: Option<&ColumnTypeSpec>, db_type: &str) -> Option<&'static str> {
    use ColumnTypeSpec as Spec;

    match (spec, db_type) {
        (None | Some(Spec::Unknown), ty) => match ty {
            "UUID" => Some("uuid"),
            "NUMERIC" | "DECIMAL" => Some("decimal"),
            "TIMESTAMP" | "TIMESTAMPTZ" => Some("datetime"),
            "DATE" => Some("date"),
            "TIME" | "TIMETZ" => Some("time"),
            _ => pg_array_item(ty).and_then(|item| pg_array_kind(&item, ty)),
        },
        (Some(Spec::Uuid), "UUID") => Some("uuid"),
        (Some(Spec::Decimal { .. }), "NUMERIC") => Some("decimal"),
        (Some(Spec::DateTime | Spec::DateTimeUtc), "TIMESTAMPTZ")
        | (Some(Spec::DateTime), "TIMESTAMP") => Some("datetime"),
        (Some(Spec::Date), "DATE") => Some("date"),
        (Some(Spec::Time), "TIME" | "TIMETZ") => Some("time"),
        (Some(Spec::Timedelta), "INT8") => Some("timedelta"),
        (Some(Spec::Array { item }), ty) => pg_array_kind(item, ty),
        _ => None,
    }
}

fn pg_array_kind(item: &ColumnTypeSpec, db_type: &str) -> Option<&'static str> {
    use ColumnTypeSpec as Spec;

    match (item, db_type) {
        (Spec::Uuid, "UUID[]") => Some("uuid[]"),
        (Spec::Decimal { .. }, "NUMERIC[]") => Some("decimal[]"),
        (Spec::DateTime, "TIMESTAMP[]") | (Spec::DateTime | Spec::DateTimeUtc, "TIMESTAMPTZ[]") => {
            Some("datetime[]")
        }
        (Spec::Date, "DATE[]") => Some("date[]"),
        (Spec::Time, "TIME[]" | "TIMETZ[]") => Some("time[]"),
        (Spec::Timedelta, "INT8[]") => Some("timedelta[]"),
        _ => None,
    }
}

fn decode_pg_time(row: &PgRow, idx: usize, with_offset: bool) -> Cell {
    if with_offset {
        decode_or_text::<PgTemporal<PgChronoTimeTz>>(row, idx, Cell::from)
    } else {
        decode_or_text::<PgTemporal<NaiveTime>>(row, idx, Cell::from)
    }
}

fn decode_aggregate_numeric(
    row: &PgRow,
    idx: usize,
    encoding: ResultEncoding,
) -> (Cell, Option<&'static str>) {
    let value = match row.try_get::<Option<PgNumeric<BigDecimal>>, _>(idx) {
        Ok(Some(PgNumeric::Finite(value))) => value,
        Ok(Some(PgNumeric::Special(text))) => {
            return (Cell::DecimalText(Cow::Borrowed(text)), Some("decimal"));
        }
        Ok(None) => return (Cell::Null, None),
        Err(_) => return (fallback_str(row, idx), None),
    };
    let cell = match encoding {
        ResultEncoding::IntegerSum => {
            if let Some(value) = value.to_i64() {
                Cell::Int(value)
            } else {
                return (Cell::DecimalText(value.to_string().into()), Some("decimal"));
            }
        }
        ResultEncoding::DurationMicros => {
            // A Python timedelta has whole microseconds, rounded half to even.
            match value.with_scale_round(0, RoundingMode::HalfEven).to_i128() {
                Some(micros) => Cell::DurationAggregate(micros),
                None => Cell::Str(value.to_string()),
            }
        }
        ResultEncoding::Float => match value.to_f64() {
            Some(value) => Cell::Float(value),
            None => Cell::Str(value.to_string()),
        },
        ResultEncoding::Decimal => Cell::DecimalText(value.to_string().into()),
        ResultEncoding::Wire | ResultEncoding::Runtime => {
            unreachable!("these encodings do not specialize the numeric result");
        }
    };
    (cell, None)
}

fn decode_pg_int(row: &PgRow, idx: usize) -> Cell {
    match row.columns()[idx].type_info().oid() {
        Some(OID) => decode_or_text(row, idx, |oid: Oid| Cell::UInt(u64::from(oid.0))),
        Some(OID8) => decode_or_text(row, idx, |oid: PgOid8| Cell::UInt(oid.0)),
        Some(INT2) => decode_or_text(row, idx, |v: i16| Cell::Int(i64::from(v))),
        Some(INT4) => decode_or_text(row, idx, |v: i32| Cell::Int(i64::from(v))),
        Some(INT8) => decode_or_text(row, idx, Cell::Int),
        _ => try_cell(row, idx, Cell::Int)
            .or_else(|_| try_cell(row, idx, |v: i32| Cell::Int(i64::from(v))))
            .or_else(|_| try_cell(row, idx, |v: i16| Cell::Int(i64::from(v))))
            .unwrap_or_else(|_| fallback_str(row, idx)),
    }
}

#[inline]
fn try_cell<T>(
    row: &PgRow,
    idx: usize,
    into_cell: impl FnOnce(T) -> Cell,
) -> Result<Cell, sqlx::Error>
where
    T: Type<Postgres> + for<'r> Decode<'r, Postgres>,
{
    row.try_get::<Option<T>, _>(idx)
        .map(|value| value.map_or(Cell::Null, into_cell))
}

#[inline]
fn decode_or_text<T>(row: &PgRow, idx: usize, into_cell: impl FnOnce(T) -> Cell) -> Cell
where
    T: Type<Postgres> + for<'r> Decode<'r, Postgres>,
{
    try_cell(row, idx, into_cell).unwrap_or_else(|_| fallback_str(row, idx))
}

fn fallback_str(row: &PgRow, idx: usize) -> Cell {
    try_cell(row, idx, Cell::Str).unwrap_or(Cell::Null)
}

/// Decode a PostgreSQL array, retaining NULL elements.
fn decode_pg_array(row: &PgRow, idx: usize, item: &ColumnTypeSpec) -> Cell {
    let typed = match item {
        ColumnTypeSpec::BigInteger => match row.columns()[idx].type_info().oid() {
            Some(OID_ARRAY) => typed_array(row, idx, |values| {
                ArrayValues::U32(map_array(values, |v: Oid| v.0))
            }),
            Some(OID8_ARRAY) => typed_array(row, idx, |values| {
                ArrayValues::U64(map_array(values, |v: PgOid8| v.0))
            }),
            _ => typed_array(row, idx, ArrayValues::I64)
                .or_else(|_| typed_array(row, idx, ArrayValues::I32))
                .or_else(|_| typed_array(row, idx, ArrayValues::I16)),
        },
        ColumnTypeSpec::Timedelta => typed_array(row, idx, ArrayValues::Duration),
        ColumnTypeSpec::Double => typed_array(row, idx, ArrayValues::F64)
            .or_else(|_| typed_array(row, idx, ArrayValues::F32)),
        ColumnTypeSpec::Boolean => typed_array(row, idx, ArrayValues::Bool),
        ColumnTypeSpec::Text | ColumnTypeSpec::String { .. } => {
            typed_array(row, idx, ArrayValues::Str)
        }
        ColumnTypeSpec::Enum { .. } => typed_array(row, idx, |values| {
            ArrayValues::Str(map_array(values, |v: PgEnumText| v.0))
        }),
        ColumnTypeSpec::Uuid => typed_array(row, idx, ArrayValues::Uuid),
        ColumnTypeSpec::Decimal { .. } => typed_array(row, idx, ArrayValues::Decimal),
        ColumnTypeSpec::DateTime if row.columns()[idx].type_info().name() != "TIMESTAMPTZ[]" => {
            typed_array(row, idx, ArrayValues::DateTime)
        }
        ColumnTypeSpec::DateTime | ColumnTypeSpec::DateTimeUtc => {
            typed_array(row, idx, ArrayValues::DateTimeUtc)
        }
        ColumnTypeSpec::Date => typed_array(row, idx, ArrayValues::Date),
        ColumnTypeSpec::Time if row.columns()[idx].type_info().name() == "TIMETZ[]" => {
            typed_array(row, idx, ArrayValues::TimeTz)
        }
        ColumnTypeSpec::Time => typed_array(row, idx, ArrayValues::Time),
        ColumnTypeSpec::Json | ColumnTypeSpec::JsonBinary => {
            typed_array(row, idx, ArrayValues::Json)
        }
        ColumnTypeSpec::Blob => typed_array(row, idx, ArrayValues::Bytes),
        // Unknown / nested element types use the JSON fallback.
        ColumnTypeSpec::Unknown | ColumnTypeSpec::Array { .. } => {
            return decode_or_text(row, idx, Cell::Json);
        }
    };
    typed.unwrap_or(Cell::Null)
}

fn typed_array<T>(
    row: &PgRow,
    idx: usize,
    into_values: impl FnOnce(Vec<Option<T>>) -> ArrayValues,
) -> Result<Cell, sqlx::Error>
where
    Vec<Option<T>>: Type<Postgres> + for<'r> Decode<'r, Postgres>,
{
    try_cell(row, idx, |values| {
        Cell::Array(Box::new(into_values(values)))
    })
}

/// Unwrap decoded newtypes in place when their nullable layouts match.
fn map_array<T, U>(values: Vec<Option<T>>, unwrap: impl Fn(T) -> U) -> Vec<Option<U>> {
    values.into_iter().map(|value| value.map(&unwrap)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::encoder::{encode_stream, write_cell, RelationInfo, ResultOptions};
    use rmpv::Value;
    use std::collections::{BTreeSet, HashMap};

    const DECIMAL: ColumnTypeSpec = ColumnTypeSpec::Decimal {
        precision: None,
        scale: None,
    };

    #[test]
    fn result_kind_matches_supported_specs_and_runtime_types() {
        use ColumnTypeSpec as Spec;

        let typed = [
            (Spec::Uuid, &["UUID"][..], &["UUID[]"][..], "uuid", "uuid[]"),
            (
                DECIMAL,
                &["NUMERIC"],
                &["NUMERIC[]"],
                "decimal",
                "decimal[]",
            ),
            (
                Spec::DateTime,
                &["TIMESTAMP", "TIMESTAMPTZ"],
                &["TIMESTAMP[]", "TIMESTAMPTZ[]"],
                "datetime",
                "datetime[]",
            ),
            (
                Spec::DateTimeUtc,
                &["TIMESTAMPTZ"],
                &["TIMESTAMPTZ[]"],
                "datetime",
                "datetime[]",
            ),
            (Spec::Date, &["DATE"], &["DATE[]"], "date", "date[]"),
            (
                Spec::Time,
                &["TIME", "TIMETZ"],
                &["TIME[]", "TIMETZ[]"],
                "time",
                "time[]",
            ),
            (
                Spec::Timedelta,
                &["INT8"],
                &["INT8[]"],
                "timedelta",
                "timedelta[]",
            ),
        ];
        let runtime_types = [
            "UUID",
            "NUMERIC",
            "DECIMAL",
            "TIMESTAMP",
            "TIMESTAMPTZ",
            "DATE",
            "TIME",
            "INT8",
            "TEXT",
            "CITEXT",
            "TIMETZ",
            "MONEY",
            "UUID_DOMAIN",
            "INT4",
            "JSONB",
            "UUID[]",
            "NUMERIC[]",
            "TIMESTAMP[]",
            "TIMESTAMPTZ[]",
            "DATE[]",
            "TIME[]",
            "INT8[]",
            "INT4[]",
            "TEXT[]",
            "TIMETZ[]",
        ];
        let mut tags = BTreeSet::new();
        for (spec, scalar_types, array_types, scalar_kind, array_kind) in typed {
            let array = Spec::Array {
                item: Box::new(spec.clone()),
            };
            for ty in runtime_types {
                let scalar = pg_result_kind(Some(&spec), ty);
                let array = pg_result_kind(Some(&array), ty);
                assert_eq!(
                    scalar,
                    scalar_types.contains(&ty).then_some(scalar_kind),
                    "{spec:?} on {ty}"
                );
                assert_eq!(
                    array,
                    array_types.contains(&ty).then_some(array_kind),
                    "{spec:?} array on {ty}"
                );
                tags.extend(scalar);
                tags.extend(array);
            }
        }
        for spec in [None, Some(&Spec::Unknown)] {
            for ty in runtime_types {
                let expected = match ty {
                    "UUID" => Some("uuid"),
                    "NUMERIC" | "DECIMAL" => Some("decimal"),
                    "TIMESTAMP" | "TIMESTAMPTZ" => Some("datetime"),
                    "DATE" => Some("date"),
                    "TIME" | "TIMETZ" => Some("time"),
                    "UUID[]" => Some("uuid[]"),
                    "NUMERIC[]" => Some("decimal[]"),
                    "TIMESTAMP[]" | "TIMESTAMPTZ[]" => Some("datetime[]"),
                    "DATE[]" => Some("date[]"),
                    "TIME[]" | "TIMETZ[]" => Some("time[]"),
                    _ => None,
                };
                assert_eq!(pg_result_kind(spec, ty), expected);
            }
        }
        for spec in [Spec::Text, Spec::BigInteger, Spec::JsonBinary] {
            for ty in runtime_types {
                assert_eq!(pg_result_kind(Some(&spec), ty), None);
            }
        }
        assert_eq!(
            tags,
            BTreeSet::from([
                "date",
                "date[]",
                "datetime",
                "datetime[]",
                "decimal",
                "decimal[]",
                "time",
                "time[]",
                "timedelta",
                "timedelta[]",
                "uuid",
                "uuid[]",
            ])
        );
    }

    const NATIVE_ARRAYS_AND_NUMERIC_AGGREGATES: &str = "
            SELECT ARRAY[1::int2, NULL] AS smalls, ARRAY[2::int4] AS ints,
                   ARRAY[3::int8] AS bigs, ARRAY[1.25::float4] AS reals,
                   ARRAY[2.5::float8] AS doubles, ARRAY[true] AS flags,
                   ARRAY['a'::text] AS texts, ARRAY['b'::varchar] AS varchars,
                   total, 7::numeric AS small_total, 500000.5::numeric AS duration, 2.5::numeric AS average,
                   123456789012345678901234567890.25::numeric AS price
            FROM (VALUES (7::numeric), (9223372036854775808::numeric)) AS totals(total)
            ORDER BY total
        ";

    #[test]
    fn newtype_array_maps_reuse_the_source_buffer() {
        fn assert_in_place<T, U: std::fmt::Debug + PartialEq>(
            mut values: Vec<Option<T>>,
            unwrap: impl Fn(T) -> U,
            expected: U,
        ) {
            values.reserve(7);
            let ptr = values.as_ptr().cast::<()>();
            let capacity = values.capacity();
            let mapped = map_array(values, unwrap);
            assert_eq!(mapped.as_ptr().cast::<()>(), ptr);
            assert_eq!(mapped.capacity(), capacity);
            assert_eq!(mapped, [Some(expected), None]);
        }

        assert_in_place(vec![Some(Oid(u32::MAX)), None], |v| v.0, u32::MAX);
        assert_in_place(vec![Some(PgOid8(u64::MAX)), None], |v| v.0, u64::MAX);
        assert_in_place(
            vec![Some(PgEnumText("ready".into())), None],
            |v| v.0,
            String::from("ready"),
        );
    }

    #[tokio::test]
    #[ignore = "requires OXYDE_TEST_POSTGRES_URL"]
    async fn postgres_native_arrays_use_typed_storage() {
        use ColumnTypeSpec as Spec;

        let url = std::env::var("OXYDE_TEST_POSTGRES_URL").unwrap();
        let pool = sqlx::PgPool::connect(&url).await.unwrap();
        let row = sqlx::query(
            r#"SELECT ARRAY[1::int2, NULL] AS smalls, ARRAY[2::int4] AS ints,
                      ARRAY[3::int8] AS bigs, ARRAY[1.25::float4] AS reals,
                      ARRAY[2.5::float8] AS doubles, ARRAY[true] AS flags,
                      ARRAY['4294967295'::oid, NULL] AS oids,
                      ARRAY['a'::text, NULL] AS texts, ARRAY['b'::varchar] AS varchars,
                      ARRAY[decode('00ff', 'hex'), NULL] AS blobs,
                      ARRAY[12.34::numeric, 'NaN', NULL] AS decimals,
                      ARRAY['{"ok":true}'::jsonb, 'null', NULL] AS objects,
                      ARRAY['2026-11-26 13:04:05'::timestamp, 'infinity', NULL] AS stamps,
                      ARRAY['2026-11-26 13:04:05+00'::timestamptz, 'infinity', NULL] AS utc_stamps,
                      ARRAY['2026-11-26'::date, 'infinity', NULL] AS dates,
                      ARRAY['13:04:05'::time, '24:00:00', NULL] AS times,
                      ARRAY['12:30:00+02'::timetz, '24:00:00+00', NULL] AS offset_times,
                      ARRAY[1500000::int8, NULL] AS durations,
                      ARRAY['11111111-2222-4333-8444-555555555555'::uuid, NULL] AS uuids"#,
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let specs = HashMap::from(
            [
                ("blobs", Spec::Blob),
                ("objects", Spec::JsonBinary),
                ("durations", Spec::Timedelta),
            ]
            .map(|(name, item)| {
                (
                    name.into(),
                    Spec::Array {
                        item: Box::new(item),
                    },
                )
            }),
        );
        let cases = [
            (ArrayValues::I16(vec![]), vec![Value::from(1), Value::Nil]),
            (ArrayValues::I32(vec![]), vec![Value::from(2)]),
            (ArrayValues::I64(vec![]), vec![Value::from(3)]),
            (ArrayValues::F32(vec![]), vec![Value::F64(1.25)]),
            (ArrayValues::F64(vec![]), vec![Value::F64(2.5)]),
            (ArrayValues::Bool(vec![]), vec![Value::from(true)]),
            (
                ArrayValues::U32(vec![]),
                vec![Value::from(u32::MAX), Value::Nil],
            ),
            (ArrayValues::Str(vec![]), vec![Value::from("a"), Value::Nil]),
            (ArrayValues::Str(vec![]), vec![Value::from("b")]),
            (
                ArrayValues::Bytes(vec![]),
                vec![Value::Binary(vec![0, 255]), Value::Nil],
            ),
            (
                ArrayValues::Decimal(vec![]),
                vec![Value::from("12.34"), Value::from("NaN"), Value::Nil],
            ),
            (
                ArrayValues::Json(vec![]),
                vec![
                    Value::Map(vec![(Value::from("ok"), Value::from(true))]),
                    Value::Nil,
                    Value::Nil,
                ],
            ),
            (
                ArrayValues::DateTime(vec![]),
                vec![
                    Value::from("2026-11-26T13:04:05"),
                    Value::from("infinity"),
                    Value::Nil,
                ],
            ),
            (
                ArrayValues::DateTimeUtc(vec![]),
                vec![
                    Value::from("2026-11-26T13:04:05+00:00"),
                    Value::from("infinity"),
                    Value::Nil,
                ],
            ),
            (
                ArrayValues::Date(vec![]),
                vec![
                    Value::from("2026-11-26"),
                    Value::from("infinity"),
                    Value::Nil,
                ],
            ),
            (
                ArrayValues::Time(vec![]),
                vec![Value::from("13:04:05"), Value::from("24:00:00"), Value::Nil],
            ),
            (
                ArrayValues::TimeTz(vec![]),
                vec![
                    Value::from("12:30:00+02:00"),
                    Value::from("24:00:00+00:00"),
                    Value::Nil,
                ],
            ),
            (
                ArrayValues::Duration(vec![]),
                vec![Value::F64(1.5), Value::Nil],
            ),
            (
                ArrayValues::Uuid(vec![]),
                vec![
                    Value::from("11111111-2222-4333-8444-555555555555"),
                    Value::Nil,
                ],
            ),
        ];
        let columns = PgEncoder::extract_columns(&row, Some(&specs));
        assert_eq!(columns.len(), cases.len());
        for (idx, (column, (variant, expected))) in columns.iter().zip(cases).enumerate() {
            let (cell, _) = PgEncoder::decode_cell(&row, idx, column);
            let Cell::Array(values) = &cell else {
                panic!("{} decoded as {cell:?}", column.name);
            };
            assert_eq!(
                std::mem::discriminant(values.as_ref()),
                std::mem::discriminant(&variant),
                "{} decoded as {values:?}",
                column.name,
            );
            let mut bytes = Vec::new();
            write_cell(&mut bytes, &cell);
            assert_eq!(
                rmp_serde::from_slice::<Value>(&bytes).unwrap(),
                Value::Array(expected),
                "{}",
                column.name
            );
        }
    }

    #[tokio::test]
    #[ignore = "requires OXYDE_TEST_POSTGRES_URL"]
    async fn postgres_enum_array_uses_typed_storage() {
        use sqlx::{Connection, Executor};

        let url = std::env::var("OXYDE_TEST_POSTGRES_URL").unwrap();
        let mut conn = sqlx::PgConnection::connect(&url).await.unwrap();
        let mut tx = conn.begin().await.unwrap();
        tx.execute("CREATE TYPE oxyde_array_status AS ENUM ('ready', 'done')")
            .await
            .unwrap();
        let row = sqlx::query("SELECT ARRAY['ready'::oxyde_array_status, NULL, 'done'] AS items")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        let specs = HashMap::from([(
            "items".into(),
            ColumnTypeSpec::Array {
                item: Box::new(ColumnTypeSpec::Enum {
                    name: "oxyde_array_status".into(),
                    values: vec!["ready".into(), "done".into()],
                }),
            },
        )]);
        let columns = PgEncoder::extract_columns(&row, Some(&specs));
        let (cell, _) = PgEncoder::decode_cell(&row, 0, &columns[0]);
        assert!(
            matches!(&cell, Cell::Array(values) if matches!(values.as_ref(), ArrayValues::Str(_))),
            "{cell:?}"
        );
        let mut bytes = Vec::new();
        write_cell(&mut bytes, &cell);
        assert_eq!(
            rmp_serde::from_slice::<Value>(&bytes).unwrap(),
            Value::Array(vec![Value::from("ready"), Value::Nil, Value::from("done")]),
        );
        tx.rollback().await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires OXYDE_TEST_POSTGRES_URL"]
    async fn postgres_native_arrays_and_numeric_aggregate_encodings() {
        let url = std::env::var("OXYDE_TEST_POSTGRES_URL").unwrap();
        let pool = sqlx::PgPool::connect(&url).await.unwrap();
        let encodings = HashMap::from([
            ("total".into(), ResultEncoding::IntegerSum),
            ("small_total".into(), ResultEncoding::IntegerSum),
            ("duration".into(), ResultEncoding::DurationMicros),
            ("average".into(), ResultEncoding::Float),
            ("price".into(), ResultEncoding::Decimal),
        ]);
        let (bytes, count) = encode_stream::<PgEncoder, _>(
            sqlx::query(NATIVE_ARRAYS_AND_NUMERIC_AGGREGATES).fetch(&pool),
            None,
            None,
            ResultOptions {
                emit_kinds: true,
                encodings: Some(&encodings),
            },
        )
        .await
        .unwrap();
        assert_eq!(count, 2);
        let result: Vec<Value> = rmp_serde::from_slice(&bytes).unwrap();
        let rows = result[1].as_array().unwrap();
        let mut expected = vec![
            Value::Array(vec![Value::from(1), Value::Nil]),
            Value::Array(vec![Value::from(2)]),
            Value::Array(vec![Value::from(3)]),
            Value::Array(vec![Value::F64(1.25)]),
            Value::Array(vec![Value::F64(2.5)]),
            Value::Array(vec![Value::from(true)]),
            Value::Array(vec![Value::from("a")]),
            Value::Array(vec![Value::from("b")]),
            Value::from(7),
            Value::from(7),
            Value::from("P0DT0.500000S"),
            Value::F64(2.5),
            Value::from("123456789012345678901234567890.2500"),
        ];
        assert_eq!(rows[0].as_array().unwrap(), &expected);
        expected[8] = Value::from("9223372036854775808");
        assert_eq!(rows[1].as_array().unwrap(), &expected);
        assert_eq!(
            result[3].as_array().unwrap(),
            &[
                Value::Nil,
                Value::Nil,
                Value::Nil,
                Value::Nil,
                Value::Nil,
                Value::Nil,
                Value::Nil,
                Value::Nil,
                Value::from("decimal"),
                Value::Nil,
                Value::from("timedelta"),
                Value::Nil,
                Value::from("decimal"),
            ]
        );
    }

    #[test]
    fn duration_iso_keeps_adjacent_large_values_and_negatives() {
        for (micros, expected) in [
            (1 << 53, "P104249DT85654.740992S"),
            ((1 << 53) + 1, "P104249DT85654.740993S"),
            (-((1 << 53) + 1), "-P104249DT85654.740993S"),
            (-1, "-P0DT0.000001S"),
        ] {
            assert_eq!(
                Cell::DurationAggregate(micros)
                    .wire_text()
                    .unwrap()
                    .as_str(),
                expected
            );
        }
    }

    #[tokio::test]
    #[ignore = "requires OXYDE_TEST_POSTGRES_URL"]
    async fn postgres_numeric_aggregate_specials_override_conversion_kinds() {
        let url = std::env::var("OXYDE_TEST_POSTGRES_URL").unwrap();
        let pool = sqlx::PgPool::connect(&url).await.unwrap();
        let sql = "SELECT sum(value) AS total, avg(value) AS average, avg(value) AS duration
                   FROM (VALUES ('Infinity'::numeric), (1::numeric)) AS input(value)";
        let encodings = HashMap::from([
            ("total".into(), ResultEncoding::IntegerSum),
            ("average".into(), ResultEncoding::Float),
            ("duration".into(), ResultEncoding::DurationMicros),
        ]);
        let (bytes, _) = encode_stream::<PgEncoder, _>(
            sqlx::query(sql).fetch(&pool),
            None,
            None,
            ResultOptions {
                emit_kinds: true,
                encodings: Some(&encodings),
            },
        )
        .await
        .unwrap();
        let result: Vec<Value> = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(
            result[1],
            Value::Array(vec![Value::Array(vec![Value::from("Infinity"); 3])])
        );
        assert_eq!(result[3], Value::Array(vec![Value::from("decimal"); 3]));
    }

    #[tokio::test]
    #[ignore = "requires OXYDE_TEST_POSTGRES_URL"]
    async fn postgres_oid8_decodes_when_shadowed_in_search_path() {
        use sqlx::{Connection, Executor};
        use ColumnTypeSpec as Spec;

        let url = std::env::var("OXYDE_TEST_POSTGRES_URL").unwrap();
        // A fresh connection, so SQLx has not cached oid8 under its bare name.
        let mut conn = sqlx::PgConnection::connect(&url).await.unwrap();
        let version: i32 = sqlx::query_scalar("SELECT current_setting('server_version_num')::int4")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        if version < 190_000 {
            return;
        }

        let mut tx = conn.begin().await.unwrap();
        // pg_catalog is searched first unless listed, so put it after the shadow.
        tx.execute(
            "CREATE SCHEMA oxyde_oid8_shadow; \
             CREATE DOMAIN oxyde_oid8_shadow.oid8 AS int8; \
             SET LOCAL search_path = oxyde_oid8_shadow, pg_catalog",
        )
        .await
        .unwrap();

        let sql = "SELECT '18446744073709551615'::pg_catalog.oid8 AS scalar, \
                   ARRAY['18446744073709551615'::pg_catalog.oid8, NULL] AS items, \
                   NULL::pg_catalog.oid8 AS missing";
        let row = sqlx::query(sql).fetch_one(&mut *tx).await.unwrap();
        assert_eq!(row.columns()[0].type_info().name(), "pg_catalog.oid8");
        assert_eq!(row.columns()[1].type_info().name(), "pg_catalog.oid8[]");

        let col_types = HashMap::from([
            ("scalar".into(), Spec::BigInteger),
            (
                "items".into(),
                Spec::Array {
                    item: Box::new(Spec::BigInteger),
                },
            ),
            ("missing".into(), Spec::BigInteger),
        ]);
        for types in [Some(&col_types), None] {
            let columns = PgEncoder::extract_columns(&row, types);
            let (cell, _) = PgEncoder::decode_cell(&row, 1, &columns[1]);
            assert!(
                matches!(&cell, Cell::Array(values) if matches!(values.as_ref(), ArrayValues::U64(_))),
                "{cell:?}"
            );
            let (bytes, count) =
                encode_stream::<PgEncoder, _>(sqlx::query(sql).fetch(&mut *tx), types, None, true)
                    .await
                    .unwrap();
            let result: Vec<Value> = rmp_serde::from_slice(&bytes).unwrap();
            assert_eq!(count, 1);
            assert_eq!(
                result[1],
                Value::Array(vec![Value::Array(vec![
                    Value::from(u64::MAX),
                    Value::Array(vec![Value::from(u64::MAX), Value::Nil]),
                    Value::Nil,
                ])]),
                "types: {types:?}"
            );
        }

        tx.rollback().await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires OXYDE_TEST_POSTGRES_URL"]
    async fn postgres_result_kinds_wire_format() {
        use ColumnTypeSpec as Spec;

        let url = std::env::var("OXYDE_TEST_POSTGRES_URL").unwrap();
        let pool = sqlx::PgPool::connect(&url).await.unwrap();
        let uuid = "11111111-2222-4333-8444-555555555555";
        let cases = [
            (
                Spec::Uuid,
                "uuid",
                "'11111111-2222-4333-8444-555555555555'::uuid",
                Value::from(uuid),
            ),
            (DECIMAL, "decimal", "12.34::numeric", Value::from("12.34")),
            (DECIMAL, "decimal", "'NaN'::numeric", Value::from("NaN")),
            (
                DECIMAL,
                "decimal",
                "'Infinity'::numeric",
                Value::from("Infinity"),
            ),
            (
                DECIMAL,
                "decimal",
                "'-Infinity'::numeric",
                Value::from("-Infinity"),
            ),
            (
                Spec::DateTime,
                "datetime",
                "'infinity'::timestamp",
                Value::from("infinity"),
            ),
            (
                Spec::DateTimeUtc,
                "datetime",
                "'-infinity'::timestamptz",
                Value::from("-infinity"),
            ),
            (
                Spec::DateTime,
                "datetime",
                "'294276-12-31 23:59:58.1234'::timestamp",
                Value::from("294276-12-31 23:59:58.1234"),
            ),
            (
                Spec::Date,
                "date",
                "'-infinity'::date",
                Value::from("-infinity"),
            ),
            (
                Spec::Date,
                "date",
                "'294276-12-31'::date",
                Value::from("294276-12-31"),
            ),
            (
                Spec::Time,
                "time",
                "'24:00:00'::time",
                Value::from("24:00:00"),
            ),
            (
                Spec::Time,
                "time",
                "'24:00:00+02'::timetz",
                Value::from("24:00:00+02:00"),
            ),
            (
                Spec::DateTime,
                "datetime",
                "'2026-11-26 13:04:05'::timestamp",
                Value::from("2026-11-26T13:04:05"),
            ),
            (
                Spec::DateTimeUtc,
                "datetime",
                "'2026-11-26 13:04:05+00'::timestamptz",
                Value::from("2026-11-26T13:04:05+00:00"),
            ),
            (
                Spec::Date,
                "date",
                "'2026-11-26'::date",
                Value::from("2026-11-26"),
            ),
            (
                Spec::Time,
                "time",
                "'13:04:05'::time",
                Value::from("13:04:05"),
            ),
            (
                Spec::Time,
                "time",
                "'12:30:00.000001+02:00'::timetz",
                Value::from("12:30:00.000001+02:00"),
            ),
            (
                Spec::Timedelta,
                "timedelta",
                "1500000::bigint",
                Value::F64(1.5),
            ),
        ];
        let version: i32 = sqlx::query_scalar("SELECT current_setting('server_version_num')::int4")
            .fetch_one(&pool)
            .await
            .unwrap();
        let oid_cases = [
            ("1::oid", Value::from(1u64)),
            ("'4294967295'::oid", Value::from(u64::from(u32::MAX))),
            ("NULL::oid", Value::Nil),
        ];
        let oid8_case =
            (version >= 190_000).then(|| ("'18446744073709551615'::oid8", Value::from(u64::MAX)));
        let cases = cases
            .into_iter()
            .map(|(spec, kind, literal, value)| (spec, Some(kind), literal, value))
            .chain(
                oid_cases
                    .into_iter()
                    .chain(oid8_case)
                    .map(|(literal, value)| (Spec::BigInteger, None, literal, value)),
            );
        for (spec, kind, literal, value) in cases {
            let sql = format!("SELECT {literal} AS scalar, ARRAY[{literal}, NULL] AS items");
            let col_types = HashMap::from([
                ("scalar".into(), spec.clone()),
                (
                    "items".into(),
                    Spec::Array {
                        item: Box::new(spec),
                    },
                ),
            ]);
            for types in [Some(&col_types), None] {
                if types.is_none() && kind == Some("timedelta") {
                    continue;
                }
                let (bytes, count) = encode_stream::<PgEncoder, _>(
                    sqlx::query(sqlx::AssertSqlSafe(sql.as_str())).fetch(&pool),
                    types,
                    None,
                    true,
                )
                .await
                .unwrap();
                let result: Vec<Value> = rmp_serde::from_slice(&bytes).unwrap();
                assert_eq!(count, 1);
                assert_eq!(
                    result,
                    [
                        Value::Array(vec![Value::from("scalar"), Value::from("items")]),
                        Value::Array(vec![Value::Array(vec![
                            value.clone(),
                            Value::Array(vec![value.clone(), Value::Nil])
                        ])]),
                        Value::Nil,
                        kind.map_or(Value::Nil, |kind| Value::Array(vec![
                            Value::from(kind),
                            Value::from(format!("{kind}[]"))
                        ])),
                    ]
                );
            }
        }

        let sql = "SELECT 42 AS main, '11111111-2222-4333-8444-555555555555'::uuid AS related_id, \
                   1250000::bigint AS related_duration FROM generate_series(1, 2)";
        let relations = [RelationInfo {
            prefix: "related".into(),
            pk_index: 0,
            field_names: vec!["id".into(), "duration".into()],
            column_types: vec![None, Some(Spec::Timedelta)],
            text_columns: std::collections::HashSet::new(),
        }];
        let (bytes, count) = encode_stream::<PgEncoder, _>(
            sqlx::query(sql).fetch(&pool),
            None,
            Some(&relations),
            true,
        )
        .await
        .unwrap();
        let result: Vec<Value> = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(count, 2);
        assert_eq!(result[3], Value::Nil);
        let relation = Value::Map(vec![
            (
                Value::from("columns"),
                Value::Array(vec![Value::from("id"), Value::from("duration")]),
            ),
            (
                Value::from("data"),
                Value::Map(vec![(
                    Value::from(uuid),
                    Value::Array(vec![Value::from(uuid), Value::F64(1.25)]),
                )]),
            ),
            (
                Value::from("refs"),
                Value::Array(vec![Value::from(uuid); 2]),
            ),
        ]);
        let mut tagged_relation = relation.as_map().unwrap().clone();
        tagged_relation.push((
            Value::from("kinds"),
            Value::Array(vec![Value::from("uuid"), Value::from("timedelta")]),
        ));
        assert_eq!(
            result[2],
            Value::Map(vec![(Value::from("related"), Value::Map(tagged_relation))])
        );

        let (bytes, _) = encode_stream::<PgEncoder, _>(
            sqlx::query(sql).fetch(&pool),
            None,
            Some(&relations),
            false,
        )
        .await
        .unwrap();
        let legacy = vec![
            result[0].clone(),
            result[1].clone(),
            Value::Map(vec![(Value::from("related"), relation)]),
        ];
        assert_eq!(bytes, rmp_serde::to_vec(&legacy).unwrap());

        for (relations, legacy, tagged) in [
            (
                None,
                &[0x92, 0x90, 0x90][..],
                &[0x94, 0x90, 0x90, 0xc0, 0xc0][..],
            ),
            (
                Some(relations.as_slice()),
                &[0x93, 0x90, 0x90, 0x80][..],
                &[0x94, 0x90, 0x90, 0x80, 0xc0][..],
            ),
        ] {
            for (emit_kinds, expected) in [(false, legacy), (true, tagged)] {
                let (bytes, count) = encode_stream::<PgEncoder, _>(
                    sqlx::query("SELECT now() WHERE false").fetch(&pool),
                    None,
                    relations,
                    emit_kinds,
                )
                .await
                .unwrap();
                assert_eq!(count, 0);
                assert_eq!(bytes, expected);
            }
        }
    }
}
