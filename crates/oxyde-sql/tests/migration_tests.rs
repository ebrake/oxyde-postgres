//! Integration tests for the diff → DDL pipeline.
//!
//! Tests for public API: MigrationOpExt::to_sql(), compute_diff(), Snapshot.

use oxyde_codec::{
    CheckDef, ColumnTypeSpec, FieldDef, ForeignKeyDef, IndexDef, MigrationOp, Snapshot, TableDef,
};
use oxyde_migrate::compute_diff;
use oxyde_sql::{Dialect, Migration, MigrationOpExt};

fn sample_field(name: &str) -> FieldDef {
    FieldDef {
        replay_shape: Default::default(),
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
                replay_shape: Default::default(),
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
            replay_shape: Default::default(),
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
        replay_shape: Default::default(),
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
                replay_shape: Default::default(),
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
                replay_shape: Default::default(),
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
        replay_shape: Default::default(),
        name: "age_positive".into(),
        expression: "age >= 0".into(),
    });
    old.add_table(old_table);

    let mut new = Snapshot::new();
    let mut new_table = sample_table();
    new_table.checks.push(CheckDef {
        replay_shape: Default::default(),
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
        replay_shape: Default::default(),
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
            replay_shape: Default::default(),
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
            replay_shape: Default::default(),
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
                replay_shape: Default::default(),
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
                replay_shape: Default::default(),
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
                replay_shape: Default::default(),
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
                replay_shape: Default::default(),
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
        replay_shape: Default::default(),
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
        replay_shape: Default::default(),
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
        replay_shape: Default::default(),
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
        replay_shape: Default::default(),
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
        replay_shape: Default::default(),
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
        replay_shape: Default::default(),
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
            replay_shape: Default::default(),
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
            replay_shape: Default::default(),
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
                replay_shape: Default::default(),
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
                replay_shape: Default::default(),
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
                replay_shape: Default::default(),
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
                replay_shape: Default::default(),
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
                replay_shape: Default::default(),
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
                replay_shape: Default::default(),
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
        replay_shape: Default::default(),
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
            replay_shape: Default::default(),
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
            replay_shape: Default::default(),
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
