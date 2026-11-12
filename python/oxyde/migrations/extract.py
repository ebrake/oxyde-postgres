"""Extract database schema from registered models."""

from __future__ import annotations

import json
import warnings
from datetime import date, datetime, time
from decimal import Decimal
from enum import Enum
from typing import Any
from uuid import UUID

from pydantic.fields import PydanticUndefined

from oxyde.core import migration_validate_schema
from oxyde.core.column_types import compute_column_type
from oxyde.core.types import serialize_value
from oxyde.models.base import Model
from oxyde.models.metadata import db_column_for
from oxyde.models.registry import iter_tables, registered_tables
from oxyde.queries.expressions import (
    _Expression,
    _map_expression_columns,
    _resolve_column_name,
)


def _resolve_fk_target(
    target_key: str, target_field: str | None = None
) -> tuple[str, str]:
    """Resolve FK target to (table_name, column_name).

    Args:
        target_key: Model key like "__main__.User" or table name for primitive FK
        target_field: Target field name (for model FK) or column name
                      (for primitive FK). If None, auto-detects PK.

    Returns:
        Tuple of (table_name, column_name)
    """
    # Get all registered tables
    tables = registered_tables()

    # Try to find by full key or class name
    for key, model_cls in tables.items():
        if key == target_key or model_cls.__name__ == target_key.split(".")[-1]:
            table_name = model_cls._db_meta.table_name or model_cls.__name__.lower()

            if target_field:
                # Look up the specified field's db_column
                for field_name, field_meta in model_cls._db_meta.field_metadata.items():
                    if field_name == target_field:
                        return table_name, field_meta.db_column
                # Field not found in metadata - use target_field as column name
                return table_name, target_field
            else:
                # Find PK column (fallback for backwards compatibility)
                pk_column = "id"  # default
                for field_name, field_meta in model_cls._db_meta.field_metadata.items():
                    if field_meta.primary_key:
                        pk_column = field_meta.db_column
                        break
                return table_name, pk_column

    # No matching model found - target_key is likely a plain table name (primitive FK)
    # Use target_key as table name and target_field as column name
    if target_field:
        return target_key, target_field

    # Fallback: extract table name from key, assume "id" as column
    class_name = target_key.split(".")[-1]
    return class_name.lower(), "id"


def _serialize_default(value: Any, dialect: str) -> str | None:
    """Serialize Python default value to SQL default expression.

    Args:
        value: Python default value
        dialect: Database dialect

    Returns:
        SQL default expression string, or None if cannot serialize
    """
    if value is None:
        return "NULL"

    # Callable defaults (like datetime.now) cannot be serialized
    # User should use db_default for SQL expressions
    if callable(value):
        return None

    if isinstance(value, Enum):
        escaped = str(value.value).replace("'", "''")
        return f"'{escaped}'"

    # String - quote it
    if isinstance(value, str):
        # Escape single quotes
        escaped = value.replace("'", "''")
        return f"'{escaped}'"

    # Boolean
    if isinstance(value, bool):
        return "TRUE" if value else "FALSE"

    # Numbers
    if isinstance(value, (int, float, Decimal)):
        return str(value)

    # UUID
    if isinstance(value, UUID):
        return f"'{value}'"

    # Date/time types
    if isinstance(value, datetime):
        return f"'{value.isoformat()}'"
    if isinstance(value, date):
        return f"'{value.isoformat()}'"
    if isinstance(value, time):
        return f"'{value.isoformat()}'"

    # Bytes
    if isinstance(value, bytes):
        return f"'\\x{value.hex()}'"

    # Fallback - try str() but return None if it looks like a repr
    str_val = str(value)
    if str_val.startswith("<") or "object at" in str_val:
        return None
    return str_val


def _snapshot_literal_value(value: Any) -> Any:
    """Encode MessagePack values as JSON, retaining bytes as octet lists."""
    if isinstance(value, bytes):
        return list(value)
    if isinstance(value, list):
        return [_snapshot_literal_value(item) for item in value]
    if isinstance(value, dict):
        return {key: _snapshot_literal_value(item) for key, item in value.items()}
    return value


def _bytes_list_spec(value: Any) -> dict[str, Any] | None:
    """Type of a non-empty (nested) list of ``bytes``, else ``None``.

    JSON carries bytes only as octet lists, which look like lists of small
    integers, so the literal has to say what it is for the core to restore it.
    """
    if isinstance(value, (bytes, bytearray)):
        return {"kind": "blob"}
    if isinstance(value, (list, tuple)) and value:
        items = [_bytes_list_spec(item) for item in value]
        if all(item is not None and item == items[0] for item in items):
            return {"kind": "array", "item": items[0]}
    return None


def _normalize_index_literals(node: Any) -> Any:
    """Make the literals of an index expression snapshot-safe.

    The value goes through ``serialize_value`` so Decimal/date/UUID/Enum
    become plain JSON values: they must survive the snapshot, the generated
    migration file (``repr``) and replay. Bytes become octet lists.

    Nothing schema-aware happens here. The core canonicalizes an expression
    key when it reads it: enum value lists are dropped from ``column_types``
    and from literal types (they would make the index diff, and be recreated,
    whenever the enum gains a member), and an octet list typed ``blob`` becomes
    bytes. Doing it once in the core means the snapshot, the migration files
    and replay all agree.
    """
    if isinstance(node, dict):
        if node.get("type") == "value":
            raw = node["value"]
            out = {**node, "value": _snapshot_literal_value(serialize_value(raw))}
            if "value_type" not in out and isinstance(raw, (list, tuple)):
                spec = _bytes_list_spec(raw)
                if spec is not None:
                    out["value_type"] = spec
            return out
        return {key: _normalize_index_literals(val) for key, val in node.items()}
    if isinstance(node, list):
        return [_normalize_index_literals(item) for item in node]
    return node


def _extract_index_keys(
    index: Any, model_cls: type[Model], column_types: dict[str, Any]
) -> list[Any]:
    """Wire form of ``index.fields`` (the Rust ``IndexKey`` encoding).

    A plain column with the default operator class is a bare string; anything
    else is ``{"column"|"expr": ..., "opclass": ...}``.

    Field names are mapped to their ``db_column`` (FK virtual fields to their
    ``<name>_id`` column). Names that are not model fields pass through, so a
    raw column name keeps working and snapshots without remapped fields do not
    change.
    """
    opclasses = index.opclasses or (None,) * len(index.fields)

    keys: list[Any] = []
    for key, opclass in zip(index.fields, opclasses, strict=True):
        wire: Any
        if isinstance(key, _Expression):
            array_types: dict[str, Any] = {}

            def resolve(name: str) -> str:
                column = _resolve_column_name(name, model_cls)
                spec = column_types.get(column, {})
                if spec.get("kind") == "array":
                    array_types[column] = spec
                return column

            mapped = _map_expression_columns(key._expr, resolve)
            wire = {"expr": _normalize_index_literals(mapped)}
            if array_types:
                wire["column_types"] = array_types
            if opclass is not None:
                wire["opclass"] = opclass
        else:
            column = db_column_for(model_cls, key)
            wire = column if opclass is None else {"column": column, "opclass": opclass}
        keys.append(wire)
    return keys


def _auto_index_name(table_name: str, index: Any) -> str:
    """Default name of an unnamed Meta index.

    ``<table>_<field>_..._idx``. A key with an operator class contributes
    ``<field>_<opclass>``, so ``Index(("email",), opclasses=("text_pattern_ops",))``
    does not collide with the plain index on the same column.

    ``Index`` requires an explicit name for expression keys, so only plain
    field names reach this function.
    """
    opclasses = index.opclasses or (None,) * len(index.fields)
    parts = [
        key if opclass is None else f"{key}_{opclass.replace('.', '_')}"
        for key, opclass in zip(index.fields, opclasses, strict=True)
    ]
    return f"{table_name}_{'_'.join(parts)}_idx"


def extract_current_schema(dialect: str = "postgres") -> dict[str, Any]:
    """Extract current schema from registered models.

    Args:
        dialect: Target database dialect (postgres)

    Returns:
        Schema snapshot compatible with Rust Snapshot structure:
        {
            "version": 1,
            "tables": {
                "table_name": {
                    "name": "table_name",
                    "fields": [...],
                    "indexes": [...],
                    "comment": None
                }
            }
        }
    """
    from oxyde.migrations.utils import validate_dialect

    validate_dialect(dialect)
    tables = {}

    for model_cls in iter_tables():
        meta = model_cls._db_meta
        table_name = meta.table_name
        assert table_name is not None, "Registered tables must have a table name"

        # Extract fields
        fields = []
        for field_name, field_meta in meta.field_metadata.items():
            # Skip virtual relation fields - these are for ORM, not actual DB columns:
            # 1. FK fields (python_type is Model) - real column is synthetic field
            # 2. Reverse FK fields (db_reverse_fk) - no DB column, just relation
            # 3. M2M fields (db_m2m) - no DB column, uses junction table
            if isinstance(field_meta.python_type, type) and issubclass(
                field_meta.python_type, Model
            ):
                continue
            if field_meta.extra.get("reverse_fk") or field_meta.extra.get("m2m"):
                continue

            # Determine Python type for SQL mapping
            python_type_for_sql = field_meta.python_type

            auto_increment = False

            # Convert default value to SQL expression
            default_val = None
            # Priority: db_default (explicit SQL) > default (Python value)
            if hasattr(field_meta, "db_default") and field_meta.db_default is not None:
                # db_default is a raw SQL expression - use as-is
                default_val = field_meta.db_default
            elif (
                field_meta.default is not None
                and field_meta.default is not PydanticUndefined
            ):
                # Serialize Python value to SQL
                default_val = _serialize_default(field_meta.default, dialect)

            # Compute the ColumnTypeSpec via the single mapping point;
            # unknown annotations collapse to "unknown" (TEXT DDL, native bind)
            column_type = compute_column_type(
                python_type_for_sql,
                field_meta.db_type,
                max_length=field_meta.max_length,
                max_digits=field_meta.max_digits,
                decimal_places=field_meta.decimal_places,
            ) or {"kind": "unknown"}

            field_dict: dict[str, Any] = {
                "name": field_meta.db_column,
                "column_type": column_type,
                "db_type": field_meta.db_type,  # explicit user override (verbatim DDL)
                "nullable": field_meta.nullable,
                "primary_key": field_meta.primary_key,
                "unique": field_meta.unique,
                "default": default_val,
                "auto_increment": auto_increment,
            }
            if field_meta.nulls_distinct is False:
                field_dict["nulls_not_distinct"] = True
            for attr in ("max_length", "max_digits", "decimal_places"):
                val = getattr(field_meta, attr, None)
                if val is not None:
                    field_dict[attr] = val
            fields.append(field_dict)

        # Extract indexes
        indexes: list[dict[str, Any]] = []
        if hasattr(meta, "indexes") and meta.indexes:
            for index in meta.indexes:
                index_name = index.name or _auto_index_name(table_name, index)

                indexes.append(
                    {
                        "name": index_name,
                        "fields": _extract_index_keys(
                            index,
                            model_cls,
                            {field["name"]: field["column_type"] for field in fields},
                        ),
                        "unique": index.unique,
                        **(
                            {"nulls_not_distinct": True}
                            if index.nulls_distinct is False
                            else {}
                        ),
                        "method": index.method,
                        "where": index.where.strip() or None
                        if isinstance(index.where, str)
                        else index.where,
                    }
                )

        # Field-level indexes: Field(db_index=True) on any column, FK included.
        # Skipped for unique columns — the UNIQUE constraint already creates
        # an index, an extra plain one is redundant.
        # Only indexes made of bare db column names count: an expression or
        # operator-class key is a different index than a plain one on the column.
        declared_column_sets = {
            frozenset(index["fields"])
            for index in indexes
            if all(isinstance(key, str) for key in index["fields"])
        }
        for field_name, field_meta in meta.field_metadata.items():
            if isinstance(field_meta.python_type, type) and issubclass(
                field_meta.python_type, Model
            ):
                continue  # virtual FK field — flags live on the synthetic column
            if field_meta.extra.get("reverse_fk") or field_meta.extra.get("m2m"):
                continue
            if not field_meta.index or field_meta.unique:
                continue

            column = field_meta.db_column
            if frozenset([column]) in declared_column_sets:
                warnings.warn(
                    f"Field '{field_name}' on table '{meta.table_name}' declares "
                    f"db_index=True, but Meta.indexes already contains an index "
                    f"on ('{column}',). Skipping the field-level index — drop "
                    "one of the two declarations.",
                    stacklevel=2,
                )
                continue

            indexes.append(
                {
                    "name": field_meta.index_name or f"{meta.table_name}_{column}_idx",
                    "fields": [column],
                    "unique": False,
                    "method": field_meta.index_method,
                    "where": None,
                }
            )

        # Extract foreign keys from field metadata
        foreign_keys = []
        for field_name, field_meta in meta.field_metadata.items():
            if field_meta.foreign_key:
                fk = field_meta.foreign_key
                # Resolve target model to actual table name and column
                ref_table, ref_column = _resolve_fk_target(fk.target, fk.target_field)
                # Generate FK constraint name
                fk_name = f"fk_{meta.table_name}_{field_meta.db_column}"
                foreign_keys.append(
                    {
                        "name": fk_name,
                        "columns": [field_meta.db_column],
                        "ref_table": ref_table,
                        "ref_columns": [ref_column],
                        "on_delete": fk.on_delete,
                        "on_update": fk.on_update,
                    }
                )

        # Extract check constraints from Meta.constraints
        checks = []
        if hasattr(meta, "constraints") and meta.constraints:
            for i, check in enumerate(meta.constraints):
                # Check object has .expression and .name attributes
                check_name = check.name or f"chk_{meta.table_name}_{i}"
                checks.append(
                    {
                        "name": check_name,
                        "expression": check.expression,
                    }
                )

        # Build table definition
        tables[meta.table_name] = {
            "name": meta.table_name,
            "fields": fields,
            "indexes": indexes,
            "foreign_keys": foreign_keys,
            "checks": checks,
            "comment": meta.comment,
        }

    snapshot = {
        "version": 1,
        "tables": tables,
    }
    # Rust owns the cross-checks (index columns exist, index names are unique
    # schema-wide and after PostgreSQL's 63-byte truncation).
    migration_validate_schema(json.dumps(snapshot))
    return snapshot


__all__ = ["extract_current_schema"]
