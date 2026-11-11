//! Migration contract types: schema definitions, snapshots, and operations.
//!
//! These structs cross the Python boundary as JSON (snapshots, migration
//! operations) — they live in codec next to the query IR for the same
//! reason: single home for every serialized contract. SQL rendering for
//! `MigrationOp` lives in oxyde-sql; diff computation in oxyde-migrate.

use crate::ColumnTypeSpec;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashMap;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MigrateError {
    #[error("Migration error: {0}")]
    MigrationError(String),

    #[error("Snapshot error: {0}")]
    SnapshotError(String),

    #[error("Diff error: {0}")]
    DiffError(String),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    SerializationError(String),
}

type Result<T> = std::result::Result<T, MigrateError>;

fn normalize_optional_sql_fragment(value: Option<String>) -> Option<String> {
    value
        .map(|fragment| fragment.trim().to_string())
        .filter(|fragment| !fragment.is_empty())
}

// serde's skip_serializing_if / serialize_with dictate the &Option<String>
// signatures below.
#[allow(clippy::ref_option)]
fn is_none_or_blank(value: &Option<String>) -> bool {
    value.as_deref().map(str::trim).is_none_or(str::is_empty)
}

// serde's skip_serializing_if requires a borrowed value.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(value: &bool) -> bool {
    !value
}

#[allow(clippy::ref_option)]
fn serialize_normalized_optional_sql_fragment<S>(
    value: &Option<String>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match value
        .as_deref()
        .map(str::trim)
        .filter(|fragment| !fragment.is_empty())
    {
        Some(fragment) => serializer.serialize_some(fragment),
        None => serializer.serialize_none(),
    }
}

fn deserialize_normalized_optional_sql_fragment<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    Ok(normalize_optional_sql_fragment(value))
}

/// Original JSON shape of a schema member. Replay must preserve omitted keys
/// and legacy metadata for Python callers inspecting intermediate state.
#[derive(Debug, Clone, Default)]
pub struct ReplayShape {
    pub preserve: bool,
    original: Option<serde_json::Map<String, serde_json::Value>>,
    canonical: serde_json::Map<String, serde_json::Value>,
}

// Serialization provenance does not participate in semantic schema equality.
impl PartialEq for ReplayShape {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

macro_rules! schema_member {
    ($(#[$meta:meta])* pub struct $name:ident { $($(#[$attr:meta])* pub $field:ident: $ty:ty,)* }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq)]
        pub struct $name {
            $(pub $field: $ty,)*
            #[doc(hidden)]
            pub replay_shape: ReplayShape,
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
                #[derive(Serialize)]
                struct Wire { $($(#[$attr])* $field: $ty,)* }
                let canonical = serde_json::to_value(Wire { $($field: self.$field.clone(),)* })
                    .map_err(serde::ser::Error::custom)?;
                let Some(original) = self.replay_shape.original.as_ref().filter(|_| self.replay_shape.preserve) else {
                    return canonical.serialize(serializer);
                };
                let current = canonical.as_object().expect("schema member is an object");
                let mut output = original.clone();
                for key in self.replay_shape.canonical.keys().chain(current.keys()) {
                    if self.replay_shape.canonical.get(key) != current.get(key) {
                        if let Some(value) = current.get(key) {
                            output.insert(key.clone(), value.clone());
                        } else {
                            output.remove(key);
                        }
                    }
                }
                output.serialize(serializer)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
                #[derive(Serialize, Deserialize)]
                struct Wire { $($(#[$attr])* $field: $ty,)* }
                let raw = serde_json::Value::deserialize(deserializer)?;
                let wire: Wire = serde_json::from_value(raw.clone()).map_err(serde::de::Error::custom)?;
                let canonical = serde_json::to_value(&wire).map_err(serde::de::Error::custom)?;
                Ok(Self {
                    $($field: wire.$field,)*
                    replay_shape: ReplayShape {
                        preserve: false,
                        original: raw.as_object().cloned(),
                        canonical: canonical.as_object().cloned().unwrap_or_default(),
                    },
                })
            }
        }
    };
}

schema_member! {
/// Field definition in schema
pub struct FieldDef {
    pub name: String,
    /// Semantic column type for cross-dialect DDL generation and binding.
    /// Python computes it once (`compute_column_type`); legacy migration
    /// files are normalized to this form on the Python side before reaching
    /// Rust — there is no string-typed fallback here.
    pub column_type: ColumnTypeSpec,
    /// Explicit db_type from user (e.g., "JSONB", "VARCHAR(255)")
    #[serde(default)]
    pub db_type: Option<String>,
    #[serde(default)]
    pub nullable: bool,
    #[serde(default)]
    pub primary_key: bool,
    #[serde(default)]
    pub unique: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub nulls_not_distinct: bool,
    pub default: Option<String>,
    #[serde(default)]
    pub auto_increment: bool,
    #[serde(default)]
    pub max_length: Option<u32>,
    /// Total digits for decimal fields (used for DECIMAL(M,D))
    #[serde(default)]
    pub max_digits: Option<u32>,
    /// Decimal places for decimal fields (used for DECIMAL(M,D))
    #[serde(default)]
    pub decimal_places: Option<u32>,
}
}

schema_member! {
/// Index definition
pub struct IndexDef {
    pub name: String,
    #[serde(default)]
    pub fields: Vec<String>,
    #[serde(default)]
    pub unique: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub nulls_not_distinct: bool,
    pub method: Option<String>,
    #[serde(
        default,
        rename = "where",
        skip_serializing_if = "is_none_or_blank",
        serialize_with = "serialize_normalized_optional_sql_fragment",
        deserialize_with = "deserialize_normalized_optional_sql_fragment"
    )]
    pub where_clause: Option<String>,
}
}

impl IndexDef {
    pub fn normalized_where_clause(&self) -> Option<&str> {
        self.where_clause
            .as_deref()
            .map(str::trim)
            .filter(|fragment| !fragment.is_empty())
    }

    #[must_use]
    pub fn semantically_eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.fields == other.fields
            && self.unique == other.unique
            && self.nulls_not_distinct == other.nulls_not_distinct
            && self.method == other.method
            && self.normalized_where_clause() == other.normalized_where_clause()
    }
}

schema_member! {
/// Foreign key definition
pub struct ForeignKeyDef {
    pub name: String,
    #[serde(default)]
    pub columns: Vec<String>,
    pub ref_table: String,
    #[serde(default)]
    pub ref_columns: Vec<String>,
    pub on_delete: Option<String>, // CASCADE, SET NULL, RESTRICT, NO ACTION
    pub on_update: Option<String>,
}
}

schema_member! {
/// Check constraint definition
pub struct CheckDef {
    pub name: String,
    pub expression: String,
}
}

/// Table definition in schema
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TableDef {
    pub name: String,
    pub fields: Vec<FieldDef>,
    #[serde(default)]
    pub indexes: Vec<IndexDef>,
    #[serde(default)]
    pub foreign_keys: Vec<ForeignKeyDef>,
    #[serde(default)]
    pub checks: Vec<CheckDef>,
    pub comment: Option<String>,
}

/// Schema snapshot
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Snapshot {
    pub version: u32,
    pub tables: HashMap<String, TableDef>,
}

impl Snapshot {
    /// Create a new empty snapshot
    #[must_use]
    pub fn new() -> Self {
        Self {
            version: 1,
            tables: HashMap::new(),
        }
    }

    /// Add a table to the snapshot
    pub fn add_table(&mut self, table: TableDef) {
        self.tables.insert(table.name.clone(), table);
    }

    /// Discard replay-only JSON provenance before producing canonical diff ops.
    pub fn clear_replay_shapes(&mut self) {
        for table in self.tables.values_mut() {
            for field in &mut table.fields {
                field.replay_shape = ReplayShape::default();
            }
            for index in &mut table.indexes {
                index.replay_shape = ReplayShape::default();
            }
            for fk in &mut table.foreign_keys {
                fk.replay_shape = ReplayShape::default();
            }
            for check in &mut table.checks {
                check.replay_shape = ReplayShape::default();
            }
        }
    }

    /// Serialize to JSON
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string_pretty(self)
            .map_err(|e| MigrateError::SerializationError(e.to_string()))
    }

    /// Deserialize from JSON
    pub fn from_json(json: &str) -> Result<Self> {
        serde_json::from_str(json).map_err(|e| MigrateError::SerializationError(e.to_string()))
    }
}

impl Default for Snapshot {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnumFieldRef {
    pub table: String,
    pub field: FieldDef,
}
/// Migration operation
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MigrationOp {
    CreateEnumType {
        name: String,
        values: Vec<String>,
    },
    DropEnumType {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        values: Option<Vec<String>>,
    },
    AddEnumValue {
        name: String,
        value: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        fields: Vec<EnumFieldRef>,
    },
    /// Manual migration: no SQL by design — user writes it, `ctx.require_manual` guards.
    AlterEnumType {
        name: String,
        old_values: Vec<String>,
        new_values: Vec<String>,
    },
    CreateTable {
        table: TableDef,
    },
    DropTable {
        name: String,
        /// Full table definition for reverse migration (optional for forward migration)
        #[serde(skip_serializing_if = "Option::is_none")]
        table: Option<TableDef>,
    },
    RenameTable {
        old_name: String,
        new_name: String,
    },
    AddColumn {
        table: String,
        field: FieldDef,
    },
    DropColumn {
        table: String,
        field: String,
        /// Full field definition for reverse migration (optional for forward migration)
        #[serde(skip_serializing_if = "Option::is_none")]
        field_def: Option<FieldDef>,
    },
    RenameColumn {
        table: String,
        old_name: String,
        new_name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        field_def: Option<FieldDef>,
    },
    AlterColumn {
        table: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        old_field: Option<Box<FieldDef>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        new_field: Option<Box<FieldDef>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        column: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        changes: Option<serde_json::Map<String, serde_json::Value>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        table_fields: Option<Vec<FieldDef>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        table_indexes: Option<Vec<IndexDef>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        table_foreign_keys: Option<Vec<ForeignKeyDef>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        table_checks: Option<Vec<CheckDef>>,
    },
    CreateIndex {
        table: String,
        index: IndexDef,
    },
    DropIndex {
        table: String,
        name: String,
        /// Full index definition for reverse migration (optional for forward migration)
        #[serde(default, skip_serializing_if = "Option::is_none")]
        index_def: Option<IndexDef>,
    },
    AddForeignKey {
        table: String,
        fk: ForeignKeyDef,
    },
    DropForeignKey {
        table: String,
        name: String,
        /// Full foreign key definition for reverse migration (optional for forward migration)
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fk_def: Option<ForeignKeyDef>,
    },
    AddCheck {
        table: String,
        check: CheckDef,
    },
    DropCheck {
        table: String,
        name: String,
        /// Full check definition for reverse migration (optional for forward migration)
        #[serde(default, skip_serializing_if = "Option::is_none")]
        check_def: Option<CheckDef>,
    },
}
