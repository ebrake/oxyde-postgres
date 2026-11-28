//! CellEncoder trait and generic columnar encoding.
//!
//! Writes decoded cells to MessagePack one at a time. Backend-specific
//! decoding and the columnar result layout are kept separate.

use std::collections::{HashMap, HashSet};

use oxyde_codec::ColumnTypeSpec;
use sqlx::{Column, Database, Row};

use super::cell::{ArrayItem, Cell};

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

/// Backend-specific result cell decoding.
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

    /// Name the Python conversion for a column whose MessagePack type is lossy.
    fn result_kind(col: &ColumnMeta) -> Option<&'static str>;

    /// Decode one cell and report any conversion kind discovered from its value.
    fn decode_cell(row: &Self::Row, idx: usize, col: &ColumnMeta) -> (Cell, Option<&'static str>);
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

/// Write a decoded cell using the legacy MessagePack representation.
#[inline]
pub fn write_cell(buf: &mut Vec<u8>, cell: &Cell) {
    match cell {
        Cell::Null => write_nil(buf),
        Cell::Bool(v) => write_bool(buf, *v),
        Cell::Int(v) => write_i64(buf, *v),
        Cell::UInt(v) => write_u64(buf, *v),
        Cell::Float(v) => write_f64(buf, *v),
        Cell::Str(v) => write_str(buf, v),
        Cell::DecimalText(v) => write_str(buf, v),
        Cell::Bytes(v) => write_bin(buf, v),
        Cell::Duration(micros) => write_f64(buf, *micros as f64 / 1_000_000.0),
        Cell::Json(value) => write_json_value(buf, value),
        Cell::Array(values) => {
            write_array_len(buf, values.len() as u32);
            values.for_each(|item| match item {
                ArrayItem::Cell(value) => write_cell(buf, value),
                ArrayItem::Str(value) => write_str(buf, value),
                ArrayItem::Bytes(value) => write_bin(buf, value),
                ArrayItem::Json(value) => write_json_value(buf, value),
            });
        }
        Cell::Uuid(v) => write_str(
            buf,
            v.hyphenated()
                .encode_lower(&mut uuid::Uuid::encode_buffer()),
        ),
        Cell::Decimal(_)
        | Cell::DateTime(_)
        | Cell::DateTimeTz(_)
        | Cell::Date(_)
        | Cell::Time(_)
        | Cell::TimeTz(_, _)
        | Cell::DurationAggregate(_) => {
            write_str(buf, &cell.wire_text().expect("typed string cell"));
        }
    }
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

struct RelationLayout<'a> {
    prefix: &'a str,
    start: usize,
    pk_index: usize,
    columns: Vec<ColumnMeta>,
    field_names: &'a [String],
}

impl RelationLayout<'_> {
    fn primary_key<E: CellEncoder>(&self, row: &E::Row) -> Option<(Cell, Vec<u8>)> {
        let (pk, _) = E::decode_cell(
            row,
            self.start + self.pk_index,
            &self.columns[self.pk_index],
        );
        let bytes = encode_primary_key(&pk)?;
        Some((pk, bytes))
    }

    fn other_cells<'a, E: CellEncoder>(
        &'a self,
        row: &'a E::Row,
    ) -> impl Iterator<Item = Cell> + 'a {
        self.columns
            .iter()
            .enumerate()
            .filter(|(position, _)| *position != self.pk_index)
            .map(|(position, column)| E::decode_cell(row, self.start + position, column).0)
    }
}

struct ResultLayout<'a> {
    columns: Vec<ColumnMeta>,
    relations: Vec<RelationLayout<'a>>,
}

fn result_layout<'a, E: CellEncoder>(
    row: &E::Row,
    col_types: Option<&HashMap<String, ColumnTypeSpec>>,
    relations: &'a [RelationInfo],
    encodings: Option<&HashMap<String, ResultEncoding>>,
) -> Result<ResultLayout<'a>, sqlx::Error>
where
    <<E::Row as Row>::Database as Database>::Column: Column,
{
    let mut columns = E::extract_columns(row, col_types);
    let relation_width: usize = relations.iter().map(|r| r.column_types.len()).sum();
    let Some(main_width) = columns.len().checked_sub(relation_width) else {
        return Err(sqlx::Error::Decode(
            format!(
                "joined result has {} columns, fewer than the {relation_width} its relations project",
                columns.len()
            )
            .into(),
        ));
    };

    let mut joined_columns = columns.drain(main_width..);
    let mut layouts = Vec::with_capacity(relations.len());
    let mut start = main_width;
    for relation in relations {
        let width = relation.column_types.len();
        if relation.pk_index >= width {
            return Err(sqlx::Error::ColumnNotFound(format!(
                "{}__<primary key>",
                relation.prefix
            )));
        }
        if relation.field_names.len() != width {
            return Err(sqlx::Error::Decode(
                format!(
                    "relation '{}' has {} field names for its {width} columns",
                    relation.prefix,
                    relation.field_names.len()
                )
                .into(),
            ));
        }
        let columns = joined_columns
            .by_ref()
            .take(width)
            .zip(&relation.column_types)
            .enumerate()
            .map(|(position, (mut column, spec))| {
                column.spec.clone_from(spec);
                column.result_encoding = relation
                    .text_columns
                    .contains(&position)
                    .then_some(ResultEncoding::Wire);
                column
            })
            .collect();
        layouts.push(RelationLayout {
            prefix: &relation.prefix,
            start,
            pk_index: relation.pk_index,
            columns,
            field_names: &relation.field_names,
        });
        start += width;
    }
    drop(joined_columns);
    if let Some(encodings) = encodings {
        for column in &mut columns {
            column.result_encoding = encodings.get(&column.name).copied();
        }
    }
    Ok(ResultLayout {
        columns,
        relations: layouts,
    })
}

#[derive(Default)]
struct EncodedRelation {
    seen: HashSet<Vec<u8>>,
    data: Vec<u8>,
    refs: Vec<u8>,
}

fn encode_primary_key(cell: &Cell) -> Option<Vec<u8>> {
    if matches!(cell, Cell::Null) {
        return None;
    }
    let mut bytes = Vec::with_capacity(16);
    write_cell(&mut bytes, cell);
    // A JSON null primary key also encodes as nil in the bytes protocol.
    (bytes.as_slice() != [0xc0]).then_some(bytes)
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

    let ResultLayout { columns, relations } =
        result_layout::<E>(&first_row, col_types, rels, encodings)?;
    let mut kinds = vec![None; columns.len()];
    let mut encoded_relations: Vec<_> = relations
        .iter()
        .map(|_| EncodedRelation::default())
        .collect();
    let mut main_rows_buf = Vec::new();
    let mut num_rows = 0;

    let mut process_row = |row: &E::Row, num_rows: &mut usize| {
        *num_rows += 1;

        write_array_len(&mut main_rows_buf, columns.len() as u32);
        for (idx, (col, kind)) in columns.iter().zip(&mut kinds).enumerate() {
            let (cell, value_kind) = E::decode_cell(row, idx, col);
            write_cell(&mut main_rows_buf, &cell);
            if value_kind.is_some() {
                *kind = value_kind;
            }
        }

        for (relation, encoded) in relations.iter().zip(&mut encoded_relations) {
            let Some((_, pk)) = relation.primary_key::<E>(row) else {
                write_nil(&mut encoded.refs);
                continue;
            };
            let pk_start = encoded.refs.len();
            encoded.refs.extend_from_slice(&pk);
            if encoded.seen.insert(pk) {
                let key = &encoded.refs[pk_start..];
                encoded.data.extend_from_slice(key);
                write_array_len(&mut encoded.data, relation.columns.len() as u32);
                let mut cells = relation.other_cells::<E>(row);
                for position in 0..relation.columns.len() {
                    if position == relation.pk_index {
                        encoded.data.extend_from_slice(key);
                    } else {
                        write_cell(&mut encoded.data, &cells.next().expect("related column"));
                    }
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

    write_array_len(&mut buf, columns.len() as u32);
    for col in &columns {
        write_str(&mut buf, &col.name);
    }

    write_array_len(&mut buf, num_rows as u32);
    buf.extend_from_slice(&main_rows_buf);

    if has_rels {
        write_map_len(&mut buf, relations.len() as u32);
        for (relation, encoded) in relations.iter().zip(encoded_relations) {
            write_str(&mut buf, relation.prefix);
            write_map_len(&mut buf, 3 + u32::from(emit_kinds));

            write_str(&mut buf, "columns");
            write_array_len(&mut buf, relation.field_names.len() as u32);
            for name in relation.field_names {
                write_str(&mut buf, name);
            }

            write_str(&mut buf, "data");
            write_map_len(&mut buf, encoded.seen.len() as u32);
            buf.extend_from_slice(&encoded.data);

            write_str(&mut buf, "refs");
            write_array_len(&mut buf, num_rows as u32);
            buf.extend_from_slice(&encoded.refs);

            if emit_kinds {
                write_str(&mut buf, "kinds");
                write_kinds(&mut buf, relation.columns.iter().map(E::result_kind));
            }
        }
    } else if emit_kinds {
        write_nil(&mut buf);
    }
    if emit_kinds {
        write_kinds(
            &mut buf,
            columns
                .iter()
                .zip(kinds)
                .map(|(col, kind)| kind.or_else(|| E::result_kind(col))),
        );
    }

    Ok((buf, num_rows))
}

/// SELECT rows in SQL projection order, with joined rows stored once per key.
#[derive(Default)]
pub struct RowSet {
    pub columns: Vec<String>,
    pub kinds: Vec<Option<&'static str>>,
    pub rows: Vec<Vec<Cell>>,
    pub relations: Vec<RelationRows>,
}

pub struct RelationRows {
    pub prefix: String,
    pub columns: Vec<String>,
    pub kinds: Vec<Option<&'static str>>,
    pub rows: Vec<Vec<Cell>>,
    pub refs: Vec<Option<u32>>,
}

pub(crate) async fn collect_rows<E, S>(
    mut stream: S,
    col_types: Option<&HashMap<String, ColumnTypeSpec>>,
    relations: &[RelationInfo],
    options: ResultOptions<'_>,
) -> Result<RowSet, sqlx::Error>
where
    E: CellEncoder,
    S: futures::Stream<Item = Result<E::Row, sqlx::Error>> + Unpin,
    <<E::Row as Row>::Database as Database>::Column: Column,
{
    let Some(first) = stream.next().await.transpose()? else {
        return Ok(RowSet::default());
    };
    let layout = result_layout::<E>(&first, col_types, relations, options.encodings)?;
    let result_kind = |col: &ColumnMeta| options.emit_kinds.then(|| E::result_kind(col)).flatten();
    let mut kinds: Vec<_> = layout.columns.iter().map(result_kind).collect();
    let mut result_relations: Vec<_> = layout
        .relations
        .iter()
        .map(|relation| RelationRows {
            prefix: relation.prefix.to_owned(),
            columns: relation.field_names.to_vec(),
            kinds: relation.columns.iter().map(result_kind).collect(),
            rows: Vec::new(),
            refs: Vec::new(),
        })
        .collect();
    let mut keys: Vec<HashMap<Vec<u8>, u32>> =
        layout.relations.iter().map(|_| HashMap::new()).collect();
    let mut rows = Vec::new();
    let mut next = Some(first);
    while let Some(row) = next {
        rows.push(
            layout
                .columns
                .iter()
                .zip(&mut kinds)
                .enumerate()
                .map(|(idx, (col, kind))| {
                    let (value, discovered) = E::decode_cell(&row, idx, col);
                    if options.emit_kinds && discovered.is_some() {
                        *kind = discovered;
                    }
                    value
                })
                .collect(),
        );

        for ((relation, output), keys) in layout
            .relations
            .iter()
            .zip(&mut result_relations)
            .zip(&mut keys)
        {
            let reference = relation.primary_key::<E>(&row).map(|(pk, bytes)| {
                *keys.entry(bytes).or_insert_with(|| {
                    let index = output.rows.len() as u32;
                    let mut cells: Vec<_> = relation.other_cells::<E>(&row).collect();
                    cells.insert(relation.pk_index, pk);
                    output.rows.push(cells);
                    index
                })
            });
            output.refs.push(reference);
        }
        next = stream.next().await.transpose()?;
    }
    Ok(RowSet {
        columns: layout.columns.into_iter().map(|col| col.name).collect(),
        kinds,
        rows,
        relations: result_relations,
    })
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
        write_cell(&mut rows_buf, &E::decode_cell(&first_row, i, col).0);
    }

    while let Some(row_res) = stream.next().await {
        let row = row_res?;
        num_rows += 1;
        write_array_len(&mut rows_buf, columns.len() as u32);
        for (i, col) in columns.iter().enumerate() {
            write_cell(&mut rows_buf, &E::decode_cell(&row, i, col).0);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ArrayValues;
    use chrono::{FixedOffset, NaiveDate, NaiveTime, TimeZone};
    use rust_decimal::Decimal;
    use std::borrow::Cow;
    use uuid::Uuid;

    #[test]
    fn temporal_wire_formats_match_chrono_strftime() {
        for year in [-12345, -1, 0, 1, 2026, 9999, 10000, 12345] {
            let date = NaiveDate::from_ymd_opt(year, 11, 26).unwrap();
            assert_eq!(
                Cell::Date(date).wire_text().unwrap().as_str(),
                date.format("%Y-%m-%d").to_string(),
            );
            for fraction in [0, 10, 100, 1000, 123_456_000, 123_456_789] {
                let time = NaiveTime::from_hms_nano_opt(13, 4, 5, fraction).unwrap();
                let stamp = date.and_time(time);
                assert_eq!(
                    Cell::Time(time).wire_text().unwrap().as_str(),
                    time.format("%H:%M:%S%.f").to_string(),
                );
                assert_eq!(
                    Cell::DateTime(stamp).wire_text().unwrap().as_str(),
                    stamp.format("%Y-%m-%dT%H:%M:%S%.f").to_string(),
                );
            }
        }
    }

    #[test]
    fn encoded_nil_primary_keys_are_missing() {
        assert!(encode_primary_key(&Cell::Null).is_none());
        assert!(encode_primary_key(&Cell::Json(serde_json::Value::Null)).is_none());
        assert_eq!(encode_primary_key(&Cell::Int(1)), Some(vec![1]));
    }

    #[test]
    fn decoded_cells_keep_the_legacy_messagepack_bytes() {
        let date = NaiveDate::from_ymd_opt(2026, 11, 26).unwrap();
        let time = NaiveTime::from_hms_micro_opt(13, 4, 5, 6).unwrap();
        let stamp = date.and_time(time);
        let offset = FixedOffset::east_opt(2 * 3600).unwrap();
        let cells = [
            (Cell::Null, rmpv::Value::Nil),
            (Cell::Bool(true), true.into()),
            (Cell::Int(-200), (-200).into()),
            (Cell::UInt(u64::MAX), u64::MAX.into()),
            (Cell::Float(1.5), rmpv::Value::F64(1.5)),
            (Cell::Str("text".into()), "text".into()),
            (Cell::Bytes(vec![0, 255]), rmpv::Value::Binary(vec![0, 255])),
            (
                Cell::Uuid(Uuid::nil()),
                "00000000-0000-0000-0000-000000000000".into(),
            ),
            (Cell::Decimal(Decimal::new(12300, 4)), "1.2300".into()),
            (Cell::DecimalText(Cow::Borrowed("NaN")), "NaN".into()),
            (Cell::DateTime(stamp), "2026-11-26T13:04:05.000006".into()),
            (
                Cell::DateTimeTz(offset.from_local_datetime(&stamp).unwrap()),
                "2026-11-26T13:04:05.000006+02:00".into(),
            ),
            (Cell::Date(date), "2026-11-26".into()),
            (Cell::Time(time), "13:04:05.000006".into()),
            (Cell::TimeTz(time, offset), "13:04:05.000006+02:00".into()),
            (Cell::Duration(-1_500_000), rmpv::Value::F64(-1.5)),
            (Cell::DurationAggregate(1_500_001), "P0DT1.500001S".into()),
            (
                Cell::Array(Box::new(ArrayValues::Bool(vec![Some(false), None]))),
                rmpv::Value::Array(vec![false.into(), rmpv::Value::Nil]),
            ),
            (
                Cell::Array(Box::new(ArrayValues::Cells(vec![
                    Cell::Null,
                    Cell::Json(serde_json::json!({"a": [1, true]})),
                ]))),
                rmpv::Value::Array(vec![
                    rmpv::Value::Nil,
                    rmpv::Value::Map(vec![(
                        "a".into(),
                        rmpv::Value::Array(vec![1.into(), true.into()]),
                    )]),
                ]),
            ),
        ];
        for (cell, value) in cells {
            let mut actual = Vec::new();
            write_cell(&mut actual, &cell);
            let expected = rmp_serde::to_vec(&value).unwrap();
            assert_eq!(actual, expected, "{cell:?}");
        }
    }

    #[test]
    fn typed_arrays_match_cell_array_messagepack_bytes() {
        use chrono::{FixedOffset, NaiveDate};
        use rust_decimal::Decimal;

        use crate::{PgNumeric, PgTemporal};

        let uuid = Uuid::from_u128(42);
        let decimal = Decimal::new(1234, 2);
        let date = NaiveDate::from_ymd_opt(2026, 11, 28).unwrap();
        let datetime = date.and_hms_micro_opt(13, 4, 5, 123).unwrap();
        let utc = datetime.and_utc();
        let time = datetime.time();
        let offset = FixedOffset::east_opt(3600).unwrap();
        let timetz = sqlx::postgres::types::PgTimeTz { time, offset };
        let json = serde_json::json!({"items": [true, null, 1]});
        let cases = [
            (ArrayValues::Bool(vec![Some(true), None]), Cell::Bool(true)),
            (ArrayValues::I16(vec![Some(-2), None]), Cell::Int(-2)),
            (
                ArrayValues::I32(vec![Some(40_000), None]),
                Cell::Int(40_000),
            ),
            (
                ArrayValues::I64(vec![Some(i64::MAX), None]),
                Cell::Int(i64::MAX),
            ),
            (
                ArrayValues::F32(vec![Some(0.1), None]),
                Cell::Float(f64::from(0.1_f32)),
            ),
            (ArrayValues::F64(vec![Some(0.1), None]), Cell::Float(0.1)),
            (
                ArrayValues::Duration(vec![Some(-1_500_000), None]),
                Cell::Duration(-1_500_000),
            ),
            (ArrayValues::Uuid(vec![Some(uuid), None]), Cell::Uuid(uuid)),
            (
                ArrayValues::U32(vec![Some(u32::MAX), None]),
                Cell::UInt(u64::from(u32::MAX)),
            ),
            (
                ArrayValues::U64(vec![Some(u64::MAX), None]),
                Cell::UInt(u64::MAX),
            ),
            (
                ArrayValues::Str(vec![Some("ready".into()), None]),
                Cell::Str("ready".into()),
            ),
            (
                ArrayValues::Bytes(vec![Some(vec![0, 255]), None]),
                Cell::Bytes(vec![0, 255]),
            ),
            (
                ArrayValues::Json(vec![Some(json.clone()), None]),
                Cell::Json(json),
            ),
            (
                ArrayValues::Decimal(vec![Some(PgNumeric::Finite(decimal)), None]),
                Cell::Decimal(decimal),
            ),
            (
                ArrayValues::Decimal(vec![Some(PgNumeric::Special("NaN")), None]),
                Cell::DecimalText("NaN".into()),
            ),
            (
                ArrayValues::DateTime(vec![Some(PgTemporal::Native(datetime)), None]),
                Cell::DateTime(datetime),
            ),
            (
                ArrayValues::DateTimeUtc(vec![Some(PgTemporal::Native(utc)), None]),
                Cell::DateTimeTz(utc.fixed_offset()),
            ),
            (
                ArrayValues::Date(vec![Some(PgTemporal::Native(date)), None]),
                Cell::Date(date),
            ),
            (
                ArrayValues::Time(vec![Some(PgTemporal::Native(time)), None]),
                Cell::Time(time),
            ),
            (
                ArrayValues::TimeTz(vec![Some(PgTemporal::Native(timetz)), None]),
                Cell::TimeTz(time, offset),
            ),
            (
                ArrayValues::Date(vec![Some(PgTemporal::Text("infinity".into())), None]),
                Cell::Str("infinity".into()),
            ),
        ];
        for (typed, value) in cases {
            let typed = Cell::Array(Box::new(typed));
            let cells = Cell::Array(Box::new(ArrayValues::Cells(vec![value, Cell::Null])));
            let mut actual = Vec::new();
            let mut expected = Vec::new();
            write_cell(&mut actual, &typed);
            write_cell(&mut expected, &cells);
            assert_eq!(actual, expected, "{typed:?}");
        }
    }
}
