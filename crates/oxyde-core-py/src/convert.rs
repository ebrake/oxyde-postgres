//! Conversion helpers: backend/dialect mapping, pool settings extraction,
//! and value conversion between Rust types and Python objects.

use std::{collections::HashMap, time::Duration};

use chrono::{Datelike, FixedOffset, Timelike};

use oxyde_driver::{ArrayItem, Cell, DatabaseBackend, PoolSettings as DriverPoolSettings};
use oxyde_sql::Dialect;
use pyo3::exceptions::{PyOverflowError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{
    PyBool, PyBytes, PyDateTime, PyDelta, PyDict, PyList, PyString, PyTime, PyTuple, PyType,
    PyTzInfo,
};
use pyo3::IntoPyObjectExt;
use sea_query::Value as QueryValue;
use serde_json::Value as JsonValue;

/// Map driver's `DatabaseBackend` to query's `Dialect`.
pub(crate) fn backend_to_dialect(backend: DatabaseBackend) -> Dialect {
    match backend {
        DatabaseBackend::Postgres => Dialect::Postgres,
    }
}

/// Extract `PoolSettings` from a Python dict or object with `to_payload()` method.
pub(crate) fn extract_pool_settings(
    _py: Python<'_>,
    settings: Option<Bound<'_, PyAny>>,
) -> PyResult<DriverPoolSettings> {
    let mut parsed = DriverPoolSettings::default();

    let Some(obj) = settings else {
        return Ok(parsed);
    };

    if obj.is_none() {
        return Ok(parsed);
    }

    if let Ok(dict) = obj.cast::<PyDict>() {
        parse_pool_dict(dict, &mut parsed)?;
        return Ok(parsed);
    }

    if obj.hasattr("to_payload")? {
        let payload = obj.call_method0("to_payload")?;
        if payload.is_none() {
            return Ok(parsed);
        }
        let dict = payload.cast::<PyDict>()?;
        parse_pool_dict(dict, &mut parsed)?;
        return Ok(parsed);
    }

    let type_name = obj.get_type().name()?.to_string();
    Err(PyErr::new::<PyTypeError, _>(format!(
        "Pool settings must be a dict or expose to_payload(), got {type_name}"
    )))
}

/// Convert sea_query `Value` slice to a Python list.
pub(crate) fn values_to_py<'py>(
    py: Python<'py>,
    values: &[QueryValue],
) -> PyResult<Bound<'py, PyAny>> {
    let list = PyList::empty(py);
    for value in values {
        list.append(value_to_py(py, value))?;
    }
    Ok(list.into_any())
}

/// Convert a single sea_query `Value` to a Python object.
#[allow(unreachable_patterns)]
pub(crate) fn value_to_py(py: Python<'_>, value: &QueryValue) -> Py<PyAny> {
    match value {
        QueryValue::Bool(Some(v)) => PyBool::new(py, *v).to_owned().unbind().into_any(),
        QueryValue::Bool(None) => py.None(),
        QueryValue::TinyInt(Some(v)) => (*v).into_pyobject(py).unwrap().unbind().into_any(),
        QueryValue::TinyInt(None) => py.None(),
        QueryValue::SmallInt(Some(v)) => (*v).into_pyobject(py).unwrap().unbind().into_any(),
        QueryValue::SmallInt(None) => py.None(),
        QueryValue::Int(Some(v)) => (*v).into_pyobject(py).unwrap().unbind().into_any(),
        QueryValue::Int(None) => py.None(),
        QueryValue::BigInt(Some(v)) => (*v).into_pyobject(py).unwrap().unbind().into_any(),
        QueryValue::BigInt(None) => py.None(),
        QueryValue::TinyUnsigned(Some(v)) => (*v).into_pyobject(py).unwrap().unbind().into_any(),
        QueryValue::TinyUnsigned(None) => py.None(),
        QueryValue::SmallUnsigned(Some(v)) => (*v).into_pyobject(py).unwrap().unbind().into_any(),
        QueryValue::SmallUnsigned(None) => py.None(),
        QueryValue::Unsigned(Some(v)) => (*v).into_pyobject(py).unwrap().unbind().into_any(),
        QueryValue::Unsigned(None) => py.None(),
        QueryValue::BigUnsigned(Some(v)) => (*v).into_pyobject(py).unwrap().unbind().into_any(),
        QueryValue::BigUnsigned(None) => py.None(),
        QueryValue::Float(Some(v)) => (*v).into_pyobject(py).unwrap().unbind().into_any(),
        QueryValue::Float(None) => py.None(),
        QueryValue::Double(Some(v)) => (*v).into_pyobject(py).unwrap().unbind().into_any(),
        QueryValue::Double(None) => py.None(),
        QueryValue::String(Some(s)) => PyString::new(py, s.as_str()).unbind().into_any(),
        QueryValue::String(None) => py.None(),
        QueryValue::Char(Some(c)) => {
            let text = c.to_string();
            PyString::new(py, &text).unbind().into_any()
        }
        QueryValue::Char(None) => py.None(),
        QueryValue::Bytes(Some(bytes)) => PyBytes::new(py, bytes.as_slice()).unbind().into_any(),
        QueryValue::Bytes(None) => py.None(),
        _ => PyString::new(py, &format!("{value:?}")).unbind().into_any(),
    }
}

/// Convert sea_query values to Python list of (type_tag, value) tuples.
pub(crate) fn values_to_py_tagged<'py>(
    py: Python<'py>,
    values: &[QueryValue],
) -> PyResult<Bound<'py, PyAny>> {
    let list = PyList::empty(py);
    for value in values {
        list.append(value_to_py_tagged(py, value)?)?;
    }
    Ok(list.into_any())
}

/// Convert a single sea_query `Value` to a Python (type_tag, value) tuple.
#[allow(unreachable_patterns)]
fn value_to_py_tagged<'py>(py: Python<'py>, value: &QueryValue) -> PyResult<Bound<'py, PyTuple>> {
    let (tag, val): (&str, Py<PyAny>) = match value {
        QueryValue::Bool(Some(v)) => ("Bool", PyBool::new(py, *v).to_owned().unbind().into_any()),
        QueryValue::Bool(None) => ("Bool", py.None()),
        QueryValue::TinyInt(Some(v)) => (
            "TinyInt",
            (*v).into_pyobject(py).unwrap().unbind().into_any(),
        ),
        QueryValue::TinyInt(None) => ("TinyInt", py.None()),
        QueryValue::SmallInt(Some(v)) => (
            "SmallInt",
            (*v).into_pyobject(py).unwrap().unbind().into_any(),
        ),
        QueryValue::SmallInt(None) => ("SmallInt", py.None()),
        QueryValue::Int(Some(v)) => ("Int", (*v).into_pyobject(py).unwrap().unbind().into_any()),
        QueryValue::Int(None) => ("Int", py.None()),
        QueryValue::BigInt(Some(v)) => (
            "BigInt",
            (*v).into_pyobject(py).unwrap().unbind().into_any(),
        ),
        QueryValue::BigInt(None) => ("BigInt", py.None()),
        QueryValue::Float(Some(v)) => {
            ("Float", (*v).into_pyobject(py).unwrap().unbind().into_any())
        }
        QueryValue::Float(None) => ("Float", py.None()),
        QueryValue::Double(Some(v)) => (
            "Double",
            (*v).into_pyobject(py).unwrap().unbind().into_any(),
        ),
        QueryValue::Double(None) => ("Double", py.None()),
        QueryValue::String(Some(s)) => {
            ("String", PyString::new(py, s.as_str()).unbind().into_any())
        }
        QueryValue::String(None) => ("String", py.None()),
        QueryValue::Bytes(Some(bytes)) => (
            "Bytes",
            PyBytes::new(py, bytes.as_slice()).unbind().into_any(),
        ),
        QueryValue::Bytes(None) => ("Bytes", py.None()),
        QueryValue::Json(Some(j)) => (
            "Json",
            PyString::new(py, &j.to_string()).unbind().into_any(),
        ),
        QueryValue::Json(None) => ("Json", py.None()),
        QueryValue::Uuid(Some(u)) => (
            "Uuid",
            PyString::new(py, &u.to_string()).unbind().into_any(),
        ),
        QueryValue::Uuid(None) => ("Uuid", py.None()),
        QueryValue::Decimal(Some(d)) => (
            "Decimal",
            PyString::new(py, &d.to_string()).unbind().into_any(),
        ),
        QueryValue::Decimal(None) => ("Decimal", py.None()),
        QueryValue::ChronoDateTime(Some(dt)) => (
            "ChronoDateTime",
            PyString::new(py, &dt.to_string()).unbind().into_any(),
        ),
        QueryValue::ChronoDateTime(None) => ("ChronoDateTime", py.None()),
        QueryValue::ChronoDateTimeUtc(Some(dt)) => (
            "ChronoDateTimeUtc",
            PyString::new(py, &dt.to_rfc3339()).unbind().into_any(),
        ),
        QueryValue::ChronoDateTimeUtc(None) => ("ChronoDateTimeUtc", py.None()),
        QueryValue::ChronoDate(Some(d)) => (
            "ChronoDate",
            PyString::new(py, &d.to_string()).unbind().into_any(),
        ),
        QueryValue::ChronoDate(None) => ("ChronoDate", py.None()),
        QueryValue::ChronoTime(Some(t)) => (
            "ChronoTime",
            PyString::new(py, &t.to_string()).unbind().into_any(),
        ),
        QueryValue::ChronoTime(None) => ("ChronoTime", py.None()),
        QueryValue::Array(arr_type, Some(vals)) => {
            let inner = PyList::empty(py);
            for v in vals.iter() {
                let (_, py_val) = value_to_py_tagged(py, v).map_or_else(
                    |_| ("Unknown".to_string(), py.None()),
                    |t| {
                        let tag: String = t.get_item(0).unwrap().extract().unwrap();
                        let val: Py<PyAny> = t.get_item(1).unwrap().unbind();
                        (tag, val)
                    },
                );
                inner.append(py_val).ok();
            }
            (
                "Array",
                PyTuple::new(
                    py,
                    &[
                        PyString::new(py, &format!("{arr_type:?}")).into_any(),
                        inner.into_any(),
                    ],
                )
                .unwrap()
                .unbind()
                .into_any(),
            )
        }
        QueryValue::Array(arr_type, None) => (
            "Array",
            PyTuple::new(
                py,
                &[
                    PyString::new(py, &format!("{arr_type:?}")).into_any(),
                    py.None().into_bound(py).into_any(),
                ],
            )
            .unwrap()
            .unbind()
            .into_any(),
        ),
        other => {
            let repr = format!("{other:?}");
            let tag = repr.split('(').next().unwrap_or("Unknown");
            (
                "Unknown",
                PyString::new(py, &format!("{tag}:{repr}"))
                    .unbind()
                    .into_any(),
            )
        }
    };
    PyTuple::new(
        py,
        &[
            PyString::new(py, tag).into_any(),
            val.into_bound(py).into_any(),
        ],
    )
}

pub(crate) static UUID_TYPE: PyOnceLock<Py<PyType>> = PyOnceLock::new();
pub(crate) static DECIMAL_TYPE: PyOnceLock<Py<PyType>> = PyOnceLock::new();

#[derive(Default)]
pub(crate) struct CellConverter<'py> {
    offsets: HashMap<i32, Bound<'py, PyTzInfo>>,
}

impl<'py> CellConverter<'py> {
    fn offset(&mut self, py: Python<'py>, offset: FixedOffset) -> PyResult<&Bound<'py, PyTzInfo>> {
        use std::collections::hash_map::Entry;
        match self.offsets.entry(offset.local_minus_utc()) {
            Entry::Occupied(entry) => Ok(entry.into_mut()),
            Entry::Vacant(entry) => {
                let tz = if offset.local_minus_utc() == 0 {
                    PyTzInfo::utc(py)?.to_owned()
                } else {
                    offset.into_pyobject(py)?
                };
                Ok(entry.insert(tz))
            }
        }
    }
}

/// Convert a decoded cell to its Python representation. Untagged typed cells
/// use the same wire representation as MessagePack.
pub(crate) fn cell_to_py<'py>(
    py: Python<'py>,
    converter: &mut CellConverter<'py>,
    cell: &Cell,
    native: bool,
) -> PyResult<Bound<'py, PyAny>> {
    Ok(match cell {
        Cell::Null => py.None().into_bound(py),
        Cell::Bool(value) => PyBool::new(py, *value).to_owned().into_any(),
        Cell::Int(value) => value.into_bound_py_any(py)?,
        Cell::UInt(value) => value.into_bound_py_any(py)?,
        Cell::Float(value) => value.into_bound_py_any(py)?,
        Cell::Str(value) => PyString::new(py, value).into_any(),
        Cell::Bytes(value) => PyBytes::new(py, value).into_any(),
        Cell::Json(value) => json_to_py(py, value)?.into_bound(py),
        Cell::Array(values) => {
            let list = PyList::empty(py);
            values.try_for_each(|item| {
                list.append(array_item_to_py(py, converter, &item, native)?)
            })?;
            list.into_any()
        }
        Cell::Uuid(_)
        | Cell::Decimal(_)
        | Cell::DecimalText(_)
        | Cell::DateTime(_)
        | Cell::DateTimeTz(_)
        | Cell::Date(_)
        | Cell::Time(_)
        | Cell::TimeTz(..)
        | Cell::Duration(_)
        | Cell::DurationAggregate(_) => {
            if native {
                typed_cell_to_py(py, converter, cell)?
            } else {
                typed_cell_to_wire_py(py, cell)?
            }
        }
    })
}

fn typed_cell_to_wire_py<'py>(py: Python<'py>, cell: &Cell) -> PyResult<Bound<'py, PyAny>> {
    match cell {
        Cell::DecimalText(value) => Ok(PyString::new(py, value).into_any()),
        #[allow(clippy::cast_precision_loss)]
        Cell::Duration(micros) => (*micros as f64 / 1_000_000.0).into_bound_py_any(py),
        Cell::Uuid(_)
        | Cell::Decimal(_)
        | Cell::DateTime(_)
        | Cell::DateTimeTz(_)
        | Cell::Date(_)
        | Cell::Time(_)
        | Cell::TimeTz(..)
        | Cell::DurationAggregate(_) => {
            let text = cell
                .wire_text()
                .ok_or_else(|| PyTypeError::new_err("cell has no wire text"))?;
            Ok(PyString::new(py, &text).into_any())
        }
        Cell::Null
        | Cell::Bool(_)
        | Cell::Int(_)
        | Cell::UInt(_)
        | Cell::Float(_)
        | Cell::Str(_)
        | Cell::Bytes(_)
        | Cell::Json(_)
        | Cell::Array(_) => Err(PyTypeError::new_err("expected a typed cell")),
    }
}

fn array_item_to_py<'py>(
    py: Python<'py>,
    converter: &mut CellConverter<'py>,
    item: &ArrayItem<'_>,
    native: bool,
) -> PyResult<Bound<'py, PyAny>> {
    match item {
        ArrayItem::Cell(cell) => cell_to_py(py, converter, cell, native),
        ArrayItem::Str(value) => Ok(PyString::new(py, value).into_any()),
        ArrayItem::Bytes(value) => Ok(PyBytes::new(py, value).into_any()),
        ArrayItem::Json(value) => Ok(json_to_py(py, value)?.into_bound(py)),
    }
}

fn typed_cell_to_py<'py>(
    py: Python<'py>,
    converter: &mut CellConverter<'py>,
    cell: &Cell,
) -> PyResult<Bound<'py, PyAny>> {
    let result = match cell {
        Cell::Uuid(value) => {
            let uuid_type = UUID_TYPE.import(py, "uuid", "UUID")?;
            let kwargs = PyDict::new(py);
            kwargs
                .set_item(pyo3::intern!(py, "int"), value.as_u128())
                .and_then(|()| uuid_type.call((), Some(&kwargs)))
        }
        Cell::Decimal(value) => DECIMAL_TYPE
            .import(py, "decimal", "Decimal")?
            .call1((value.to_string(),)),
        Cell::DecimalText(value) => DECIMAL_TYPE
            .import(py, "decimal", "Decimal")?
            .call1((value.as_ref(),)),
        _ => temporal_cell_to_py(py, converter, cell),
    };
    match result {
        Err(err)
            if err.is_instance_of::<PyValueError>(py)
                || err.is_instance_of::<PyOverflowError>(py) =>
        {
            typed_cell_to_wire_py(py, cell)
        }
        value => value,
    }
}

fn temporal_cell_to_py<'py>(
    py: Python<'py>,
    converter: &mut CellConverter<'py>,
    cell: &Cell,
) -> PyResult<Bound<'py, PyAny>> {
    match cell {
        Cell::DateTime(value) => value.into_bound_py_any(py),
        Cell::DateTimeTz(value) => {
            let local = value
                .naive_utc()
                .checked_add_offset(*value.offset())
                .ok_or_else(|| PyValueError::new_err("local datetime is outside Chrono's range"))?;
            let tz = converter.offset(py, *value.offset())?;
            Ok(PyDateTime::new(
                py,
                local.year(),
                local.month().try_into()?,
                local.day().try_into()?,
                local.hour().try_into()?,
                local.minute().try_into()?,
                local.second().try_into()?,
                local.nanosecond() / 1000,
                Some(tz),
            )?
            .into_any())
        }
        Cell::Date(value) => value.into_bound_py_any(py),
        Cell::Time(value) => value.into_bound_py_any(py),
        Cell::TimeTz(value, offset) => {
            let tz = converter.offset(py, *offset)?;
            Ok(PyTime::new(
                py,
                value.hour().try_into()?,
                value.minute().try_into()?,
                value.second().try_into()?,
                value.nanosecond() / 1000,
                Some(tz),
            )?
            .into_any())
        }
        Cell::Duration(micros) => chrono::TimeDelta::microseconds(*micros).into_bound_py_any(py),
        Cell::DurationAggregate(micros) => {
            const DAY: i128 = 86_400_000_000;
            let within_day = micros.rem_euclid(DAY);
            Ok(PyDelta::new(
                py,
                micros.div_euclid(DAY).try_into()?,
                (within_day / 1_000_000).try_into()?,
                (within_day % 1_000_000).try_into()?,
                false,
            )?
            .into_any())
        }
        Cell::Null
        | Cell::Bool(_)
        | Cell::Int(_)
        | Cell::UInt(_)
        | Cell::Float(_)
        | Cell::Str(_)
        | Cell::Bytes(_)
        | Cell::Json(_)
        | Cell::Array(_)
        | Cell::Uuid(_)
        | Cell::Decimal(_)
        | Cell::DecimalText(_) => Err(PyTypeError::new_err("expected a temporal cell")),
    }
}

/// Recursively convert `serde_json::Value` to a Python object.
pub(crate) fn json_to_py(py: Python<'_>, value: &JsonValue) -> PyResult<Py<PyAny>> {
    Ok(match value {
        JsonValue::Null => py.None(),
        JsonValue::Bool(v) => PyBool::new(py, *v).to_owned().unbind().into_any(),
        JsonValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.into_pyobject(py).unwrap().unbind().into_any()
            } else if let Some(u) = n.as_u64() {
                u.into_pyobject(py).unwrap().unbind().into_any()
            } else if let Some(f) = n.as_f64() {
                f.into_pyobject(py).unwrap().unbind().into_any()
            } else {
                py.None()
            }
        }
        JsonValue::String(s) => PyString::new(py, s).unbind().into_any(),
        JsonValue::Array(items) => {
            let list = PyList::empty(py);
            for item in items {
                list.append(json_to_py(py, item)?)?;
            }
            list.unbind().into_any()
        }
        JsonValue::Object(map) => {
            let dict = PyDict::new(py);
            for (key, val) in map {
                dict.set_item(key, json_to_py(py, val)?)?;
            }
            dict.unbind().into_any()
        }
    })
}

fn parse_pool_dict(dict: &Bound<'_, PyDict>, parsed: &mut DriverPoolSettings) -> PyResult<()> {
    if let Some(value) = dict.get_item("max_connections")? {
        parsed.max_connections = extract_optional_u32(&value)?;
    }
    if let Some(value) = dict.get_item("min_connections")? {
        parsed.min_connections = extract_optional_u32(&value)?;
    }
    if let Some(value) = dict.get_item("connect_timeout")? {
        parsed.acquire_timeout = extract_optional_duration(&value)?;
    }
    if let Some(value) = dict.get_item("idle_timeout")? {
        parsed.idle_timeout = extract_optional_duration(&value)?;
    }
    if let Some(value) = dict.get_item("acquire_timeout")? {
        parsed.acquire_timeout = extract_optional_duration(&value)?;
    }
    if let Some(value) = dict.get_item("max_lifetime")? {
        parsed.max_lifetime = extract_optional_duration(&value)?;
    }
    if let Some(value) = dict.get_item("test_before_acquire")? {
        parsed.test_before_acquire = extract_optional_bool(&value)?;
    }
    // Extract transaction cleanup settings
    if let Some(value) = dict.get_item("transaction_timeout")? {
        parsed.transaction_timeout = extract_optional_duration(&value)?;
    }
    if let Some(value) = dict.get_item("transaction_cleanup_interval")? {
        parsed.transaction_cleanup_interval = extract_optional_duration(&value)?;
    }

    // TLS settings
    if let Some(value) = dict.get_item("ssl_mode")? {
        parsed.ssl_mode = extract_optional_string(&value)?;
    }
    if let Some(value) = dict.get_item("ssl_root_cert")? {
        parsed.ssl_root_cert = extract_optional_string(&value)?;
    }
    if let Some(value) = dict.get_item("ssl_client_cert")? {
        parsed.ssl_client_cert = extract_optional_string(&value)?;
    }
    if let Some(value) = dict.get_item("ssl_client_key")? {
        parsed.ssl_client_key = extract_optional_string(&value)?;
    }

    // PostgreSQL-specific
    if let Some(value) = dict.get_item("pg_application_name")? {
        parsed.pg_application_name = extract_optional_string(&value)?;
    }
    if let Some(value) = dict.get_item("pg_statement_cache_capacity")? {
        parsed.pg_statement_cache_capacity = extract_optional_u32(&value)?;
    }

    Ok(())
}

fn extract_optional_u32(value: &Bound<'_, PyAny>) -> PyResult<Option<u32>> {
    if value.is_none() {
        Ok(None)
    } else {
        value
            .extract::<u32>()
            .map(Some)
            .map_err(|e| PyErr::new::<PyTypeError, _>(e.to_string()))
    }
}

fn extract_optional_string(value: &Bound<'_, PyAny>) -> PyResult<Option<String>> {
    if value.is_none() {
        Ok(None)
    } else {
        value
            .extract::<String>()
            .map(Some)
            .map_err(|e| PyErr::new::<PyTypeError, _>(e.to_string()))
    }
}

fn extract_optional_bool(value: &Bound<'_, PyAny>) -> PyResult<Option<bool>> {
    if value.is_none() {
        Ok(None)
    } else {
        value
            .extract::<bool>()
            .map(Some)
            .map_err(|e| PyErr::new::<PyTypeError, _>(e.to_string()))
    }
}

fn extract_optional_duration(value: &Bound<'_, PyAny>) -> PyResult<Option<Duration>> {
    if value.is_none() {
        return Ok(None);
    }

    if let Ok(seconds) = value.extract::<f64>() {
        return Ok(Some(Duration::from_secs_f64(seconds)));
    }

    if let Ok(seconds) = value.extract::<u64>() {
        return Ok(Some(Duration::from_secs(seconds)));
    }

    if value.hasattr("total_seconds")? {
        let seconds_obj = value.call_method0("total_seconds")?;
        let seconds = seconds_obj.extract::<f64>()?;
        return Ok(Some(Duration::from_secs_f64(seconds)));
    }

    Err(PyErr::new::<PyTypeError, _>(
        "Duration must be provided as seconds (float/int) or datetime.timedelta".to_string(),
    ))
}

#[cfg(all(test, not(feature = "extension-module")))]
mod tests {
    use super::*;
    use pyo3::types::PyDict;

    #[test]
    fn test_cell_to_py_falls_back_when_local_datetime_exceeds_chrono_range() {
        Python::initialize();
        Python::attach(|py| {
            let offset = FixedOffset::east_opt(3600).unwrap();
            let cell = Cell::DateTimeTz(chrono::DateTime::from_naive_utc_and_offset(
                chrono::NaiveDateTime::MAX,
                offset,
            ));
            let mut converter = CellConverter::default();
            let value = cell_to_py(py, &mut converter, &cell, true).unwrap();
            assert_eq!(
                value.extract::<String>().unwrap(),
                cell.wire_text().unwrap().as_str()
            );
        });
    }

    #[test]
    fn test_cell_to_py_falls_back_for_native_range_errors() {
        Python::initialize();
        Python::attach(|py| {
            const DAY: i128 = 86_400_000_000;
            let mut converter = CellConverter::default();
            for (cell, exception) in [
                (
                    Cell::DateTime(
                        chrono::NaiveDate::from_ymd_opt(10_000, 1, 1)
                            .unwrap()
                            .and_hms_opt(0, 0, 0)
                            .unwrap(),
                    ),
                    py.get_type::<PyValueError>(),
                ),
                (
                    Cell::DurationAggregate(1_000_000_000 * DAY),
                    py.get_type::<PyOverflowError>(),
                ),
                (
                    Cell::DurationAggregate(i128::from(i32::MAX) * DAY + DAY),
                    py.get_type::<PyValueError>(),
                ),
            ] {
                let err = temporal_cell_to_py(py, &mut converter, &cell).unwrap_err();
                assert!(err.matches(py, &exception).unwrap());
                let value = cell_to_py(py, &mut converter, &cell, true).unwrap();
                assert_eq!(
                    value.extract::<String>().unwrap(),
                    cell.wire_text().unwrap().as_str()
                );
            }
        });
    }

    #[test]
    fn test_cell_to_py_propagates_non_range_errors() {
        Python::initialize();
        Python::attach(|py| {
            let mut converter = CellConverter::default();
            let cell = Cell::DecimalText("invalid decimal".into());
            let err = cell_to_py(py, &mut converter, &cell, true).unwrap_err();
            let invalid_operation = py
                .import("decimal")
                .unwrap()
                .getattr("InvalidOperation")
                .unwrap();
            assert!(err.matches(py, &invalid_operation).unwrap());
        });
    }

    #[test]
    fn test_cell_to_py_preserves_special_values_and_numeric_scale() {
        Python::initialize();
        Python::attach(|py| {
            let mut converter = CellConverter::default();
            for wire in ["NaN", "Infinity", "-Infinity", "1.2300"] {
                let cell = Cell::DecimalText(wire.into());
                let value = cell_to_py(py, &mut converter, &cell, true).unwrap();
                assert_eq!(
                    value.get_type().name().unwrap().to_str().unwrap(),
                    "Decimal"
                );
                assert_eq!(value.str().unwrap().to_str().unwrap(), wire);
            }
            let finite = DECIMAL_TYPE.import(py, "decimal", "Decimal").unwrap();
            let cell = Cell::Decimal("1.2300".parse().unwrap());
            let value = cell_to_py(py, &mut converter, &cell, true).unwrap();
            assert!(value.is_exact_instance(finite.as_any()));
            assert_eq!(value.str().unwrap().to_str().unwrap(), "1.2300");

            for (cell, wire) in [
                (Cell::Str("infinity".into()), "infinity"),
                (Cell::Str("24:00:00".into()), "24:00:00"),
                (
                    Cell::Date(chrono::NaiveDate::from_ymd_opt(0, 1, 2).unwrap()),
                    "0000-01-02",
                ),
                (
                    Cell::Date(chrono::NaiveDate::from_ymd_opt(10000, 1, 2).unwrap()),
                    "+10000-01-02",
                ),
            ] {
                assert_eq!(
                    cell_to_py(py, &mut converter, &cell, true)
                        .unwrap()
                        .extract::<String>()
                        .unwrap(),
                    wire
                );
            }
            assert!(cell_to_py(py, &mut converter, &Cell::Null, true)
                .unwrap()
                .is_none());
        });
    }

    #[test]
    fn test_cell_to_py_preserves_timedelta_and_fixed_offset_precision() {
        Python::initialize();
        Python::attach(|py| {
            let mut converter = CellConverter::default();
            for micros in [(1 << 53) + 1, -((1 << 53) + 1)] {
                let value = cell_to_py(py, &mut converter, &Cell::Duration(micros), true).unwrap();
                assert_eq!(
                    value
                        .extract::<chrono::TimeDelta>()
                        .unwrap()
                        .num_microseconds(),
                    Some(micros),
                );
            }
            for seconds in [0, 7261, -7261] {
                let time = chrono::NaiveTime::from_hms_micro_opt(12, 30, 0, 1).unwrap();
                let offset = FixedOffset::east_opt(seconds).unwrap();
                let value =
                    cell_to_py(py, &mut converter, &Cell::TimeTz(time, offset), true).unwrap();
                let python_offset = value.call_method0("utcoffset").unwrap();
                assert_eq!(
                    python_offset
                        .extract::<chrono::TimeDelta>()
                        .unwrap()
                        .num_seconds(),
                    i64::from(seconds),
                );
                let tzinfo = value.getattr("tzinfo").unwrap();
                assert_eq!(
                    tzinfo.get_type().name().unwrap().to_str().unwrap(),
                    "timezone"
                );
                if seconds == 0 {
                    assert!(tzinfo.is(PyTzInfo::utc(py).unwrap()));
                }
            }
        });
    }

    #[test]
    fn test_extract_pool_settings_from_dict() {
        Python::initialize();
        Python::attach(|py| {
            let dict = PyDict::new(py);
            dict.set_item("max_connections", 20u32).unwrap();
            dict.set_item("min_connections", 5u32).unwrap();
            dict.set_item("connect_timeout", 1.5f64).unwrap();
            dict.set_item("idle_timeout", 2.0f64).unwrap();
            dict.set_item("acquire_timeout", 3.0f64).unwrap();
            dict.set_item("max_lifetime", 4.0f64).unwrap();
            dict.set_item("test_before_acquire", true).unwrap();

            let settings = extract_pool_settings(py, Some(dict.into_any())).unwrap();
            assert_eq!(settings.max_connections, Some(20));
            assert_eq!(settings.min_connections, Some(5));
            assert!((settings.acquire_timeout.unwrap().as_secs_f64() - 3.0).abs() < f64::EPSILON);
            assert!((settings.idle_timeout.unwrap().as_secs_f64() - 2.0).abs() < f64::EPSILON);
            assert!((settings.max_lifetime.unwrap().as_secs_f64() - 4.0).abs() < f64::EPSILON);
            assert_eq!(settings.test_before_acquire, Some(true));
        });
    }

    #[test]
    fn test_extract_pool_settings_rejects_invalid_type() {
        Python::initialize();
        Python::attach(|py| {
            let value = "invalid".into_pyobject(py).unwrap().into_any();
            let err = extract_pool_settings(py, Some(value)).unwrap_err();
            assert!(err.to_string().contains("Pool settings must be a dict"));
        });
    }
}
