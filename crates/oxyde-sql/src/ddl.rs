//! DDL generation using sea-query builders + migration statement ordering.
//!
//! Type mapping is spec-driven (`spec_sql::resolve_spec_type`): semantic kind
//! from `ColumnTypeSpec`, verbatim user DDL from `FieldDef.db_type`. DDL
//! structure (CREATE/ALTER/DROP TABLE, indexes, foreign keys) uses sea-query
//! for dialect-specific syntax and identifier quoting.

use sea_query::{
    Alias, ColumnDef as SeaColumnDef, Expr, ForeignKey as SeaForeignKey,
    ForeignKeyAction as SeaFkAction, Index as SeaIndex, IndexType, IntoIden, PostgresQueryBuilder,
    Table as SeaTable,
};
use serde::{Deserialize, Serialize};

use crate::Dialect;
use oxyde_codec::{FieldDef, ForeignKeyDef, IndexDef, MigrateError, MigrationOp};

type Result<T> = std::result::Result<T, MigrateError>;

/// Migration file: a named list of operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Migration {
    pub name: String,
    pub operations: Vec<MigrationOp>,
}

impl Migration {
    /// Generate SQL statements for this migration.
    ///
    /// Operations are rendered in authored order. Statements emitted by one
    /// operation remain contiguous and in renderer order. Callers are
    /// responsible for supplying dependency-safe operation ordering.
    pub fn to_sql(&self, dialect: Dialect) -> Result<Vec<String>> {
        let mut all_sql = Vec::new();
        for op in &self.operations {
            all_sql.extend(op.to_sql(dialect)?);
        }
        Ok(all_sql)
    }
}

/// Build SQL string from a sea-query schema statement for the given dialect.
macro_rules! build_sql {
    ($stmt:expr, $dialect:expr) => {
        match $dialect {
            Dialect::Postgres => $stmt.build(PostgresQueryBuilder),
        }
    };
}

// ── sea-query helpers ───────────────────────────────────────────────────────

/// Convert `FieldDef` to sea-query `ColumnDef` with dialect-appropriate type and constraints.
fn field_to_column_def(field: &FieldDef, dialect: Dialect) -> SeaColumnDef {
    let sql_type = crate::spec_sql::resolve_spec_type(
        &field.column_type,
        field.db_type.as_deref(),
        dialect,
        field.primary_key,
    );
    let mut col = SeaColumnDef::new(Alias::new(&field.name));
    col.custom(Alias::new(sql_type));

    // sea-query's .extra() renders before .primary_key(), so for this case
    // we emit both keywords via .extra() and skip .primary_key().

    if field.primary_key {
        col.primary_key();
    }

    if field.auto_increment {
        match dialect {
            Dialect::Postgres => {} // SERIAL type handles auto-increment
        }
    }

    if !field.nullable && !field.primary_key {
        col.not_null();
    }

    if field.unique && !field.primary_key {
        col.unique_key();
    }

    if let Some(default) = &field.default {
        col.default(Expr::cust(default));
    }

    col
}

/// Parse FK action string to sea-query `ForeignKeyAction`.
fn parse_fk_action(action: Option<&str>) -> SeaFkAction {
    match action.unwrap_or("NO ACTION").to_uppercase().as_str() {
        "CASCADE" => SeaFkAction::Cascade,
        "SET NULL" => SeaFkAction::SetNull,
        "SET DEFAULT" => SeaFkAction::SetDefault,
        "RESTRICT" => SeaFkAction::Restrict,
        _ => SeaFkAction::NoAction,
    }
}

/// Build a sea-query `ForeignKeyCreateStatement` from `ForeignKeyDef`.
fn build_fk_stmt(table: &str, fk: &ForeignKeyDef) -> sea_query::ForeignKeyCreateStatement {
    let mut stmt = SeaForeignKey::create();
    stmt.name(&fk.name)
        .from_tbl(Alias::new(table))
        .to_tbl(Alias::new(&fk.ref_table))
        .on_delete(parse_fk_action(fk.on_delete.as_deref()))
        .on_update(parse_fk_action(fk.on_update.as_deref()));
    for col in &fk.columns {
        stmt.from_col(Alias::new(col));
    }
    for col in &fk.ref_columns {
        stmt.to_col(Alias::new(col));
    }
    stmt
}

/// Build CREATE INDEX SQL for an index on a table.
fn build_create_index(table: &str, index: &IndexDef, dialect: Dialect) -> String {
    let mut stmt = SeaIndex::create();
    stmt.name(&index.name).table(Alias::new(table));

    if index.unique {
        stmt.unique();
    }

    for field in &index.fields {
        stmt.col(Alias::new(field));
    }

    // Index method (USING btree/hash/gin/gist) — Postgres only
    {
        if let Some(method) = &index.method {
            stmt.index_type(IndexType::Custom(Alias::new(method).into_iden()));
        }
    }

    let mut sql = build_sql!(stmt, dialect);

    if let Some(predicate) = index.normalized_where_clause() {
        sql.push_str(" WHERE ");
        sql.push_str(predicate);
    }

    sql
}

// ── MigrationOp::to_sql ────────────────────────────────────────────────────

/// SQL rendering for `MigrationOp` (the type itself lives in oxyde-codec).
pub trait MigrationOpExt {
    fn to_sql(&self, dialect: Dialect) -> Result<Vec<String>>;
}

impl MigrationOpExt for MigrationOp {
    /// Generate SQL for this migration operation.
    ///
    /// Returns `Err` for operations not supported by the dialect
    fn to_sql(&self, dialect: Dialect) -> Result<Vec<String>> {
        match self {
            MigrationOp::CreateEnumType { name, values } => {
                let labels = values
                    .iter()
                    .map(|value| crate::spec_sql::quote_sql_string(value))
                    .collect::<Vec<_>>()
                    .join(", ");
                Ok(vec![format!(
                    "CREATE TYPE {} AS ENUM ({})",
                    crate::utils::bind::quote_pg_type_path(name),
                    labels
                )])
            }

            MigrationOp::DropEnumType { name, values: _ } => Ok(vec![format!(
                "DROP TYPE {}",
                crate::utils::bind::quote_pg_type_path(name)
            )]),

            MigrationOp::AddEnumValue {
                name,
                value,
                fields: _,
            } => Ok(vec![format!(
                "ALTER TYPE {} ADD VALUE IF NOT EXISTS {}",
                crate::utils::bind::quote_pg_type_path(name),
                crate::spec_sql::quote_sql_string(value)
            )]),

            MigrationOp::AlterEnumType { .. } => Ok(Vec::new()),

            MigrationOp::CreateTable { table } => {
                let mut create = SeaTable::create();
                create.table(Alias::new(&table.name));

                for field in &table.fields {
                    create.col(field_to_column_def(field, dialect));
                }

                let mut sql = vec![build_sql!(create, dialect)];

                // Indexes (all dialects)
                for index in &table.indexes {
                    sql.push(build_create_index(&table.name, index, dialect));
                }

                // The migration plan must create referenced tables first.
                {
                    for fk in &table.foreign_keys {
                        sql.push(build_sql!(build_fk_stmt(&table.name, fk), dialect));
                    }
                    for check in &table.checks {
                        sql.push(format!(
                            "ALTER TABLE {} ADD CONSTRAINT {} CHECK ({})",
                            table.name, check.name, check.expression
                        ));
                    }
                }

                Ok(sql)
            }

            MigrationOp::DropTable { name, table: _ } => Ok(vec![build_sql!(
                SeaTable::drop().table(Alias::new(name)),
                dialect
            )]),

            MigrationOp::RenameTable { old_name, new_name } => Ok(vec![build_sql!(
                SeaTable::rename().table(Alias::new(old_name), Alias::new(new_name)),
                dialect
            )]),

            MigrationOp::AddColumn { table, field } => {
                let col = field_to_column_def(field, dialect);
                Ok(vec![build_sql!(
                    SeaTable::alter().table(Alias::new(table)).add_column(col),
                    dialect
                )])
            }

            MigrationOp::DropColumn {
                table,
                field,
                field_def: _,
            } => Ok(vec![build_sql!(
                SeaTable::alter()
                    .table(Alias::new(table))
                    .drop_column(Alias::new(field)),
                dialect
            )]),

            MigrationOp::RenameColumn {
                table,
                old_name,
                new_name,
                field_def: _,
            } => Ok(vec![build_sql!(
                SeaTable::alter()
                    .table(Alias::new(table))
                    .rename_column(Alias::new(old_name), Alias::new(new_name)),
                dialect
            )]),

            MigrationOp::AlterColumn {
                table,
                old_field,
                new_field,
                table_fields: _,
                table_indexes: _,
                table_foreign_keys: _,
                table_checks: _,
            } => match dialect {
                Dialect::Postgres => {
                    let mut stmts = Vec::new();
                    let old_sql_type = crate::spec_sql::resolve_spec_type(
                        &old_field.column_type,
                        old_field.db_type.as_deref(),
                        dialect,
                        old_field.primary_key,
                    );
                    let new_sql_type = crate::spec_sql::resolve_spec_type(
                        &new_field.column_type,
                        new_field.db_type.as_deref(),
                        dialect,
                        new_field.primary_key,
                    );

                    if old_sql_type != new_sql_type {
                        // ::text bridge — universal cast path (PG has no implicit text → enum).
                        stmts.push(format!(
                            "ALTER TABLE \"{}\" ALTER COLUMN \"{}\" TYPE {} USING \"{}\"::text::{}",
                            table, new_field.name, new_sql_type, new_field.name, new_sql_type
                        ));
                    }

                    if old_field.nullable != new_field.nullable {
                        let null_action = if new_field.nullable {
                            "DROP NOT NULL"
                        } else {
                            "SET NOT NULL"
                        };
                        stmts.push(format!(
                            "ALTER TABLE \"{}\" ALTER COLUMN \"{}\" {}",
                            table, new_field.name, null_action
                        ));
                    }

                    if old_field.default != new_field.default {
                        if let Some(default) = &new_field.default {
                            stmts.push(format!(
                                "ALTER TABLE \"{}\" ALTER COLUMN \"{}\" SET DEFAULT {}",
                                table, new_field.name, default
                            ));
                        } else {
                            stmts.push(format!(
                                "ALTER TABLE \"{}\" ALTER COLUMN \"{}\" DROP DEFAULT",
                                table, new_field.name
                            ));
                        }
                    }

                    if old_field.unique != new_field.unique {
                        if new_field.unique {
                            stmts.push(format!(
                                "ALTER TABLE \"{}\" ADD CONSTRAINT \"{}_{}_key\" UNIQUE (\"{}\")",
                                table, table, new_field.name, new_field.name
                            ));
                        } else {
                            stmts.push(format!(
                                "ALTER TABLE \"{}\" DROP CONSTRAINT \"{}_{}_key\"",
                                table, table, new_field.name
                            ));
                        }
                    }

                    Ok(stmts)
                }
            },

            MigrationOp::CreateIndex { table, index } => {
                Ok(vec![build_create_index(table, index, dialect)])
            }

            MigrationOp::DropIndex {
                table,
                name,
                index_def: _,
            } => {
                let mut stmt = SeaIndex::drop();
                stmt.name(name).table(Alias::new(table));
                Ok(vec![build_sql!(stmt, dialect)])
            }

            MigrationOp::AddForeignKey { table, fk } => {
                Ok(vec![build_sql!(build_fk_stmt(table, fk), dialect)])
            }

            MigrationOp::DropForeignKey {
                table,
                name,
                fk_def: _,
            } => {
                let mut stmt = SeaForeignKey::drop();
                stmt.name(name).table(Alias::new(table));
                Ok(vec![build_sql!(stmt, dialect)])
            }

            MigrationOp::AddCheck { table, check } => Ok(vec![format!(
                "ALTER TABLE {} ADD CONSTRAINT {} CHECK ({})",
                table, check.name, check.expression
            )]),

            MigrationOp::DropCheck {
                table,
                name,
                check_def: _,
            } => Ok(match dialect {
                Dialect::Postgres => {
                    vec![format!("ALTER TABLE {} DROP CONSTRAINT {}", table, name)]
                }
            }),
        }
    }
}
