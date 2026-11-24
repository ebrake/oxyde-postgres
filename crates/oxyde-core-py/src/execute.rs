//! Query execution: execute, execute_in_transaction, render_sql, explain.

use std::{borrow::Cow, collections::HashMap, time::Instant};

use crate::convert::{
    backend_to_dialect, json_to_py, value_to_py, values_to_py, values_to_py_tagged,
};
use crate::types::{encode_insert_result, encode_mutation_result};
use oxyde_codec::{AggregateOp, ColumnTypeSpec, QueryIR};
use oxyde_driver::{
    execute_insert_returning, execute_insert_returning_in_transaction, execute_mutation_returning,
    execute_mutation_returning_in_transaction, execute_query_columnar,
    execute_query_columnar_dedup, execute_query_columnar_dedup_in_transaction,
    execute_query_columnar_in_transaction, execute_statement, execute_statement_in_transaction,
    explain_query, pool_backend as driver_pool_backend, ExplainFormat, ExplainOptions,
    RelationInfo,
};
use oxyde_sql::utils::identifier::{grouped_projection, output_label, resolve_column};
use oxyde_sql::utils::{value::scalar_result_spec, ExprTypes};
use oxyde_sql::{build_sql, emits_returning, Dialect};
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyString, PyTuple};

/// Check if profiling is enabled via OXYDE_PROFILE env var
fn is_profiling_enabled() -> bool {
    std::env::var("OXYDE_PROFILE").is_ok_and(|v| v == "1")
}

/// Decode and validate a msgpack IR for SQL generation and execution.
fn decode_ir(bytes: &[u8]) -> PyResult<QueryIR> {
    let ir =
        QueryIR::from_msgpack(bytes).map_err(|e| PyErr::new::<PyValueError, _>(e.to_string()))?;
    ir.validate()
        .map_err(|e| PyErr::new::<PyValueError, _>(e.to_string()))?;
    Ok(ir)
}

fn relations_for(ir: &QueryIR) -> Option<Vec<RelationInfo>> {
    if !ir.projects_join_columns() {
        return None;
    }

    // Joined columns are typed by their `alias.column` reference, not by
    // their result label, which a base column of the same name may share.
    let column_types = ir.column_types.as_ref();
    ir.joins.as_ref().map(|joins| {
        joins
            .iter()
            .map(|join| {
                let pk_column = join.pk_column_name();
                // The encoder reports an out-of-range index if the pk is missing.
                let pk_index = join
                    .columns
                    .iter()
                    .position(|c| c.column == pk_column)
                    .unwrap_or(join.columns.len());
                RelationInfo {
                    prefix: join.result_prefix.clone(),
                    pk_index,
                    field_names: join.columns.iter().map(|c| c.field.clone()).collect(),
                    column_types: join
                        .columns
                        .iter()
                        .map(|c| {
                            column_types
                                .and_then(|types| {
                                    types.get(&format!("{}.{}", join.alias, c.column))
                                })
                                .cloned()
                        })
                        .collect(),
                }
            })
            .collect()
    })
}

/// Column types for decoding a result, keyed by the result's column labels.
///
/// `ir.column_types` is keyed by column reference and stays as it is for SQL
/// generation, including HAVING. Result labels can differ from it:
///
/// - A grouped query's keys come back under their output labels, typed by the
///   column each one names.
/// - Scalar annotations take their types from the expression. Grouped
///   annotations can reuse physical column names, so use the runtime type
///   when the expression's type is unknown.
/// - An aggregate alias can reuse a column name while producing another type.
///   MIN and MAX return their input's type, so they take the input column's
///   hint. COUNT, SUM and AVG drop the hint and decode by the runtime type:
///   SUM over a BIGINT is NUMERIC, which the BIGINT and Timedelta decoders
///   cannot read.
fn decode_column_types(ir: &QueryIR) -> Option<Cow<'_, HashMap<String, ColumnTypeSpec>>> {
    if ir.count.unwrap_or(false) || ir.exists.unwrap_or(false) {
        return None;
    }
    let column_types = ir.column_types.as_ref()?;
    let joins = ir.joins.as_deref().unwrap_or_default();
    let column_type = |field| {
        let resolved = resolve_column(Some(&ir.table), joins, field, false);
        resolved.lookup(Some(column_types))
    };
    let mut types = Cow::Borrowed(column_types);
    for field in grouped_projection(ir) {
        let label = output_label(ir, field);
        match column_type(field) {
            Some(spec) if types.get(&label) != Some(spec) => {
                types.to_mut().insert(label, spec.clone());
            }
            Some(_) => {}
            None if types.contains_key(&label) => {
                types.to_mut().remove(&label);
            }
            None => {}
        }
    }
    let grouped = ir
        .group_by
        .as_ref()
        .is_some_and(|fields| !fields.is_empty());
    let expr_types = ExprTypes::from(ir.column_types.as_ref());
    for annotation in ir.annotations.iter().flatten() {
        match scalar_result_spec(&annotation.expr, expr_types) {
            Some(spec) if types.get(&annotation.alias) != Some(spec) => {
                types
                    .to_mut()
                    .insert(annotation.alias.clone(), spec.clone());
            }
            None if grouped && types.contains_key(&annotation.alias) => {
                types.to_mut().remove(&annotation.alias);
            }
            _ => {}
        }
    }
    for agg in ir.aggregates.iter().flatten() {
        let Some(alias) = agg.alias.as_ref() else {
            continue;
        };
        let input = match agg.op {
            AggregateOp::Min | AggregateOp::Max => agg.field.as_deref().and_then(column_type),
            AggregateOp::Count | AggregateOp::Sum | AggregateOp::Avg => None,
        };
        match input {
            Some(spec) => {
                types.to_mut().insert(alias.clone(), spec.clone());
            }
            None if types.contains_key(alias) => {
                types.to_mut().remove(alias);
            }
            None => {}
        }
    }
    Some(types)
}

/// Execute a query: deserialize IR → validate → build SQL → execute → msgpack bytes → Coroutine[bytes].
#[pyfunction]
pub(crate) fn execute<'py>(
    py: Python<'py>,
    pool_name: String,
    ir_bytes: &Bound<'py, PyBytes>,
) -> PyResult<Bound<'py, PyAny>> {
    let ir_data = ir_bytes.as_bytes().to_vec();

    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        let profile = is_profiling_enabled();
        let total_start = Instant::now();

        // Stages 1-2: Deserialize and validate IR
        let ir = decode_ir(&ir_data)?;

        // Stage 3: Get backend/dialect
        let backend = driver_pool_backend(&pool_name)
            .await
            .map_err(|e| crate::errors::driver_err(&e))?;
        let dialect = backend_to_dialect(backend);

        // Stage 4: Build SQL
        let (sql, params) =
            build_sql(&ir, dialect).map_err(|e| PyErr::new::<PyRuntimeError, _>(e.to_string()))?;

        let results = match ir.op {
            oxyde_codec::Operation::Select | oxyde_codec::Operation::Raw => {
                let exec_start = Instant::now();
                let column_types = decode_column_types(&ir);
                let (result, num_rows) = if let Some(relations) = relations_for(&ir) {
                    execute_query_columnar_dedup(
                        &pool_name,
                        &sql,
                        &params,
                        column_types.as_deref(),
                        &relations,
                    )
                    .await
                    .map_err(|e| crate::errors::driver_err(&e))?
                } else {
                    execute_query_columnar(&pool_name, &sql, &params, column_types.as_deref())
                        .await
                        .map_err(|e| crate::errors::driver_err(&e))?
                };
                let exec_us = exec_start.elapsed().as_micros();

                if profile {
                    let total_us = total_start.elapsed().as_micros();
                    eprintln!(
                        "[OXYDE_PROFILE] SELECT columnar ({} rows): exec={} µs, total={} µs, bytes={}",
                        num_rows, exec_us, total_us, result.len()
                    );
                }
                result
            }
            oxyde_codec::Operation::Insert => {
                // fall through to execute_insert_returning (last_insert_id()).
                // Routing is decided by IR intent + dialect, never by SQL text.
                if emits_returning(&ir, dialect) {
                    execute_mutation_returning(&pool_name, &sql, &params, ir.column_types.as_ref())
                        .await
                        .map_err(|e| crate::errors::driver_err(&e))?
                } else {
                    // Bulk insert: return only PKs
                    let pk_column = ir.pk_column.as_deref();
                    let ids = execute_insert_returning(&pool_name, &sql, &params, pk_column)
                        .await
                        .map_err(|e| crate::errors::driver_err(&e))?;

                    encode_insert_result(ids.len(), &ids)
                }
            }
            oxyde_codec::Operation::Update | oxyde_codec::Operation::Delete => {
                let op_name = if matches!(ir.op, oxyde_codec::Operation::Delete) {
                    "DELETE"
                } else {
                    "UPDATE"
                };

                // Use mutation returning only when the builder emitted RETURNING
                // through to execute_statement to preserve affected-row count.
                if emits_returning(&ir, dialect) {
                    let exec_start = Instant::now();
                    let result = execute_mutation_returning(
                        &pool_name,
                        &sql,
                        &params,
                        ir.column_types.as_ref(),
                    )
                    .await
                    .map_err(|e| crate::errors::driver_err(&e))?;
                    let exec_us = exec_start.elapsed().as_micros();

                    if profile {
                        let total_us = total_start.elapsed().as_micros();
                        eprintln!(
                            "[OXYDE_PROFILE] {op_name} RETURNING: exec={exec_us} µs, total={total_us} µs"
                        );
                    }
                    result
                } else {
                    let exec_start = Instant::now();
                    let affected = execute_statement(&pool_name, &sql, &params)
                        .await
                        .map_err(|e| crate::errors::driver_err(&e))?;
                    let exec_us = exec_start.elapsed().as_micros();

                    let serialize_start = Instant::now();
                    let result = encode_mutation_result(affected);
                    let serialize_us = serialize_start.elapsed().as_micros();

                    if profile {
                        let total_us = total_start.elapsed().as_micros();
                        eprintln!(
                            "[OXYDE_PROFILE] {op_name} (affected={affected}): exec={exec_us} µs, serialize={serialize_us} µs, total={total_us} µs, sql={sql}"
                        );
                    }
                    result
                }
            }
        };

        Ok(results)
    })
}

/// Execute a query within a transaction → Coroutine[bytes].
#[pyfunction]
pub(crate) fn execute_in_transaction<'py>(
    py: Python<'py>,
    pool_name: String,
    tx_id: u64,
    ir_bytes: &Bound<'py, PyBytes>,
) -> PyResult<Bound<'py, PyAny>> {
    let ir_data = ir_bytes.as_bytes().to_vec();

    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        let ir = decode_ir(&ir_data)?;

        let backend = driver_pool_backend(&pool_name)
            .await
            .map_err(|e| crate::errors::driver_err(&e))?;
        let dialect = backend_to_dialect(backend);

        let (sql, params) =
            build_sql(&ir, dialect).map_err(|e| PyErr::new::<PyRuntimeError, _>(e.to_string()))?;

        let results = match ir.op {
            oxyde_codec::Operation::Select | oxyde_codec::Operation::Raw => {
                let column_types = decode_column_types(&ir);
                let (result, _num_rows) = if let Some(relations) = relations_for(&ir) {
                    execute_query_columnar_dedup_in_transaction(
                        tx_id,
                        &sql,
                        &params,
                        column_types.as_deref(),
                        &relations,
                    )
                    .await
                    .map_err(|e| crate::errors::driver_err(&e))?
                } else {
                    execute_query_columnar_in_transaction(
                        tx_id,
                        &sql,
                        &params,
                        column_types.as_deref(),
                    )
                    .await
                    .map_err(|e| crate::errors::driver_err(&e))?
                };
                result
            }
            oxyde_codec::Operation::Insert => {
                if emits_returning(&ir, dialect) {
                    execute_mutation_returning_in_transaction(
                        tx_id,
                        &sql,
                        &params,
                        ir.column_types.as_ref(),
                    )
                    .await
                    .map_err(|e| crate::errors::driver_err(&e))?
                } else {
                    let pk_column = ir.pk_column.as_deref();
                    let ids =
                        execute_insert_returning_in_transaction(tx_id, &sql, &params, pk_column)
                            .await
                            .map_err(|e| crate::errors::driver_err(&e))?;

                    encode_insert_result(ids.len(), &ids)
                }
            }
            oxyde_codec::Operation::Update | oxyde_codec::Operation::Delete => {
                if emits_returning(&ir, dialect) {
                    execute_mutation_returning_in_transaction(
                        tx_id,
                        &sql,
                        &params,
                        ir.column_types.as_ref(),
                    )
                    .await
                    .map_err(|e| crate::errors::driver_err(&e))?
                } else {
                    let affected = execute_statement_in_transaction(tx_id, &sql, &params)
                        .await
                        .map_err(|e| crate::errors::driver_err(&e))?;

                    encode_mutation_result(affected)
                }
            }
        };

        Ok(results)
    })
}

/// Render SQL from IR without executing (requires pool for dialect detection) → Coroutine[(str, list)].
#[pyfunction]
pub(crate) fn render_sql<'py>(
    py: Python<'py>,
    pool_name: String,
    ir_bytes: &Bound<'py, PyBytes>,
) -> PyResult<Bound<'py, PyAny>> {
    let ir_data = ir_bytes.as_bytes().to_vec();
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        let ir = decode_ir(&ir_data)?;

        let backend = driver_pool_backend(&pool_name)
            .await
            .map_err(|e| crate::errors::driver_err(&e))?;
        let dialect = backend_to_dialect(backend);

        let (sql, params) =
            build_sql(&ir, dialect).map_err(|e| PyErr::new::<PyRuntimeError, _>(e.to_string()))?;

        Python::attach(|py| -> PyResult<(String, Vec<Py<PyAny>>)> {
            let params_vec: Vec<Py<PyAny>> = params.iter().map(|v| value_to_py(py, v)).collect();
            Ok((sql, params_vec))
        })
    })
}

/// Render SQL from IR without pool (sync, defaults to Postgres dialect) → (str, list).
///
/// When `with_types` is true, params are returned as list of (type_tag, value) tuples
/// instead of plain values. This allows tests to verify the exact sea_query Value
/// variant used for parameter binding.
#[pyfunction]
#[pyo3(signature = (ir_bytes, dialect_name=None, with_types=false))]
pub(crate) fn render_sql_debug<'py>(
    py: Python<'py>,
    ir_bytes: &Bound<'py, PyBytes>,
    dialect_name: Option<&str>,
    with_types: bool,
) -> PyResult<Bound<'py, PyTuple>> {
    let ir_data = ir_bytes.as_bytes();

    let ir = decode_ir(ir_data)?;

    // Parse dialect name (default to Postgres)
    let dialect = match dialect_name {
        Some("postgres" | "postgresql") => Dialect::Postgres,
        None => Dialect::Postgres, // Default
        Some(other) => {
            return Err(PyErr::new::<PyValueError, _>(format!(
                "Unknown dialect '{other}'. Use 'postgres'"
            )))
        }
    };

    let (sql, params) =
        build_sql(&ir, dialect).map_err(|e| PyErr::new::<PyRuntimeError, _>(e.to_string()))?;

    let sql_obj = PyString::new(py, &sql);
    let params_obj = if with_types {
        values_to_py_tagged(py, &params)?
    } else {
        values_to_py(py, &params)?
    };
    PyTuple::new(py, &[sql_obj.into_any(), params_obj])
}

/// Run EXPLAIN (ANALYZE) on a query, return plan as JSON or text → Coroutine.
#[pyfunction]
pub(crate) fn explain<'py>(
    py: Python<'py>,
    pool_name: String,
    ir_bytes: &Bound<'py, PyBytes>,
    analyze: Option<bool>,
    format: Option<String>,
) -> PyResult<Bound<'py, PyAny>> {
    let ir_data = ir_bytes.as_bytes().to_vec();
    let format_token = format.unwrap_or_else(|| "text".to_string());
    let explain_format = if format_token.eq_ignore_ascii_case("json") {
        ExplainFormat::Json
    } else {
        ExplainFormat::Text
    };
    let explain_options = ExplainOptions {
        analyze: analyze.unwrap_or(false),
        format: explain_format,
    };

    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        let ir = decode_ir(&ir_data)?;

        let backend = driver_pool_backend(&pool_name)
            .await
            .map_err(|e| crate::errors::driver_err(&e))?;
        let dialect = backend_to_dialect(backend);

        let (sql, params) =
            build_sql(&ir, dialect).map_err(|e| PyErr::new::<PyRuntimeError, _>(e.to_string()))?;

        let plan = explain_query(&pool_name, &sql, &params, explain_options)
            .await
            .map_err(|e| crate::errors::driver_err(&e))?;

        Python::attach(|py| json_to_py(py, &plan))
    })
}

#[cfg(test)]
mod tests {
    use super::{decode_column_types, relations_for};
    use oxyde_codec::{
        Aggregate, AggregateOp, Annotation, ColumnTypeSpec, JoinColumn, JoinSpec, QueryIR,
        ScalarExpr,
    };
    use std::collections::HashMap;

    #[test]
    fn only_joined_row_queries_use_relation_encoding() {
        let mut ir = QueryIR::default();
        assert!(relations_for(&ir).is_none());

        ir.joins = Some(vec![JoinSpec {
            path: "author".into(),
            alias: "author".into(),
            parent: None,
            table: "authors".into(),
            source_column: "author_id".into(),
            target_column: "author_pk".into(),
            pk_column: None,
            result_prefix: "author".into(),
            columns: vec![
                JoinColumn {
                    field: "name".into(),
                    column: "name".into(),
                },
                JoinColumn {
                    field: "id".into(),
                    column: "author_pk".into(),
                },
            ],
        }]);
        // The result label "author__name" may belong to a base column of
        // another type; the joined column is typed by "author.name".
        ir.column_types = Some(HashMap::from([
            ("author__name".into(), ColumnTypeSpec::BigInteger),
            ("author.name".into(), ColumnTypeSpec::Text),
        ]));
        let relations = relations_for(&ir).unwrap();
        assert_eq!(relations.len(), 1);
        assert_eq!(relations[0].prefix, "author");
        assert_eq!(relations[0].pk_index, 1);
        assert_eq!(relations[0].field_names, ["name", "id"]);
        assert_eq!(
            relations[0].column_types,
            vec![Some(ColumnTypeSpec::Text), None]
        );

        ir.count = Some(true);
        assert!(relations_for(&ir).is_none());
        ir.count = None;
        ir.exists = Some(true);
        assert!(relations_for(&ir).is_none());
        ir.exists = None;

        ir.group_by = Some(vec!["author.author_pk".into()]);
        assert!(relations_for(&ir).is_none());
        ir.group_by = None;

        ir.aggregates = Some(vec![Aggregate {
            op: AggregateOp::Sum,
            field: Some("views".into()),
            alias: Some("total".into()),
            distinct: None,
        }]);
        assert!(relations_for(&ir).is_none());
        ir.aggregates = Some(vec![]);
        assert!(relations_for(&ir).is_none());
    }

    #[test]
    fn relation_identity_uses_the_primary_key_when_joining_an_alternate_key() {
        let ir = QueryIR {
            joins: Some(vec![JoinSpec {
                path: "author".into(),
                alias: "author".into(),
                parent: None,
                table: "authors".into(),
                source_column: "author_tags".into(),
                target_column: "tags".into(),
                pk_column: Some("author_pk".into()),
                result_prefix: "author".into(),
                columns: vec![
                    JoinColumn {
                        field: "tags".into(),
                        column: "tags".into(),
                    },
                    JoinColumn {
                        field: "id".into(),
                        column: "author_pk".into(),
                    },
                ],
            }]),
            ..Default::default()
        };
        let relations = relations_for(&ir).unwrap();
        assert_eq!(relations[0].pk_index, 1);
        assert_eq!(relations[0].field_names[relations[0].pk_index], "id");
    }

    fn agg(op: AggregateOp, field: &str, alias: &str) -> Aggregate {
        Aggregate {
            op,
            field: Some(field.into()),
            alias: Some(alias.into()),
            distinct: None,
        }
    }

    #[test]
    fn counted_and_summed_aliases_use_runtime_decode_types() {
        let ir = QueryIR {
            column_types: Some(HashMap::from([
                ("author__name".into(), ColumnTypeSpec::Text),
                ("views".into(), ColumnTypeSpec::BigInteger),
                ("duration".into(), ColumnTypeSpec::Timedelta),
                ("author.name".into(), ColumnTypeSpec::Text),
            ])),
            aggregates: Some(vec![
                agg(AggregateOp::Count, "author.name", "author__name"),
                agg(AggregateOp::Sum, "views", "views"),
                agg(AggregateOp::Avg, "duration", "duration"),
            ]),
            ..Default::default()
        };

        let decode_types = decode_column_types(&ir).unwrap();
        assert!(!decode_types.contains_key("author__name"));
        assert!(!decode_types.contains_key("views"));
        assert!(!decode_types.contains_key("duration"));
        assert_eq!(decode_types.get("author.name"), Some(&ColumnTypeSpec::Text));
        assert_eq!(
            ir.column_types.as_ref().unwrap().get("views"),
            Some(&ColumnTypeSpec::BigInteger),
        );
    }

    #[test]
    fn min_and_max_aliases_decode_as_their_input() {
        let ir = QueryIR {
            column_types: Some(HashMap::from([
                ("duration".into(), ColumnTypeSpec::Timedelta),
                ("started".into(), ColumnTypeSpec::Timedelta),
                ("label".into(), ColumnTypeSpec::Text),
                ("author.score".into(), ColumnTypeSpec::Double),
                ("author.duration".into(), ColumnTypeSpec::Timedelta),
            ])),
            aggregates: Some(vec![
                agg(AggregateOp::Max, "duration", "duration"),
                agg(AggregateOp::Min, "started", "started"),
                agg(AggregateOp::Max, "author.score", "label"),
                agg(AggregateOp::Max, "author.duration", "longest"),
                agg(AggregateOp::Sum, "author.duration", "total"),
            ]),
            ..Default::default()
        };

        let decode_types = decode_column_types(&ir).unwrap();
        assert_eq!(
            decode_types.get("duration"),
            Some(&ColumnTypeSpec::Timedelta)
        );
        assert_eq!(
            decode_types.get("started"),
            Some(&ColumnTypeSpec::Timedelta)
        );
        assert_eq!(decode_types.get("label"), Some(&ColumnTypeSpec::Double));
        assert_eq!(
            decode_types.get("longest"),
            Some(&ColumnTypeSpec::Timedelta)
        );
        assert!(!decode_types.contains_key("total"));
    }

    #[test]
    fn count_and_exists_use_runtime_decode_types() {
        let mut ir = QueryIR {
            column_types: Some(HashMap::from([
                ("_count".into(), ColumnTypeSpec::Date),
                ("elapsed_us".into(), ColumnTypeSpec::Timedelta),
            ])),
            count: Some(true),
            ..Default::default()
        };
        assert!(decode_column_types(&ir).is_none());

        ir.count = None;
        ir.exists = Some(true);
        ir.cols = Some(vec!["elapsed_us".into()]);
        ir.group_by = ir.cols.clone();
        ir.output_labels = Some(HashMap::from([("elapsed_us".into(), "exists".into())]));
        assert!(decode_column_types(&ir).is_none());
    }

    #[test]
    fn grouped_keys_decode_under_their_output_labels() {
        let ir = QueryIR {
            table: "items".into(),
            column_types: Some(HashMap::from([
                ("items.supplier__name".into(), ColumnTypeSpec::BigInteger),
                ("supplier.name".into(), ColumnTypeSpec::Text),
            ])),
            group_by: Some(vec!["items.supplier__name".into(), "supplier.name".into()]),
            aggregates: Some(vec![agg(AggregateOp::Count, "id", "n")]),
            output_labels: Some(HashMap::from([
                ("items.supplier__name".into(), "cached_supplier".into()),
                ("supplier.name".into(), "supplier__name".into()),
            ])),
            ..Default::default()
        };

        let decode_types = decode_column_types(&ir).unwrap();
        assert_eq!(
            decode_types.get("cached_supplier"),
            Some(&ColumnTypeSpec::BigInteger)
        );
        assert_eq!(
            decode_types.get("supplier__name"),
            Some(&ColumnTypeSpec::Text)
        );
    }

    #[test]
    fn grouped_scalar_alias_decodes_independently_of_its_namesake_column() {
        let mut ir = QueryIR {
            table: "posts".into(),
            column_types: Some(HashMap::from([
                ("bucket".into(), ColumnTypeSpec::Timedelta),
                ("name".into(), ColumnTypeSpec::Text),
            ])),
            cols: Some(vec!["posts.bucket".into()]),
            group_by: Some(vec!["posts.bucket".into(), "bucket".into()]),
            output_labels: Some(HashMap::from([("posts.bucket".into(), "duration".into())])),
            annotations: Some(vec![Annotation {
                alias: "bucket".into(),
                expr: ScalarExpr::Column {
                    name: "name".into(),
                },
            }]),
            ..Default::default()
        };

        let types = decode_column_types(&ir).unwrap();
        assert_eq!(types.get("duration"), Some(&ColumnTypeSpec::Timedelta));
        assert_eq!(types.get("bucket"), Some(&ColumnTypeSpec::Text));

        ir.annotations.as_mut().unwrap()[0].expr = ScalarExpr::Raw { sql: "42".into() };
        let types = decode_column_types(&ir).unwrap();
        assert_eq!(types.get("duration"), Some(&ColumnTypeSpec::Timedelta));
        assert!(!types.contains_key("bucket"));
    }
}
