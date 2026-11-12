//! Apply migration operations to a snapshot without a database or SQL renderer.

use crate::{FieldDef, MigrateError, MigrationOp, Result, Snapshot};
use oxyde_codec::ColumnTypeSpec;
use serde_json::Value;

/// Resolve collect-mode column changes using the field currently in the schema.
/// Other operations (including already-resolved alterations) are returned unchanged.
pub fn resolve_operation(state: &Snapshot, op: &MigrationOp) -> Result<MigrationOp> {
    let MigrationOp::AlterColumn {
        table,
        old_field,
        new_field,
        column,
        changes,
        ..
    } = op
    else {
        return Ok(op.clone());
    };
    if old_field.is_some() && new_field.is_some() {
        return Ok(op.clone());
    }
    let column = column
        .as_deref()
        .or_else(|| old_field.as_ref().map(|field| field.name.as_str()))
        .ok_or_else(|| {
            MigrateError::MigrationError(
                "alter_column requires old_field/new_field or column/changes".into(),
            )
        })?;
    let old = old_field
        .as_deref()
        .or_else(|| {
            state
                .tables
                .get(table)
                .and_then(|table| table.fields.iter().find(|field| field.name == column))
        })
        .ok_or_else(|| MigrateError::MigrationError(format!("Unknown column {table}.{column}")))?;
    let mut value = serde_json::to_value(old)
        .map_err(|error| MigrateError::SerializationError(error.to_string()))?;
    let field = value.as_object_mut().expect("field is an object");
    if let Some(changes) = changes {
        for key in [
            "column_type",
            "db_type",
            "nullable",
            "default",
            "unique",
            "max_length",
            "max_digits",
            "decimal_places",
        ] {
            if let Some(value) = changes.get(key) {
                field.insert(key.into(), value.clone());
            }
        }
        if let Some(value) = changes.get("nulls_not_distinct") {
            let enabled = value.as_bool().ok_or_else(|| {
                MigrateError::MigrationError("nulls_not_distinct must be a boolean".into())
            })?;
            if enabled {
                field.insert("nulls_not_distinct".into(), Value::Bool(true));
            } else {
                field.remove("nulls_not_distinct");
            }
        }
    }
    let new: FieldDef = serde_json::from_value(value)
        .map_err(|error| MigrateError::SerializationError(error.to_string()))?;
    let mut resolved = op.clone();
    if let MigrationOp::AlterColumn {
        old_field,
        new_field,
        column,
        changes,
        ..
    } = &mut resolved
    {
        *old_field = Some(Box::new(old.clone()));
        *new_field = Some(Box::new(new));
        *column = None;
        *changes = None;
    }
    Ok(resolved)
}

fn rewrite_enum_spec(spec: &mut ColumnTypeSpec, name: &str, values: &[String], append: bool) {
    match spec {
        ColumnTypeSpec::Enum {
            name: enum_name,
            values: current,
        } if enum_name == name => {
            if append {
                for value in values {
                    if !current.contains(value) {
                        current.push(value.clone());
                    }
                }
            } else {
                current.clone_from(&values.to_vec());
            }
        }
        ColumnTypeSpec::Array { item, .. } => rewrite_enum_spec(item, name, values, append),
        _ => {}
    }
}

fn rewrite_enum_fields(state: &mut Snapshot, name: &str, values: &[String], append: bool) {
    for table in state.tables.values_mut() {
        for field in &mut table.fields {
            rewrite_enum_spec(&mut field.column_type, name, values, append);
        }
    }
}

/// Follow a column rename through everything PostgreSQL rewrites with it:
/// the table's index keys (including column references inside expression
/// keys), its foreign-key columns, and the `ref_columns` of foreign keys that
/// point at it. `where` clauses, CHECK expressions and raw expression
/// fragments are opaque SQL and stay as written.
fn rename_column_references(state: &mut Snapshot, table: &str, old: &str, new: &str) {
    let rename_in = |columns: &mut [String]| {
        for column in columns {
            if column == old {
                new.clone_into(column);
            }
        }
    };

    if let Some(def) = state.tables.get_mut(table) {
        for index in &mut def.indexes {
            index.rename_column(old, new);
        }
        for fk in &mut def.foreign_keys {
            rename_in(&mut fk.columns);
        }
    }
    for def in state.tables.values_mut() {
        for fk in &mut def.foreign_keys {
            if fk.ref_table == table {
                rename_in(&mut fk.ref_columns);
            }
        }
    }
}

/// Follow a dropped column through what PostgreSQL drops with it: every index
/// of the table that uses the column (a plain key, a column inside an
/// expression key, any key of a composite index, whole index either way) and
/// the table's own foreign keys on it.
///
/// A foreign key in *another* table that points at the column is left alone:
/// migrations render `DROP COLUMN` without `CASCADE`, so PostgreSQL refuses
/// that drop and the migration never gets this far.
///
/// Dependents inside opaque SQL are invisible here: `where` predicates,
/// `ScalarExpr::Raw` fragments and CHECK expressions. PostgreSQL also drops
/// (or rejects) those, but replay cannot tell which column they use.
fn drop_column_dependents(state: &mut Snapshot, table: &str, column: &str) {
    if let Some(def) = state.tables.get_mut(table) {
        def.indexes.retain(|index| !index.references_column(column));
        def.foreign_keys
            .retain(|fk| !fk.columns.iter().any(|used| used == column));
    }
}

/// Apply one operation. Missing tables/columns are ignored, matching Python replay.
/// Enum declarations and drops are schema-neutral; enum value edits visit nested arrays.
pub fn apply_operation(state: &mut Snapshot, op: &MigrationOp) -> Result<()> {
    match op {
        MigrationOp::CreateEnumType { .. } | MigrationOp::DropEnumType { .. } => {}
        MigrationOp::AddEnumValue { name, value, .. } => {
            rewrite_enum_fields(state, name, std::slice::from_ref(value), true);
        }
        MigrationOp::AlterEnumType {
            name, new_values, ..
        } => {
            rewrite_enum_fields(state, name, new_values, false);
        }
        MigrationOp::CreateTable { table } => {
            state.add_table(table.clone());
        }
        MigrationOp::DropTable { name, .. } => {
            state.tables.remove(name);
        }
        MigrationOp::RenameTable { old_name, new_name } => {
            if let Some(mut table) = state.tables.remove(old_name) {
                table.name.clone_from(new_name);
                state.tables.insert(new_name.clone(), table);
                // PostgreSQL repoints foreign keys at the renamed table.
                for table in state.tables.values_mut() {
                    for fk in &mut table.foreign_keys {
                        if fk.ref_table == *old_name {
                            fk.ref_table.clone_from(new_name);
                        }
                    }
                }
            }
        }
        MigrationOp::AddColumn { table, field } => {
            if let Some(table) = state.tables.get_mut(table) {
                table.fields.push(field.clone());
            }
        }
        MigrationOp::DropColumn { table, field, .. } => {
            let dropped = state.tables.get_mut(table).is_some_and(|def| {
                let before = def.fields.len();
                def.fields.retain(|f| f.name != *field);
                def.fields.len() != before
            });
            if dropped {
                drop_column_dependents(state, table, field);
            }
        }
        MigrationOp::RenameColumn {
            table,
            old_name,
            new_name,
            ..
        } => {
            let renamed = state
                .tables
                .get_mut(table)
                .and_then(|t| t.fields.iter_mut().find(|f| f.name == *old_name))
                .map(|field| field.name.clone_from(new_name))
                .is_some();
            if renamed {
                rename_column_references(state, table, old_name, new_name);
            }
        }
        MigrationOp::AlterColumn {
            table,
            column,
            old_field,
            new_field,
            ..
        } => {
            let name = column
                .as_deref()
                .or_else(|| old_field.as_ref().map(|f| f.name.as_str()));
            let Some(name) = name else {
                return Err(MigrateError::MigrationError(
                    "alter_column requires a column or old_field".into(),
                ));
            };
            // Python ignores alterations to a missing table or field.
            if !state
                .tables
                .get(table)
                .is_some_and(|t| t.fields.iter().any(|f| f.name == name))
            {
                return Ok(());
            }
            let resolved;
            let new = if let Some(field) = new_field.as_deref() {
                field
            } else {
                resolved = resolve_operation(state, op)?;
                let MigrationOp::AlterColumn {
                    new_field: Some(ref field),
                    ..
                } = resolved
                else {
                    unreachable!()
                };
                field.as_ref()
            };
            if let Some(field) = state
                .tables
                .get_mut(table)
                .and_then(|t| t.fields.iter_mut().find(|f| f.name == name))
            {
                *field = new.clone();
            }
        }
        MigrationOp::CreateIndex { table, index } => {
            if let Some(table) = state.tables.get_mut(table) {
                table.indexes.push(index.clone());
            }
        }
        MigrationOp::DropIndex { table, name, .. } => {
            if let Some(table) = state.tables.get_mut(table) {
                table.indexes.retain(|i| i.name != *name);
            }
        }
        MigrationOp::AddForeignKey { table, fk } => {
            if let Some(table) = state.tables.get_mut(table) {
                table.foreign_keys.push(fk.clone());
            }
        }
        MigrationOp::DropForeignKey { table, name, .. } => {
            if let Some(table) = state.tables.get_mut(table) {
                table.foreign_keys.retain(|f| f.name != *name);
            }
        }
        MigrationOp::AddCheck { table, check } => {
            if let Some(table) = state.tables.get_mut(table) {
                table.checks.push(check.clone());
            }
        }
        MigrationOp::DropCheck { table, name, .. } => {
            if let Some(table) = state.tables.get_mut(table) {
                table.checks.retain(|c| c.name != *name);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxyde_codec::IndexKey;
    use serde_json::json;

    fn apply(state: &mut Snapshot, op: Value) {
        apply_operation(state, &serde_json::from_value(op).unwrap()).unwrap();
    }

    #[test]
    fn collect_and_resolved_alterations_share_replay() {
        let mut state = Snapshot::new();
        apply(
            &mut state,
            json!({"type": "create_table", "table": {"name": "t", "indexes": [], "fields": [
                {"name": "x", "nullable": true, "primary_key": false, "unique": false, "column_type": {"kind": "text"}}
            ]}}),
        );
        let collect: MigrationOp = serde_json::from_value(json!({"type": "alter_column", "table": "t",
            "column": "x", "changes": {"nullable": false, "unique": true, "nulls_not_distinct": true}
        })).unwrap();
        let resolved = resolve_operation(&state, &collect).unwrap();
        let mut other = state.clone();
        apply_operation(&mut state, &collect).unwrap();
        apply_operation(&mut other, &resolved).unwrap();
        assert_eq!(state.to_json().unwrap(), other.to_json().unwrap());
        let field = &state.tables["t"].fields[0];
        assert!(!field.nullable);
        assert!(field.unique);
        assert!(field.nulls_not_distinct);
        apply(
            &mut state,
            json!({"type": "alter_column", "table": "t", "column": "x",
            "changes": {"nulls_not_distinct": false}}),
        );
        let raw: Value = serde_json::from_str(&state.to_json().unwrap()).unwrap();
        assert!(raw["tables"]["t"]["fields"][0]
            .get("nulls_not_distinct")
            .is_none());
    }

    #[test]
    fn supplied_old_field_resolves_without_a_tracked_column() {
        let op = serde_json::from_value(json!({
            "type": "alter_column", "table": "untracked",
            "old_field": {"name": "x", "column_type": {"kind": "text"},
                "nullable": true, "primary_key": false, "unique": false},
            "changes": {"nullable": false}
        }))
        .unwrap();
        let resolved = resolve_operation(&Snapshot::new(), &op).unwrap();
        let MigrationOp::AlterColumn {
            old_field: Some(old),
            new_field: Some(new),
            ..
        } = resolved
        else {
            panic!("expected resolved alteration");
        };
        assert!(old.nullable);
        assert!(!new.nullable);
    }

    #[test]
    fn native_resolution_rejects_non_boolean_nulls_not_distinct() {
        let op = serde_json::from_value(json!({
            "type": "alter_column", "table": "untracked", "column": "x",
            "old_field": {"name": "x", "column_type": {"kind": "text"},
                "nullable": true, "primary_key": false, "unique": false},
            "changes": {"nulls_not_distinct": 1}
        }))
        .unwrap();
        let error = resolve_operation(&Snapshot::new(), &op).unwrap_err();
        assert!(error.to_string().contains("must be a boolean"));
    }

    #[test]
    fn enum_rewrites_reach_scalar_and_nested_arrays_without_duplicates() {
        let mut state = Snapshot::new();
        apply(
            &mut state,
            json!({"type": "create_table", "table": {"name": "t", "indexes": [], "fields": [
                {"name": "scalar", "nullable": true, "primary_key": false, "unique": false, "column_type": {"kind": "enum", "name": "status", "values": []}},
                {"name": "nested", "nullable": true, "primary_key": false, "unique": false, "column_type": {"kind": "array", "item": {"kind": "array", "item": {
                    "kind": "enum", "name": "status", "values": ["draft"]}}}},
                {"name": "other", "nullable": true, "primary_key": false, "unique": false, "column_type": {"kind": "enum", "name": "other", "values": ["same"]}}
            ]}}),
        );
        for _ in 0..2 {
            apply(
                &mut state,
                json!({"type": "add_enum_value", "name": "status", "value": "published"}),
            );
        }
        let raw: Value = serde_json::from_str(&state.to_json().unwrap()).unwrap();
        let fields = &raw["tables"]["t"]["fields"];
        assert_eq!(fields[0]["column_type"]["values"], json!(["published"]));
        assert_eq!(
            fields[1]["column_type"]["item"]["item"]["values"],
            json!(["draft", "published"])
        );
        apply(
            &mut state,
            json!({"type": "alter_enum_type", "name": "status", "old_values": [], "new_values": ["archived"]}),
        );
        let raw: Value = serde_json::from_str(&state.to_json().unwrap()).unwrap();
        assert_eq!(
            raw["tables"]["t"]["fields"][1]["column_type"]["item"]["item"]["values"],
            json!(["archived"])
        );
        assert_eq!(
            raw["tables"]["t"]["fields"][2]["column_type"]["values"],
            json!(["same"])
        );
    }

    #[test]
    fn canonical_members_round_trip_in_declaration_order() {
        let raw = r#"{"name":"x","column_type":{"kind":"array","item":{"kind":"text"}},"nullable":true,"primary_key":false,"unique":false}"#;
        let field: FieldDef = serde_json::from_str(raw).unwrap();
        let serialized = serde_json::to_string(&field).unwrap();
        assert!(serialized
            .starts_with(r#"{"name":"x","column_type":{"kind":"array","item":{"kind":"text"}}"#));
        assert_eq!(
            serde_json::from_str::<FieldDef>(&serialized).unwrap(),
            field
        );
    }

    #[test]
    fn missing_targets_are_noops_and_invalid_changes_are_atomic() {
        let mut state = Snapshot::new();
        apply(&mut state, json!({"type": "drop_table", "name": "missing"}));
        apply(
            &mut state,
            json!({"type": "rename_table", "old_name": "missing", "new_name": "t"}),
        );
        apply(
            &mut state,
            json!({"type": "alter_column", "table": "missing", "column": "x", "changes": {}}),
        );
        assert!(state.tables.is_empty());
        apply(
            &mut state,
            json!({"type": "create_table", "table": {"name": "t", "indexes": [], "fields": [
                {"name": "x", "nullable": true, "primary_key": false, "unique": false, "column_type": {"kind": "text"}}
            ]}}),
        );
        let before = state.to_json().unwrap();
        let invalid = serde_json::from_value(json!({"type": "alter_column", "table": "t", "column": "x", "changes": {"nullable": "invalid"}})).unwrap();
        assert!(apply_operation(&mut state, &invalid).is_err());
        assert_eq!(state.to_json().unwrap(), before);
    }

    fn replay_state() -> Snapshot {
        let field = |name: &str| {
            json!({"name": name, "nullable": true, "primary_key": false,
                   "unique": false, "column_type": {"kind": "text"}})
        };
        let mut state = Snapshot::new();
        apply(
            &mut state,
            json!({"type": "create_table", "table": {
                "name": "users",
                "fields": [field("id"), field("email")],
                "indexes": [
                    {"name": "users_email_idx", "fields": ["email"], "unique": false, "method": null},
                    {"name": "users_email_lower", "unique": false, "method": null, "fields": [
                        {"expr": {"type": "func", "name": "lower", "args": [
                            {"type": "column", "name": "email"}]}, "opclass": "text_pattern_ops"},
                        {"column": "email", "opclass": "text_pattern_ops"},
                    ]},
                ],
                "foreign_keys": [
                    {"name": "fk_users_email", "columns": ["email"], "ref_table": "users",
                     "ref_columns": ["email"], "on_delete": null, "on_update": null},
                ],
            }}),
        );
        apply(
            &mut state,
            json!({"type": "create_table", "table": {
                "name": "posts",
                "fields": [field("id"), field("author_email")],
                "indexes": [],
                "foreign_keys": [
                    {"name": "fk_posts_author", "columns": ["author_email"], "ref_table": "users",
                     "ref_columns": ["email"], "on_delete": null, "on_update": null},
                ],
            }}),
        );
        state
    }

    #[test]
    fn rename_column_follows_index_keys_and_foreign_keys() {
        let mut state = replay_state();
        apply(
            &mut state,
            json!({"type": "rename_column", "table": "users",
                   "old_name": "email", "new_name": "mail"}),
        );

        let users = &state.tables["users"];
        assert_eq!(users.fields[1].name, "mail");
        assert_eq!(users.indexes[0].fields, vec![IndexKey::from("mail")]);
        assert!(users.indexes[1].references_column("mail"));
        assert!(!users.indexes[1].references_column("email"));
        assert_eq!(
            users.indexes[1].fields[1].opclass(),
            Some("text_pattern_ops")
        );
        assert_eq!(users.foreign_keys[0].columns, vec!["mail"]);
        assert_eq!(users.foreign_keys[0].ref_columns, vec!["mail"]);
        // The referencing table keeps its own column, but its ref_columns follow.
        let posts = &state.tables["posts"];
        assert_eq!(posts.foreign_keys[0].columns, vec!["author_email"]);
        assert_eq!(posts.foreign_keys[0].ref_columns, vec!["mail"]);
    }

    fn index_names(state: &Snapshot, table: &str) -> Vec<String> {
        state.tables[table]
            .indexes
            .iter()
            .map(|index| index.name.clone())
            .collect()
    }

    #[test]
    fn drop_column_removes_indexes_and_foreign_keys_that_use_it() {
        let mut state = replay_state();
        apply(
            &mut state,
            json!({"type": "create_index", "table": "users", "index": {
                "name": "users_id_idx", "fields": ["id"], "unique": false, "method": null}}),
        );
        apply(
            &mut state,
            json!({"type": "create_index", "table": "users", "index": {
                "name": "users_id_email_idx", "fields": ["id", "email"],
                "unique": false, "method": null}}),
        );

        apply(
            &mut state,
            json!({"type": "drop_column", "table": "users", "field": "email"}),
        );

        let users = &state.tables["users"];
        assert_eq!(users.fields.len(), 1);
        // PostgreSQL drops every index that uses the column: a plain key, an
        // expression key and a composite index alike. Unrelated ones stay.
        assert_eq!(index_names(&state, "users"), vec!["users_id_idx"]);
        assert_eq!(users.foreign_keys, Vec::new());
        // A foreign key in another table that points at the column is not
        // dropped with it: DROP COLUMN without CASCADE refuses to run.
        assert_eq!(state.tables["posts"].foreign_keys[0].ref_columns, vec!["email"]);
    }

    #[test]
    fn drop_of_a_missing_column_changes_nothing() {
        let mut state = replay_state();
        let before = state.clone();
        apply(
            &mut state,
            json!({"type": "drop_column", "table": "users", "field": "nope"}),
        );
        assert_eq!(state, before);
    }

    #[test]
    fn dropping_a_column_leaves_nothing_for_the_next_diff_to_drop() {
        // A hand-written migration drops the column and not its indexes.
        // PostgreSQL drops them with the column, so models without either
        // must diff clean: no DropIndex that would fail at migrate time.
        let before = replay_state();
        let mut replayed = before.clone();
        apply(
            &mut replayed,
            json!({"type": "drop_column", "table": "users", "field": "email"}),
        );

        let mut models = before;
        let users = models.tables.get_mut("users").unwrap();
        users.fields.retain(|field| field.name != "email");
        users.indexes.clear();
        users.foreign_keys.clear();

        let ops = crate::compute_diff(&replayed, &models).unwrap();
        assert!(ops.is_empty(), "{ops:?}");
    }

    #[test]
    fn rename_of_a_missing_column_changes_nothing() {
        let mut state = replay_state();
        let before = state.clone();
        apply(
            &mut state,
            json!({"type": "rename_column", "table": "users",
                   "old_name": "nope", "new_name": "mail"}),
        );
        assert_eq!(state, before);
    }

    #[test]
    fn rename_table_repoints_foreign_keys() {
        let mut state = replay_state();
        apply(
            &mut state,
            json!({"type": "rename_table", "old_name": "users", "new_name": "people"}),
        );
        assert_eq!(state.tables["posts"].foreign_keys[0].ref_table, "people");
        assert_eq!(state.tables["people"].foreign_keys[0].ref_table, "people");
    }
}
