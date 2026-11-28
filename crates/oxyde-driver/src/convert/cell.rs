//! Decoded PostgreSQL result cells used by result encoders.

use std::{borrow::Cow, convert::Infallible, fmt};

use arrayvec::ArrayString;
use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use rust_decimal::Decimal;
use sqlx::postgres::types::PgTimeTz;
use uuid::Uuid;

pub(super) type PgChronoTimeTz = PgTimeTz<NaiveTime, FixedOffset>;

/// A PostgreSQL temporal value, retaining its spelling outside the native range.
#[derive(Debug)]
pub enum PgTemporal<T> {
    Native(T),
    Text(String),
}

/// A PostgreSQL numeric value, including NaN and infinities.
#[derive(Debug)]
pub enum PgNumeric<T> {
    Finite(T),
    Special(&'static str),
}

#[derive(Debug)]
pub enum Cell {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    Str(String),
    Bytes(Vec<u8>),
    Uuid(Uuid),
    Decimal(Decimal),
    DecimalText(Cow<'static, str>),
    DateTime(NaiveDateTime),
    DateTimeTz(DateTime<FixedOffset>),
    Date(NaiveDate),
    Time(NaiveTime),
    TimeTz(NaiveTime, FixedOffset),
    Duration(i64),
    DurationAggregate(i128),
    Json(serde_json::Value),
    // Keep the array discriminant out of the scalar cell layout.
    Array(Box<ArrayValues>),
}

/// An array element borrowed for the duration of a visit.
#[derive(Debug)]
pub enum ArrayItem<'a> {
    Cell(&'a Cell),
    Str(&'a str),
    Bytes(&'a [u8]),
    Json(&'a serde_json::Value),
}

/// Decoded array elements in typed storage, with a cell variant for hand-built arrays.
#[derive(Debug)]
pub enum ArrayValues {
    Bool(Vec<Option<bool>>),
    I16(Vec<Option<i16>>),
    I32(Vec<Option<i32>>),
    I64(Vec<Option<i64>>),
    U32(Vec<Option<u32>>),
    U64(Vec<Option<u64>>),
    F32(Vec<Option<f32>>),
    F64(Vec<Option<f64>>),
    Duration(Vec<Option<i64>>),
    Uuid(Vec<Option<Uuid>>),
    Str(Vec<Option<String>>),
    Bytes(Vec<Option<Vec<u8>>>),
    Json(Vec<Option<serde_json::Value>>),
    Decimal(Vec<Option<PgNumeric<Decimal>>>),
    DateTime(Vec<Option<PgTemporal<NaiveDateTime>>>),
    DateTimeUtc(Vec<Option<PgTemporal<DateTime<Utc>>>>),
    Date(Vec<Option<PgTemporal<NaiveDate>>>),
    Time(Vec<Option<PgTemporal<NaiveTime>>>),
    TimeTz(Vec<Option<PgTemporal<PgChronoTimeTz>>>),
    Cells(Vec<Cell>),
}

impl ArrayValues {
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Bool(values) => values.len(),
            Self::I16(values) => values.len(),
            Self::I32(values) => values.len(),
            Self::I64(values) => values.len(),
            Self::U32(values) => values.len(),
            Self::U64(values) => values.len(),
            Self::F32(values) => values.len(),
            Self::F64(values) => values.len(),
            Self::Duration(values) => values.len(),
            Self::Uuid(values) => values.len(),
            Self::Str(values) => values.len(),
            Self::Bytes(values) => values.len(),
            Self::Json(values) => values.len(),
            Self::Decimal(values) => values.len(),
            Self::DateTime(values) => values.len(),
            Self::DateTimeUtc(values) => values.len(),
            Self::Date(values) => values.len(),
            Self::Time(values) => values.len(),
            Self::TimeTz(values) => values.len(),
            Self::Cells(values) => values.len(),
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Visit elements, stopping at the first error.
    pub fn try_for_each<E>(
        &self,
        mut f: impl FnMut(ArrayItem<'_>) -> Result<(), E>,
    ) -> Result<(), E> {
        match self {
            Self::Bool(values) => {
                visit_typed(values, |v, f| f(ArrayItem::Cell(&Cell::Bool(*v))), f)
            }
            Self::I16(values) => visit_typed(
                values,
                |v, f| f(ArrayItem::Cell(&Cell::Int(i64::from(*v)))),
                f,
            ),
            Self::I32(values) => visit_typed(
                values,
                |v, f| f(ArrayItem::Cell(&Cell::Int(i64::from(*v)))),
                f,
            ),
            Self::I64(values) => visit_typed(values, |v, f| f(ArrayItem::Cell(&Cell::Int(*v))), f),
            Self::U32(values) => visit_typed(
                values,
                |v, f| f(ArrayItem::Cell(&Cell::UInt(u64::from(*v)))),
                f,
            ),
            Self::U64(values) => visit_typed(values, |v, f| f(ArrayItem::Cell(&Cell::UInt(*v))), f),
            Self::F32(values) => visit_typed(
                values,
                |v, f| f(ArrayItem::Cell(&Cell::Float(f64::from(*v)))),
                f,
            ),
            Self::F64(values) => {
                visit_typed(values, |v, f| f(ArrayItem::Cell(&Cell::Float(*v))), f)
            }
            Self::Duration(values) => {
                visit_typed(values, |v, f| f(ArrayItem::Cell(&Cell::Duration(*v))), f)
            }
            Self::Uuid(values) => {
                visit_typed(values, |v, f| f(ArrayItem::Cell(&Cell::Uuid(*v))), f)
            }
            Self::Str(values) => visit_typed(values, |v, f| f(ArrayItem::Str(v)), f),
            Self::Bytes(values) => visit_typed(values, |v, f| f(ArrayItem::Bytes(v)), f),
            Self::Json(values) => visit_typed(values, |v, f| f(ArrayItem::Json(v)), f),
            Self::Decimal(values) => {
                visit_typed(values, |v, f| f(ArrayItem::Cell(&numeric_cell(v))), f)
            }
            Self::DateTime(values) => visit_typed(values, visit_temporal, f),
            Self::DateTimeUtc(values) => visit_typed(values, visit_temporal, f),
            Self::Date(values) => visit_typed(values, visit_temporal, f),
            Self::Time(values) => visit_typed(values, visit_temporal, f),
            Self::TimeTz(values) => visit_typed(values, visit_temporal, f),
            Self::Cells(values) => values
                .iter()
                .try_for_each(|value| f(ArrayItem::Cell(value))),
        }
    }

    /// Visit each element.
    pub fn for_each(&self, mut f: impl FnMut(ArrayItem<'_>)) {
        let Ok(()) = self.try_for_each(|item| {
            f(item);
            Ok::<(), Infallible>(())
        });
    }
}

#[inline]
fn visit_typed<T, E, F>(
    values: &[Option<T>],
    visit: impl Fn(&T, &mut F) -> Result<(), E>,
    mut f: F,
) -> Result<(), E>
where
    F: FnMut(ArrayItem<'_>) -> Result<(), E>,
{
    for value in values {
        match value {
            Some(value) => visit(value, &mut f)?,
            None => f(ArrayItem::Cell(&Cell::Null))?,
        }
    }
    Ok(())
}

fn visit_temporal<T: Copy, E>(
    value: &PgTemporal<T>,
    f: &mut impl FnMut(ArrayItem<'_>) -> Result<(), E>,
) -> Result<(), E>
where
    Cell: From<T>,
{
    match value {
        PgTemporal::Native(value) => f(ArrayItem::Cell(&Cell::from(*value))),
        PgTemporal::Text(text) => f(ArrayItem::Str(text)),
    }
}

pub(super) fn numeric_cell(value: &PgNumeric<Decimal>) -> Cell {
    match value {
        PgNumeric::Finite(value) => Cell::Decimal(*value),
        PgNumeric::Special(text) => Cell::DecimalText(Cow::Borrowed(text)),
    }
}

impl<T> From<PgTemporal<T>> for Cell
where
    Self: From<T>,
{
    fn from(value: PgTemporal<T>) -> Self {
        match value {
            PgTemporal::Native(value) => Self::from(value),
            PgTemporal::Text(text) => Self::Str(text),
        }
    }
}

impl From<NaiveDateTime> for Cell {
    fn from(value: NaiveDateTime) -> Self {
        Self::DateTime(value)
    }
}

impl From<DateTime<Utc>> for Cell {
    fn from(value: DateTime<Utc>) -> Self {
        Self::DateTimeTz(value.fixed_offset())
    }
}

impl From<NaiveDate> for Cell {
    fn from(value: NaiveDate) -> Self {
        Self::Date(value)
    }
}

impl From<NaiveTime> for Cell {
    fn from(value: NaiveTime) -> Self {
        Self::Time(value)
    }
}

impl From<PgChronoTimeTz> for Cell {
    fn from(value: PgChronoTimeTz) -> Self {
        Self::TimeTz(value.time, value.offset)
    }
}

impl Cell {
    /// Format typed cells whose MessagePack representation is a string.
    #[must_use]
    pub fn wire_text(&self) -> Option<ArrayString<64>> {
        use fmt::Write;

        // The longest typed wire string fits in this buffer, including the
        // sign and fractional part of the largest duration aggregate.
        let mut text = ArrayString::new();
        match self {
            Self::Uuid(value) => write!(text, "{}", value.hyphenated()),
            Self::Decimal(value) => write!(text, "{value}"),
            // Chrono's ISO Debug format uses the same T separator and
            // fractional precision as %Y-%m-%dT%H:%M:%S%.f.
            Self::DateTime(value) => write!(text, "{value:?}"),
            Self::DateTimeTz(value) => {
                if let Some(local) = value.naive_utc().checked_add_offset(*value.offset()) {
                    let offset = value.offset().local_minus_utc();
                    let sign = if offset < 0 { '-' } else { '+' };
                    // Match Chrono's RFC 3339 rounding for offsets with seconds.
                    let minutes = (offset.abs() + 30) / 60;
                    write!(
                        text,
                        "{local:?}{sign}{:02}:{:02}",
                        minutes / 60,
                        minutes % 60
                    )
                } else {
                    // Chrono's to_rfc3339 supports the adjacent out-of-range dates.
                    write!(text, "{}", value.to_rfc3339())
                }
            }
            Self::Date(value) => write!(text, "{value}"),
            Self::Time(value) => write!(text, "{value}"),
            Self::TimeTz(time, offset) => write!(text, "{time}{offset}"),
            Self::DurationAggregate(micros) => write_duration_iso(&mut text, *micros),
            Self::Null
            | Self::Bool(_)
            | Self::Int(_)
            | Self::UInt(_)
            | Self::Float(_)
            | Self::Str(_)
            | Self::Bytes(_)
            | Self::DecimalText(_)
            | Self::Duration(_)
            | Self::Json(_)
            | Self::Array(_) => return None,
        }
        .expect("typed wire string fits");
        Some(text)
    }
}

fn write_duration_iso(out: &mut impl fmt::Write, micros: i128) -> fmt::Result {
    let sign = if micros < 0 { "-" } else { "" };
    let micros = micros.unsigned_abs();
    let seconds = micros / 1_000_000;
    let days = seconds / 86_400;
    let seconds = seconds % 86_400;
    let fraction = micros % 1_000_000;
    write!(out, "{sign}P{days}DT{seconds}.{fraction:06}S")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{SecondsFormat, TimeZone};

    // serde_json/preserve_order stores an IndexMap inline in Value and grows Cell.
    // Check the size budget in the default workspace test configuration.
    #[test]
    fn cell_fits_in_32_bytes() {
        let size = std::mem::size_of::<Cell>();
        assert!(size <= 32, "Cell is {size} bytes");
    }

    #[test]
    fn datetime_tz_wire_text_matches_chrono_rfc3339() {
        let dates = [-4713, -1, 0, 1, 999, 9999, 10000]
            .map(|year| NaiveDate::from_ymd_opt(year, 11, 26).unwrap());
        for date in dates.into_iter().chain([NaiveDate::MIN, NaiveDate::MAX]) {
            for nanos in [0, 123_000_000, 123_456_000, 123_456_789, 1_123_456_789] {
                // Keep UTC on the same date at both boundaries for every offset.
                let local = date.and_hms_nano_opt(14, 4, 59, nanos).unwrap();
                for seconds in [0, -(5 * 3600 + 30 * 60), 14 * 3600, -29, 3599] {
                    let offset = FixedOffset::east_opt(seconds).unwrap();
                    let value = offset.from_local_datetime(&local).unwrap();
                    let expected = value.to_rfc3339_opts(SecondsFormat::AutoSi, false);
                    assert_eq!(
                        Cell::DateTimeTz(value).wire_text().unwrap().as_str(),
                        expected
                    );
                }
            }
        }
    }
    #[test]
    fn datetime_tz_wire_text_preserves_offsets_at_chrono_boundaries() {
        for (utc, seconds, expected) in [
            (
                NaiveDateTime::MAX,
                3600,
                "+262143-01-01T00:59:59.999999999+01:00",
            ),
            (NaiveDateTime::MIN, -3600, "-262144-12-31T23:00:00-01:00"),
        ] {
            let offset = FixedOffset::east_opt(seconds).unwrap();
            let value = DateTime::from_naive_utc_and_offset(utc, offset);
            assert_eq!(value.to_rfc3339(), expected);
            assert_eq!(
                Cell::DateTimeTz(value).wire_text().unwrap().as_str(),
                expected
            );
        }
    }
}
