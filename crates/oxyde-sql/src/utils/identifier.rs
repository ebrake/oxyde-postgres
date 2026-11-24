//! Dynamic SQL identifiers for sea_query

use std::collections::HashMap;

use oxyde_codec::{JoinSpec, QueryIR};
use sea_query::{ColumnRef, Iden, IntoColumnRef};

/// Dynamic table identifier
#[derive(Debug, Clone)]
pub struct TableIdent(pub String);

impl Iden for TableIdent {
    fn unquoted(&self) -> &str {
        &self.0
    }
}

/// Dynamic column identifier
#[derive(Debug, Clone)]
pub struct ColumnIdent(pub String);

impl Iden for ColumnIdent {
    fn unquoted(&self) -> &str {
        &self.0
    }
}

/// A SQL column reference and its column type lookup key.
pub struct ResolvedColumn<'a> {
    pub(crate) column: &'a str,
    pub(crate) join: Option<&'a JoinSpec>,
    /// Column type maps use bare base columns and qualified joined columns.
    pub type_key: &'a str,
    qualified_key: Option<&'a str>,
    qualifier: Option<&'a str>,
}

impl ResolvedColumn<'_> {
    /// Look up a column type hint, preferring the bare key for base columns.
    #[must_use]
    pub fn lookup<'m, V>(&self, map: Option<&'m HashMap<String, V>>) -> Option<&'m V> {
        let map = map?;
        map.get(self.type_key)
            .or_else(|| self.qualified_key.and_then(|key| map.get(key)))
    }

    pub(crate) fn column_ref(&self, default_table: Option<&str>) -> ColumnRef {
        let column = ColumnIdent(self.column.to_owned());
        match self.qualifier.or(default_table) {
            Some(table) => (TableIdent(table.to_owned()), column).into_column_ref(),
            None => column.into_column_ref(),
        }
    }
}

/// Resolve a join alias or an explicit base table qualifier. Filters also
/// accept arbitrary qualified columns for low-level callers.
#[must_use]
pub fn resolve_column<'a>(
    base_table: Option<&'a str>,
    joins: &'a [JoinSpec],
    name: &'a str,
    for_filter: bool,
) -> ResolvedColumn<'a> {
    if let Some((prefix, column)) = name.split_once('.') {
        let join = joins.iter().find(|join| join.alias == prefix);
        if join.is_some() {
            return ResolvedColumn {
                column,
                join,
                type_key: name,
                qualified_key: None,
                qualifier: Some(prefix),
            };
        }
        if let Some((table, column)) = base_table.and_then(|table| {
            name.strip_prefix(table)
                .and_then(|rest| rest.strip_prefix('.'))
                .map(|column| (table, column))
        }) {
            // Filter IR uses the base prefix to escape dotted column names.
            // Preserve explicit qualification for ordinary table.column filters.
            let dotted = column.contains('.');
            return ResolvedColumn {
                column,
                join: None,
                type_key: column,
                qualified_key: (!dotted).then_some(name),
                qualifier: (!for_filter || !dotted).then_some(table),
            };
        }
        if for_filter {
            return ResolvedColumn {
                column,
                join: None,
                type_key: name,
                qualified_key: None,
                qualifier: Some(prefix),
            };
        }
    }
    ResolvedColumn {
        column: name,
        join: None,
        type_key: name,
        qualified_key: None,
        qualifier: None,
    }
}

/// Output label of a projected column reference.
///
/// The label Python sent in `output_labels` wins. Otherwise a joined column
/// is labeled `{result_prefix}__{field}` and a base column by its own name.
#[must_use]
pub fn output_label(ir: &QueryIR, field: &str) -> String {
    let joins = ir.joins.as_deref().unwrap_or_default();
    let resolved = resolve_column(Some(&ir.table), joins, field, false);
    if let Some(label) = ir
        .output_labels
        .as_ref()
        .and_then(|labels| labels.get(field).or_else(|| labels.get(resolved.type_key)))
    {
        return label.clone();
    }
    resolved.join.map_or_else(
        || resolved.column.to_owned(),
        |join| {
            let field = join
                .columns
                .iter()
                .find(|column| column.column == resolved.column)
                .map_or(resolved.column, |column| column.field.as_str());
            join.result_column(field)
        },
    )
}

/// Column references a grouped SELECT projects besides its aggregates.
///
/// With aggregates the group keys are projected; a `group_by()` without
/// aggregates projects the selected columns. Other queries return nothing.
#[must_use]
pub fn grouped_projection(ir: &QueryIR) -> &[String] {
    let group_by = ir.group_by.as_deref().unwrap_or_default();
    if ir.aggregates.is_some() {
        group_by
    } else if !group_by.is_empty() {
        ir.cols.as_deref().unwrap_or_default()
    } else {
        &[]
    }
}
