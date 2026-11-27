//! PostgreSQL CellEncoder implementation.
//!
//! Encodes PostgreSQL row cells directly to msgpack bytes, dispatched by
//! `ColumnTypeSpec`. Error-handling per arm (nil vs string-fallback) is
//! ported verbatim from the legacy string-hint implementation.

use bigdecimal::{BigDecimal, RoundingMode, ToPrimitive};
use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use oxyde_codec::ColumnTypeSpec;
use rust_decimal::Decimal;
use sqlx::{
    error::BoxDynError,
    postgres::{types::Oid, PgHasArrayType, PgRow, PgTypeInfo, PgTypeKind, PgValueRef, Postgres},
    Column, Decode, Row, Type, TypeInfo,
};
use uuid::Uuid;

use super::encoder::{
    write_array_len, write_bin, write_bool, write_f64, write_i64, write_json_value, write_nil,
    write_str, write_u64, CellEncoder, ColumnMeta, ResultEncoding,
};

use super::postgres_types::{
    PgChronoTimeTz, PgNumeric, PgOid8, PgTemporal, OID, OID8, OID8_ARRAY, OID_ARRAY,
};

pub struct PgEncoder;

#[derive(Debug, Clone)]
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

    fn encode_cell(
        buf: &mut Vec<u8>,
        row: &PgRow,
        idx: usize,
        col: &ColumnMeta,
    ) -> Option<&'static str> {
        if col.result_encoding == Some(ResultEncoding::Runtime) {
            Self::encode_by_db_type(buf, row, idx, &col.db_type);
            return None;
        }
        if let Some(encoding) = col.result_encoding.filter(|encoding| {
            col.db_type == "NUMERIC" && !matches!(encoding, ResultEncoding::Wire)
        }) {
            return encode_aggregate_numeric(buf, row, idx, encoding);
        } else if !col
            .spec
            .as_ref()
            .is_some_and(|spec| Self::try_encode_by_spec(buf, row, idx, spec))
        {
            Self::encode_by_db_type(buf, row, idx, &col.db_type);
        }
        None
    }

    fn try_encode_by_spec(
        buf: &mut Vec<u8>,
        row: &PgRow,
        idx: usize,
        spec: &ColumnTypeSpec,
    ) -> bool {
        match spec {
            ColumnTypeSpec::BigInteger => {
                encode_pg_int(buf, row, idx);
                true
            }
            ColumnTypeSpec::Text | ColumnTypeSpec::String { .. } => {
                match row.try_get::<Option<String>, _>(idx) {
                    Ok(Some(v)) => write_str(buf, &v),
                    Ok(None) => write_nil(buf),
                    Err(_) => write_nil(buf),
                }
                true
            }
            ColumnTypeSpec::Enum { .. } => {
                match row.try_get::<Option<PgEnumText>, _>(idx) {
                    Ok(Some(v)) => write_str(buf, &v.0),
                    Ok(None) => write_nil(buf),
                    Err(_) => write_nil(buf),
                }
                true
            }
            ColumnTypeSpec::Double => {
                match row.try_get::<Option<f64>, _>(idx) {
                    Ok(Some(v)) => write_f64(buf, v),
                    Ok(None) => write_nil(buf),
                    Err(_) => fallback_str(buf, row, idx),
                }
                true
            }
            ColumnTypeSpec::Boolean => {
                match row.try_get::<Option<bool>, _>(idx) {
                    Ok(Some(v)) => write_bool(buf, v),
                    Ok(None) => write_nil(buf),
                    Err(_) => fallback_str(buf, row, idx),
                }
                true
            }
            ColumnTypeSpec::Blob => {
                match row.try_get::<Option<Vec<u8>>, _>(idx) {
                    Ok(Some(v)) => write_bin(buf, &v),
                    Ok(None) => write_nil(buf),
                    Err(_) => fallback_str(buf, row, idx),
                }
                true
            }
            ColumnTypeSpec::DateTime => {
                if row.columns()[idx].type_info().name() == "TIMESTAMPTZ" {
                    encode_pg_temporal::<DateTime<Utc>>(buf, row, idx);
                } else {
                    encode_pg_temporal::<NaiveDateTime>(buf, row, idx);
                }
                true
            }
            ColumnTypeSpec::DateTimeUtc => {
                encode_pg_temporal::<DateTime<Utc>>(buf, row, idx);
                true
            }
            ColumnTypeSpec::Date => {
                encode_pg_temporal::<NaiveDate>(buf, row, idx);
                true
            }
            ColumnTypeSpec::Time => {
                encode_pg_time(
                    buf,
                    row,
                    idx,
                    row.columns()[idx].type_info().name() == "TIMETZ",
                );
                true
            }
            ColumnTypeSpec::Uuid => {
                match row.try_get::<Option<Uuid>, _>(idx) {
                    Ok(Some(v)) => write_str(buf, &v.to_string()),
                    Ok(None) => write_nil(buf),
                    Err(_) => fallback_str(buf, row, idx),
                }
                true
            }
            ColumnTypeSpec::Decimal { .. } => {
                encode_pg_numeric(buf, row, idx);
                true
            }
            ColumnTypeSpec::Timedelta => {
                match row.try_get::<Option<i64>, _>(idx) {
                    Ok(Some(v)) => write_f64(buf, v as f64 / 1_000_000.0),
                    Ok(None) => write_nil(buf),
                    Err(_) => write_nil(buf),
                }
                true
            }
            ColumnTypeSpec::Json | ColumnTypeSpec::JsonBinary => {
                match row.try_get::<Option<serde_json::Value>, _>(idx) {
                    Ok(Some(v)) => write_json_value(buf, &v),
                    Ok(None) => write_nil(buf),
                    Err(_) => fallback_str(buf, row, idx),
                }
                true
            }
            ColumnTypeSpec::Array { item } => {
                encode_pg_array(buf, row, idx, item);
                true
            }
            ColumnTypeSpec::Unknown => false,
        }
    }

    fn encode_by_db_type(buf: &mut Vec<u8>, row: &PgRow, idx: usize, db_type: &str) {
        // oid8's display name depends on search_path; dispatch it by catalog OID.
        match row.columns()[idx].type_info().oid() {
            Some(OID8) => return encode_pg_int(buf, row, idx),
            Some(OID8_ARRAY) => {
                return encode_pg_array(buf, row, idx, &ColumnTypeSpec::BigInteger);
            }
            _ => {}
        }
        if let Some(item) = pg_array_item(db_type) {
            encode_pg_array(buf, row, idx, &item);
            return;
        }
        match db_type {
            "BOOL" | "BOOLEAN" => match row.try_get::<Option<bool>, _>(idx) {
                Ok(Some(v)) => write_bool(buf, v),
                Ok(None) => write_nil(buf),
                Err(_) => fallback_str(buf, row, idx),
            },
            "OID" => encode_pg_int(buf, row, idx),
            name if name.contains("INT") => encode_pg_int(buf, row, idx),
            "FLOAT4" => match row.try_get::<Option<f32>, _>(idx) {
                Ok(Some(v)) => write_f64(buf, f64::from(v)),
                Ok(None) => write_nil(buf),
                Err(_) => fallback_str(buf, row, idx),
            },
            name if name.contains("FLOAT") || name.contains("DOUBLE") || name.contains("REAL") => {
                match row.try_get::<Option<f64>, _>(idx) {
                    Ok(Some(v)) => write_f64(buf, v),
                    Ok(None) => write_nil(buf),
                    Err(_) => fallback_str(buf, row, idx),
                }
            }
            "NUMERIC" | "DECIMAL" => encode_pg_numeric(buf, row, idx),
            "UUID" => match row.try_get::<Option<Uuid>, _>(idx) {
                Ok(Some(v)) => write_str(buf, &v.to_string()),
                Ok(None) => write_nil(buf),
                Err(_) => fallback_str(buf, row, idx),
            },
            name if name == "JSON" || name == "JSONB" => {
                match row.try_get::<Option<serde_json::Value>, _>(idx) {
                    Ok(Some(v)) => write_json_value(buf, &v),
                    Ok(None) => write_nil(buf),
                    Err(_) => fallback_str(buf, row, idx),
                }
            }
            "TIMESTAMPTZ" => encode_pg_temporal::<DateTime<Utc>>(buf, row, idx),
            "TIMESTAMP" => encode_pg_temporal::<NaiveDateTime>(buf, row, idx),
            "DATE" => encode_pg_temporal::<NaiveDate>(buf, row, idx),
            "TIME" | "TIMETZ" => encode_pg_time(buf, row, idx, db_type == "TIMETZ"),
            "BYTEA" => match row.try_get::<Option<Vec<u8>>, _>(idx) {
                Ok(Some(v)) => write_bin(buf, &v),
                Ok(None) => write_nil(buf),
                Err(_) => fallback_str(buf, row, idx),
            },
            _ => match row.try_get::<Option<String>, _>(idx) {
                Ok(Some(v)) => write_str(buf, &v),
                Ok(None) => write_nil(buf),
                Err(_) => fallback_str(buf, row, idx),
            },
        }
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

fn write_pg_temporal<T>(buf: &mut Vec<u8>, value: &PgTemporal<T>) {
    write_str(buf, &value.0);
}

fn encode_pg_temporal<T>(buf: &mut Vec<u8>, row: &PgRow, idx: usize)
where
    PgTemporal<T>: Type<Postgres> + for<'r> Decode<'r, Postgres>,
{
    match row.try_get::<Option<PgTemporal<T>>, _>(idx) {
        Ok(Some(value)) => write_pg_temporal(buf, &value),
        Ok(None) => write_nil(buf),
        Err(_) => fallback_str(buf, row, idx),
    }
}

fn encode_pg_time(buf: &mut Vec<u8>, row: &PgRow, idx: usize, with_offset: bool) {
    if with_offset {
        encode_pg_temporal::<PgChronoTimeTz>(buf, row, idx);
    } else {
        encode_pg_temporal::<NaiveTime>(buf, row, idx);
    }
}

fn write_pg_numeric(buf: &mut Vec<u8>, value: &PgNumeric<Decimal>) {
    match value {
        PgNumeric::Finite(value) => write_str(buf, &value.to_string()),
        PgNumeric::Special(text) => write_str(buf, text),
    }
}

fn encode_pg_numeric(buf: &mut Vec<u8>, row: &PgRow, idx: usize) {
    match row.try_get::<Option<PgNumeric<Decimal>>, _>(idx) {
        Ok(Some(value)) => write_pg_numeric(buf, &value),
        Ok(None) => write_nil(buf),
        Err(_) => fallback_str(buf, row, idx),
    }
}

fn encode_aggregate_numeric(
    buf: &mut Vec<u8>,
    row: &PgRow,
    idx: usize,
    encoding: ResultEncoding,
) -> Option<&'static str> {
    let value = match row.try_get::<Option<PgNumeric<BigDecimal>>, _>(idx) {
        Ok(Some(PgNumeric::Finite(value))) => value,
        Ok(Some(PgNumeric::Special(text))) => {
            write_str(buf, text);
            return Some("decimal");
        }
        Ok(None) => {
            write_nil(buf);
            return None;
        }
        Err(_) => {
            fallback_str(buf, row, idx);
            return None;
        }
    };
    match encoding {
        ResultEncoding::IntegerSum => {
            if let Some(value) = value.to_i64() {
                write_i64(buf, value);
            } else {
                write_str(buf, &value.to_string());
                return Some("decimal");
            }
        }
        ResultEncoding::DurationMicros => {
            // A Python timedelta has whole microseconds, rounded half to even.
            if let Some(micros) = value.with_scale_round(0, RoundingMode::HalfEven).to_i128() {
                write_str(buf, &duration_iso(micros));
            } else {
                write_str(buf, &value.to_string());
            }
        }
        ResultEncoding::Float => {
            if let Some(value) = value.to_f64() {
                write_f64(buf, value);
            } else {
                write_str(buf, &value.to_string());
            }
        }
        ResultEncoding::Decimal => write_str(buf, &value.to_string()),
        ResultEncoding::Wire | ResultEncoding::Runtime => {
            unreachable!("these encodings do not specialize the numeric result");
        }
    }
    None
}

fn duration_iso(micros: i128) -> String {
    let sign = if micros < 0 { "-" } else { "" };
    let micros = micros.unsigned_abs();
    let seconds = micros / 1_000_000;
    let days = seconds / 86_400;
    let seconds = seconds % 86_400;
    let fraction = micros % 1_000_000;
    format!("{sign}P{days}DT{seconds}.{fraction:06}S")
}

fn encode_pg_int(buf: &mut Vec<u8>, row: &PgRow, idx: usize) {
    let value = match row.columns()[idx].type_info().oid() {
        Some(OID) => row
            .try_get::<Option<Oid>, _>(idx)
            .map(|v| v.map(|oid| u64::from(oid.0))),
        Some(OID8) => row
            .try_get::<Option<PgOid8>, _>(idx)
            .map(|v| v.map(|oid| oid.0)),
        _ => return encode_pg_signed_int(buf, row, idx),
    };
    match value {
        Ok(Some(v)) => write_u64(buf, v),
        Ok(None) => write_nil(buf),
        Err(_) => fallback_str(buf, row, idx),
    }
}

fn encode_pg_signed_int(buf: &mut Vec<u8>, row: &PgRow, idx: usize) {
    match row.try_get::<Option<i64>, _>(idx) {
        Ok(Some(v)) => write_i64(buf, v),
        Ok(None) => write_nil(buf),
        Err(_) => match row.try_get::<Option<i32>, _>(idx) {
            Ok(Some(v)) => write_i64(buf, i64::from(v)),
            Ok(None) => write_nil(buf),
            Err(_) => match row.try_get::<Option<i16>, _>(idx) {
                Ok(Some(v)) => write_i64(buf, i64::from(v)),
                Ok(None) => write_nil(buf),
                Err(_) => fallback_str(buf, row, idx),
            },
        },
    }
}

fn fallback_str(buf: &mut Vec<u8>, row: &PgRow, idx: usize) {
    match row.try_get::<Option<String>, _>(idx) {
        Ok(Some(v)) => write_str(buf, &v),
        Ok(None) | Err(_) => write_nil(buf),
    }
}

/// Encode a native PostgreSQL array column to a msgpack array, dispatched by
/// the element spec. Uses `Vec<Option<T>>` to handle NULL elements.
fn encode_pg_array(buf: &mut Vec<u8>, row: &PgRow, idx: usize, item: &ColumnTypeSpec) {
    match item {
        ColumnTypeSpec::BigInteger => {
            match row.columns()[idx].type_info().oid() {
                Some(OID_ARRAY) => {
                    return encode_pg_array_opt::<Oid>(buf, row, idx, |b, v| {
                        write_u64(b, u64::from(v.0));
                    })
                }
                Some(OID8_ARRAY) => {
                    return encode_pg_array_opt::<PgOid8>(buf, row, idx, |b, v| write_u64(b, v.0))
                }
                _ => {}
            }
            // Try i64 (BIGINT[]), then i32 (INTEGER[]), then i16 (SMALLINT[]).
            if !try_encode_pg_array_opt::<i64>(buf, row, idx, write_i64)
                && !try_encode_pg_array_opt::<i32>(buf, row, idx, |b, v| {
                    write_i64(b, i64::from(v));
                })
            {
                encode_pg_array_opt::<i16>(buf, row, idx, |b, v| write_i64(b, i64::from(v)));
            }
        }
        ColumnTypeSpec::Timedelta => {
            encode_pg_array_opt::<i64>(buf, row, idx, |b, v| write_f64(b, v as f64 / 1_000_000.0));
        }
        ColumnTypeSpec::Double => {
            if !try_encode_pg_array_opt::<f64>(buf, row, idx, write_f64) {
                encode_pg_array_opt::<f32>(buf, row, idx, |b, v| write_f64(b, f64::from(v)));
            }
        }
        ColumnTypeSpec::Boolean => {
            encode_pg_array_opt::<bool>(buf, row, idx, write_bool);
        }
        ColumnTypeSpec::Text | ColumnTypeSpec::String { .. } => {
            encode_pg_array_ref::<String>(buf, row, idx, |b, v| write_str(b, v));
        }
        ColumnTypeSpec::Enum { .. } => {
            encode_pg_array_ref::<PgEnumText>(buf, row, idx, |b, v| write_str(b, &v.0));
        }
        ColumnTypeSpec::Uuid => {
            encode_pg_array_ref::<Uuid>(buf, row, idx, |b, v| {
                write_str(b, &v.to_string());
            });
        }
        ColumnTypeSpec::Decimal { .. } => {
            encode_pg_array_ref::<PgNumeric<Decimal>>(buf, row, idx, write_pg_numeric);
        }
        ColumnTypeSpec::DateTime if row.columns()[idx].type_info().name() == "TIMESTAMPTZ[]" => {
            encode_pg_array_ref::<PgTemporal<DateTime<Utc>>>(buf, row, idx, write_pg_temporal);
        }
        ColumnTypeSpec::DateTime => {
            encode_pg_array_ref::<PgTemporal<NaiveDateTime>>(buf, row, idx, write_pg_temporal);
        }
        ColumnTypeSpec::DateTimeUtc => {
            encode_pg_array_ref::<PgTemporal<DateTime<Utc>>>(buf, row, idx, write_pg_temporal);
        }
        ColumnTypeSpec::Date => {
            encode_pg_array_ref::<PgTemporal<NaiveDate>>(buf, row, idx, write_pg_temporal);
        }
        ColumnTypeSpec::Time if row.columns()[idx].type_info().name() == "TIMETZ[]" => {
            encode_pg_array_ref::<PgTemporal<PgChronoTimeTz>>(buf, row, idx, write_pg_temporal);
        }
        ColumnTypeSpec::Time => {
            encode_pg_array_ref::<PgTemporal<NaiveTime>>(buf, row, idx, write_pg_temporal);
        }
        ColumnTypeSpec::Json | ColumnTypeSpec::JsonBinary => {
            encode_pg_array_ref::<serde_json::Value>(buf, row, idx, write_json_value);
        }
        ColumnTypeSpec::Blob => {
            encode_pg_array_ref::<Vec<u8>>(buf, row, idx, |b, v| write_bin(b, v));
        }
        // Unknown / nested element types — JSON array fallback.
        ColumnTypeSpec::Unknown | ColumnTypeSpec::Array { .. } => {
            match row.try_get::<Option<serde_json::Value>, _>(idx) {
                Ok(Some(v)) => write_json_value(buf, &v),
                Ok(None) => write_nil(buf),
                Err(_) => fallback_str(buf, row, idx),
            }
        }
    }
}

/// Try to encode `Vec<Option<T>>` where T is Copy. Returns false on type mismatch.
fn try_encode_pg_array_opt<T>(
    buf: &mut Vec<u8>,
    row: &PgRow,
    idx: usize,
    write_fn: impl Fn(&mut Vec<u8>, T),
) -> bool
where
    T: sqlx::Type<sqlx::Postgres> + for<'r> sqlx::Decode<'r, sqlx::Postgres> + Copy,
    Vec<Option<T>>: sqlx::Type<sqlx::Postgres> + for<'r> sqlx::Decode<'r, sqlx::Postgres>,
{
    match row.try_get::<Option<Vec<Option<T>>>, _>(idx) {
        Ok(Some(v)) => {
            write_array_len(buf, v.len() as u32);
            for item in &v {
                match item {
                    Some(val) => write_fn(buf, *val),
                    None => write_nil(buf),
                }
            }
            true
        }
        Ok(None) => {
            write_nil(buf);
            true
        }
        Err(_) => false,
    }
}

/// Helper: encode `Vec<Option<T>>` where T is Copy (i64, f64, bool).
/// Handles NULL elements within arrays.
fn encode_pg_array_opt<T>(
    buf: &mut Vec<u8>,
    row: &PgRow,
    idx: usize,
    write_fn: impl Fn(&mut Vec<u8>, T),
) where
    T: sqlx::Type<sqlx::Postgres> + for<'r> sqlx::Decode<'r, sqlx::Postgres> + Copy,
    Vec<Option<T>>: sqlx::Type<sqlx::Postgres> + for<'r> sqlx::Decode<'r, sqlx::Postgres>,
{
    match row.try_get::<Option<Vec<Option<T>>>, _>(idx) {
        Ok(Some(v)) => {
            write_array_len(buf, v.len() as u32);
            for item in &v {
                match item {
                    Some(val) => write_fn(buf, *val),
                    None => write_nil(buf),
                }
            }
        }
        Ok(None) => write_nil(buf),
        Err(_) => write_nil(buf),
    }
}

/// Helper: encode `Vec<Option<T>>` where T is not Copy (String, Uuid, etc.).
/// Handles NULL elements within arrays.
fn encode_pg_array_ref<T>(
    buf: &mut Vec<u8>,
    row: &PgRow,
    idx: usize,
    write_fn: impl Fn(&mut Vec<u8>, &T),
) where
    T: sqlx::Type<sqlx::Postgres> + for<'r> sqlx::Decode<'r, sqlx::Postgres>,
    Vec<Option<T>>: sqlx::Type<sqlx::Postgres> + for<'r> sqlx::Decode<'r, sqlx::Postgres>,
{
    match row.try_get::<Option<Vec<Option<T>>>, _>(idx) {
        Ok(Some(v)) => {
            write_array_len(buf, v.len() as u32);
            for item in &v {
                match item {
                    Some(val) => write_fn(buf, val),
                    None => write_nil(buf),
                }
            }
        }
        Ok(None) => write_nil(buf),
        Err(_) => write_nil(buf),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::encoder::{encode_stream, RelationInfo, ResultOptions};
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

    #[tokio::test]
    #[ignore = "requires OXYDE_TEST_POSTGRES_URL"]
    async fn postgres_native_arrays_and_numeric_aggregate_encodings() {
        let url = std::env::var("OXYDE_TEST_POSTGRES_URL").unwrap();
        let pool = sqlx::PgPool::connect(&url).await.unwrap();
        let sql = "
            SELECT ARRAY[1::int2, NULL] AS smalls, ARRAY[2::int4] AS ints,
                   ARRAY[3::int8] AS bigs, ARRAY[1.25::float4] AS reals,
                   ARRAY[2.5::float8] AS doubles, ARRAY[true] AS flags,
                   ARRAY['a'::text] AS texts, ARRAY['b'::varchar] AS varchars,
                   total, 7::numeric AS small_total, 500000.5::numeric AS duration, 2.5::numeric AS average,
                   123456789012345678901234567890.25::numeric AS price
            FROM (VALUES (7::numeric), (9223372036854775808::numeric)) AS totals(total)
            ORDER BY total
        ";
        let encodings = HashMap::from([
            ("total".into(), ResultEncoding::IntegerSum),
            ("small_total".into(), ResultEncoding::IntegerSum),
            ("duration".into(), ResultEncoding::DurationMicros),
            ("average".into(), ResultEncoding::Float),
            ("price".into(), ResultEncoding::Decimal),
        ]);
        let (bytes, count) = encode_stream::<PgEncoder, _>(
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
        assert_eq!(duration_iso(1 << 53), "P104249DT85654.740992S");
        assert_eq!(duration_iso((1 << 53) + 1), "P104249DT85654.740993S");
        assert_eq!(duration_iso(-((1 << 53) + 1)), "-P104249DT85654.740993S");
        assert_eq!(duration_iso(-1), "-P0DT0.000001S");
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
