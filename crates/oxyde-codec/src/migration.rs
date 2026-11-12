//! Migration contract types: schema definitions, snapshots, and operations.
//!
//! These structs cross the Python boundary as JSON (snapshots, migration
//! operations) — they live in codec next to the query IR for the same
//! reason: single home for every serialized contract. SQL rendering for
//! `MigrationOp` lives in oxyde-sql; diff computation in oxyde-migrate.

use crate::{ColumnTypeSpec, ScalarExpr};
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
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

/// Field definition in schema
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
    pub nullable: bool,
    pub primary_key: bool,
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

fn spec_has_enum(spec: &ColumnTypeSpec) -> bool {
    match spec {
        ColumnTypeSpec::Enum { .. } => true,
        ColumnTypeSpec::Array { item } => spec_has_enum(item),
        _ => false,
    }
}

impl FieldDef {
    /// Whether `CREATE TABLE` gives this column a sequence of its own
    /// (`<table>_<column>_seq`): its PostgreSQL type is a serial type.
    ///
    /// An integer primary key without an explicit `db_type` renders as
    /// `BIGSERIAL`; an explicit `db_type` of `SERIAL`, `BIGSERIAL` and the like
    /// does too. `auto_increment` alone changes nothing in PostgreSQL DDL.
    ///
    /// Mirrors `oxyde_sql::resolve_spec_type`, which owns the type mapping; a
    /// test in oxyde-sql keeps the two in step.
    #[must_use]
    pub fn creates_sequence(&self) -> bool {
        // An enum (or array of enums) renders as its declared type whatever
        // `db_type` says.
        if spec_has_enum(&self.column_type) {
            return false;
        }
        match self.db_type.as_deref().map(str::trim) {
            Some(db_type) => [
                "serial",
                "serial2",
                "serial4",
                "serial8",
                "smallserial",
                "bigserial",
            ]
            .iter()
            .any(|serial| db_type.eq_ignore_ascii_case(serial)),
            None => self.primary_key && matches!(self.column_type, ColumnTypeSpec::BigInteger),
        }
    }
}

/// One key of an index: a plain column or an expression, with an optional
/// operator class.
///
/// Wire form (see `IndexKeyRepr`): a plain column without an operator class is
/// a bare string, exactly as before expression indexes existed, so existing
/// snapshots and migration files round-trip byte-identically. Everything else
/// is an object: `{"column": "name", "opclass": "text_pattern_ops"}` or
/// `{"expr": {...ScalarExpr...}, "opclass": "gin_trgm_ops"}` (`opclass`
/// optional). A column spelled as an object, or as an `expr` that is just a
/// column reference, normalizes to the `Column` variant, so equal keys compare
/// equal however they were written.
///
/// Expression keys are canonicalized when read (see `canonicalize_expr_key`):
/// enum value lists are dropped and octet-list binary literals become bytes,
/// so a key compares and serializes the same whichever side produced it.
/// `column_types` is ordered so its serialized form is reproducible.
#[derive(Debug, Clone, PartialEq)]
pub enum IndexKey {
    Column {
        name: String,
        opclass: Option<String>,
    },
    Expr {
        expr: ScalarExpr,
        opclass: Option<String>,
        column_types: BTreeMap<String, ColumnTypeSpec>,
    },
}

impl From<&str> for IndexKey {
    fn from(name: &str) -> Self {
        IndexKey::Column {
            name: name.to_string(),
            opclass: None,
        }
    }
}

impl From<String> for IndexKey {
    fn from(name: String) -> Self {
        IndexKey::Column {
            name,
            opclass: None,
        }
    }
}

impl IndexKey {
    #[must_use]
    pub fn opclass(&self) -> Option<&str> {
        match self {
            IndexKey::Column { opclass, .. } | IndexKey::Expr { opclass, .. } => opclass.as_deref(),
        }
    }

    /// The column name of a column key, including explicit operator classes.
    fn plain_column(&self) -> Option<&str> {
        match self {
            IndexKey::Column { name, .. } => Some(name),
            IndexKey::Expr { .. } => None,
        }
    }

    /// Whether the key uses `column`, directly or inside an expression.
    /// Allocation-free, for the hot dependency-ordering loops.
    fn references_column(&self, column: &str) -> bool {
        match self {
            IndexKey::Column { name, .. } => name == column,
            IndexKey::Expr { expr, .. } => {
                let mut found = false;
                walk_scalar(expr, &mut |node| {
                    if matches!(node, ScalarExpr::Column { name } if name == column) {
                        found = true;
                    }
                });
                found
            }
        }
    }

    fn rename_column(&mut self, old: &str, new: &str) {
        match self {
            IndexKey::Column { name, .. } => {
                if name == old {
                    new.clone_into(name);
                }
            }
            IndexKey::Expr {
                expr, column_types, ..
            } => {
                walk_scalar_mut(expr, &mut |node| {
                    if let ScalarExpr::Column { name } = node {
                        if name == old {
                            new.clone_into(name);
                        }
                    }
                });
                if let Some(spec) = column_types.remove(old) {
                    column_types.insert(new.to_string(), spec);
                }
            }
        }
    }

    fn collect_columns<'a>(&'a self, out: &mut Vec<&'a str>) {
        let mut push = |name: &'a str| {
            if !out.contains(&name) {
                out.push(name);
            }
        };
        match self {
            IndexKey::Column { name, .. } => push(name),
            IndexKey::Expr { expr, .. } => walk_scalar(expr, &mut |node| {
                if let ScalarExpr::Column { name } = node {
                    push(name);
                }
            }),
        }
    }
}

/// Pre-order walk over a scalar expression tree.
fn walk_scalar<'a>(expr: &'a ScalarExpr, visit: &mut impl FnMut(&'a ScalarExpr)) {
    visit(expr);
    match expr {
        ScalarExpr::Op { lhs, rhs, .. } => {
            walk_scalar(lhs, visit);
            walk_scalar(rhs, visit);
        }
        ScalarExpr::Neg { expr } => walk_scalar(expr, visit),
        ScalarExpr::Func { args, .. } => args.iter().for_each(|arg| walk_scalar(arg, visit)),
        ScalarExpr::Column { .. } | ScalarExpr::Value { .. } | ScalarExpr::Raw { .. } => {}
    }
}

/// Pre-order walk with mutable access to every node.
fn walk_scalar_mut(expr: &mut ScalarExpr, visit: &mut impl FnMut(&mut ScalarExpr)) {
    visit(expr);
    match expr {
        ScalarExpr::Op { lhs, rhs, .. } => {
            walk_scalar_mut(lhs, visit);
            walk_scalar_mut(rhs, visit);
        }
        ScalarExpr::Neg { expr } => walk_scalar_mut(expr, visit),
        ScalarExpr::Func { args, .. } => {
            for arg in args {
                walk_scalar_mut(arg, visit);
            }
        }
        ScalarExpr::Column { .. } | ScalarExpr::Value { .. } | ScalarExpr::Raw { .. } => {}
    }
}

/// Serialized form of [`IndexKey`], borrowing from the key. A bare string
/// stays a bare string.
///
/// Serialize-only: reading is hand-written (`impl Deserialize for IndexKey`)
/// because an untagged enum would replace every inner error with serde's
/// generic "data did not match any variant".
#[derive(Serialize)]
#[serde(untagged)]
enum IndexKeyRepr<'a> {
    Column(&'a str),
    Spec(IndexKeySpecRef<'a>),
}

#[derive(Serialize)]
struct IndexKeySpecRef<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    column: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expr: Option<&'a ScalarExpr>,
    #[serde(skip_serializing_if = "Option::is_none")]
    opclass: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    column_types: Option<&'a BTreeMap<String, ColumnTypeSpec>>,
}

impl Serialize for IndexKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let repr = match self {
            IndexKey::Column {
                name,
                opclass: None,
            }
            | IndexKey::Expr {
                expr: ScalarExpr::Column { name },
                opclass: None,
                ..
            } => IndexKeyRepr::Column(name.as_str()),
            IndexKey::Column { name, opclass }
            | IndexKey::Expr {
                expr: ScalarExpr::Column { name },
                opclass,
                ..
            } => IndexKeyRepr::Spec(IndexKeySpecRef {
                column: Some(name.as_str()),
                expr: None,
                opclass: opclass.as_deref(),
                column_types: None,
            }),
            IndexKey::Expr {
                expr,
                opclass,
                column_types,
            } => IndexKeyRepr::Spec(IndexKeySpecRef {
                column: None,
                expr: Some(expr),
                opclass: opclass.as_deref(),
                column_types: (!column_types.is_empty()).then_some(column_types),
            }),
        };
        repr.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for IndexKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct KeyVisitor;

        impl<'de> de::Visitor<'de> for KeyVisitor {
            type Value = IndexKey;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(
                    "an index key: a column name string, or an object with `column` or `expr` \
                     and an optional `opclass`",
                )
            }

            fn visit_str<E: de::Error>(self, value: &str) -> std::result::Result<IndexKey, E> {
                Ok(IndexKey::from(value))
            }

            fn visit_string<E: de::Error>(self, value: String) -> std::result::Result<IndexKey, E> {
                Ok(IndexKey::from(value))
            }

            fn visit_map<A: de::MapAccess<'de>>(
                self,
                map: A,
            ) -> std::result::Result<IndexKey, A::Error> {
                // Deserializing the spec directly keeps `deny_unknown_fields`
                // and `ScalarExpr` errors (field name, bad variant) intact.
                let spec = IndexKeySpec::deserialize(de::value::MapAccessDeserializer::new(map))?;
                IndexKey::from_spec(spec).map_err(de::Error::custom)
            }
        }

        deserializer.deserialize_any(KeyVisitor)
    }
}

/// Object form of an index key. Unknown fields are rejected so a key written
/// for a newer core fails loudly instead of silently losing a modifier.
/// Deserialize-only; writing goes through [`IndexKeySpecRef`].
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IndexKeySpec {
    #[serde(default)]
    column: Option<String>,
    #[serde(default)]
    expr: Option<ScalarExpr>,
    #[serde(default)]
    opclass: Option<String>,
    #[serde(default)]
    column_types: BTreeMap<String, ColumnTypeSpec>,
}

/// Empty an enum's value list, keeping its name (the only part DDL reads).
/// Looks through arrays.
fn clear_enum_values(spec: &mut ColumnTypeSpec) {
    match spec {
        ColumnTypeSpec::Enum { values, .. } => values.clear(),
        ColumnTypeSpec::Array { item } => clear_enum_values(item),
        _ => {}
    }
}

/// A JSON list of octets, the snapshot encoding of binary data.
fn octets(value: &rmpv::Value) -> Option<Vec<u8>> {
    let rmpv::Value::Array(items) = value else {
        return None;
    };
    items
        .iter()
        .map(|item| item.as_u64().and_then(|n| u8::try_from(n).ok()))
        .collect()
}

/// Turn the octet lists a literal of type `spec` holds into bytes: the literal
/// itself for `blob`, each element (recursively) for an array of `blob`.
fn octets_to_bytes(value: &mut rmpv::Value, spec: &ColumnTypeSpec) {
    match spec {
        ColumnTypeSpec::Blob => {
            if let Some(bytes) = octets(value) {
                *value = rmpv::Value::Binary(bytes);
            }
        }
        ColumnTypeSpec::Array { item } => {
            if let rmpv::Value::Array(elements) = value {
                for element in elements {
                    octets_to_bytes(element, item);
                }
            }
        }
        _ => {}
    }
}

/// Bring an expression key to one canonical form, whichever side wrote it:
///
/// - Enum value lists are dropped from `column_types` and from literal types.
///   They would otherwise make the key differ (and the index be recreated)
///   whenever the enum gains a member. A literal whose type is a bare enum
///   loses its type entirely: PostgreSQL resolves the string against the
///   enum column it is compared or coalesced with.
/// - A literal typed `blob`, or `array` of `blob`, that arrives as octet lists
///   (how a JSON snapshot spells bytes) becomes bytes. Serialization writes
///   the same octet lists back, so the wire form does not change, and binding
///   sees bytes without any special case on the query path. The literal must
///   say its type: octets are indistinguishable from a list of small integers
///   otherwise.
fn canonicalize_expr_key(
    expr: &mut ScalarExpr,
    column_types: &mut BTreeMap<String, ColumnTypeSpec>,
) {
    column_types.values_mut().for_each(clear_enum_values);
    walk_scalar_mut(expr, &mut |node| {
        let ScalarExpr::Value { value, value_type } = node else {
            return;
        };
        if matches!(value_type, Some(ColumnTypeSpec::Enum { .. })) {
            *value_type = None;
        } else if let Some(spec) = value_type {
            clear_enum_values(spec);
            octets_to_bytes(value, spec);
        }
    });
}

impl IndexKey {
    /// Validate and normalize the object form of a key.
    fn from_spec(spec: IndexKeySpec) -> std::result::Result<Self, String> {
        let IndexKeySpec {
            column,
            expr,
            opclass,
            mut column_types,
        } = spec;
        if let Some(opclass) = &opclass {
            validate_opclass(opclass)?;
        }
        match (column, expr) {
            (Some(name), None) => Ok(IndexKey::Column { name, opclass }),
            (None, Some(ScalarExpr::Column { name })) => Ok(IndexKey::Column { name, opclass }),
            (None, Some(mut expr)) => {
                reject_unindexable_functions(&expr)?;
                canonicalize_expr_key(&mut expr, &mut column_types);
                Ok(IndexKey::Expr {
                    expr,
                    opclass,
                    column_types,
                })
            }
            (Some(_), Some(_)) => {
                Err("index key must set exactly one of `column` and `expr`, got both".to_string())
            }
            (None, None) => Err(
                "index key must set exactly one of `column` and `expr`, got neither".to_string(),
            ),
        }
    }
}

/// Operator classes are written unquoted into DDL, so only a plain identifier
/// (optionally schema-qualified: `public.gin_trgm_ops`) is accepted.
fn validate_opclass(opclass: &str) -> std::result::Result<(), String> {
    let is_identifier = |segment: &str| {
        let mut chars = segment.chars();
        chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    let mut segments = opclass.split('.');
    let valid = match (segments.next(), segments.next(), segments.next()) {
        (Some(only), None, None) => is_identifier(only),
        (Some(schema), Some(name), None) => is_identifier(schema) && is_identifier(name),
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(format!(
            "invalid operator class {opclass:?}: expected an identifier such as \
             `text_pattern_ops` or `public.gin_trgm_ops`"
        ))
    }
}

/// Scalar functions an index expression may call. PostgreSQL requires index
/// expressions to be IMMUTABLE, and these are (`concat_ws`, also supported in
/// queries, is STABLE). An allowlist, so a function added to expression
/// compilation later is not accepted here until someone has checked its
/// volatility. Names match exactly, as compilation does.
const INDEXABLE_FUNCTIONS: &[&str] = &["coalesce", "greatest", "lower", "upper"];

/// Reject other functions when the key is read, instead of letting
/// `CREATE INDEX` fail at migrate time.
fn reject_unindexable_functions(expr: &ScalarExpr) -> std::result::Result<(), String> {
    let mut offender = None;
    walk_scalar(expr, &mut |node| {
        if let ScalarExpr::Func { name, .. } = node {
            if !INDEXABLE_FUNCTIONS.contains(&name.as_str()) {
                offender.get_or_insert(name.as_str());
            }
        }
    });
    match offender {
        Some(name) => Err(format!(
            "{name}() cannot be used in an index expression: index expressions must be \
             IMMUTABLE, so only coalesce, greatest, lower and upper are allowed"
        )),
        None => Ok(()),
    }
}

/// Index definition
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IndexDef {
    pub name: String,
    pub fields: Vec<IndexKey>,
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

impl IndexDef {
    /// The leading run of column keys, including explicit operator classes.
    /// An expression ends the prefix; it cannot back an FK on its input column.
    pub fn leading_columns(&self) -> impl Iterator<Item = &str> {
        self.fields.iter().map_while(IndexKey::plain_column)
    }

    /// Every column this index depends on: column keys plus each column
    /// referenced inside an expression key, deduplicated, in first-seen order.
    ///
    /// `ScalarExpr::Raw` is opaque. Columns used only inside a raw fragment
    /// are not reported, so dependency ordering cannot see them.
    #[must_use]
    pub fn referenced_columns(&self) -> Vec<&str> {
        let mut columns = Vec::new();
        for key in &self.fields {
            key.collect_columns(&mut columns);
        }
        columns
    }

    /// Whether any key uses `column`, directly or inside an expression.
    /// Same answer as `referenced_columns().contains(&column)` without the
    /// allocation, for the dependency-ordering loops.
    #[must_use]
    pub fn references_column(&self, column: &str) -> bool {
        self.fields.iter().any(|key| key.references_column(column))
    }

    /// Rewrite every use of column `old` (plain keys and column references
    /// inside expression keys) to `new`, as a column rename does in PostgreSQL.
    /// The `where` clause and `ScalarExpr::Raw` fragments are opaque SQL and
    /// stay as written.
    pub fn rename_column(&mut self, old: &str, new: &str) {
        for key in &mut self.fields {
            key.rename_column(old, new);
        }
    }

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

/// Foreign key definition
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ForeignKeyDef {
    pub name: String,
    pub columns: Vec<String>,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
    pub on_delete: Option<String>, // CASCADE, SET NULL, RESTRICT, NO ACTION
    pub on_update: Option<String>,
}

/// Check constraint definition
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CheckDef {
    pub name: String,
    pub expression: String,
}

/// Table definition in schema
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TableDef {
    pub name: String,
    pub fields: Vec<FieldDef>,
    pub indexes: Vec<IndexDef>,
    #[serde(default)]
    pub foreign_keys: Vec<ForeignKeyDef>,
    #[serde(default)]
    pub checks: Vec<CheckDef>,
    pub comment: Option<String>,
}

/// PostgreSQL truncates identifiers longer than this many bytes (NAMEDATALEN - 1).
const PG_MAX_IDENTIFIER_BYTES: usize = 63;

/// The longest prefix of `name` that is at most `max` bytes and ends on a
/// character boundary (PostgreSQL's `pg_mbcliplen`).
fn clip_bytes(name: &str, max: usize) -> &str {
    if name.len() <= max {
        return name;
    }
    let mut end = max;
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    &name[..end]
}

/// `name` as PostgreSQL stores it: cut to 63 bytes on a character boundary.
fn truncate_identifier(name: &str) -> &str {
    clip_bytes(name, PG_MAX_IDENTIFIER_BYTES)
}

/// PostgreSQL's `makeObjectName`: `name1[_name2]_label`, which is how it
/// names the relations `CREATE TABLE` creates itself (`<table>_pkey`,
/// `<table>_<column>_key`, `<table>_<column>_seq`).
///
/// It does not cut the result at 63 bytes. It shortens the longer of the two
/// names, one byte at a time, until everything fits, so the label survives.
/// Ties shorten `name2`.
fn make_object_name(name1: &str, name2: Option<&str>, label: &str) -> String {
    // Underscore before `name2` (if any), and `_label`.
    let overhead = usize::from(name2.is_some()) + label.len() + 1;
    let available = PG_MAX_IDENTIFIER_BYTES.saturating_sub(overhead);
    let mut name1_bytes = name1.len();
    let mut name2_bytes = name2.map_or(0, str::len);
    while name1_bytes + name2_bytes > available {
        if name1_bytes > name2_bytes {
            name1_bytes -= 1;
        } else {
            name2_bytes -= 1;
        }
    }

    let mut out = clip_bytes(name1, name1_bytes).to_string();
    if let Some(name2) = name2 {
        out.push('_');
        out.push_str(clip_bytes(name2, name2_bytes));
    }
    out.push('_');
    out.push_str(label);
    out
}

/// The name PostgreSQL gives the unique constraint (and its index) of an
/// inline `UNIQUE` column: `<table>_<column>_key`, shortened by
/// `makeObjectName` when it does not fit in 63 bytes.
///
/// One source for everything that has to agree with it: `ALTER TABLE ... ADD
/// CONSTRAINT` and `DROP CONSTRAINT` (a `CREATE TABLE` made the constraint, so
/// the drop must name it the way PostgreSQL did) and the check that no index
/// is given the same name.
#[must_use]
pub fn unique_constraint_name(table: &str, column: &str) -> String {
    make_object_name(table, Some(column), "key")
}

/// Whatever already holds a relation name when an index is created.
/// PostgreSQL keeps tables, indexes and sequences in one namespace.
enum RelationOwner<'a> {
    Table(&'a str),
    /// The index behind a table's primary key.
    PrimaryKey(&'a str),
    /// The index behind an inline `UNIQUE` column.
    UniqueConstraint {
        table: &'a str,
        column: &'a str,
    },
    /// The sequence behind a serial column.
    Sequence {
        table: &'a str,
        column: &'a str,
    },
    Index {
        table: &'a str,
        name: &'a str,
    },
}

impl RelationOwner<'_> {
    fn describe(&self) -> String {
        match self {
            RelationOwner::Table(table) => format!("the table '{table}'"),
            RelationOwner::PrimaryKey(table) => {
                format!("the primary key index PostgreSQL creates for table '{table}'")
            }
            RelationOwner::UniqueConstraint { table, column } => format!(
                "the unique constraint index PostgreSQL creates for '{table}.{column}'"
            ),
            RelationOwner::Sequence { table, column } => {
                format!("the sequence PostgreSQL creates for '{table}.{column}'")
            }
            RelationOwner::Index { table, name } => {
                format!("the index '{name}' on table '{table}'")
            }
        }
    }
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

    /// Validate a snapshot freshly extracted from models.
    ///
    /// Not part of deserialization or diffing: historical snapshots and
    /// migration files must keep loading. Checks, per table in name order:
    /// - every column an index uses (plain keys and references inside
    ///   expression keys) is a column of the table;
    /// - index names are unique across the whole schema (PostgreSQL index
    ///   names share one namespace) and stay unique once PostgreSQL truncates
    ///   them to 63 bytes;
    /// - an index name is not already taken by a table or by a relation
    ///   `CREATE TABLE` creates on its own: `<table>_pkey` for a primary key,
    ///   `<table>_<column>_key` for an inline `UNIQUE` column and
    ///   `<table>_<column>_seq` for a serial column
    ///   ([`FieldDef::creates_sequence`]). Those exist
    ///   before any index is created, so the clash would only surface as
    ///   "relation already exists" at migrate time.
    ///
    /// PostgreSQL gives its own relations a numeric suffix when a name is
    /// taken, so clashes between those, or with a table, are not errors.
    pub fn validate_model_schema(&self) -> Result<()> {
        let mut tables: Vec<&TableDef> = self.tables.values().collect();
        tables.sort_by(|a, b| a.name.cmp(&b.name));

        // truncated relation name -> its first owner
        let mut seen: HashMap<String, RelationOwner<'_>> = HashMap::new();
        for table in &tables {
            seen.entry(truncate_identifier(&table.name).to_string())
                .or_insert(RelationOwner::Table(&table.name));
        }
        for table in &tables {
            let table_name = table.name.as_str();
            if table.fields.iter().any(|field| field.primary_key) {
                seen.entry(make_object_name(table_name, None, "pkey"))
                    .or_insert(RelationOwner::PrimaryKey(table_name));
            }
            for field in &table.fields {
                let column = field.name.as_str();
                if field.unique && !field.primary_key {
                    seen.entry(unique_constraint_name(table_name, column))
                        .or_insert(RelationOwner::UniqueConstraint {
                            table: table_name,
                            column,
                        });
                }
                if field.creates_sequence() {
                    seen.entry(make_object_name(table_name, Some(column), "seq"))
                        .or_insert(RelationOwner::Sequence {
                            table: table_name,
                            column,
                        });
                }
            }
        }

        for table in tables {
            for index in &table.indexes {
                for column in index.referenced_columns() {
                    if !table.fields.iter().any(|field| field.name == column) {
                        return Err(MigrateError::SnapshotError(format!(
                            "index '{}' on table '{}' uses column '{column}', \
                             which is not a column of the table",
                            index.name, table.name
                        )));
                    }
                }

                let truncated = truncate_identifier(&index.name);
                let owner = RelationOwner::Index {
                    table: &table.name,
                    name: &index.name,
                };
                if let Some(existing) = seen.insert(truncated.to_string(), owner) {
                    let same_name = match &existing {
                        RelationOwner::Index { name, .. } => *name == index.name,
                        _ => truncated == index.name,
                    };
                    let reason = if same_name {
                        "the same name".to_string()
                    } else {
                        format!(
                            "PostgreSQL truncates identifiers to \
                             {PG_MAX_IDENTIFIER_BYTES} bytes, so they are the same name"
                        )
                    };
                    return Err(MigrateError::SnapshotError(format!(
                        "index '{}' on table '{}' clashes with {}: {reason}; relation \
                         names are shared across the whole schema, give the index an \
                         explicit distinct name=",
                        index.name,
                        table.name,
                        existing.describe()
                    )));
                }
            }
        }
        Ok(())
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
