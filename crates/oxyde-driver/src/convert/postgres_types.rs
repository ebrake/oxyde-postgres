//! PostgreSQL values whose wire range exceeds the corresponding Rust type.

use std::marker::PhantomData;

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use sqlx::{
    error::BoxDynError,
    postgres::{
        types::{Oid, PgTimeTz},
        PgHasArrayType, PgTypeInfo, PgValueFormat, PgValueRef, Postgres,
    },
    Decode, Type,
};

const MICROS_PER_SECOND: i64 = 1_000_000;
const MICROS_PER_DAY: i64 = 86_400 * MICROS_PER_SECOND;

pub(super) type PgChronoTimeTz = PgTimeTz<NaiveTime, FixedOffset>;

// Built-in catalog OIDs from PostgreSQL's `pg_type.dat`. SQLx names types it
// does not know (such as `oid8`) via `regtype::text`, which qualifies the name
// as `pg_catalog.oid8` when `search_path` shadows it, so dispatch by OID.
pub(super) const OID: Oid = Oid(26);
pub(super) const OID_ARRAY: Oid = Oid(1028);
pub(super) const OID8: Oid = Oid(6437);
pub(super) const OID8_ARRAY: Oid = Oid(6442);

#[derive(Clone, Copy)]
pub(super) struct PgOid8(pub u64);

impl Type<Postgres> for PgOid8 {
    fn type_info() -> PgTypeInfo {
        PgTypeInfo::with_oid(OID8)
    }

    fn compatible(ty: &PgTypeInfo) -> bool {
        ty.oid() == Some(OID8)
    }
}

impl PgHasArrayType for PgOid8 {
    fn array_type_info() -> PgTypeInfo {
        PgTypeInfo::with_oid(OID8_ARRAY)
    }

    fn array_compatible(ty: &PgTypeInfo) -> bool {
        ty.oid() == Some(OID8_ARRAY)
    }
}

impl Decode<'_, Postgres> for PgOid8 {
    fn decode(value: PgValueRef<'_>) -> Result<Self, BoxDynError> {
        Ok(Self(match value.format() {
            PgValueFormat::Binary => u64::from_be_bytes(value.as_bytes()?.try_into()?),
            PgValueFormat::Text => value.as_str()?.parse()?,
        }))
    }
}

pub(super) struct PgTemporal<T>(pub String, PhantomData<T>);

impl<T: Type<Postgres>> Type<Postgres> for PgTemporal<T> {
    fn type_info() -> PgTypeInfo {
        T::type_info()
    }
}

impl<T: PgHasArrayType> PgHasArrayType for PgTemporal<T> {
    fn array_type_info() -> PgTypeInfo {
        T::array_type_info()
    }
}

impl<'r, T: Decode<'r, Postgres> + TemporalWire> Decode<'r, Postgres> for PgTemporal<T> {
    fn decode(value: PgValueRef<'r>) -> Result<Self, BoxDynError> {
        // SQLx 0.9.1 checks chrono's bounds. Preserve the PostgreSQL spelling
        // when the ordinary decoder rejects a valid value outside those bounds.
        let text = match T::decode(value.clone()) {
            Ok(value) => value.wire_text(),
            Err(_) if value.format() == PgValueFormat::Text => value.as_str()?.to_owned(),
            Err(error) => T::binary_fallback(value.as_bytes()?)?.ok_or(error)?,
        };
        Ok(Self(text, PhantomData))
    }
}

trait TemporalWire {
    fn wire_text(self) -> String;
    fn binary_fallback(bytes: &[u8]) -> Result<Option<String>, BoxDynError>;
}

impl TemporalWire for NaiveDateTime {
    fn wire_text(self) -> String {
        self.format("%Y-%m-%dT%H:%M:%S%.f").to_string()
    }

    fn binary_fallback(bytes: &[u8]) -> Result<Option<String>, BoxDynError> {
        Ok(Some(timestamp_fallback(
            i64::from_be_bytes(bytes.try_into()?),
            "",
        )))
    }
}

impl TemporalWire for DateTime<Utc> {
    fn wire_text(self) -> String {
        self.to_rfc3339()
    }

    fn binary_fallback(bytes: &[u8]) -> Result<Option<String>, BoxDynError> {
        Ok(Some(timestamp_fallback(
            i64::from_be_bytes(bytes.try_into()?),
            "+00",
        )))
    }
}

impl TemporalWire for NaiveDate {
    fn wire_text(self) -> String {
        self.format("%Y-%m-%d").to_string()
    }

    fn binary_fallback(bytes: &[u8]) -> Result<Option<String>, BoxDynError> {
        let days = i32::from_be_bytes(bytes.try_into()?);
        Ok(Some(match days {
            i32::MAX => "infinity".into(),
            i32::MIN => "-infinity".into(),
            days => {
                let (date, era) = format_pg_date(i64::from(days));
                date + era
            }
        }))
    }
}

impl TemporalWire for NaiveTime {
    fn wire_text(self) -> String {
        self.format("%H:%M:%S%.f").to_string()
    }

    fn binary_fallback(bytes: &[u8]) -> Result<Option<String>, BoxDynError> {
        Ok((i64::from_be_bytes(bytes.try_into()?) == MICROS_PER_DAY).then(|| "24:00:00".into()))
    }
}

impl TemporalWire for PgChronoTimeTz {
    fn wire_text(self) -> String {
        format!("{}{}", self.time.wire_text(), self.offset)
    }

    fn binary_fallback(bytes: &[u8]) -> Result<Option<String>, BoxDynError> {
        let Some((micros, offset)) = bytes.split_first_chunk::<8>() else {
            return Ok(None);
        };
        if i64::from_be_bytes(*micros) != MICROS_PER_DAY {
            return Ok(None);
        }
        let offset = FixedOffset::west_opt(i32::from_be_bytes(offset.try_into()?));
        Ok(offset.map(|offset| format!("24:00:00{offset}")))
    }
}

fn timestamp_fallback(micros: i64, offset: &str) -> String {
    match micros {
        i64::MAX => "infinity".into(),
        i64::MIN => "-infinity".into(),
        _ => {
            let (date, era) = format_pg_date(micros.div_euclid(MICROS_PER_DAY));
            let within_day = micros.rem_euclid(MICROS_PER_DAY);
            let seconds = within_day / MICROS_PER_SECOND;
            let hours = seconds / 3600;
            let minutes = seconds / 60 % 60;
            let seconds = seconds % 60;
            let fraction = within_day % MICROS_PER_SECOND;
            let fractional = if fraction == 0 {
                String::new()
            } else {
                format!(".{fraction:06}").trim_end_matches('0').to_owned()
            };
            format!("{date} {hours:02}:{minutes:02}:{seconds:02}{fractional}{offset}{era}")
        }
    }
}

/// Convert PostgreSQL days since 2000-01-01 to a proleptic Gregorian date.
/// Civil-from-days uses 400-year eras starting on March 1, with 146097 days
/// per era. The offset shifts PostgreSQL's epoch to astronomical 0000-03-01.
fn format_pg_date(days: i64) -> (String, &'static str) {
    let days = days + 730_425;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let march_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * march_month + 2) / 5 + 1;
    let month = if march_month < 10 {
        march_month + 3
    } else {
        march_month - 9
    };
    let year = era * 400 + year_of_era + i64::from(month <= 2);
    if year > 0 {
        (format!("{year:04}-{month:02}-{day:02}"), "")
    } else {
        (format!("{:04}-{month:02}-{day:02}", 1 - year), " BC")
    }
}

pub(super) enum PgNumeric<T> {
    Finite(T),
    Special(&'static str),
}

impl<T: Type<Postgres>> Type<Postgres> for PgNumeric<T> {
    fn type_info() -> PgTypeInfo {
        T::type_info()
    }
}

impl<T: PgHasArrayType> PgHasArrayType for PgNumeric<T> {
    fn array_type_info() -> PgTypeInfo {
        T::array_type_info()
    }
}

impl<'r, T: Decode<'r, Postgres>> Decode<'r, Postgres> for PgNumeric<T> {
    fn decode(value: PgValueRef<'r>) -> Result<Self, BoxDynError> {
        let special = match value.format() {
            PgValueFormat::Binary => match value.as_bytes()?.get(4..6) {
                Some([0xC0, 0]) => Some("NaN"),
                Some([0xD0, 0]) => Some("Infinity"),
                Some([0xF0, 0]) => Some("-Infinity"),
                _ => None,
            },
            PgValueFormat::Text => match value.as_str()? {
                "NaN" => Some("NaN"),
                "Infinity" => Some("Infinity"),
                "-Infinity" => Some("-Infinity"),
                _ => None,
            },
        };
        match special {
            Some(text) => Ok(Self::Special(text)),
            None => T::decode(value).map(Self::Finite),
        }
    }
}
