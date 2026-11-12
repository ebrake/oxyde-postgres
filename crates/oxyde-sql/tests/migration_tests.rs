//! Integration tests for the diff → DDL pipeline.
//!
//! Tests for public API: MigrationOpExt::to_sql(), compute_diff(), Snapshot.

use oxyde_codec::{
    CheckDef, ColumnTypeSpec, FieldDef, ForeignKeyDef, IndexDef, IndexKey, MigrationOp, ScalarExpr,
    Snapshot, TableDef,
};
use oxyde_migrate::compute_diff;
use oxyde_sql::{Dialect, Migration, MigrationOpExt};

fn sample_field(name: &str) -> FieldDef {
    FieldDef {
        name: name.to_string(),
        column_type: ColumnTypeSpec::String { length: None },
        db_type: None,
        nullable: false,
        primary_key: false,
        unique: false,
        nulls_not_distinct: false,
        default: None,
        auto_increment: false,
        max_length: None,
        max_digits: None,
        decimal_places: None,
    }
}

fn sample_table() -> TableDef {
    TableDef {
        name: "users".into(),
        fields: vec![
            FieldDef {
                name: "id".into(),
                column_type: ColumnTypeSpec::BigInteger,
                db_type: None,
                nullable: false,
                primary_key: true,
                unique: true,
                nulls_not_distinct: false,
                default: None,
                auto_increment: false,
                max_length: None,
                max_digits: None,
                decimal_places: None,
            },
            sample_field("email"),
        ],
        indexes: vec![IndexDef {
            name: "users_email_idx".into(),
            fields: vec!["email".into()],
            unique: true,
            nulls_not_distinct: false,
            method: Some("btree".into()),
            where_clause: None,
        }],
        foreign_keys: vec![],
        checks: vec![],
        comment: Some("User accounts".into()),
    }
}

fn table_with_fields(name: &str, fields: &[&str]) -> TableDef {
    TableDef {
        name: name.into(),
        fields: fields.iter().map(|field| sample_field(field)).collect(),
        indexes: vec![],
        foreign_keys: vec![],
        checks: vec![],
        comment: None,
    }
}

fn sample_foreign_key(
    name: &str,
    columns: &[&str],
    ref_table: &str,
    ref_columns: &[&str],
) -> ForeignKeyDef {
    ForeignKeyDef {
        name: name.into(),
        columns: columns.iter().map(|column| (*column).into()).collect(),
        ref_table: ref_table.into(),
        ref_columns: ref_columns.iter().map(|column| (*column).into()).collect(),
        on_delete: None,
        on_update: None,
    }
}

fn enum_spec(values: &[&str]) -> ColumnTypeSpec {
    ColumnTypeSpec::Enum {
        name: "post_status_enum".into(),
        values: values.iter().map(|value| (*value).into()).collect(),
    }
}

fn enum_table(values: &[&str]) -> TableDef {
    TableDef {
        name: "posts".into(),
        fields: vec![
            FieldDef {
                name: "id".into(),
                column_type: ColumnTypeSpec::BigInteger,
                db_type: None,
                nullable: false,
                primary_key: true,
                unique: false,
                nulls_not_distinct: false,
                default: None,
                auto_increment: false,
                max_length: None,
                max_digits: None,
                decimal_places: None,
            },
            FieldDef {
                name: "status".into(),
                column_type: enum_spec(values),
                db_type: None,
                nullable: false,
                primary_key: false,
                unique: false,
                nulls_not_distinct: false,
                default: None,
                auto_increment: false,
                max_length: None,
                max_digits: None,
                decimal_places: None,
            },
        ],
        indexes: vec![],
        foreign_keys: vec![],
        checks: vec![],
        comment: None,
    }
}

#[test]
fn test_snapshot_serialization_roundtrip() {
    let mut snapshot = Snapshot::new();
    snapshot.add_table(sample_table());

    let json = snapshot.to_json().unwrap();
    let deserialized = Snapshot::from_json(&json).unwrap();
    assert_eq!(snapshot, deserialized);
}

#[test]
fn nulls_not_distinct_serde_backwards_compatibility() {
    let field_json = serde_json::json!({
        "name": "email", "column_type": {"kind": "string"},
        "db_type": null, "nullable": true, "primary_key": false, "unique": true,
        "default": null, "auto_increment": false, "max_length": null,
        "max_digits": null, "decimal_places": null,
    });
    let index_json = serde_json::json!({
        "name": "ux_email", "fields": ["email"], "unique": true, "method": null,
    });
    let mut field: FieldDef = serde_json::from_value(field_json.clone()).unwrap();
    let mut index: IndexDef = serde_json::from_value(index_json.clone()).unwrap();
    assert!(!field.nulls_not_distinct);
    assert!(!index.nulls_not_distinct);
    assert_eq!(serde_json::to_value(&field).unwrap(), field_json);
    assert_eq!(serde_json::to_value(&index).unwrap(), index_json);
    for enabled in [true, false] {
        field.nulls_not_distinct = enabled;
        index.nulls_not_distinct = enabled;
        let field_value = serde_json::to_value(&field).unwrap();
        let index_value = serde_json::to_value(&index).unwrap();
        if enabled {
            assert_eq!(field_value["nulls_not_distinct"], true);
            assert_eq!(index_value["nulls_not_distinct"], true);
        } else {
            assert!(field_value.get("nulls_not_distinct").is_none());
            assert!(index_value.get("nulls_not_distinct").is_none());
        }
        assert_eq!(
            serde_json::from_value::<FieldDef>(field_value).unwrap(),
            field
        );
        assert_eq!(
            serde_json::from_value::<IndexDef>(index_value).unwrap(),
            index
        );
    }
    let mut explicit_false = field_json;
    explicit_false["nulls_not_distinct"] = false.into();
    assert!(
        !serde_json::from_value::<FieldDef>(explicit_false)
            .unwrap()
            .nulls_not_distinct
    );
    let mut explicit_false = index_json;
    explicit_false["nulls_not_distinct"] = false.into();
    assert!(
        !serde_json::from_value::<IndexDef>(explicit_false)
            .unwrap()
            .nulls_not_distinct
    );
}

#[test]
fn index_nulls_not_distinct_change_recreates_index() {
    for enabled in [true, false] {
        let mut old_table = sample_table();
        old_table.indexes[0].nulls_not_distinct = !enabled;
        let mut new_table = old_table.clone();
        new_table.indexes[0].nulls_not_distinct = enabled;
        assert!(!old_table.indexes[0].semantically_eq(&new_table.indexes[0]));
        let mut old = Snapshot::new();
        old.add_table(old_table);
        let mut new = Snapshot::new();
        new.add_table(new_table);
        let ops = compute_diff(&old, &new).unwrap();
        assert_eq!(ops.len(), 2);
        assert!(
            matches!(&ops[0], MigrationOp::DropIndex { index_def: Some(index), .. }
            if index.nulls_not_distinct != enabled)
        );
        assert!(matches!(&ops[1], MigrationOp::CreateIndex { index, .. }
            if index.nulls_not_distinct == enabled));
    }
}

#[test]
fn field_nulls_not_distinct_change_alters_only_unique_columns() {
    for unique in [false, true] {
        for enabled in [true, false] {
            let mut old_table = sample_table();
            old_table.fields[1].unique = unique;
            old_table.fields[1].nulls_not_distinct = !enabled;
            let mut new_table = old_table.clone();
            new_table.fields[1].nulls_not_distinct = enabled;
            let mut old = Snapshot::new();
            old.add_table(old_table);
            let mut new = Snapshot::new();
            new.add_table(new_table);
            let ops = compute_diff(&old, &new).unwrap();
            if unique {
                assert_eq!(ops.len(), 1);
                assert!(
                    matches!(&ops[0], MigrationOp::AlterColumn { old_field: Some(old_field), new_field: Some(new_field), .. }
                    if old_field.nulls_not_distinct != enabled && new_field.nulls_not_distinct == enabled)
                );
                assert_eq!(ops[0].to_sql(Dialect::Postgres).unwrap().len(), 2);
            } else {
                assert!(ops.is_empty());
            }
        }
    }
}

#[test]
fn test_check_constraint_expression_change_generates_drop_and_add() {
    let mut old = Snapshot::new();
    let mut old_table = sample_table();
    old_table.checks.push(CheckDef {
        name: "age_positive".into(),
        expression: "age >= 0".into(),
    });
    old.add_table(old_table);

    let mut new = Snapshot::new();
    let mut new_table = sample_table();
    new_table.checks.push(CheckDef {
        name: "age_positive".into(),
        expression: "age > 0".into(),
    });
    new.add_table(new_table);

    let ops = compute_diff(&old, &new).unwrap();
    assert_eq!(ops.len(), 2);

    match &ops[0] {
        MigrationOp::DropCheck {
            table,
            name,
            check_def,
        } => {
            assert_eq!(table, "users");
            assert_eq!(name, "age_positive");
            assert_eq!(check_def.as_ref().unwrap().expression, "age >= 0");
        }
        op => panic!("expected DropCheck, got {op:?}"),
    }

    match &ops[1] {
        MigrationOp::AddCheck { table, check } => {
            assert_eq!(table, "users");
            assert_eq!(check.name, "age_positive");
            assert_eq!(check.expression, "age > 0");
        }
        op => panic!("expected AddCheck, got {op:?}"),
    }
}

#[test]
fn test_migration_create_table_generates_sql() {
    let sql = MigrationOp::CreateTable {
        table: sample_table(),
    }
    .to_sql(Dialect::Postgres)
    .unwrap();

    assert!(sql[0].contains(r#"CREATE TABLE "users""#));
    assert!(sql[1].contains(r#"CREATE UNIQUE INDEX "users_email_idx""#));
}

#[test]
fn test_migration_preserves_authored_operation_order() {
    let migration = Migration {
        name: "drop_users".into(),
        operations: vec![
            MigrationOp::DropCheck {
                table: "users".into(),
                name: "chk_users_id".into(),
                check_def: None,
            },
            MigrationOp::DropTable {
                name: "users".into(),
                table: None,
            },
        ],
    };

    let sql = migration.to_sql(Dialect::Postgres).unwrap();
    assert!(sql[0].contains("DROP CONSTRAINT chk_users_id"));
    assert!(sql[1].starts_with("DROP TABLE"));
}

#[test]
fn test_postgres_enum_type_is_created_before_table() {
    let migration = Migration {
        name: "enum".into(),
        operations: vec![
            MigrationOp::CreateEnumType {
                name: "post_status_enum".into(),
                values: vec!["draft".into(), "published".into()],
            },
            MigrationOp::CreateTable {
                table: enum_table(&["draft", "published"]),
            },
        ],
    };

    let sql = migration.to_sql(Dialect::Postgres).unwrap();
    assert_eq!(
        sql[0],
        r#"CREATE TYPE "post_status_enum" AS ENUM ('draft', 'published')"#
    );
    assert!(
        sql[1].contains(r#""status" "post_status_enum" NOT NULL"#),
        "{}",
        sql[1]
    );
}

#[test]
fn test_compute_diff_creates_enum_before_dependent_table() {
    let old = Snapshot::new();
    let mut new = Snapshot::new();
    new.add_table(enum_table(&["draft", "published"]));

    let operations = compute_diff(&old, &new).unwrap();
    assert!(
        matches!(operations.first(), Some(MigrationOp::CreateEnumType { name, .. }) if name == "post_status_enum"),
        "enum type must be created before its dependent table: {operations:?}"
    );
    assert!(
        matches!(operations.get(1), Some(MigrationOp::CreateTable { table }) if table.name == "posts"),
        "dependent table must follow its enum type: {operations:?}"
    );
}

#[test]
fn test_compute_diff_drops_enum_after_dependent_table() {
    let mut old = Snapshot::new();
    old.add_table(enum_table(&["draft", "published"]));
    let new = Snapshot::new();

    let operations = compute_diff(&old, &new).unwrap();
    assert!(
        matches!(operations.first(), Some(MigrationOp::DropTable { name, .. }) if name == "posts"),
        "dependent table must be dropped before its enum type: {operations:?}"
    );
    assert!(
        matches!(operations.get(1), Some(MigrationOp::DropEnumType { name, .. }) if name == "post_status_enum"),
        "enum type must be dropped after its dependent table: {operations:?}"
    );
}

#[test]
fn test_compute_diff_adds_enum_value_without_altering_column() {
    let mut old = Snapshot::new();
    old.add_table(enum_table(&["draft", "published"]));
    let mut new = Snapshot::new();
    new.add_table(enum_table(&["draft", "published", "archived"]));

    let ops = compute_diff(&old, &new).unwrap();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        MigrationOp::AddEnumValue {
            name,
            value,
            fields,
        } => {
            assert_eq!(name, "post_status_enum");
            assert_eq!(value, "archived");
            assert_eq!(fields.len(), 1);
            assert_eq!(fields[0].table, "posts");
            assert_eq!(fields[0].field.name, "status");
        }
        op => panic!("expected AddEnumValue, got {op:?}"),
    }
}

#[test]
fn test_compute_diff_adds_postgres_enum_value_before_dependent_ddl() {
    let mut old = Snapshot::new();
    old.add_table(enum_table(&["draft", "published"]));

    let mut new_table = enum_table(&["draft", "published", "archived"]);
    new_table.indexes.push(IndexDef {
        name: "posts_archived_idx".into(),
        fields: vec!["status".into()],
        unique: false,
        nulls_not_distinct: false,
        method: Some("btree".into()),
        where_clause: Some("status = 'archived'".into()),
    });
    let mut new = Snapshot::new();
    new.add_table(new_table);

    let operations = compute_diff(&old, &new).unwrap();
    let migration = Migration {
        name: "enum_value".into(),
        operations,
    };

    let sql = migration.to_sql(Dialect::Postgres).unwrap();
    assert_eq!(
        sql[0],
        r#"ALTER TYPE "post_status_enum" ADD VALUE IF NOT EXISTS 'archived'"#
    );
    assert!(sql[1].contains(r#"CREATE INDEX "posts_archived_idx""#));
}

#[test]
fn test_compute_diff_emits_manual_enum_alter_for_value_removal() {
    let mut old = Snapshot::new();
    old.add_table(enum_table(&["draft", "published"]));
    let mut new = Snapshot::new();
    new.add_table(enum_table(&["draft"]));

    let ops = compute_diff(&old, &new).unwrap();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        MigrationOp::AlterEnumType {
            name,
            old_values,
            new_values,
        } => {
            assert_eq!(name, "post_status_enum");
            assert_eq!(
                old_values,
                &vec!["draft".to_string(), "published".to_string()]
            );
            assert_eq!(new_values, &vec!["draft".to_string()]);
        }
        op => panic!("expected AlterEnumType, got {op:?}"),
    }
}

#[test]
fn test_postgres_create_table_with_fk_as_alter() {
    // PostgreSQL should have FK constraints as separate ALTER TABLE
    let table = TableDef {
        name: "posts".into(),
        fields: vec![FieldDef {
            name: "id".into(),
            column_type: ColumnTypeSpec::BigInteger,
            db_type: None,
            nullable: false,
            primary_key: true,
            unique: false,
            nulls_not_distinct: false,
            default: None,
            auto_increment: false,
            max_length: None,
            max_digits: None,
            decimal_places: None,
        }],
        indexes: vec![],
        foreign_keys: vec![ForeignKeyDef {
            name: "fk_posts_author".into(),
            columns: vec!["author_id".into()],
            ref_table: "users".into(),
            ref_columns: vec!["id".into()],
            on_delete: Some("CASCADE".into()),
            on_update: None,
        }],
        checks: vec![],
        comment: None,
    };

    let sql = MigrationOp::CreateTable { table }
        .to_sql(Dialect::Postgres)
        .unwrap();

    // Should have 2 statements (CREATE TABLE + ALTER TABLE for FK)
    assert_eq!(
        sql.len(),
        2,
        "PostgreSQL should generate ALTER TABLE for FK"
    );
    assert!(sql[1].contains("ADD CONSTRAINT"));
    assert!(sql[1].contains("FOREIGN KEY"));
}

#[test]
fn test_migration_add_column_sql() {
    let sql = MigrationOp::AddColumn {
        table: "users".into(),
        field: sample_field("name"),
    }
    .to_sql(Dialect::Postgres)
    .unwrap();

    assert_eq!(
        sql,
        vec![r#"ALTER TABLE "users" ADD COLUMN "name" VARCHAR(255) NOT NULL"#.to_string()]
    );
}

#[test]
fn test_compute_diff_detects_new_table_and_column() {
    let old = Snapshot::new();
    let mut new_snapshot = Snapshot::new();
    let mut table = sample_table();
    table.fields.push(sample_field("name"));
    new_snapshot.add_table(table);

    let ops = compute_diff(&old, &new_snapshot).unwrap();
    assert!(matches!(ops[0], MigrationOp::CreateTable { .. }));
}

#[test]
fn test_compute_diff_detects_alter_column() {
    // Create old snapshot with a table
    let mut old = Snapshot::new();
    let old_table = TableDef {
        name: "users".into(),
        fields: vec![
            FieldDef {
                name: "id".into(),
                column_type: ColumnTypeSpec::BigInteger,
                db_type: None,
                nullable: false,
                primary_key: true,
                unique: false,
                nulls_not_distinct: false,
                default: None,
                auto_increment: true,
                max_length: None,
                max_digits: None,
                decimal_places: None,
            },
            FieldDef {
                name: "email".into(),
                column_type: ColumnTypeSpec::String { length: None },
                db_type: Some("VARCHAR(100)".into()),
                nullable: false,
                primary_key: false,
                unique: true,
                nulls_not_distinct: false,
                default: None,
                auto_increment: false,
                max_length: None,
                max_digits: None,
                decimal_places: None,
            },
        ],
        indexes: vec![],
        foreign_keys: vec![],
        checks: vec![],
        comment: None,
    };
    old.add_table(old_table);

    // Create new snapshot with modified email field
    let mut new_snapshot = Snapshot::new();
    let new_table = TableDef {
        name: "users".into(),
        fields: vec![
            FieldDef {
                name: "id".into(),
                column_type: ColumnTypeSpec::BigInteger,
                db_type: None,
                nullable: false,
                primary_key: true,
                unique: false,
                nulls_not_distinct: false,
                default: None,
                auto_increment: true,
                max_length: None,
                max_digits: None,
                decimal_places: None,
            },
            FieldDef {
                name: "email".into(),
                column_type: ColumnTypeSpec::String { length: None },
                db_type: Some("VARCHAR(255)".into()), // Changed db_type
                nullable: true,                       // Changed nullable
                primary_key: false,
                unique: true,
                nulls_not_distinct: false,
                default: None,
                auto_increment: false,
                max_length: None,
                max_digits: None,
                decimal_places: None,
            },
        ],
        indexes: vec![],
        foreign_keys: vec![],
        checks: vec![],
        comment: None,
    };
    new_snapshot.add_table(new_table);

    let ops = compute_diff(&old, &new_snapshot).unwrap();

    // Should detect AlterColumn for email field
    assert_eq!(ops.len(), 1, "Should have exactly one operation");
    match &ops[0] {
        MigrationOp::AlterColumn {
            table,
            old_field: Some(old_field),
            new_field: Some(new_field),
            ..
        } => {
            assert_eq!(table, "users");
            assert_eq!(old_field.name, "email");
            assert_eq!(old_field.db_type, Some("VARCHAR(100)".into()));
            assert_eq!(new_field.db_type, Some("VARCHAR(255)".into()));
            assert!(!old_field.nullable);
            assert!(new_field.nullable);
        }
        other => panic!("Expected AlterColumn, got {other:?}"),
    }
}

#[test]
fn test_postgres_alter_column_unique_constraint() {
    // Test adding unique constraint
    let old_field = FieldDef {
        name: "email".into(),
        column_type: ColumnTypeSpec::String { length: None },
        db_type: None,
        nullable: false,
        primary_key: false,
        unique: false,
        nulls_not_distinct: false,
        default: None,
        auto_increment: false,
        max_length: None,
        max_digits: None,
        decimal_places: None,
    };
    let new_field = FieldDef {
        name: "email".into(),
        column_type: ColumnTypeSpec::String { length: None },
        db_type: None,
        nullable: false,
        primary_key: false,
        unique: true, // Changed to unique
        nulls_not_distinct: false,
        default: None,
        auto_increment: false,
        max_length: None,
        max_digits: None,
        decimal_places: None,
    };

    let sql = MigrationOp::AlterColumn {
        column: None,
        changes: None,
        table: "users".into(),
        old_field: Some(Box::new(old_field.clone())),
        new_field: Some(Box::new(new_field.clone())),
        table_fields: None,
        table_indexes: None,
        table_foreign_keys: None,
        table_checks: None,
    }
    .to_sql(Dialect::Postgres)
    .unwrap();

    assert_eq!(sql.len(), 1, "Should have one statement");
    assert!(
        sql[0].contains("ADD CONSTRAINT"),
        "Should add constraint: {}",
        sql[0]
    );
    assert!(
        sql[0].contains("UNIQUE"),
        "Should be UNIQUE constraint: {}",
        sql[0]
    );

    // Test removing unique constraint
    let sql = MigrationOp::AlterColumn {
        column: None,
        changes: None,
        table: "users".into(),
        old_field: Some(Box::new(new_field)),
        new_field: Some(Box::new(old_field)),
        table_fields: None,
        table_indexes: None,
        table_foreign_keys: None,
        table_checks: None,
    }
    .to_sql(Dialect::Postgres)
    .unwrap();

    assert_eq!(sql.len(), 1, "Should have one statement");
    assert!(
        sql[0].contains("DROP CONSTRAINT"),
        "Should drop constraint: {}",
        sql[0]
    );
}

#[test]
fn alter_unique_uses_the_name_postgres_gives_an_inline_unique() {
    // An inline `UNIQUE` is named by PostgreSQL's `makeObjectName`: it shortens
    // the longer of table and column name and keeps the `_key` suffix. The
    // ALTER path must use the same name, or its DROP CONSTRAINT misses the
    // constraint CREATE TABLE made. (A plain cut at 63 bytes loses the suffix.)
    let table = "a".repeat(60);
    let expected = format!("\"{}_code_key\"", "a".repeat(54));
    let plain = sample_field("code");
    let unique = FieldDef {
        unique: true,
        ..sample_field("code")
    };
    let alter = |old: &FieldDef, new: &FieldDef| {
        MigrationOp::AlterColumn {
            column: None,
            changes: None,
            table: table.clone(),
            old_field: Some(Box::new(old.clone())),
            new_field: Some(Box::new(new.clone())),
            table_fields: None,
            table_indexes: None,
            table_foreign_keys: None,
            table_checks: None,
        }
        .to_sql(Dialect::Postgres)
        .unwrap()
    };

    let added = alter(&plain, &unique);
    assert_eq!(added.len(), 1, "{added:?}");
    assert!(
        added[0].contains(&format!("ADD CONSTRAINT {expected} UNIQUE")),
        "{added:?}"
    );
    let dropped = alter(&unique, &plain);
    assert_eq!(dropped.len(), 1, "{dropped:?}");
    assert!(
        dropped[0].ends_with(&format!("DROP CONSTRAINT {expected}")),
        "{dropped:?}"
    );

    // Short names are untouched: existing migrations keep their constraint names.
    let short = MigrationOp::AlterColumn {
        column: None,
        changes: None,
        table: "users".into(),
        old_field: Some(Box::new(unique.clone())),
        new_field: Some(Box::new(plain)),
        table_fields: None,
        table_indexes: None,
        table_foreign_keys: None,
        table_checks: None,
    }
    .to_sql(Dialect::Postgres)
    .unwrap();
    assert!(short[0].ends_with("DROP CONSTRAINT \"users_code_key\""), "{short:?}");
}

#[test]
fn test_compute_diff_detects_dropped_table() {
    let mut old = Snapshot::new();
    old.add_table(sample_table());

    let new = Snapshot::new(); // Empty - table dropped

    let ops = compute_diff(&old, &new).unwrap();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        MigrationOp::DropTable { name, table } => {
            assert_eq!(name, "users");
            assert!(table.is_some(), "Should include table def for rollback");
        }
        _ => panic!("Expected DropTable"),
    }
}

#[test]
fn test_compute_diff_detects_dropped_column() {
    let mut old = Snapshot::new();
    old.add_table(sample_table());

    let mut new = Snapshot::new();
    let mut table = sample_table();
    table.fields.retain(|f| f.name != "email"); // Remove email column
    new.add_table(table);

    let ops = compute_diff(&old, &new).unwrap();
    let drop_ops: Vec<_> = ops
        .iter()
        .filter(|op| matches!(op, MigrationOp::DropColumn { .. }))
        .collect();
    assert_eq!(drop_ops.len(), 1);
    match drop_ops[0] {
        MigrationOp::DropColumn {
            table,
            field,
            field_def,
        } => {
            assert_eq!(table, "users");
            assert_eq!(field, "email");
            assert!(field_def.is_some());
        }
        _ => panic!("Expected DropColumn"),
    }
}

#[test]
fn test_compute_diff_drops_column_dependencies_before_the_column() {
    let parent = table_with_fields("parents", &["id"]);
    let mut old_users = table_with_fields("users", &["id", "age"]);
    old_users.indexes.push(IndexDef {
        name: "ix_users_age".into(),
        fields: vec!["age".into()],
        unique: false,
        nulls_not_distinct: false,
        method: None,
        where_clause: None,
    });
    old_users.foreign_keys.push(sample_foreign_key(
        "fk_users_age",
        &["age"],
        "parents",
        &["id"],
    ));
    old_users.checks.push(CheckDef {
        name: "chk_users_age".into(),
        expression: "age IS NOT NULL".into(),
    });

    let mut old = Snapshot::new();
    old.add_table(parent.clone());
    old.add_table(old_users);

    let mut new = Snapshot::new();
    new.add_table(parent);
    new.add_table(table_with_fields("users", &["id"]));

    let operations = compute_diff(&old, &new).unwrap();
    for dialect in [Dialect::Postgres] {
        let sql = Migration {
            name: "drop_age".into(),
            operations: operations.clone(),
        }
        .to_sql(dialect)
        .unwrap();
        let drop_column = sql
            .iter()
            .position(|statement| statement.contains("DROP COLUMN"))
            .unwrap();

        for dependency in ["ix_users_age", "fk_users_age", "chk_users_age"] {
            let dependency_drop = sql
                .iter()
                .position(|statement| statement.contains(dependency))
                .unwrap();
            assert!(
                dependency_drop < drop_column,
                "{dependency} must be dropped before the column for {dialect:?}: {sql:?}"
            );
        }
    }
}

#[test]
fn test_compute_diff_drops_fk_before_its_referenced_column() {
    let old_parent = table_with_fields("a_parent", &["id", "code"]);
    let mut old_child = table_with_fields("z_child", &["id", "parent_code"]);
    old_child.foreign_keys.push(sample_foreign_key(
        "fk_child_parent_code",
        &["parent_code"],
        "a_parent",
        &["code"],
    ));

    let mut old = Snapshot::new();
    old.add_table(old_parent);
    old.add_table(old_child);

    let mut new = Snapshot::new();
    new.add_table(table_with_fields("a_parent", &["id"]));
    new.add_table(table_with_fields("z_child", &["id", "parent_code"]));

    let operations = compute_diff(&old, &new).unwrap();
    {
        let dialect = Dialect::Postgres;
        let sql = Migration {
            name: "drop_parent_code".into(),
            operations: operations.clone(),
        }
        .to_sql(dialect)
        .unwrap();
        let drop_fk = sql
            .iter()
            .position(|statement| statement.contains("fk_child_parent_code"))
            .unwrap();
        let drop_column = sql
            .iter()
            .position(|statement| statement.contains("DROP COLUMN"))
            .unwrap();

        assert!(
            drop_fk < drop_column,
            "the FK must be dropped before its referenced column for {dialect:?}: {sql:?}"
        );
    }
}

#[test]
fn test_compute_diff_drops_fk_before_its_referenced_table() {
    let parent = table_with_fields("a_parent", &["id"]);
    let mut old_child = table_with_fields("z_child", &["id", "parent_id"]);
    old_child.foreign_keys.push(sample_foreign_key(
        "fk_child_parent",
        &["parent_id"],
        "a_parent",
        &["id"],
    ));

    let mut old = Snapshot::new();
    old.add_table(parent);
    old.add_table(old_child);

    let mut new = Snapshot::new();
    new.add_table(table_with_fields("z_child", &["id", "parent_id"]));

    let operations = compute_diff(&old, &new).unwrap();
    {
        let dialect = Dialect::Postgres;
        let sql = Migration {
            name: "drop_parent".into(),
            operations: operations.clone(),
        }
        .to_sql(dialect)
        .unwrap();
        let drop_fk = sql
            .iter()
            .position(|statement| statement.contains("fk_child_parent"))
            .unwrap();
        let drop_table = sql
            .iter()
            .position(|statement| statement.starts_with("DROP TABLE"))
            .unwrap();

        assert!(
            drop_fk < drop_table,
            "the FK must be dropped before its referenced table for {dialect:?}: {sql:?}"
        );
    }
}

#[test]
fn test_compute_diff_adds_local_and_referenced_columns_before_fk() {
    let mut old = Snapshot::new();
    old.add_table(table_with_fields("a_child", &["id"]));
    old.add_table(table_with_fields("z_parent", &["id"]));

    let mut new_child = table_with_fields("a_child", &["id", "parent_code"]);
    new_child.foreign_keys.push(sample_foreign_key(
        "fk_child_parent_code",
        &["parent_code"],
        "z_parent",
        &["code"],
    ));
    let mut new = Snapshot::new();
    new.add_table(new_child);
    new.add_table(table_with_fields("z_parent", &["id", "code"]));

    let operations = compute_diff(&old, &new).unwrap();
    {
        let dialect = Dialect::Postgres;
        let sql = Migration {
            name: "add_parent_code_fk".into(),
            operations: operations.clone(),
        }
        .to_sql(dialect)
        .unwrap();
        let add_local_column = sql
            .iter()
            .position(|statement| statement.contains("ADD COLUMN") && statement.contains("a_child"))
            .unwrap();
        let add_referenced_column = sql
            .iter()
            .position(|statement| {
                statement.contains("ADD COLUMN") && statement.contains("z_parent")
            })
            .unwrap();
        let add_fk = sql
            .iter()
            .position(|statement| statement.contains("fk_child_parent_code"))
            .unwrap();

        assert!(
            add_local_column < add_fk && add_referenced_column < add_fk,
            "both FK columns must exist before the FK for {dialect:?}: {sql:?}"
        );
    }
}

#[test]
fn test_compute_diff_adds_existing_table_column_before_new_table_fk() {
    let mut old = Snapshot::new();
    old.add_table(table_with_fields("z_parent", &["id"]));

    let mut new_parent = table_with_fields("z_parent", &["id", "code"]);
    new_parent
        .fields
        .iter_mut()
        .find(|field| field.name == "code")
        .unwrap()
        .unique = true;

    let mut new_child = table_with_fields("a_child", &["id", "parent_code"]);
    new_child.foreign_keys.push(sample_foreign_key(
        "fk_child_parent_code",
        &["parent_code"],
        "z_parent",
        &["code"],
    ));

    let mut new = Snapshot::new();
    new.add_table(new_parent);
    new.add_table(new_child);

    let operations = compute_diff(&old, &new).unwrap();
    {
        let dialect = Dialect::Postgres;
        let sql = Migration {
            name: "create_child_with_new_parent_column".into(),
            operations: operations.clone(),
        }
        .to_sql(dialect)
        .unwrap();
        let add_referenced_column = sql
            .iter()
            .position(|statement| {
                statement.contains("ADD COLUMN")
                    && statement.contains("z_parent")
                    && statement.contains("code")
            })
            .unwrap();
        let add_fk = sql
            .iter()
            .position(|statement| statement.contains("fk_child_parent_code"))
            .unwrap();

        assert!(
            add_referenced_column < add_fk,
            "the referenced column must exist before the new table's FK for {dialect:?}: {sql:?}"
        );
    }
}

#[test]
fn test_compute_diff_preserves_staging_while_unblocking_new_table() {
    let obsolete = table_with_fields("obsolete", &["id"]);
    let parent = table_with_fields("z_parent", &["id"]);

    let mut old = Snapshot::new();
    old.add_table(obsolete);
    old.add_table(parent.clone());

    let mut new_parent = parent;
    new_parent.fields.push(sample_field("code"));
    let mut child = table_with_fields("a_child", &["id", "parent_code"]);
    child.foreign_keys.push(sample_foreign_key(
        "fk_child_parent_code",
        &["parent_code"],
        "z_parent",
        &["code"],
    ));

    let mut new = Snapshot::new();
    new.add_table(new_parent);
    new.add_table(child);

    let operations = compute_diff(&old, &new).unwrap();
    assert!(
        matches!(operations.first(), Some(MigrationOp::DropTable { name, .. }) if name == "obsolete"),
        "an unrelated staged drop must remain before the later column change: {operations:?}"
    );
    assert!(
        matches!(operations.get(1), Some(MigrationOp::AddColumn { table, field }) if table == "z_parent" && field.name == "code"),
        "the referenced column must precede the new dependent table: {operations:?}"
    );
    assert!(
        matches!(operations.get(2), Some(MigrationOp::CreateTable { table }) if table.name == "a_child"),
        "the dependent table must be unblocked without crossing the staged drop: {operations:?}"
    );
}

#[test]
fn test_compute_diff_creates_referenced_index_before_fk() {
    let child = table_with_fields("a_child", &["id", "parent_code"]);
    let parent = table_with_fields("z_parent", &["id", "code"]);

    let mut old = Snapshot::new();
    old.add_table(child.clone());
    old.add_table(parent.clone());

    let mut new_child = child;
    new_child.foreign_keys.push(sample_foreign_key(
        "fk_child_parent_code",
        &["parent_code"],
        "z_parent",
        &["code"],
    ));
    let mut new_parent = parent;
    new_parent.indexes.push(IndexDef {
        name: "ux_parent_code".into(),
        fields: vec!["code".into()],
        unique: true,
        nulls_not_distinct: false,
        method: None,
        where_clause: None,
    });

    let mut new = Snapshot::new();
    new.add_table(new_child);
    new.add_table(new_parent);

    let operations = compute_diff(&old, &new).unwrap();
    {
        let dialect = Dialect::Postgres;
        let sql = Migration {
            name: "add_index_and_fk".into(),
            operations: operations.clone(),
        }
        .to_sql(dialect)
        .unwrap();
        let create_index = sql
            .iter()
            .position(|statement| statement.contains("ux_parent_code"))
            .unwrap();
        let add_fk = sql
            .iter()
            .position(|statement| statement.contains("fk_child_parent_code"))
            .unwrap();

        assert!(
            create_index < add_fk,
            "the referenced index must exist before the FK for {dialect:?}: {sql:?}"
        );
    }
}

#[test]
fn test_compute_diff_detects_index_changes() {
    let mut old = Snapshot::new();
    old.add_table(sample_table());

    let mut new = Snapshot::new();
    let mut table = sample_table();
    table.indexes.clear(); // Remove index
    table.indexes.push(IndexDef {
        name: "users_name_idx".into(), // New index
        fields: vec!["name".into()],
        unique: false,
        nulls_not_distinct: false,
        method: None,
        where_clause: None,
    });
    new.add_table(table);

    let ops = compute_diff(&old, &new).unwrap();

    let create_idx = ops.iter().any(
        |op| matches!(op, MigrationOp::CreateIndex { index, .. } if index.name == "users_name_idx"),
    );
    let drop_idx = ops
        .iter()
        .any(|op| matches!(op, MigrationOp::DropIndex { name, .. } if name == "users_email_idx"));

    assert!(create_idx, "Should detect new index");
    assert!(drop_idx, "Should detect dropped index");
}

#[test]
fn test_compute_diff_detects_partial_index_predicate_change() {
    let mut old = Snapshot::new();
    old.add_table(sample_table());

    let mut new = Snapshot::new();
    let mut table = sample_table();
    table.indexes[0].where_clause = Some("deleted_at IS NULL".into());
    new.add_table(table);

    let ops = compute_diff(&old, &new).unwrap();

    assert!(
        matches!(ops.first(), Some(MigrationOp::DropIndex { name, .. }) if name == "users_email_idx")
    );
    assert!(
        matches!(
            ops.get(1),
            Some(MigrationOp::CreateIndex { index, .. })
                if index.name == "users_email_idx"
                    && index.where_clause.as_deref() == Some("deleted_at IS NULL")
        ),
        "predicate changes should rebuild the index, got {ops:?}"
    );
}

#[test]
fn test_compute_diff_ignores_partial_index_predicate_whitespace() {
    let mut old = Snapshot::new();
    old.add_table(sample_table());

    let mut new = Snapshot::new();
    let mut table = sample_table();
    table.indexes[0].where_clause = Some("  deleted_at IS NULL  ".into());
    new.add_table(table);

    let mut old_table = old.tables.get_mut("users").unwrap().clone();
    old_table.indexes[0].where_clause = Some("deleted_at IS NULL".into());
    old.tables.insert("users".into(), old_table);

    let ops = compute_diff(&old, &new).unwrap();

    assert!(
        ops.is_empty(),
        "whitespace-only predicate changes should not diff"
    );
}

#[test]
fn test_drop_table_sql() {
    let sql = MigrationOp::DropTable {
        name: "users".into(),
        table: None,
    }
    .to_sql(Dialect::Postgres)
    .unwrap();

    assert_eq!(sql.len(), 1);
    assert_eq!(sql[0], r#"DROP TABLE "users""#);
}

#[test]
fn test_create_drop_index_sql() {
    // CreateIndex
    let sql = MigrationOp::CreateIndex {
        table: "users".into(),
        index: IndexDef {
            name: "users_email_idx".into(),
            fields: vec!["email".into()],
            unique: true,
            nulls_not_distinct: false,
            method: Some("btree".into()),
            where_clause: None,
        },
    }
    .to_sql(Dialect::Postgres)
    .unwrap();

    assert_eq!(sql.len(), 1);
    assert!(sql[0].contains("CREATE UNIQUE INDEX"));
    assert!(sql[0].contains("users_email_idx"));
    assert!(sql[0].contains("btree"));

    // DropIndex
    let sql = MigrationOp::DropIndex {
        table: "users".into(),
        name: "users_email_idx".into(),
        index_def: Some(IndexDef {
            name: "users_email_idx".into(),
            fields: vec!["email".into()],
            unique: true,
            nulls_not_distinct: false,
            method: None,
            where_clause: None,
        }),
    }
    .to_sql(Dialect::Postgres)
    .unwrap();

    assert_eq!(sql.len(), 1);
    assert!(sql[0].contains("DROP INDEX"));
    assert!(sql[0].contains("users_email_idx"));
}

#[test]
fn test_partial_index_json_roundtrip_trims_predicate() {
    let snapshot = Snapshot::from_json(
        r#"{
            "version": 1,
            "tables": {
                "users": {
                    "name": "users",
                    "fields": [],
                    "indexes": [{
                        "name": "users_active_email_idx",
                        "fields": ["email"],
                        "unique": true,
                        "method": "btree",
                        "where": "  deleted_at IS NULL  "
                    }],
                    "foreign_keys": [],
                    "checks": [],
                    "comment": null
                }
            }
        }"#,
    )
    .unwrap();

    let json = snapshot.to_json().unwrap();

    assert!(json.contains(r#""where": "deleted_at IS NULL""#));
    assert!(!json.contains("  deleted_at IS NULL  "));
}

#[test]
fn test_rename_table_sql() {
    let sql = MigrationOp::RenameTable {
        old_name: "users".into(),
        new_name: "accounts".into(),
    }
    .to_sql(Dialect::Postgres)
    .unwrap();

    assert_eq!(sql.len(), 1);
    assert!(sql[0].contains("RENAME") || sql[0].contains("ALTER TABLE"));
}

#[test]
fn test_compute_diff_emits_create_tables_in_topological_order() {
    // Chain: posts → comments → reactions (reactions depends on comments, etc.)
    // Topological order must be: posts, comments, reactions — independently
    // of HashMap iteration order.
    fn table_with_fk(name: &str, ref_table: Option<&str>) -> TableDef {
        let mut fks = Vec::new();
        if let Some(r) = ref_table {
            fks.push(ForeignKeyDef {
                name: format!("fk_{name}_{r}"),
                columns: vec![format!("{}_id", r)],
                ref_table: r.to_string(),
                ref_columns: vec!["id".to_string()],
                on_delete: Some("CASCADE".to_string()),
                on_update: Some("NO ACTION".to_string()),
            });
        }
        TableDef {
            name: name.to_string(),
            fields: vec![FieldDef {
                name: "id".into(),
                column_type: ColumnTypeSpec::BigInteger,
                db_type: None,
                nullable: false,
                primary_key: true,
                unique: true,
                nulls_not_distinct: false,
                default: None,
                auto_increment: false,
                max_length: None,
                max_digits: None,
                decimal_places: None,
            }],
            indexes: vec![],
            foreign_keys: fks,
            checks: vec![],
            comment: None,
        }
    }

    // Run multiple times: HashMap iteration order varies per process, but
    // topo-sort output must remain stable within a process — and across runs
    // after the fix.
    for _ in 0..10 {
        let old = Snapshot::new();
        let mut new = Snapshot::new();
        new.add_table(table_with_fk("reactions", Some("comments")));
        new.add_table(table_with_fk("comments", Some("posts")));
        new.add_table(table_with_fk("posts", None));

        let ops = compute_diff(&old, &new).unwrap();
        let create_names: Vec<&str> = ops
            .iter()
            .filter_map(|op| match op {
                MigrationOp::CreateTable { table } => Some(table.name.as_str()),
                _ => None,
            })
            .collect();

        assert_eq!(
            create_names,
            vec!["posts", "comments", "reactions"],
            "CreateTable ops must be in topological order (referenced tables first)"
        );
    }
}

#[test]
fn test_compute_diff_rejects_cyclic_foreign_keys() {
    // a → b → a cycle
    fn table_with_fk(name: &str, ref_table: &str) -> TableDef {
        TableDef {
            name: name.to_string(),
            fields: vec![FieldDef {
                name: "id".into(),
                column_type: ColumnTypeSpec::BigInteger,
                db_type: None,
                nullable: false,
                primary_key: true,
                unique: true,
                nulls_not_distinct: false,
                default: None,
                auto_increment: false,
                max_length: None,
                max_digits: None,
                decimal_places: None,
            }],
            indexes: vec![],
            foreign_keys: vec![ForeignKeyDef {
                name: format!("fk_{name}_{ref_table}"),
                columns: vec![format!("{}_id", ref_table)],
                ref_table: ref_table.to_string(),
                ref_columns: vec!["id".into()],
                on_delete: Some("NO ACTION".into()),
                on_update: Some("NO ACTION".into()),
            }],
            checks: vec![],
            comment: None,
        }
    }

    let old = Snapshot::new();
    let mut new = Snapshot::new();
    new.add_table(table_with_fk("a", "b"));
    new.add_table(table_with_fk("b", "a"));

    let result = compute_diff(&old, &new);
    assert!(result.is_err(), "cyclic FK schema must error out");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("cyclic foreign key"),
        "error should mention cycle: {err}"
    );
    assert!(err.contains('a') && err.contains('b'));
}

#[test]
fn test_compute_diff_emits_drop_tables_in_reverse_topological_order() {
    fn table_with_fk(name: &str, ref_table: Option<&str>) -> TableDef {
        let mut fks = Vec::new();
        if let Some(r) = ref_table {
            fks.push(ForeignKeyDef {
                name: format!("fk_{name}_{r}"),
                columns: vec![format!("{}_id", r)],
                ref_table: r.to_string(),
                ref_columns: vec!["id".to_string()],
                on_delete: Some("CASCADE".to_string()),
                on_update: Some("NO ACTION".to_string()),
            });
        }
        TableDef {
            name: name.to_string(),
            fields: vec![FieldDef {
                name: "id".into(),
                column_type: ColumnTypeSpec::BigInteger,
                db_type: None,
                nullable: false,
                primary_key: true,
                unique: true,
                nulls_not_distinct: false,
                default: None,
                auto_increment: false,
                max_length: None,
                max_digits: None,
                decimal_places: None,
            }],
            indexes: vec![],
            foreign_keys: fks,
            checks: vec![],
            comment: None,
        }
    }

    let mut old = Snapshot::new();
    old.add_table(table_with_fk("posts", None));
    old.add_table(table_with_fk("comments", Some("posts")));
    let new = Snapshot::new();

    let ops = compute_diff(&old, &new).unwrap();
    let drop_names: Vec<&str> = ops
        .iter()
        .filter_map(|op| match op {
            MigrationOp::DropTable { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();

    assert_eq!(
        drop_names,
        vec!["comments", "posts"],
        "DropTable ops must be in reverse topological order (referencing tables first)"
    );
}

// Helper: build a table with a primary key and an arbitrary list of FK refs.
// Used by the cyclic-schema tests below.
fn cyclic_table(name: &str, fk_refs: &[(&str, &str)]) -> TableDef {
    let mut fields = vec![FieldDef {
        name: "id".into(),
        column_type: ColumnTypeSpec::BigInteger,
        db_type: None,
        nullable: false,
        primary_key: true,
        unique: true,
        nulls_not_distinct: false,
        default: None,
        auto_increment: false,
        max_length: None,
        max_digits: None,
        decimal_places: None,
    }];
    let mut foreign_keys = Vec::new();
    for (col, ref_table) in fk_refs {
        fields.push(FieldDef {
            name: (*col).to_string(),
            column_type: ColumnTypeSpec::BigInteger,
            db_type: None,
            nullable: true,
            primary_key: false,
            unique: false,
            nulls_not_distinct: false,
            default: None,
            auto_increment: false,
            max_length: None,
            max_digits: None,
            decimal_places: None,
        });
        foreign_keys.push(ForeignKeyDef {
            name: format!("fk_{name}_{col}"),
            columns: vec![(*col).to_string()],
            ref_table: (*ref_table).to_string(),
            ref_columns: vec!["id".into()],
            on_delete: Some("NO ACTION".into()),
            on_update: Some("NO ACTION".into()),
        });
    }
    TableDef {
        name: name.to_string(),
        fields,
        indexes: vec![],
        foreign_keys,
        checks: vec![],
        comment: None,
    }
}

#[test]
fn test_compute_diff_noop_on_cyclic_schema() {
    // when old == new and the schema contains a
    // mutual-FK cycle, compute_diff must return [] instead of erroring out
    // on the topological sort. Cycle is only a problem when tables actually
    // need to be created/dropped — for unchanged tables it's irrelevant.
    let mut snap = Snapshot::new();
    snap.add_table(cyclic_table("companies", &[("user_id", "users")]));
    snap.add_table(cyclic_table("users", &[("company_id", "companies")]));

    let ops =
        compute_diff(&snap, &snap).expect("no-op diff on cyclic schema must succeed, not error");
    assert!(
        ops.is_empty(),
        "no-op diff must produce zero operations, got {ops:?}"
    );
}

#[test]
fn test_compute_diff_modify_column_in_cyclic_schema() {
    // old and new both contain the same mutual-FK cycle; new adds an extra
    // column to one of the cyclic tables. Diff must produce just the
    // AddColumn operation, not an error.
    let mut old = Snapshot::new();
    old.add_table(cyclic_table("companies", &[("user_id", "users")]));
    old.add_table(cyclic_table("users", &[("company_id", "companies")]));

    let mut new = Snapshot::new();
    let mut companies_v2 = cyclic_table("companies", &[("user_id", "users")]);
    companies_v2.fields.push(sample_field("name"));
    new.add_table(companies_v2);
    new.add_table(cyclic_table("users", &[("company_id", "companies")]));

    let ops = compute_diff(&old, &new).expect("modification on cyclic schema must succeed");

    let add_columns: Vec<&str> = ops
        .iter()
        .filter_map(|op| match op {
            MigrationOp::AddColumn { table, field } if table == "companies" => {
                Some(field.name.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        add_columns,
        vec!["name"],
        "expected exactly one AddColumn(companies.name), got ops={ops:?}"
    );
}

#[test]
fn test_compute_diff_create_table_referencing_cyclic_subset() {
    // old already contains a cyclic pair (companies <-> users). new adds a
    // third table (orders) that references users via FK. The CREATE subset
    // is just {orders} and is acyclic, so diff must succeed and emit a
    // CreateTable for orders. FKs from orders to the existing 'users' must
    // be ignored by the topo-sort because users is not in the create subset.
    let mut old = Snapshot::new();
    old.add_table(cyclic_table("companies", &[("user_id", "users")]));
    old.add_table(cyclic_table("users", &[("company_id", "companies")]));

    let mut new = Snapshot::new();
    new.add_table(cyclic_table("companies", &[("user_id", "users")]));
    new.add_table(cyclic_table("users", &[("company_id", "companies")]));
    new.add_table(cyclic_table("orders", &[("user_id", "users")]));

    let ops = compute_diff(&old, &new)
        .expect("creating an acyclic table on top of a cyclic schema must succeed");

    let create_names: Vec<&str> = ops
        .iter()
        .filter_map(|op| match op {
            MigrationOp::CreateTable { table } => Some(table.name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        create_names,
        vec!["orders"],
        "expected exactly one CreateTable(orders), got ops={ops:?}"
    );
}

// ── Expression and operator-class index keys ───────────────────────────────

mod index_keys {
    use super::*;
    use serde_json::{json, Value};

    fn col(name: &str) -> Value {
        json!({"type": "column", "name": name})
    }

    #[allow(clippy::needless_pass_by_value)] // call-site ergonomics in test builders
    fn lit(value: Value) -> Value {
        json!({"type": "value", "value": value})
    }

    #[allow(clippy::needless_pass_by_value)] // call-site ergonomics in test builders
    fn func(name: &str, args: Vec<Value>) -> Value {
        json!({"type": "func", "name": name, "args": args})
    }

    fn key(value: Value) -> IndexKey {
        serde_json::from_value(value).expect("valid index key")
    }

    fn key_error(value: Value) -> String {
        serde_json::from_value::<IndexKey>(value)
            .expect_err("index key must be rejected")
            .to_string()
    }

    fn index_with(name: &str, fields: Vec<Value>) -> IndexDef {
        IndexDef {
            name: name.into(),
            fields: fields.into_iter().map(key).collect(),
            unique: false,
            nulls_not_distinct: false,
            method: None,
            where_clause: None,
        }
    }

    #[test]
    fn plain_columns_keep_the_bare_string_wire_form() {
        let index_json = json!({
            "name": "ix", "fields": ["tenant_id", "email"], "unique": false, "method": null,
        });
        let index: IndexDef = serde_json::from_value(index_json.clone()).unwrap();
        assert_eq!(
            index.fields,
            vec![IndexKey::from("tenant_id"), IndexKey::from("email")]
        );
        assert_eq!(serde_json::to_value(&index).unwrap(), index_json);
    }

    #[test]
    fn equivalent_spellings_of_a_plain_column_normalize_to_a_bare_string() {
        let expected = IndexKey::from("email");
        for spelling in [
            json!("email"),
            json!({"column": "email"}),
            json!({"expr": col("email")}),
        ] {
            let parsed = key(spelling);
            assert_eq!(parsed, expected);
            assert_eq!(serde_json::to_value(&parsed).unwrap(), json!("email"));
        }
        // With an opclass, both object spellings normalize to the column form.
        let with_opclass = json!({"column": "name", "opclass": "text_pattern_ops"});
        assert_eq!(
            key(json!({"expr": col("name"), "opclass": "text_pattern_ops"})),
            key(with_opclass.clone())
        );
        assert_eq!(
            serde_json::to_value(key(with_opclass.clone())).unwrap(),
            with_opclass
        );
    }

    #[test]
    fn expression_and_opclass_keys_round_trip() {
        for wire in [
            json!({"column": "name", "opclass": "text_pattern_ops"}),
            json!({"column": "name", "opclass": "public.text_pattern_ops"}),
            json!({"expr": func("lower", vec![col("email")])}),
            json!({
                "expr": func("lower", vec![col("name")]),
                "opclass": "gin_trgm_ops",
            }),
            json!({"expr": func("coalesce", vec![col("qty"), lit(json!(1))])}),
            json!({"expr": {"type": "raw", "sql": "(data -> 'tags')"}}),
        ] {
            let parsed = key(wire.clone());
            assert_eq!(serde_json::to_value(&parsed).unwrap(), wire);
            assert_eq!(key(serde_json::to_value(&parsed).unwrap()), parsed);
        }
    }

    #[test]
    fn malformed_keys_are_rejected_when_read() {
        let both = key_error(json!({"column": "a", "expr": col("a")}));
        assert!(both.contains("exactly one"), "{both}");
        let neither = key_error(json!({"opclass": "text_pattern_ops"}));
        assert!(neither.contains("exactly one"), "{neither}");

        for bad in [
            "x; DROP TABLE users",
            "a.b.c",
            "",
            ".x",
            "x.",
            "1abc",
            "pg trgm",
            "\"quoted\"",
        ] {
            let message = key_error(json!({"column": "name", "opclass": bad}));
            assert!(message.contains("operator class"), "{bad:?}: {message}");
        }

        // concat_ws is STABLE in PostgreSQL, so it cannot back an index,
        // however deeply it is nested.
        let nested = func(
            "coalesce",
            vec![
                func("concat_ws", vec![lit(json!(" ")), col("a")]),
                lit(json!("")),
            ],
        );
        let stable = key_error(json!({"expr": nested}));
        assert!(
            stable.contains("only coalesce, greatest, lower and upper"),
            "{stable}"
        );

        // A modifier this core does not know must fail, not be dropped.
        assert!(serde_json::from_value::<IndexKey>(json!({"column": "a", "desc": true})).is_err());
        assert!(serde_json::from_value::<IndexKey>(json!(42)).is_err());
    }

    // ── Canonical wire form of expression keys ─────────────────────────────

    fn enum_spec(values: &[&str]) -> Value {
        json!({"kind": "enum", "name": "status", "values": values})
    }

    fn enum_array_spec(values: &[&str]) -> Value {
        json!({"kind": "array", "item": enum_spec(values)})
    }

    /// An expression key over a `list[Status]` column, as Python writes it.
    fn enum_array_key(values: &[&str]) -> Value {
        json!({
            "expr": func("coalesce", vec![
                col("tags"),
                json!({"type": "value", "value": [], "value_type": enum_array_spec(values)}),
                json!({"type": "value", "value": "a", "value_type": enum_spec(values)}),
            ]),
            "column_types": {"tags": enum_array_spec(values)},
        })
    }

    #[test]
    fn enum_values_do_not_leak_into_expression_keys() {
        assert_eq!(
            key(enum_array_key(&["a"])),
            key(enum_array_key(&["a", "b"]))
        );

        let wire = serde_json::to_value(key(enum_array_key(&["a", "b"]))).unwrap();
        // The enum keeps its name (DDL needs it) but not its value list.
        assert_eq!(wire["column_types"]["tags"], enum_array_spec(&[]));
        let args = &wire["expr"]["args"];
        assert_eq!(args[1]["value_type"], enum_array_spec(&[]));
        // A bare enum literal is typed by PostgreSQL from the column.
        assert!(args[2].get("value_type").is_none(), "{args}");
    }

    #[test]
    fn enum_value_changes_do_not_recreate_expression_indexes() {
        let index = |values: &[&str]| index_with("ix_tags", vec![enum_array_key(values)]);
        let old = snapshot_with_index(&["id", "tags"], Some(index(&["a"])));
        let new = snapshot_with_index(&["id", "tags"], Some(index(&["a", "b"])));
        assert!(compute_diff(&old, &new).unwrap().is_empty());
    }

    #[test]
    fn column_types_serialize_in_a_stable_order() {
        let array = json!({"kind": "array", "item": {"kind": "big_integer"}});
        let wire = json!({
            "expr": func("greatest", vec![col("zeta"), col("mid"), col("alpha")]),
            "column_types": {"zeta": array, "mid": array, "alpha": array},
        });
        // Each parse builds a fresh map, so an unordered map would differ
        // between iterations.
        for _ in 0..32 {
            let text = serde_json::to_string(&key(wire.clone())).unwrap();
            let (alpha, mid, zeta) = (
                text.find("\"alpha\":{").unwrap(),
                text.find("\"mid\":{").unwrap(),
                text.find("\"zeta\":{").unwrap(),
            );
            assert!(alpha < mid && mid < zeta, "{text}");
        }
    }

    #[test]
    fn binary_literals_are_canonical_bytes() {
        let wire = json!({
            "expr": func("coalesce", vec![
                col("data"),
                json!({"type": "value", "value": [222, 173, 190, 239],
                       "value_type": {"kind": "blob"}}),
            ]),
        });
        let parsed = key(wire.clone());
        let IndexKey::Expr {
            expr: ScalarExpr::Func { args, .. },
            ..
        } = &parsed
        else {
            panic!("expected an expression key: {parsed:?}");
        };
        let ScalarExpr::Value { value, .. } = &args[1] else {
            panic!("expected a literal: {args:?}");
        };
        assert!(value.is_bin(), "{value:?}");
        // The octet-list wire form is unchanged.
        assert_eq!(serde_json::to_value(&parsed).unwrap(), wire);

        // Without a blob type, a list of small integers is just a list.
        let list = key(json!({
            "expr": func("coalesce", vec![col("xs"), lit(json!([1, 2, 3]))]),
        }));
        let IndexKey::Expr {
            expr: ScalarExpr::Func { args, .. },
            ..
        } = &list
        else {
            panic!("expected an expression key");
        };
        let ScalarExpr::Value { value, .. } = &args[1] else {
            panic!("expected a literal");
        };
        assert!(value.is_array(), "{value:?}");
    }

    #[test]
    fn byte_array_literals_are_canonical_and_render_as_bytes() {
        let wire = json!({
            "expr": func("coalesce", vec![
                col("blobs"),
                json!({"type": "value", "value": [[1, 2], []],
                       "value_type": {"kind": "array", "item": {"kind": "blob"}}}),
            ]),
            "column_types": {"blobs": {"kind": "array", "item": {"kind": "blob"}}},
        });
        let parsed = key(wire.clone());
        let IndexKey::Expr {
            expr: ScalarExpr::Func { args, .. },
            ..
        } = &parsed
        else {
            panic!("expected an expression key: {parsed:?}");
        };
        let ScalarExpr::Value {
            value: rmpv::Value::Array(elements),
            ..
        } = &args[1]
        else {
            panic!("expected an array literal: {args:?}");
        };
        assert!(elements.iter().all(rmpv::Value::is_bin), "{elements:?}");
        // The octet-list wire form is unchanged.
        assert_eq!(serde_json::to_value(&parsed).unwrap(), wire);

        let op = MigrationOp::CreateIndex {
            table: "t".into(),
            index: index_with("ix_blobs", vec![wire]),
        };
        let sql = op.to_sql(Dialect::Postgres).unwrap();
        assert!(sql[0].contains("0102"), "{sql:?}");
    }

    #[test]
    fn only_immutable_functions_can_back_an_index() {
        for allowed in ["lower", "upper", "coalesce", "greatest"] {
            key(json!({"expr": func(allowed, vec![col("a")])}));
        }
        for rejected in ["concat_ws", "CONCAT_WS", "LOWER", "frobnicate"] {
            let message = key_error(json!({"expr": func(rejected, vec![col("a")])}));
            assert!(message.contains(rejected), "{message}");
            assert!(
                message.contains("only coalesce, greatest, lower and upper"),
                "{message}"
            );
        }
    }

    #[test]
    fn malformed_key_fails_the_whole_index_and_snapshot() {
        let bad = json!({"name": "ix", "fields": [{"column": "a", "opclass": "x; y"}],
                         "unique": false, "method": null});
        assert!(serde_json::from_value::<IndexDef>(bad).is_err());
    }

    #[test]
    fn column_helpers_report_what_the_index_depends_on() {
        let mixed = index_with(
            "ix_mixed",
            vec![
                json!("tenant_id"),
                json!({"expr": func("lower", vec![col("email")])}),
                json!("created_at"),
            ],
        );
        // The usable prefix ends at the first expression key.
        assert_eq!(
            mixed.leading_columns().collect::<Vec<_>>(),
            vec!["tenant_id"]
        );
        assert_eq!(
            mixed.referenced_columns(),
            vec!["tenant_id", "email", "created_at"]
        );
        assert!(mixed.references_column("email"));
        assert!(!mixed.references_column("missing"));

        // Explicit operator classes can still back foreign keys.
        let opclass_first = index_with(
            "ix_opclass",
            vec![
                json!({"column": "name", "opclass": "text_pattern_ops"}),
                json!("id"),
            ],
        );
        assert_eq!(
            opclass_first.leading_columns().collect::<Vec<_>>(),
            vec!["name", "id"]
        );
        assert_eq!(opclass_first.referenced_columns(), vec!["name", "id"]);
        assert!(opclass_first.references_column("name"));

        let plain = index_with("ix_plain", vec![json!("a"), json!("b")]);
        assert_eq!(plain.leading_columns().collect::<Vec<_>>(), vec!["a", "b"]);
        assert!(plain.references_column("b"));

        // Nested expressions are walked, repeats are reported once, and raw
        // SQL is opaque.
        let nested = index_with(
            "ix_nested",
            vec![
                json!({"expr": {"type": "op", "op": "mul", "lhs": col("a"),
                    "rhs": func("coalesce", vec![col("b"), col("a"), lit(json!(1))])}}),
                json!({"expr": {"type": "raw", "sql": "lower(hidden)"}}),
            ],
        );
        assert_eq!(nested.referenced_columns(), vec!["a", "b"]);
    }

    fn snapshot_with_index(columns: &[&str], index: Option<IndexDef>) -> Snapshot {
        let mut table = table_with_fields("users", columns);
        table.indexes = index.into_iter().collect();
        let mut snapshot = Snapshot::new();
        snapshot.add_table(table);
        snapshot
    }

    fn lower_email(name: &str) -> IndexDef {
        index_with(
            name,
            vec![json!({"expr": func("lower", vec![col("email")])})],
        )
    }

    #[test]
    fn unchanged_expression_index_produces_no_ops() {
        let old = snapshot_with_index(&["id", "email"], Some(lower_email("ix_lower")));
        let new = snapshot_with_index(&["id", "email"], Some(lower_email("ix_lower")));
        assert!(compute_diff(&old, &new).unwrap().is_empty());
    }

    #[test]
    fn opclass_index_is_created_before_a_new_referencing_table_and_dropped_after_it() {
        let old = snapshot_with_index(&["id", "email"], None);
        let mut unique = index_with(
            "ux_email",
            vec![json!({
                "column": "email", "opclass": "text_pattern_ops"
            })],
        );
        unique.unique = true;
        let mut new = snapshot_with_index(&["id", "email"], Some(unique));
        let mut child = table_with_fields("child", &["email"]);
        child.foreign_keys.push(sample_foreign_key(
            "fk_child",
            &["email"],
            "users",
            &["email"],
        ));
        new.add_table(child);

        for (before, after, adding) in [(&old, &new, true), (&new, &old, false)] {
            let ops = compute_diff(before, after).unwrap();
            let index_pos = ops
                .iter()
                .position(|op| {
                    matches!(
                        op,
                        MigrationOp::CreateIndex { .. } | MigrationOp::DropIndex { .. }
                    )
                })
                .unwrap();
            let table_pos = ops
                .iter()
                .position(|op| {
                    matches!(
                        op,
                        MigrationOp::CreateTable { .. } | MigrationOp::DropTable { .. }
                    )
                })
                .unwrap();
            assert_eq!(index_pos < table_pos, adding, "{ops:?}");
        }
    }

    #[test]
    fn array_index_context_survives_round_trip_and_column_rename() {
        let wire = json!({
            "expr": func("coalesce", vec![col("tags"), lit(json!([1, 2]))]),
            "column_types": {"tags": {"kind": "array", "item": {"kind": "big_integer"}}}
        });
        assert_eq!(serde_json::to_value(key(wire.clone())).unwrap(), wire);
        let mut index = index_with("ix_tags", vec![wire]);
        index.rename_column("tags", "labels");
        let op = MigrationOp::CreateIndex {
            table: "t".into(),
            index,
        };
        let sql = op.to_sql(Dialect::Postgres).unwrap();
        assert!(
            sql[0].contains("COALESCE(\"labels\", ARRAY [1,2])"),
            "{sql:?}"
        );
    }

    #[test]
    fn changed_expression_or_opclass_recreates_the_index() {
        let changed = [
            index_with(
                "ix_lower",
                vec![json!({"expr": func("upper", vec![col("email")])})],
            ),
            index_with(
                "ix_lower",
                vec![json!({
                    "expr": func("lower", vec![col("email")]),
                    "opclass": "text_pattern_ops",
                })],
            ),
            index_with("ix_lower", vec![json!("email")]),
        ];
        for new_index in changed {
            let old = snapshot_with_index(&["id", "email"], Some(lower_email("ix_lower")));
            let new = snapshot_with_index(&["id", "email"], Some(new_index.clone()));
            let ops = compute_diff(&old, &new).unwrap();
            assert_eq!(ops.len(), 2, "{ops:?}");
            assert!(
                matches!(&ops[0], MigrationOp::DropIndex { name, index_def: Some(old_def), .. }
                if name == "ix_lower" && *old_def == lower_email("ix_lower"))
            );
            assert!(matches!(&ops[1], MigrationOp::CreateIndex { index, .. }
                if *index == new_index));
        }
    }

    // The final order is a topological sort seeded with the natural order, so
    // these pin the invariants (they hold with or without the explicit edges).
    #[test]
    fn index_on_an_expression_orders_around_its_columns() {
        let dropped_column = compute_diff(
            &snapshot_with_index(&["id", "email"], Some(lower_email("ix_lower"))),
            &snapshot_with_index(&["id"], None),
        )
        .unwrap();
        let position = |ops: &[MigrationOp], wanted: fn(&MigrationOp) -> bool| {
            ops.iter().position(wanted).expect("op present")
        };
        assert!(
            position(&dropped_column, |op| matches!(
                op,
                MigrationOp::DropIndex { .. }
            )) < position(&dropped_column, |op| matches!(
                op,
                MigrationOp::DropColumn { .. }
            ))
        );

        let added_column = compute_diff(
            &snapshot_with_index(&["id"], None),
            &snapshot_with_index(&["id", "email"], Some(lower_email("ix_lower"))),
        )
        .unwrap();
        assert!(
            position(&added_column, |op| matches!(
                op,
                MigrationOp::AddColumn { .. }
            )) < position(&added_column, |op| matches!(
                op,
                MigrationOp::CreateIndex { .. }
            ))
        );
    }

    #[test]
    fn unsupported_function_in_an_index_expression_fails_sql_generation() {
        let expr: ScalarExpr =
            serde_json::from_value(func("frobnicate", vec![col("email")])).unwrap();
        let op = MigrationOp::CreateIndex {
            table: "users".into(),
            index: IndexDef {
                name: "ix_bad".into(),
                fields: vec![IndexKey::Expr {
                    expr,
                    opclass: None,
                    column_types: std::collections::BTreeMap::new(),
                }],
                unique: false,
                nulls_not_distinct: false,
                method: None,
                where_clause: None,
            },
        };
        let error = op.to_sql(Dialect::Postgres).unwrap_err().to_string();
        assert!(error.contains("invalid index expression"), "{error}");
        assert!(error.contains("frobnicate"), "{error}");
    }

    // ── Readable key errors ────────────────────────────────────────────────

    #[test]
    fn malformed_keys_report_the_actual_problem() {
        let unknown = key_error(json!({"columns": "email"}));
        assert!(unknown.contains("unknown field `columns`"), "{unknown}");
        assert!(!unknown.contains("did not match any variant"), "{unknown}");

        let bad_expr = key_error(json!({"expr": {"type": "colum", "name": "email"}}));
        assert!(bad_expr.contains("colum"), "{bad_expr}");
        assert!(
            !bad_expr.contains("did not match any variant"),
            "{bad_expr}"
        );

        let empty_opclass = key_error(json!({"column": "name", "opclass": ""}));
        assert!(
            empty_opclass.contains("invalid operator class"),
            "{empty_opclass}"
        );

        let wrong_shape = key_error(json!(42));
        assert!(wrong_shape.contains("an index key"), "{wrong_shape}");
    }

    // ── Model snapshot validation ──────────────────────────────────────────

    fn model_snapshot(tables: Vec<(&str, Vec<&str>, Vec<IndexDef>)>) -> Snapshot {
        let mut snapshot = Snapshot::new();
        for (name, columns, indexes) in tables {
            let table: TableDef = serde_json::from_value(json!({
                "name": name,
                "fields": columns.iter().map(|column| json!({
                    "name": column, "column_type": {"kind": "text"}, "nullable": true,
                    "primary_key": false, "unique": false, "default": null,
                })).collect::<Vec<_>>(),
                "indexes": indexes,
                "foreign_keys": [],
                "checks": [],
                "comment": null,
            }))
            .unwrap();
            snapshot.add_table(table);
        }
        snapshot
    }

    #[test]
    fn model_validation_accepts_a_consistent_schema() {
        let snapshot = model_snapshot(vec![
            (
                "users",
                vec!["id", "email"],
                vec![
                    index_with("users_email_idx", vec![json!("email")]),
                    index_with(
                        "users_email_lower",
                        vec![json!({"expr": func("lower", vec![col("email")])})],
                    ),
                    // Raw SQL is opaque: its columns are not checked.
                    index_with(
                        "users_raw",
                        vec![json!({"expr": {"type": "raw", "sql": "lower(anything)"}})],
                    ),
                ],
            ),
            (
                "posts",
                vec!["id"],
                vec![index_with("posts_id_idx", vec![json!("id")])],
            ),
        ]);
        snapshot.validate_model_schema().unwrap();
    }

    #[test]
    fn model_validation_rejects_unknown_index_columns() {
        let plain = model_snapshot(vec![(
            "users",
            vec!["id", "email_addr"],
            vec![index_with("users_email_idx", vec![json!("email")])],
        )]);
        let error = plain.validate_model_schema().unwrap_err().to_string();
        assert!(error.contains("users_email_idx"), "{error}");
        assert!(error.contains("'users'"), "{error}");
        assert!(error.contains("'email'"), "{error}");

        let nested = model_snapshot(vec![(
            "users",
            vec!["id"],
            vec![index_with(
                "users_lower",
                vec![json!({"expr": func("lower", vec![func("upper", vec![col("missing")])])})],
            )],
        )]);
        let error = nested.validate_model_schema().unwrap_err().to_string();
        assert!(error.contains("'missing'"), "{error}");
    }

    #[test]
    fn model_validation_rejects_duplicate_index_names() {
        let within = model_snapshot(vec![(
            "users",
            vec!["email"],
            vec![
                index_with("users_email_idx", vec![json!("email")]),
                index_with(
                    "users_email_idx",
                    vec![json!({"column": "email", "opclass": "text_pattern_ops"})],
                ),
            ],
        )]);
        let error = within.validate_model_schema().unwrap_err().to_string();
        assert!(error.contains("users_email_idx"), "{error}");
        assert!(error.contains("name="), "{error}");

        // PostgreSQL index names share one namespace across tables.
        let across = model_snapshot(vec![
            (
                "a",
                vec!["x"],
                vec![index_with("shared_idx", vec![json!("x")])],
            ),
            (
                "b",
                vec!["x"],
                vec![index_with("shared_idx", vec![json!("x")])],
            ),
        ]);
        let error = across.validate_model_schema().unwrap_err().to_string();
        assert!(error.contains("shared_idx"), "{error}");
        assert!(error.contains("'a'") && error.contains("'b'"), "{error}");
    }

    fn flag(snapshot: &mut Snapshot, table: &str, column: &str, set: impl FnOnce(&mut FieldDef)) {
        let field = snapshot
            .tables
            .get_mut(table)
            .unwrap()
            .fields
            .iter_mut()
            .find(|field| field.name == column)
            .unwrap();
        set(field);
    }

    /// `users(id bigserial pk, email unique)` plus an empty `orders`, with one
    /// explicit index on `users.email` named `index_name`.
    fn users_with_index_named(index_name: &str) -> Snapshot {
        let mut snapshot = model_snapshot(vec![
            (
                "users",
                vec!["id", "email"],
                vec![index_with(index_name, vec![json!("email")])],
            ),
            ("orders", vec!["id"], vec![]),
        ]);
        // An integer primary key renders as BIGSERIAL, which owns a sequence.
        flag(&mut snapshot, "users", "id", |field| {
            field.primary_key = true;
            field.column_type = ColumnTypeSpec::BigInteger;
        });
        flag(&mut snapshot, "users", "email", |field| field.unique = true);
        snapshot
    }

    #[test]
    fn model_validation_rejects_index_names_taken_by_other_relations() {
        // Indexes, tables and sequences share one PostgreSQL namespace, and
        // CREATE TABLE has already taken these names for the relations it
        // creates itself.
        for (name, owner) in [
            ("users_email_key", "unique constraint"),
            ("users_pkey", "primary key"),
            ("users_id_seq", "sequence"),
            ("orders", "table"),
            ("users", "table"),
        ] {
            let error = users_with_index_named(name)
                .validate_model_schema()
                .unwrap_err()
                .to_string();
            assert!(error.contains(&format!("'{name}'")), "{name}: {error}");
            assert!(error.contains(owner), "{name}: {error}");
            assert!(error.contains("name="), "{name}: {error}");
        }
        users_with_index_named("users_email_idx")
            .validate_model_schema()
            .unwrap();
    }

    #[test]
    fn implicit_relation_names_are_only_reserved_when_they_exist() {
        // `email` is not unique, `id` is no primary key and not serial.
        let snapshot = model_snapshot(vec![(
            "users",
            vec!["id", "email"],
            vec![
                index_with("users_email_key", vec![json!("email")]),
                index_with("users_pkey", vec![json!("id")]),
                index_with("users_id_seq", vec![json!("id")]),
            ],
        )]);
        snapshot.validate_model_schema().unwrap();
    }

    #[test]
    fn long_implicit_names_are_shortened_the_way_postgres_does() {
        // PostgreSQL does not cut `<table>_<column>_key` at 63 bytes: it
        // shortens the longer of table and column name and keeps the suffix.
        let table = "a".repeat(60);
        let build = |index_name: &str| {
            let mut snapshot = model_snapshot(vec![(
                table.as_str(),
                vec!["id"],
                vec![index_with(index_name, vec![json!("id")])],
            )]);
            flag(&mut snapshot, &table, "id", |field| field.unique = true);
            snapshot
        };

        let shortened = format!("{}_id_key", "a".repeat(56));
        assert_eq!(shortened.len(), 63);
        let error = build(&shortened)
            .validate_model_schema()
            .unwrap_err()
            .to_string();
        assert!(error.contains("unique constraint"), "{error}");

        // A plain 63-byte cut is a different name and does not clash.
        build(&format!("{table}_id")).validate_model_schema().unwrap();

        // `<table>_pkey` keeps the whole suffix too.
        let long_table = "b".repeat(80);
        let mut pk = model_snapshot(vec![(
            long_table.as_str(),
            vec!["id"],
            vec![index_with(
                &format!("{}_pkey", "b".repeat(58)),
                vec![json!("id")],
            )],
        )]);
        flag(&mut pk, &long_table, "id", |field| field.primary_key = true);
        let error = pk.validate_model_schema().unwrap_err().to_string();
        assert!(error.contains("primary key"), "{error}");
    }

    #[test]
    fn model_validation_rejects_names_equal_after_truncation() {
        let long = |tail: &str| format!("{}{tail}", "a".repeat(63));
        let clash = model_snapshot(vec![(
            "t",
            vec!["x"],
            vec![
                index_with(&long("_one"), vec![json!("x")]),
                index_with(&long("_two"), vec![json!("x")]),
            ],
        )]);
        let error = clash.validate_model_schema().unwrap_err().to_string();
        assert!(error.contains("truncates"), "{error}");

        // The cut lands inside a multibyte character and moves back to the
        // boundary: 62 ASCII bytes + 'é' (2 bytes) truncates to the 62 bytes.
        let prefix = "b".repeat(62);
        let multibyte = model_snapshot(vec![(
            "t",
            vec!["x"],
            vec![
                index_with(&format!("{prefix}é_one"), vec![json!("x")]),
                index_with(&format!("{prefix}é_two"), vec![json!("x")]),
            ],
        )]);
        assert!(multibyte.validate_model_schema().is_err());

        // A long name on its own is fine; PostgreSQL truncates it silently.
        let single = model_snapshot(vec![(
            "t",
            vec!["x"],
            vec![index_with(&"c".repeat(80), vec![json!("x")])],
        )]);
        single.validate_model_schema().unwrap();
    }

    #[test]
    fn historical_snapshots_still_load_and_diff_without_validation() {
        // Validation is a separate gate: an index on a column that is not in
        // `fields` must still deserialize and replay.
        let snapshot = model_snapshot(vec![(
            "users",
            vec!["id"],
            vec![index_with("users_old_idx", vec![json!("gone")])],
        )]);
        let round_trip = Snapshot::from_json(&snapshot.to_json().unwrap()).unwrap();
        assert_eq!(round_trip, snapshot);
    }

    #[test]
    fn references_column_matches_referenced_columns() {
        let nested = index_with(
            "ix",
            vec![
                json!("a"),
                json!({"expr": func("coalesce", vec![col("b"), lit(json!(1))])}),
                json!({"expr": {"type": "raw", "sql": "c + 1"}}),
            ],
        );
        for column in ["a", "b", "c", "missing"] {
            assert_eq!(
                nested.references_column(column),
                nested.referenced_columns().contains(&column),
                "{column}"
            );
        }
    }
}
