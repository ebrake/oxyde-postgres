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
            }
        }
        MigrationOp::AddColumn { table, field } => {
            if let Some(table) = state.tables.get_mut(table) {
                table.fields.push(field.clone());
            }
        }
        MigrationOp::DropColumn { table, field, .. } => {
            if let Some(table) = state.tables.get_mut(table) {
                table.fields.retain(|f| f.name != *field);
            }
        }
        MigrationOp::RenameColumn {
            table,
            old_name,
            new_name,
            ..
        } => {
            if let Some(field) = state
                .tables
                .get_mut(table)
                .and_then(|t| t.fields.iter_mut().find(|f| f.name == *old_name))
            {
                field.name.clone_from(new_name);
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
}
