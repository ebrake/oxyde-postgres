//! PostgreSQL extension names and unambiguous object requirements.

use serde::{Deserialize, Serialize};
use std::fmt;

/// A lowercase PostgreSQL extension name, validated at every JSON boundary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(try_from = "String", into = "String")]
pub struct ExtensionName(String);

impl TryFrom<String> for ExtensionName {
    type Error = String;

    fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
        let mut bytes = value.bytes();
        if value.len() <= 63
            && bytes
                .next()
                .is_some_and(|b| b.is_ascii_lowercase() || b == b'_')
            && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        {
            Ok(Self(value))
        } else {
            Err(format!(
                "invalid extension name {value:?}: expected [a-z_][a-z0-9_-]*, at most 63 bytes"
            ))
        }
    }
}

impl From<ExtensionName> for String {
    fn from(value: ExtensionName) -> Self {
        value.0
    }
}

impl ExtensionName {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ExtensionName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// Each group is confirmed against the extension's upstream SQL / documentation.
// Core and ambiguous names (including btree_gin/btree_gist classes) are omitted.
// https://github.com/postgres/postgres/blob/REL_18_STABLE/contrib/pg_trgm/pg_trgm--1.3.sql
// https://github.com/postgres/postgres/blob/REL_18_STABLE/contrib/hstore/hstore--1.4.sql
// https://github.com/pgvector/pgvector#hnsw
// https://github.com/pgvector/pgvector/blob/v0.8.7/sql/vector.sql
// https://github.com/postgres/postgres/blob/REL_18_STABLE/contrib/citext/citext--1.4.sql
// https://github.com/postgres/postgres/blob/REL_18_STABLE/contrib/citext/citext--1.4--1.5.sql
// https://github.com/postgres/postgres/blob/REL_18_STABLE/contrib/ltree/ltree--1.1.sql
const OPCLASSES: &[(&str, &str)] = &[
    ("gin_trgm_ops", "pg_trgm"),
    ("gist_trgm_ops", "pg_trgm"),
    ("gin_hstore_ops", "hstore"),
    ("gist_hstore_ops", "hstore"),
    ("btree_hstore_ops", "hstore"),
    ("hash_hstore_ops", "hstore"),
    ("vector_l2_ops", "vector"),
    ("vector_ip_ops", "vector"),
    ("vector_cosine_ops", "vector"),
    ("vector_l1_ops", "vector"),
    ("halfvec_l2_ops", "vector"),
    ("halfvec_ip_ops", "vector"),
    ("halfvec_cosine_ops", "vector"),
    ("halfvec_l1_ops", "vector"),
    ("sparsevec_l2_ops", "vector"),
    ("sparsevec_ip_ops", "vector"),
    ("sparsevec_cosine_ops", "vector"),
    ("sparsevec_l1_ops", "vector"),
    ("bit_hamming_ops", "vector"),
    ("bit_jaccard_ops", "vector"),
    ("citext_ops", "citext"),
    ("citext_pattern_ops", "citext"),
    ("gist_ltree_ops", "ltree"),
    ("gist__ltree_ops", "ltree"),
];

// Type definitions: vector, hstore, citext and ltree use the SQL sources above.
// https://github.com/postgres/postgres/blob/REL_18_STABLE/contrib/cube/cube--1.2.sql
// https://github.com/postgis/postgis/blob/master/postgis/postgis.sql.in
// https://github.com/postgis/postgis/blob/master/postgis/geography.sql.in
const TYPES: &[(&str, &str)] = &[
    ("vector", "vector"),
    ("halfvec", "vector"),
    ("sparsevec", "vector"),
    ("hstore", "hstore"),
    ("citext", "citext"),
    ("cube", "cube"),
    ("ltree", "ltree"),
    ("lquery", "ltree"),
    ("ltxtquery", "ltree"),
    ("geometry", "postgis"),
    ("geography", "postgis"),
];

/// Extension providing a known operator class (optionally schema-qualified).
#[must_use]
pub fn extension_for_opclass(opclass: &str) -> Option<&'static str> {
    let name = opclass.rsplit('.').next().unwrap_or(opclass);
    OPCLASSES
        .iter()
        .find_map(|(class, extension)| name.eq_ignore_ascii_case(class).then_some(*extension))
}

/// Extension providing an explicit SQL type, including modifiers and arrays.
/// Unquoted names fold to lowercase; quoted names are case-sensitive, as in PostgreSQL.
#[must_use]
pub fn extension_for_type(db_type: &str) -> Option<&'static str> {
    let name = type_name(db_type)?;
    TYPES
        .iter()
        .find_map(|(ty, extension)| (name == *ty).then_some(*extension))
}

fn type_name(db_type: &str) -> Option<String> {
    let mut chars = db_type.trim().chars().peekable();
    loop {
        let mut name = String::new();
        if chars.next_if_eq(&'"').is_some() {
            loop {
                match chars.next()? {
                    '"' if chars.next_if_eq(&'"').is_some() => name.push('"'),
                    '"' => break,
                    ch => name.push(ch),
                }
            }
        } else {
            while let Some(ch) =
                chars.next_if(|ch| !matches!(ch, '.' | '(' | '[' | '"') && !ch.is_whitespace())
            {
                name.push(ch.to_ascii_lowercase());
            }
        }
        if name.is_empty() {
            return None;
        }
        match chars.peek() {
            Some('.') => {}
            Some(ch) if ch.is_whitespace() => {
                while chars.next_if(|ch| ch.is_whitespace()).is_some() {}
                if chars.peek() != Some(&'.') {
                    return Some(name);
                }
            }
            Some('(' | '[') | None => return Some(name),
            _ => return None,
        }
        chars.next(); // Schema separator.
        while chars.next_if(|ch| ch.is_whitespace()).is_some() {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MigrationOp, Snapshot};

    #[test]
    fn extension_names_are_validated_at_json_boundaries() {
        for name in [
            "pg_trgm",
            "uuid-ossp",
            "timescaledb",
            "_custom",
            &"a".repeat(63),
        ] {
            let parsed = ExtensionName::try_from(name.to_string()).unwrap();
            assert_eq!(parsed.as_str(), name);
            assert_eq!(parsed.to_string(), name);
            assert_eq!(serde_json::to_value(parsed).unwrap(), name);
        }
        for name in [
            "",
            "PG_TRGM",
            "a\"b",
            "pg trgm",
            "1abc",
            "é",
            &"a".repeat(64),
        ] {
            assert!(ExtensionName::try_from(name.to_string()).is_err());
            let snapshot = serde_json::json!({"version": 1, "tables": {}, "extensions": [name]});
            assert!(serde_json::from_value::<Snapshot>(snapshot).is_err());
            for kind in [
                "ensure_extension",
                "remove_extension_requirement",
                "drop_extension",
            ] {
                let op = serde_json::json!({"type": kind, "name": name});
                assert!(serde_json::from_value::<MigrationOp>(op).is_err());
            }
        }
    }

    #[test]
    fn known_objects_are_unambiguous_and_case_insensitive() {
        for (objects, matcher) in [
            (
                OPCLASSES,
                extension_for_opclass as fn(&str) -> Option<&'static str>,
            ),
            (
                TYPES,
                extension_for_type as fn(&str) -> Option<&'static str>,
            ),
        ] {
            for (object, extension) in objects {
                assert_eq!(matcher(object), Some(*extension));
                assert_eq!(
                    matcher(&format!("public.{}", object.to_uppercase())),
                    Some(*extension)
                );
            }
        }
        for class in [
            "text_pattern_ops",
            "bit_ops",
            "int4_ops",
            "gist_int4_ops",
            "unknown_ops",
        ] {
            assert_eq!(extension_for_opclass(class), None);
        }
        for ty in ["bit(3)", "int2vector", "oidvector", "varchar(10)"] {
            assert_eq!(extension_for_type(ty), None);
        }
    }

    #[test]
    fn type_modifiers_quotes_and_arrays() {
        for (ty, extension) in [
            ("vector(3)", "vector"),
            (" VECTOR(3) ", "vector"),
            ("\"vector\"(3)", "vector"),
            ("public.hstore", "hstore"),
            ("PUBLIC.HSTORE", "hstore"),
            ("citext[]", "citext"),
            ("\"Public\".\"citext\"[]", "citext"),
            ("\"my.schema\".\"vector\"(3)", "vector"),
            ("\"my\"\"schema\".vector", "vector"),
            ("public . \"vector\"(3)", "vector"),
            ("\"geometry\"(Point,4326)", "postgis"),
            ("\"public\".\"geography\"(Point,4326)[]", "postgis"),
            ("ltree ARRAY", "ltree"),
        ] {
            assert_eq!(extension_for_type(ty), Some(extension));
        }
    }

    #[test]
    fn quoted_type_names_are_case_sensitive_and_atomic() {
        for ty in [
            "\"Vector\"",
            "\"VECTOR\"(3)",
            "public.\"Vector\"",
            "vector . \"Vector\"",
            "\"Hstore\"[]",
            "\"my.vector\"",
            "\"my vector\"",
            "\"vector(3)\"",
            "\"vector[]\"",
            "\"my\"\".vector\"",
            "\"vector\"\"\"",
            "\"vector",
            "\"\"",
            "public.",
            "public.\"\"",
            "\"vector\"suffix",
            "vector\"",
            "",
        ] {
            assert_eq!(extension_for_type(ty), None, "{ty}");
        }
    }

    #[test]
    fn snapshot_unions_declared_and_inferred_requirements() {
        let snapshot: Snapshot = serde_json::from_value(serde_json::json!({
            "version": 1, "extensions": ["uuid-ossp", "vector"],
            "tables": {"docs": {
                "name": "docs", "fields": [{"name": "title", "column_type": {"kind": "string"},
                    "db_type": "citext[]", "nullable": false, "primary_key": false, "unique": false,
                    "default": null}],
                "indexes": [{"name": "docs_title_trgm", "fields": [{"column": "title", "opclass": "public.GIN_TRGM_OPS"}], "unique": false}],
                "foreign_keys": [], "checks": []
            }}
        })).unwrap();
        snapshot.validate_model_schema().unwrap();
        let required: Vec<_> = snapshot
            .required_extensions()
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(required, ["citext", "pg_trgm", "uuid-ossp", "vector"]);
    }

    #[test]
    fn index_method_alone_does_not_infer_vector() {
        for method in ["hnsw", "ivfflat"] {
            let snapshot: Snapshot = serde_json::from_value(serde_json::json!({
                "version": 1, "tables": {"docs": {
                    "name": "docs", "fields": [{"name": "title", "column_type": {"kind": "text"},
                        "nullable": false, "primary_key": false, "unique": false, "default": null}],
                    "indexes": [{"name": "ix", "fields": ["title"], "unique": false, "method": method}],
                    "foreign_keys": [], "checks": []
                }}
            })).unwrap();
            assert!(snapshot.required_extensions().is_empty());
        }
    }
}
