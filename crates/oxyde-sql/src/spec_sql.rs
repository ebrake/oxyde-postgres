//! PostgreSQL SQL type resolution from semantic ColumnTypeSpec metadata.
//! Explicit db_type overrides render verbatim; enums use their declared type.
//! Golden DDL snapshots preserve the generated SQL contract.

use crate::utils::bind::quote_pg_type_path;
use crate::Dialect;
use oxyde_codec::ColumnTypeSpec;

#[must_use]
pub fn resolve_spec_type(
    spec: &ColumnTypeSpec,
    db_type: Option<&str>,
    dialect: Dialect,
    is_pk: bool,
) -> String {
    if contains_enum_spec(spec) {
        return canonical_type(spec, dialect, is_pk);
    }
    db_type.map_or_else(|| canonical_type(spec, dialect, is_pk), str::to_string)
}

fn contains_enum_spec(spec: &ColumnTypeSpec) -> bool {
    match spec {
        ColumnTypeSpec::Enum { .. } => true,
        ColumnTypeSpec::Array { item } => contains_enum_spec(item),
        _ => false,
    }
}

/// Canonical PostgreSQL type. Integer primary keys use BIGSERIAL.
pub(crate) fn canonical_type(spec: &ColumnTypeSpec, _dialect: Dialect, is_pk: bool) -> String {
    use ColumnTypeSpec as S;
    match spec {
        S::String { length } => format!("VARCHAR({})", length.unwrap_or(255)),
        S::Text | S::Unknown => "TEXT".to_string(),
        S::BigInteger => if is_pk { "BIGSERIAL" } else { "BIGINT" }.to_string(),
        S::Double => "DOUBLE PRECISION".to_string(),
        S::Boolean => "BOOLEAN".to_string(),
        S::Blob => "BYTEA".to_string(),
        S::DateTime => "TIMESTAMP".to_string(),
        S::DateTimeUtc => "TIMESTAMPTZ".to_string(),
        S::Date => "DATE".to_string(),
        S::Time => "TIME".to_string(),
        S::Timedelta => "BIGINT".to_string(),
        S::Uuid => "UUID".to_string(),
        S::Decimal { precision, scale } => match precision {
            Some(p) => format!("NUMERIC({},{})", p, scale.unwrap_or(0)),
            None => "NUMERIC".to_string(),
        },
        S::Json | S::JsonBinary => "JSONB".to_string(),
        S::Enum { name, .. } => quote_pg_type_path(name),
        S::Array { item } => format!("{}[]", canonical_type(item, Dialect::Postgres, false)),
    }
}

pub(crate) fn quote_sql_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec_string(length: Option<u32>) -> ColumnTypeSpec {
        ColumnTypeSpec::String { length }
    }

    /// Canonical renderings, pinned directly (the golden DDL suite pins the
    /// full statements; this covers the resolver in isolation).
    #[test]
    fn canonical_scalar_types() {
        use ColumnTypeSpec as S;
        let cases: Vec<(ColumnTypeSpec, [&str; 1])> = vec![
            (S::BigInteger, ["BIGINT"]),
            (S::Double, ["DOUBLE PRECISION"]),
            (S::Boolean, ["BOOLEAN"]),
            (spec_string(None), ["VARCHAR(255)"]),
            (spec_string(Some(100)), ["VARCHAR(100)"]),
            (S::Blob, ["BYTEA"]),
            (S::DateTime, ["TIMESTAMP"]),
            (S::DateTimeUtc, ["TIMESTAMPTZ"]),
            (S::Date, ["DATE"]),
            (S::Time, ["TIME"]),
            (S::Timedelta, ["BIGINT"]),
            (S::Uuid, ["UUID"]),
            (
                S::Decimal {
                    precision: None,
                    scale: None,
                },
                ["NUMERIC"],
            ),
            (
                S::Decimal {
                    precision: Some(10),
                    scale: Some(2),
                },
                ["NUMERIC(10,2)"],
            ),
            (S::Json, ["JSONB"]),
            (S::JsonBinary, ["JSONB"]),
            (S::Unknown, ["TEXT"]),
        ];
        let dialects = [Dialect::Postgres];
        for (spec, expected) in &cases {
            for (dialect, want) in dialects.iter().zip(expected) {
                assert_eq!(
                    &resolve_spec_type(spec, None, *dialect, false),
                    want,
                    "spec={spec:?} dialect={dialect:?}"
                );
            }
        }
    }

    /// `FieldDef::creates_sequence` (used to predict the relation names
    /// `CREATE TABLE` takes) must agree with what this resolver renders.
    #[test]
    fn creates_sequence_matches_the_rendered_type() {
        use oxyde_codec::FieldDef;
        use ColumnTypeSpec as S;

        let specs = [
            S::BigInteger,
            spec_string(None),
            S::Uuid,
            S::Text,
            S::Enum {
                name: "status".into(),
                values: vec![],
            },
        ];
        let db_types = [
            None,
            Some("INTEGER"),
            Some("serial"),
            Some("SERIAL"),
            Some("BigSerial"),
            Some("SMALLSERIAL"),
            Some("serial8"),
            Some("  bigserial "),
            Some("BIGINT"),
            Some("TEXT"),
        ];
        for spec in &specs {
            for db_type in db_types {
                for primary_key in [false, true] {
                    let field = FieldDef {
                        name: "id".into(),
                        column_type: spec.clone(),
                        db_type: db_type.map(str::to_string),
                        nullable: false,
                        primary_key,
                        unique: false,
                        nulls_not_distinct: false,
                        default: None,
                        auto_increment: false,
                        max_length: None,
                        max_digits: None,
                        decimal_places: None,
                    };
                    let rendered = resolve_spec_type(
                        &field.column_type,
                        field.db_type.as_deref(),
                        Dialect::Postgres,
                        field.primary_key,
                    );
                    assert_eq!(
                        field.creates_sequence(),
                        rendered.to_ascii_uppercase().contains("SERIAL"),
                        "spec={spec:?} db_type={db_type:?} pk={primary_key} rendered={rendered:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn pk_str_and_uuid_do_not_become_serial() {
        assert_eq!(
            resolve_spec_type(&spec_string(Some(32)), None, Dialect::Postgres, true),
            "VARCHAR(32)"
        );
        assert_eq!(
            resolve_spec_type(&ColumnTypeSpec::Uuid, None, Dialect::Postgres, true),
            "UUID"
        );
    }
}
