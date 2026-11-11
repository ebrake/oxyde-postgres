//! Dynamic SQL identifiers for sea_query

use sea_query::Iden;

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
