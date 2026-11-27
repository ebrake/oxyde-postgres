//! Convert Python write values to the objects expected by the query IR.

use pyo3::intern;
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{
    PyBool, PyBytes, PyDate, PyDateTime, PyDict, PyFloat, PyInt, PyString, PyTime, PyType,
};
use uuid::Uuid;

static UUID_TYPE: PyOnceLock<Py<PyType>> = PyOnceLock::new();
static DECIMAL_TYPE: PyOnceLock<Py<PyType>> = PyOnceLock::new();

/// A value the core can write without calling the Python serializer.
enum NativeKind {
    /// MessagePack encodes the value as is.
    Unchanged,
    IsoFormat,
    Uuid,
    Decimal,
}

/// Classify by exact type only. Subclasses, enums, expressions and containers
/// have no native kind, so the Python serializer keeps its precedence for them.
fn native_kind(value: &Bound<'_, PyAny>) -> PyResult<Option<NativeKind>> {
    let py = value.py();
    let kind = if value.is_none()
        || value.is_exact_instance_of::<PyBool>()
        || value.is_exact_instance_of::<PyInt>()
        || value.is_exact_instance_of::<PyFloat>()
        || value.is_exact_instance_of::<PyString>()
        || value.is_exact_instance_of::<PyBytes>()
    {
        NativeKind::Unchanged
    } else if value.is_exact_instance_of::<PyDateTime>()
        || value.is_exact_instance_of::<PyDate>()
        || value.is_exact_instance_of::<PyTime>()
    {
        NativeKind::IsoFormat
    } else if value.is_exact_instance(UUID_TYPE.import(py, "uuid", "UUID")?.as_any()) {
        NativeKind::Uuid
    } else if value.is_exact_instance(DECIMAL_TYPE.import(py, "decimal", "Decimal")?.as_any()) {
        NativeKind::Decimal
    } else {
        return Ok(None);
    };
    Ok(Some(kind))
}

/// Whether the value's type still has its built-in TYPE_REGISTRY entry.
fn has_builtin_serializer(
    value: &Bound<'_, PyAny>,
    registry: &Bound<'_, PyDict>,
    builtins: &Bound<'_, PyDict>,
) -> PyResult<bool> {
    let value_type = value.get_type();
    Ok(
        match (
            registry.get_item(&value_type)?,
            builtins.get_item(&value_type)?,
        ) {
            (Some(current), Some(builtin)) => current.is(&builtin),
            (None, None) => true,
            _ => false,
        },
    )
}

/// Return a replacement value, or `None` when the original can pass through.
fn serialize_value<'py>(
    value: &Bound<'py, PyAny>,
    fallback: &Bound<'py, PyAny>,
    registry: &Bound<'py, PyDict>,
    builtins: &Bound<'py, PyDict>,
) -> PyResult<Option<Bound<'py, PyAny>>> {
    let kind = match native_kind(value)? {
        // The Python serializer applies any serializer an application registered.
        Some(kind) if has_builtin_serializer(value, registry, builtins)? => kind,
        _ => return fallback.call1((value,)).map(Some),
    };

    let py = value.py();
    let serialized = match kind {
        NativeKind::Unchanged => return Ok(None),
        NativeKind::IsoFormat => value.call_method0(intern!(py, "isoformat"))?,
        NativeKind::Uuid => {
            let int = value.getattr(intern!(py, "int"))?.extract::<u128>()?;
            let mut buffer = Uuid::encode_buffer();
            let text = Uuid::from_u128(int).hyphenated().encode_lower(&mut buffer);
            PyString::new(py, text).into_any()
        }
        NativeKind::Decimal => value.str()?.into_any(),
    };
    Ok(Some(serialized))
}

/// Map field names to columns and serialize their values for the write IR.
///
/// `columns` holds only renamed fields; other keys are already column names.
/// `builtins` is a snapshot of TYPE_REGISTRY's built-in entries.
#[pyfunction]
#[pyo3(signature = (values, columns, fallback, registry, builtins, map_first=true))]
pub(crate) fn serialize_column_values<'py>(
    values: &Bound<'py, PyDict>,
    columns: &Bound<'py, PyDict>,
    fallback: &Bound<'py, PyAny>,
    registry: &Bound<'py, PyDict>,
    builtins: &Bound<'py, PyDict>,
    map_first: bool,
) -> PyResult<Bound<'py, PyDict>> {
    let result = PyDict::new(values.py());
    if map_first {
        // INSERT and UPDATE resolve aliases first, including when keys collide.
        values.locked_for_each(|key, value| {
            let column = columns.get_item(&key)?.unwrap_or(key);
            result.set_item(column, value)
        })?;
        result.locked_for_each(|column, value| {
            if let Some(serialized) = serialize_value(&value, fallback, registry, builtins)? {
                result.set_item(column, serialized)?;
            }
            Ok(())
        })?;
    } else {
        // Bulk UPDATE converts each selected field before resolving aliases.
        values.locked_for_each(|key, value| {
            let value = serialize_value(&value, fallback, registry, builtins)?.unwrap_or(value);
            let column = columns.get_item(&key)?.unwrap_or(key);
            result.set_item(column, value)
        })?;
    }
    Ok(result)
}
