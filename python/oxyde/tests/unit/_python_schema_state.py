"""Frozen Python replay oracle; production replay lives in Rust."""

import json
from typing import Any, cast

from oxyde.core import migration_to_sql
from oxyde.migrations.context import MigrationContext
from oxyde.migrations.utils import normalize_field_dict, normalize_op_fields


def _add_enum_value_to_spec(
    spec: dict[str, Any], name: str, value: str
) -> dict[str, Any]:
    if spec.get("kind") == "enum" and spec.get("name") == name:
        updated = dict(spec)
        values = list(updated.get("values", []))
        if value not in values:
            values.append(value)
        updated["values"] = values
        return updated
    if spec.get("kind") == "array" and isinstance(spec.get("item"), dict):
        updated = dict(spec)
        updated["item"] = _add_enum_value_to_spec(spec["item"], name, value)
        return updated
    return spec


def _replace_enum_values_in_spec(
    spec: dict[str, Any],
    name: str,
    values: list[str],
) -> dict[str, Any]:
    if spec.get("kind") == "enum" and spec.get("name") == name:
        updated = dict(spec)
        updated["values"] = list(values)
        return updated
    if spec.get("kind") == "array" and isinstance(spec.get("item"), dict):
        updated = dict(spec)
        updated["item"] = _replace_enum_values_in_spec(spec["item"], name, values)
        return updated
    return spec


class _PythonSchemaState:
    """Represents database schema in memory.

    This class applies migration operations to build a virtual schema
    without touching the actual database.
    """

    def __init__(self) -> None:
        self.tables: dict[str, dict[str, Any]] = {}

    def apply_operation(self, op: dict[str, Any]) -> None:
        """Apply a migration operation to the virtual schema.

        Args:
            op: Operation dictionary from MigrationContext.get_collected_operations()
        """
        op_type = op.get("type")

        if op_type == "create_enum_type":
            pass

        elif op_type == "drop_enum_type":
            pass

        elif op_type == "add_enum_value":
            enum_name = op["name"]
            enum_value = op["value"]
            for table in self.tables.values():
                for field in table["fields"]:
                    if column_type := field.get("column_type"):
                        field["column_type"] = _add_enum_value_to_spec(
                            column_type,
                            enum_name,
                            enum_value,
                        )

        elif op_type == "alter_enum_type":
            enum_name = op["name"]
            enum_values = op["new_values"]
            for table in self.tables.values():
                for field in table["fields"]:
                    if column_type := field.get("column_type"):
                        field["column_type"] = _replace_enum_values_in_spec(
                            column_type,
                            enum_name,
                            enum_values,
                        )

        elif op_type == "create_table":
            table = op["table"]
            self.tables[table["name"]] = {
                "name": table["name"],
                "fields": list(table["fields"]),
                "indexes": list(table.get("indexes", [])),
                "foreign_keys": list(table.get("foreign_keys", [])),
                "checks": list(table.get("checks", [])),
                "comment": table.get("comment"),
            }

        elif op_type == "drop_table":
            self.tables.pop(op["name"], None)

        elif op_type == "rename_table":
            old_name = op["old_name"]
            new_name = op["new_name"]
            if old_name in self.tables:
                table = self.tables.pop(old_name)
                table["name"] = new_name
                self.tables[new_name] = table

        elif op_type == "add_column":
            table_name = op["table"]
            if table_name in self.tables:
                self.tables[table_name]["fields"].append(dict(op["field"]))

        elif op_type == "drop_column":
            table_name = op["table"]
            field_name = op["field"]
            if table_name in self.tables:
                self.tables[table_name]["fields"] = [
                    f
                    for f in self.tables[table_name]["fields"]
                    if f["name"] != field_name
                ]

        elif op_type == "rename_column":
            table_name = op["table"]
            old_name = op["old_name"]
            new_name = op["new_name"]
            if table_name in self.tables:
                for field in self.tables[table_name]["fields"]:
                    if field["name"] == old_name:
                        field["name"] = new_name
                        break

        elif op_type == "alter_column":
            table_name = op["table"]
            column_name = op["column"]
            changes = op.get("changes", {})
            if table_name in self.tables:
                for field in self.tables[table_name]["fields"]:
                    if field["name"] == column_name:
                        # Map changes keys to field keys
                        if "column_type" in changes:
                            field["column_type"] = changes["column_type"]
                        # Legacy keys from old migration files
                        if "type" in changes:
                            field["python_type"] = changes["type"]
                            field.pop("column_type", None)
                        if "python_type" in changes:
                            field["python_type"] = changes["python_type"]
                            field.pop("column_type", None)
                        if "db_type" in changes:
                            field["db_type"] = changes["db_type"]
                        if "nullable" in changes:
                            field["nullable"] = changes["nullable"]
                        if "default" in changes:
                            field["default"] = changes["default"]
                        if "unique" in changes:
                            field["unique"] = changes["unique"]
                        if "nulls_not_distinct" in changes:
                            if changes["nulls_not_distinct"]:
                                field["nulls_not_distinct"] = True
                            else:
                                field.pop("nulls_not_distinct", None)
                        if "max_length" in changes:
                            field["max_length"] = changes["max_length"]
                        if "max_digits" in changes:
                            field["max_digits"] = changes["max_digits"]
                        if "decimal_places" in changes:
                            field["decimal_places"] = changes["decimal_places"]
                        break

        elif op_type == "create_index":
            table_name = op["table"]
            if table_name in self.tables:
                self.tables[table_name]["indexes"].append(dict(op["index"]))

        elif op_type == "drop_index":
            table_name = op["table"]
            index_name = op["name"]
            if table_name in self.tables:
                self.tables[table_name]["indexes"] = [
                    idx
                    for idx in self.tables[table_name]["indexes"]
                    if idx["name"] != index_name
                ]

        elif op_type == "add_foreign_key":
            table_name = op["table"]
            if table_name in self.tables:
                if "foreign_keys" not in self.tables[table_name]:
                    self.tables[table_name]["foreign_keys"] = []
                self.tables[table_name]["foreign_keys"].append(dict(op["fk"]))

        elif op_type == "drop_foreign_key":
            table_name = op["table"]
            fk_name = op["name"]
            if table_name in self.tables and "foreign_keys" in self.tables[table_name]:
                self.tables[table_name]["foreign_keys"] = [
                    fk
                    for fk in self.tables[table_name]["foreign_keys"]
                    if fk["name"] != fk_name
                ]

        elif op_type == "add_check":
            table_name = op["table"]
            if table_name in self.tables:
                if "checks" not in self.tables[table_name]:
                    self.tables[table_name]["checks"] = []
                self.tables[table_name]["checks"].append(dict(op["check"]))

        elif op_type == "drop_check":
            table_name = op["table"]
            check_name = op["name"]
            if table_name in self.tables and "checks" in self.tables[table_name]:
                self.tables[table_name]["checks"] = [
                    c
                    for c in self.tables[table_name]["checks"]
                    if c["name"] != check_name
                ]

        # Note: ctx.execute() and Python code are ignored - they don't affect schema

    def to_snapshot(self) -> dict[str, Any]:
        """Convert to Rust-compatible snapshot format.

        Normalizes legacy field dicts (python_type form from old migration
        files) into the ColumnTypeSpec form Rust requires. This is the single
        legacy-reading point; it becomes a hard error in 1.0.

        Returns:
            Snapshot dictionary compatible with Rust Snapshot structure
        """
        tables: dict[str, Any] = {}
        for name, table in self.tables.items():
            table = dict(table)
            table["fields"] = [normalize_field_dict(f) for f in table["fields"]]
            # Copy mutable collections: a snapshot must not alias live state
            for key in ("indexes", "foreign_keys", "checks"):
                table[key] = [dict(item) for item in table.get(key, [])]
            tables[name] = table
        return {
            "version": 1,
            "tables": tables,
        }


class _PythonMigrationContext(MigrationContext):
    """Frozen Python alteration resolution, with sequential schema tracking."""

    _schema_state: Any

    def __init__(self, *, schema_state: _PythonSchemaState, **kwargs: Any):
        super().__init__(schema_state=cast(Any, schema_state), **kwargs)

    def _execute_operation(self, op):
        self._sql_statements.extend(
            migration_to_sql(json.dumps([normalize_op_fields(op)]), self._dialect)
        )
        # Frozen replay consumes collect alterations; alter_column below applies
        # that original shape after SQL generation.
        if op["type"] != "alter_column":
            self._schema_state.apply_operation(op)

    def alter_column(self, table: str, field_name: str, **changes: Any) -> None:
        """Alter column properties (nullable, type, default, etc.).

        Args:
            table: Table name
            field_name: Column name
            **changes: Properties to change (nullable, type, default, etc.)
        """
        # For collect mode, store simple format
        if self._mode == "collect":
            op: dict[str, Any] = {
                "type": "alter_column",
                "table": table,
                "column": field_name,
                "changes": changes,
            }
            self._operations.append(op)
            return

        # For execute mode, build Rust-compatible format with old_field/new_field
        old_field = None

        if self._schema_state:
            table_schema = self._schema_state.tables.get(table)
            if table_schema:
                # Find old field definition
                for field in table_schema["fields"]:
                    if field["name"] == field_name:
                        old_field = dict(field)
                        break

        if old_field is None:
            # Fallback: create minimal field definition
            old_field = {
                "name": field_name,
                "column_type": changes.get("column_type", {"kind": "unknown"}),
                "db_type": None,
                "nullable": True,
                "primary_key": False,
                "unique": False,
                "default": None,
                "auto_increment": False,
            }

        # Build new_field by applying changes to old_field
        new_field = dict(old_field)
        if "column_type" in changes:
            new_field["column_type"] = changes["column_type"]
        # Legacy keys from old migration files — drop stale column_type so
        # normalization re-derives it from the legacy name
        if "type" in changes:
            new_field["python_type"] = changes["type"]
            new_field.pop("column_type", None)
        if "python_type" in changes:
            new_field["python_type"] = changes["python_type"]
            new_field.pop("column_type", None)
        if "db_type" in changes:
            new_field["db_type"] = changes["db_type"]
        if "nullable" in changes:
            new_field["nullable"] = changes["nullable"]
        if "default" in changes:
            new_field["default"] = changes["default"]
        if "unique" in changes:
            new_field["unique"] = changes["unique"]
        if "nulls_not_distinct" in changes:
            if changes["nulls_not_distinct"]:
                new_field["nulls_not_distinct"] = True
            else:
                new_field.pop("nulls_not_distinct", None)
        if "max_length" in changes:
            new_field["max_length"] = changes["max_length"]
        if "max_digits" in changes:
            new_field["max_digits"] = changes["max_digits"]
        if "decimal_places" in changes:
            new_field["decimal_places"] = changes["decimal_places"]

        # Build Rust-compatible operation
        op = {
            "type": "alter_column",
            "table": table,
            "old_field": old_field,
            "new_field": new_field,
        }

        self._execute_operation(op)

        self._schema_state.apply_operation(
            {
                "type": "alter_column",
                "table": table,
                "column": field_name,
                "changes": changes,
            }
        )
