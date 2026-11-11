//! Schema diff computation and in-memory migration replay.
//!
//! Contract types (`Snapshot`, `TableDef`, `MigrationOp`, ...) live in
//! `oxyde-codec`. This crate compares snapshots and applies operations to
//! schema state. SQL rendering lives in `oxyde-sql`; replay has no SQL or
//! database dependencies.

mod diff;
pub mod replay;

pub use diff::compute_diff;
pub use replay::{apply_operation, resolve_operation};

// Contract types re-exported for convenience (canonical home: oxyde-codec).
pub use oxyde_codec::{
    CheckDef, EnumFieldRef, FieldDef, ForeignKeyDef, IndexDef, MigrateError, MigrationOp, Snapshot,
    TableDef,
};

pub type Result<T> = std::result::Result<T, MigrateError>;
