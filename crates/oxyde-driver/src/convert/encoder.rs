//! CellEncoder trait and generic columnar encoding.
//!
//! Encodes database rows directly to msgpack bytes without intermediate
//! serde_json::Value. Each backend implements CellEncoder with its
//! own type-specific decoding; the generic functions handle the
//! columnar structure.

use std::collections::{HashMap, HashSet};

use oxyde_codec::ColumnTypeSpec;
use sqlx::{Column, Database, Row};

/// Column metadata for encoding.
#[derive(Debug, Clone)]
pub struct ColumnMeta {
    pub name: String,
    pub db_type: String,
    /// Typed column spec from `QueryIR.column_types`. `None` for columns
    /// outside the model (raw SQL, aggregate aliases) — those fall back to
    /// the runtime DB type.
    pub spec: Option<ColumnTypeSpec>,
    pub result_encoding: Option<ResultEncoding>,
}

/// Result-only overrides for projections whose SQL type alone is insufficient.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultEncoding {
    Wire,
    Runtime,
    DurationMicros,
    IntegerSum,
    Float,
    Decimal,
}

#[derive(Clone, Copy)]
pub struct ResultOptions<'a> {
    pub emit_kinds: bool,
    pub encodings: Option<&'a HashMap<String, ResultEncoding>>,
}

impl From<bool> for ResultOptions<'_> {
    fn from(emit_kinds: bool) -> Self {
        Self {
            emit_kinds,
            encodings: None,
        }
    }
}

/// Backend-specific cell encoder.
///
/// Implementations decode a single cell from an sqlx Row and write
/// it directly to a msgpack buffer via `rmp::encode::write_*`.
pub trait CellEncoder {
    type Row: sqlx::Row;

    /// Extract column metadata from the first row.
    fn extract_columns(
        row: &Self::Row,
        col_types: Option<&HashMap<String, ColumnTypeSpec>>,
    ) -> Vec<ColumnMeta>
    where
        <<Self::Row as Row>::Database as Database>::Column: Column,
    {
        row.columns()
            .iter()
            .map(|c| {
                let name = Column::name(c).to_string();
                let spec = col_types.and_then(|ct| ct.get(&name).cloned());
                ColumnMeta {
                    db_type: Column::type_info(c).to_string().to_uppercase(),
                    spec,
                    name,
                    result_encoding: None,
                }
            })
            .collect()
    }

    /// Try to encode a cell using the typed column spec.
    /// Returns `true` if the spec was recognized and encoded;
    /// `false` (for `Unknown`) falls through to the db_type path.
    fn try_encode_by_spec(
        buf: &mut Vec<u8>,
        row: &Self::Row,
        idx: usize,
        spec: &ColumnTypeSpec,
    ) -> bool;

    /// Encode a cell using the database column type (fallback).
    fn encode_by_db_type(buf: &mut Vec<u8>, row: &Self::Row, idx: usize, db_type: &str);

    /// Name the Python conversion for a column whose MessagePack type is lossy.
    fn result_kind(col: &ColumnMeta) -> Option<&'static str>;

    /// Encode a cell and report any conversion kind discovered from its value.
    fn encode_cell(
        buf: &mut Vec<u8>,
        row: &Self::Row,
        idx: usize,
        col: &ColumnMeta,
    ) -> Option<&'static str> {
        if let Some(spec) = &col.spec {
            if Self::try_encode_by_spec(buf, row, idx, spec) {
                return None;
            }
        }
        Self::encode_by_db_type(buf, row, idx, &col.db_type);
        None
    }
}

// ── msgpack write helpers ──────────────────────────────────────────────
// Vec<u8> Write impl never fails, so unwrap is safe.

#[inline]
pub fn write_nil(buf: &mut Vec<u8>) {
    rmp::encode::write_nil(buf).unwrap();
}

#[inline]
pub fn write_bool(buf: &mut Vec<u8>, v: bool) {
    rmp::encode::write_bool(buf, v).unwrap();
}

#[inline]
pub fn write_i64(buf: &mut Vec<u8>, v: i64) {
    rmp::encode::write_sint(buf, v).unwrap();
}

#[inline]
pub fn write_u64(buf: &mut Vec<u8>, v: u64) {
    rmp::encode::write_uint(buf, v).unwrap();
}

#[inline]
pub fn write_f64(buf: &mut Vec<u8>, v: f64) {
    rmp::encode::write_f64(buf, v).unwrap();
}

#[inline]
pub fn write_str(buf: &mut Vec<u8>, v: &str) {
    rmp::encode::write_str(buf, v).unwrap();
}

#[inline]
pub fn write_bin(buf: &mut Vec<u8>, v: &[u8]) {
    rmp::encode::write_bin(buf, v).unwrap();
}

#[inline]
pub fn write_array_len(buf: &mut Vec<u8>, len: u32) {
    rmp::encode::write_array_len(buf, len).unwrap();
}

#[inline]
pub fn write_map_len(buf: &mut Vec<u8>, len: u32) {
    rmp::encode::write_map_len(buf, len).unwrap();
}

/// Write a serde_json::Value as msgpack (for JSON columns).
pub fn write_json_value(buf: &mut Vec<u8>, val: &serde_json::Value) {
    match val {
        serde_json::Value::Null => write_nil(buf),
        serde_json::Value::Bool(b) => write_bool(buf, *b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                write_i64(buf, i);
            } else if let Some(u) = n.as_u64() {
                write_u64(buf, u);
            } else if let Some(f) = n.as_f64() {
                write_f64(buf, f);
            } else {
                write_nil(buf);
            }
        }
        serde_json::Value::String(s) => write_str(buf, s),
        serde_json::Value::Array(arr) => {
            write_array_len(buf, arr.len() as u32);
            for v in arr {
                write_json_value(buf, v);
            }
        }
        serde_json::Value::Object(map) => {
            write_map_len(buf, map.len() as u32);
            for (k, v) in map {
                write_str(buf, k);
                write_json_value(buf, v);
            }
        }
    }
}

/// Write an rmpv::Value as msgpack (for PK values from INSERT RETURNING).
pub fn write_rmpv_value(buf: &mut Vec<u8>, val: &rmpv::Value) {
    match val {
        rmpv::Value::Nil => write_nil(buf),
        rmpv::Value::Boolean(b) => write_bool(buf, *b),
        rmpv::Value::Integer(n) => {
            if let Some(i) = n.as_i64() {
                write_i64(buf, i);
            } else if let Some(u) = n.as_u64() {
                write_u64(buf, u);
            } else {
                write_nil(buf);
            }
        }
        rmpv::Value::F32(f) => write_f64(buf, f64::from(*f)),
        rmpv::Value::F64(f) => write_f64(buf, *f),
        rmpv::Value::String(s) => write_str(buf, s.as_str().unwrap_or_default()),
        rmpv::Value::Binary(b) => write_bin(buf, b),
        rmpv::Value::Array(arr) => {
            write_array_len(buf, arr.len() as u32);
            for v in arr {
                write_rmpv_value(buf, v);
            }
        }
        rmpv::Value::Map(pairs) => {
            write_map_len(buf, pairs.len() as u32);
            for (k, v) in pairs {
                write_rmpv_value(buf, k);
                write_rmpv_value(buf, v);
            }
        }
        rmpv::Value::Ext(_, _) => write_nil(buf),
    }
}

// ── Generic encode functions ───────────────────────────────────────────
use futures::StreamExt;

/// Info about a JOIN relation for dedup encoding.
///
/// A joined SELECT projects each relation's columns after the main columns,
/// in relation order. The encoder assigns them by position, so a main column
/// whose name starts with `{prefix}__` stays in the main rows, and a nested
/// relation's columns are never taken by its parent.
pub struct RelationInfo {
    /// Column prefix in result set, e.g. "author"
    pub prefix: String,
    /// Position of the relation's primary key among its columns.
    pub pk_index: usize,
    /// Model field names in projection order.
    pub field_names: Vec<String>,
    /// Type of each of the relation's columns, in projection order. The
    /// length is the relation's column count; `None` decodes by runtime type.
    pub column_types: Vec<Option<ColumnTypeSpec>>,
    /// Positions that preserve their underlying wire values.
    pub text_columns: HashSet<usize>,
}

/// Msgpack nil byte (0xc0) — used to detect NULL PKs from LEFT JOINs.
const MSGPACK_NIL: u8 = 0xc0;

/// Encode a single cell into a small buffer (for PK extraction).
struct RelGroup<'a> {
    prefix: &'a str,
    pk_col_idx: usize,
    col_indices: Vec<usize>,
    col_metas: Vec<ColumnMeta>,
    field_names: &'a [String],
    seen: std::collections::HashSet<Vec<u8>>,
    data_buf: Vec<u8>,
    refs_buf: Vec<u8>,
}

fn encode_pk_cell<E: CellEncoder>(row: &E::Row, col_idx: usize, col_meta: &ColumnMeta) -> Vec<u8> {
    let mut pk_buf = Vec::with_capacity(16);
    let _ = E::encode_cell(&mut pk_buf, row, col_idx, col_meta);
    pk_buf
}

fn write_kinds(buf: &mut Vec<u8>, kinds: impl Iterator<Item = Option<&'static str>>) {
    let kinds: Vec<_> = kinds.collect();
    if kinds.iter().any(Option::is_some) {
        write_array_len(buf, kinds.len() as u32);
        for kind in kinds {
            match kind {
                Some(kind) => write_str(buf, kind),
                None => write_nil(buf),
            }
        }
    } else {
        write_nil(buf);
    }
}

/// Encode rows from a stream using columnar format (and optionally dedup for JOINs).
///
/// Returns `(msgpack_bytes, row_count)`.
pub async fn encode_stream<'a, E, S>(
    mut stream: S,
    col_types: Option<&HashMap<String, ColumnTypeSpec>>,
    relations: Option<&[RelationInfo]>,
    options: impl Into<ResultOptions<'a>>,
) -> Result<(Vec<u8>, usize), sqlx::Error>
where
    E: CellEncoder,
    S: futures::Stream<Item = Result<E::Row, sqlx::Error>> + Unpin,
    <<E::Row as Row>::Database as Database>::Column: Column,
{
    let ResultOptions {
        emit_kinds,
        encodings,
    } = options.into();
    let rels = relations.unwrap_or(&[]);
    let has_rels = !rels.is_empty();
    let result_len = if emit_kinds {
        4
    } else {
        2 + u32::from(has_rels)
    };
    let first_row = match stream.next().await {
        Some(Ok(row)) => row,
        Some(Err(e)) => return Err(e),
        None => {
            let mut buf = Vec::with_capacity(8);
            write_array_len(&mut buf, result_len);
            write_array_len(&mut buf, 0);
            write_array_len(&mut buf, 0);
            if has_rels {
                write_map_len(&mut buf, 0);
            } else if emit_kinds {
                write_nil(&mut buf);
            }
            if emit_kinds {
                write_nil(&mut buf);
            }
            return Ok((buf, 0));
        }
    };

    let mut all_columns = E::extract_columns(&first_row, col_types);
    if let Some(encodings) = encodings {
        for column in &mut all_columns {
            column.result_encoding = encodings.get(&column.name).copied();
        }
    }
    let mut main_indices: Vec<usize> = Vec::new();
    let mut main_columns = Vec::new();

    let mut rel_groups: Vec<RelGroup<'_>> = Vec::new();

    if has_rels {
        let relation_width: usize = rels.iter().map(|r| r.column_types.len()).sum();
        let Some(main_width) = all_columns.len().checked_sub(relation_width) else {
            return Err(sqlx::Error::Decode(
                format!(
                    "joined result has {} columns, fewer than the {relation_width} its relations project",
                    all_columns.len()
                )
                .into(),
            ));
        };
        for (idx, col) in all_columns.iter().enumerate().take(main_width) {
            main_indices.push(idx);
            main_columns.push((col, None));
        }

        let mut start = main_width;
        for ri in rels {
            let width = ri.column_types.len();
            if ri.pk_index >= width {
                return Err(sqlx::Error::ColumnNotFound(format!(
                    "{}__<primary key>",
                    ri.prefix
                )));
            }
            if ri.field_names.len() != width {
                return Err(sqlx::Error::Decode(
                    format!(
                        "relation '{}' has {} field names for its {width} columns",
                        ri.prefix,
                        ri.field_names.len()
                    )
                    .into(),
                ));
            }
            let col_indices: Vec<usize> = (start..start + width).collect();
            let col_metas = col_indices
                .iter()
                .zip(&ri.column_types)
                .enumerate()
                .map(|(pos, (&idx, spec))| ColumnMeta {
                    name: all_columns[idx].name.clone(),
                    db_type: all_columns[idx].db_type.clone(),
                    spec: spec.clone(),
                    result_encoding: ri
                        .text_columns
                        .contains(&pos)
                        .then_some(ResultEncoding::Wire),
                })
                .collect();
            rel_groups.push(RelGroup {
                prefix: &ri.prefix,
                pk_col_idx: start + ri.pk_index,
                col_indices,
                col_metas,
                field_names: &ri.field_names,
                seen: std::collections::HashSet::new(),
                data_buf: Vec::new(),
                refs_buf: Vec::new(),
            });
            start += width;
        }
    } else {
        for (idx, col) in all_columns.iter().enumerate() {
            main_indices.push(idx);
            main_columns.push((col, None));
        }
    }

    let mut main_rows_buf = Vec::new();
    let mut num_rows = 0;

    let mut process_row = |row: &E::Row, num_rows: &mut usize| {
        *num_rows += 1;

        write_array_len(&mut main_rows_buf, main_indices.len() as u32);
        for (pos, &idx) in main_indices.iter().enumerate() {
            let (col, kind) = &mut main_columns[pos];
            if let Some(value_kind) = E::encode_cell(&mut main_rows_buf, row, idx, col) {
                *kind = Some(value_kind);
            }
        }

        if has_rels {
            for group in &mut rel_groups {
                let pk_pos = group.pk_col_idx - group.col_indices[0];
                let pk_bytes = encode_pk_cell::<E>(row, group.pk_col_idx, &group.col_metas[pk_pos]);

                if pk_bytes.len() == 1 && pk_bytes[0] == MSGPACK_NIL {
                    write_nil(&mut group.refs_buf);
                } else {
                    if group.seen.insert(pk_bytes.clone()) {
                        group.data_buf.extend_from_slice(&pk_bytes);
                        write_array_len(&mut group.data_buf, group.col_indices.len() as u32);
                        for (pos, &idx) in group.col_indices.iter().enumerate() {
                            let _ = E::encode_cell(
                                &mut group.data_buf,
                                row,
                                idx,
                                &group.col_metas[pos],
                            );
                        }
                    }
                    group.refs_buf.extend_from_slice(&pk_bytes);
                }
            }
        }
    };

    process_row(&first_row, &mut num_rows);

    while let Some(row_res) = stream.next().await {
        let row = row_res?;
        process_row(&row, &mut num_rows);
    }

    let mut buf = Vec::with_capacity(main_rows_buf.len() + 1024);
    write_array_len(&mut buf, result_len);

    write_array_len(&mut buf, main_columns.len() as u32);
    for (col, _) in &main_columns {
        write_str(&mut buf, &col.name);
    }

    write_array_len(&mut buf, num_rows as u32);
    buf.extend_from_slice(&main_rows_buf);

    if has_rels {
        write_map_len(&mut buf, rel_groups.len() as u32);
        for group in rel_groups {
            write_str(&mut buf, group.prefix);
            write_map_len(&mut buf, 3 + u32::from(emit_kinds));

            write_str(&mut buf, "columns");
            write_array_len(&mut buf, group.field_names.len() as u32);
            for name in group.field_names {
                write_str(&mut buf, name);
            }

            write_str(&mut buf, "data");
            write_map_len(&mut buf, group.seen.len() as u32);
            buf.extend_from_slice(&group.data_buf);

            write_str(&mut buf, "refs");
            write_array_len(&mut buf, num_rows as u32);
            buf.extend_from_slice(&group.refs_buf);

            if emit_kinds {
                write_str(&mut buf, "kinds");
                write_kinds(&mut buf, group.col_metas.iter().map(E::result_kind));
            }
        }
    } else if emit_kinds {
        write_nil(&mut buf);
    }
    if emit_kinds {
        write_kinds(
            &mut buf,
            main_columns
                .iter()
                .map(|(col, kind)| kind.or_else(|| E::result_kind(col))),
        );
    }

    Ok((buf, num_rows))
}

/// Encode rows from a stream as a mutation-with-returning result:
/// `{"affected": N, "columns": [...], "rows": [[...], ...]}`
pub async fn encode_stream_mutation_returning<E, S>(
    mut stream: S,
    col_types: Option<&HashMap<String, ColumnTypeSpec>>,
) -> Result<Vec<u8>, sqlx::Error>
where
    E: CellEncoder,
    S: futures::Stream<Item = Result<E::Row, sqlx::Error>> + Unpin,
    <<E::Row as Row>::Database as Database>::Column: Column,
{
    let mut num_rows: u32 = 0;

    let first_row = match stream.next().await {
        Some(Ok(row)) => row,
        Some(Err(e)) => return Err(e),
        None => {
            let mut buf = Vec::with_capacity(32);
            write_map_len(&mut buf, 3);
            write_str(&mut buf, "affected");
            write_u64(&mut buf, 0);
            write_str(&mut buf, "columns");
            write_array_len(&mut buf, 0);
            write_str(&mut buf, "rows");
            write_array_len(&mut buf, 0);
            return Ok(buf);
        }
    };

    let columns = E::extract_columns(&first_row, col_types);
    num_rows += 1;

    let mut rows_buf = Vec::new();
    write_array_len(&mut rows_buf, columns.len() as u32);
    for (i, col) in columns.iter().enumerate() {
        let _ = E::encode_cell(&mut rows_buf, &first_row, i, col);
    }

    while let Some(row_res) = stream.next().await {
        let row = row_res?;
        num_rows += 1;
        write_array_len(&mut rows_buf, columns.len() as u32);
        for (i, col) in columns.iter().enumerate() {
            let _ = E::encode_cell(&mut rows_buf, &row, i, col);
        }
    }

    let mut buf = Vec::with_capacity(rows_buf.len() + 128);
    write_map_len(&mut buf, 3);
    write_str(&mut buf, "affected");
    write_u64(&mut buf, u64::from(num_rows));
    write_str(&mut buf, "columns");

    write_array_len(&mut buf, columns.len() as u32);
    for col in &columns {
        write_str(&mut buf, &col.name);
    }

    write_str(&mut buf, "rows");
    write_array_len(&mut buf, num_rows);
    buf.extend_from_slice(&rows_buf);

    Ok(buf)
}
