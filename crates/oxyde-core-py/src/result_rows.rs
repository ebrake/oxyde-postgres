//! Materialize Rust SELECT results on the calling Python thread.

use crate::convert::{cell_to_py, CellConverter};
use oxyde_driver::{Cell, RowSet};
use pyo3::exceptions::PyIndexError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyString, PyTuple};

type PyRelation<'py> = (Vec<String>, Bound<'py, PyList>, Vec<Option<u32>>);

#[pyclass(frozen, module = "oxyde._oxyde_core")]
pub(crate) struct ResultRows(pub RowSet);

#[pymethods]
impl ResultRows {
    #[getter]
    fn columns(&self) -> Vec<String> {
        self.0.columns.clone()
    }

    fn __len__(&self) -> usize {
        self.0.rows.len()
    }

    fn dicts<'py>(
        &self,
        py: Python<'py>,
        keys: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyList>> {
        let keys: Vec<Bound<'py, PyString>> = keys.extract()?;
        dict_rows(py, &self.0.rows, &self.0.kinds, &keys)
    }

    fn tuples<'py>(
        &self,
        py: Python<'py>,
        indices: &Bound<'py, PyList>,
    ) -> PyResult<Bound<'py, PyList>> {
        let indices: Vec<Option<usize>> = indices.extract()?;
        for index in indices.iter().flatten() {
            self.check_index(*index)?;
        }
        let mut converter = CellConverter::default();
        let rows = PyList::empty(py);
        let mut values = Vec::with_capacity(indices.len());
        for row in &self.0.rows {
            for index in &indices {
                values.push(match index {
                    Some(index) => cell_to_py(
                        py,
                        &mut converter,
                        &row[*index],
                        self.0.kinds[*index].is_some(),
                    )?,
                    None => py.None().into_bound(py),
                });
            }
            rows.append(PyTuple::new(py, values.drain(..))?)?;
        }
        Ok(rows)
    }

    fn column<'py>(&self, py: Python<'py>, index: usize) -> PyResult<Bound<'py, PyList>> {
        self.check_index(index)?;
        let mut converter = CellConverter::default();
        let values = PyList::empty(py);
        let native = self.0.kinds[index].is_some();
        for row in &self.0.rows {
            values.append(cell_to_py(py, &mut converter, &row[index], native)?)?;
        }
        Ok(values)
    }

    fn relation<'py>(&self, py: Python<'py>, prefix: &str) -> PyResult<Option<PyRelation<'py>>> {
        let Some(relation) = self
            .0
            .relations
            .iter()
            .find(|relation| relation.prefix == prefix)
        else {
            return Ok(None);
        };
        let keys: Vec<_> = relation
            .columns
            .iter()
            .map(|key| PyString::new(py, key))
            .collect();
        let rows = dict_rows(py, &relation.rows, &relation.kinds, &keys)?;
        Ok(Some((
            relation.columns.clone(),
            rows,
            relation.refs.clone(),
        )))
    }

    #[getter]
    fn relation_prefixes(&self) -> Vec<&str> {
        self.0
            .relations
            .iter()
            .map(|relation| relation.prefix.as_str())
            .collect()
    }
}

impl ResultRows {
    fn check_index(&self, index: usize) -> PyResult<()> {
        if index < self.0.columns.len() {
            Ok(())
        } else {
            Err(PyIndexError::new_err("result column index out of range"))
        }
    }
}

fn dict_rows<'py>(
    py: Python<'py>,
    rows: &[Vec<Cell>],
    kinds: &[Option<&str>],
    keys: &[Bound<'py, PyString>],
) -> PyResult<Bound<'py, PyList>> {
    let output = PyList::empty(py);
    let mut converter = CellConverter::default();
    for row in rows {
        let dict = PyDict::new(py);
        for ((key, cell), kind) in keys.iter().zip(row).zip(kinds) {
            dict.set_item(key, cell_to_py(py, &mut converter, cell, kind.is_some())?)?;
        }
        output.append(dict)?;
    }
    Ok(output)
}
