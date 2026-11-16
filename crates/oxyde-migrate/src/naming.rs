//! Stable, filesystem-safe names for generated migration files.

use oxyde_codec::MigrationOp;

fn is_extension(op: &MigrationOp) -> bool {
    matches!(
        op,
        MigrationOp::EnsureExtension { .. }
            | MigrationOp::RemoveExtensionRequirement { .. }
            | MigrationOp::DropExtension { .. }
    )
}

fn sanitize(name: &str) -> String {
    let mut result = String::with_capacity(name.len());
    for ch in name.to_lowercase().chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            result.push(ch);
        } else if !result.is_empty() && !result.ends_with('_') {
            result.push('_');
        }
    }
    result.trim_end_matches('_').to_string()
}

/// Infer a name from the first non-extension operation, or the first extension.
#[must_use]
pub fn infer_migration_name(ops: &[MigrationOp]) -> String {
    let mut structural = ops.iter().filter(|op| !is_extension(op));
    let Some(first) = structural.next() else {
        return match ops.first() {
            Some(
                MigrationOp::EnsureExtension { name }
                | MigrationOp::RemoveExtensionRequirement { name }
                | MigrationOp::DropExtension { name },
            ) => sanitize(&format!("extension_{name}")),
            _ => "empty".to_string(),
        };
    };
    let name = match first {
        MigrationOp::CreateEnumType { name, .. } => format!("create_{name}_enum"),
        MigrationOp::DropEnumType { name, .. } => format!("drop_{name}_enum"),
        MigrationOp::AddEnumValue { name, value, .. } => format!("add_{value}_to_{name}_enum"),
        MigrationOp::AlterEnumType { name, .. } => format!("alter_{name}_enum"),
        MigrationOp::CreateTable { table } => format!("create_{}_table", table.name),
        MigrationOp::DropTable { name, .. } => format!("drop_{name}_table"),
        MigrationOp::AddColumn { table, field } => format!("add_{}_to_{table}", field.name),
        MigrationOp::DropColumn { table, field, .. } => format!("drop_{field}_from_{table}"),
        MigrationOp::RenameTable { old_name, new_name } => {
            format!("rename_{old_name}_to_{new_name}")
        }
        MigrationOp::RenameColumn {
            old_name, new_name, ..
        } => {
            format!("rename_column_{old_name}_to_{new_name}")
        }
        _ if structural.next().is_some() => "auto".to_string(),
        _ => "migration".to_string(),
    };
    sanitize(&name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn infer(ops: Value) -> String {
        infer_migration_name(&serde_json::from_value::<Vec<MigrationOp>>(ops).unwrap())
    }

    #[test]
    fn names_match_existing_rules_for_every_operation() {
        let field = json!({"name": "title", "column_type": {"kind": "string"},
            "nullable": false, "primary_key": false, "unique": false, "default": null});
        let table =
            json!({"name": "docs", "fields": [], "indexes": [], "foreign_keys": [], "checks": []});
        let index = json!({"name": "ix", "fields": ["title"], "unique": false});
        for (op, expected) in [
            (
                json!({"type": "ensure_extension", "name": "pg_trgm"}),
                "extension_pg_trgm",
            ),
            (
                json!({"type": "remove_extension_requirement", "name": "pg_trgm"}),
                "extension_pg_trgm",
            ),
            (
                json!({"type": "drop_extension", "name": "pg_trgm"}),
                "extension_pg_trgm",
            ),
            (
                json!({"type": "create_enum_type", "name": "status", "values": ["draft"]}),
                "create_status_enum",
            ),
            (
                json!({"type": "drop_enum_type", "name": "status"}),
                "drop_status_enum",
            ),
            (
                json!({"type": "add_enum_value", "name": "status", "value": "done"}),
                "add_done_to_status_enum",
            ),
            (
                json!({"type": "alter_enum_type", "name": "status", "old_values": [], "new_values": []}),
                "alter_status_enum",
            ),
            (
                json!({"type": "create_table", "table": table}),
                "create_docs_table",
            ),
            (
                json!({"type": "drop_table", "name": "docs"}),
                "drop_docs_table",
            ),
            (
                json!({"type": "add_column", "table": "docs", "field": field}),
                "add_title_to_docs",
            ),
            (
                json!({"type": "drop_column", "table": "docs", "field": "title"}),
                "drop_title_from_docs",
            ),
            (
                json!({"type": "rename_table", "old_name": "docs", "new_name": "articles"}),
                "rename_docs_to_articles",
            ),
            (
                json!({"type": "rename_column", "table": "docs", "old_name": "title", "new_name": "name"}),
                "rename_column_title_to_name",
            ),
            (
                json!({"type": "alter_column", "table": "docs", "column": "title"}),
                "migration",
            ),
            (
                json!({"type": "create_index", "table": "docs", "index": index}),
                "migration",
            ),
            (
                json!({"type": "drop_index", "table": "docs", "name": "ix"}),
                "migration",
            ),
            (
                json!({"type": "add_foreign_key", "table": "docs", "fk": {"name": "fk", "columns": ["id"], "ref_table": "other", "ref_columns": ["id"]}}),
                "migration",
            ),
            (
                json!({"type": "drop_foreign_key", "table": "docs", "name": "fk"}),
                "migration",
            ),
            (
                json!({"type": "add_check", "table": "docs", "check": {"name": "ck", "expression": "true"}}),
                "migration",
            ),
            (
                json!({"type": "drop_check", "table": "docs", "name": "ck"}),
                "migration",
            ),
        ] {
            assert_eq!(infer(json!([op.clone()])), expected, "{op}");
        }
    }

    #[test]
    fn extensions_do_not_hide_structural_names_or_affect_counts() {
        let ext = json!({"type": "ensure_extension", "name": "pg_trgm"});
        let drop = json!({"type": "drop_index", "table": "docs", "name": "ix"});
        assert_eq!(infer(json!([])), "empty");
        assert_eq!(infer(json!([ext, drop])), "migration");
        assert_eq!(infer(json!([ext, drop, drop])), "auto");
        assert_eq!(
            infer(
                json!([ext, {"type": "create_table", "table": {"name": "docs", "fields": [], "indexes": [], "foreign_keys": [], "checks": []}}])
            ),
            "create_docs_table"
        );
    }

    #[test]
    fn names_are_safe_python_module_names() {
        assert_eq!(
            infer(json!([{"type": "ensure_extension", "name": "uuid-ossp"}])),
            "extension_uuid_ossp"
        );
        for (value, expected) in [
            ("in progress", "add_in_progress_to_status_enum"),
            ("a/b", "add_a_b_to_status_enum"),
            ("__A---B__", "add_a_b_to_status_enum"),
        ] {
            assert_eq!(
                infer(json!([{"type": "add_enum_value", "name": "Status", "value": value}])),
                expected
            );
        }
        assert_eq!(sanitize("__A / B__"), "a_b");
    }
}
