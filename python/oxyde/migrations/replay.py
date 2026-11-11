"""Replay migrations in memory to build schema state."""

from __future__ import annotations

import json
import warnings
from copy import deepcopy
from pathlib import Path
from typing import Any

from oxyde.migrations.context import MigrationContext
from oxyde.migrations.utils import (
    load_migration_module,
    normalize_field_dict,
    normalize_op_fields,
    op_uses_legacy_fields,
)


class MigrationReplayError(ValueError):
    """A historical operation cannot be replayed under the migration contract."""


class SchemaState:
    """Rust-backed schema with Python normalization of legacy field types.

    ``tables`` is a read-only snapshot view. Each access returns an independent,
    canonical dictionary; use ``apply_operation`` to change the schema.
    """

    def __init__(self) -> None:
        from oxyde.core import SchemaReplay

        if not isinstance(SchemaReplay, type):
            raise RuntimeError(
                "oxyde-postgres 0.3 requires oxyde-postgres-core>=0.3.0; "
                "installed core lacks SchemaReplay"
            )
        self._replay = SchemaReplay()
        self._legacy: dict[tuple[str, str], dict[str, Any]] = {}

    @property
    def tables(self) -> dict[str, dict[str, Any]]:
        tables: dict[str, dict[str, Any]] = self.to_snapshot()["tables"]
        return tables

    def get_field(self, table: str, column: str) -> dict[str, Any] | None:
        """Read one canonical field without serializing the entire schema."""
        raw = self._replay.field_json(table, column)
        return json.loads(raw) if raw is not None else None

    def _normalize_operation(self, op: dict[str, Any]) -> dict[str, Any]:
        op = normalize_op_fields(deepcopy(op))
        if op.get("type") == "alter_column":
            column = op.get("column") or op.get("old_field", {}).get("name")
            changes = op.get("changes", {})
            if "nulls_not_distinct" in changes:
                changes["nulls_not_distinct"] = bool(changes["nulls_not_distinct"])
            legacy = self._legacy.get((op["table"], column))
            changes_type = "type" in changes or "python_type" in changes
            if changes_type or (legacy is not None and "column_type" not in changes):
                current = self.get_field(op["table"], column) or op.get("old_field")
                if current is not None:
                    source = {**current, **(legacy or {}), **changes}
                    if "type" in changes:
                        source["python_type"] = changes["type"]
                    if "python_type" in changes:
                        source["python_type"] = changes["python_type"]
                    source.pop("column_type", None)
                    changes["column_type"] = normalize_field_dict(source)["column_type"]
            if "changes" in op:
                op["changes"] = changes
        return op

    def resolve_operation(self, op: dict[str, Any]) -> dict[str, Any]:
        """Resolve collect-mode changes against the current schema for SQL."""
        resolved: dict[str, Any] = json.loads(
            self._replay.resolve(json.dumps(self._normalize_operation(op)))
        )
        return resolved

    def apply_operation(self, op: dict[str, Any]) -> None:
        normalized = self._normalize_operation(op)
        kind = op.get("type")
        table: Any = op.get("table")
        exists = False
        if kind == "rename_table":
            exists = self._replay.has_table(op["old_name"])
        elif kind in ("rename_column", "alter_column"):
            column = (
                op.get("old_name")
                or op.get("column")
                or op.get("old_field", {}).get("name")
            )
            exists = self.get_field(table, column) is not None
        self._replay.apply(json.dumps(normalized))
        # Provenance is Python-only and changes only after successful Rust replay.
        if kind == "create_table":
            name = table["name"]
            self._forget_table(name)
            for field in table["fields"]:
                self._remember_field(name, field)
        elif kind == "drop_table":
            self._forget_table(op["name"])
        elif kind == "rename_table" and exists:
            old, new = op["old_name"], op["new_name"]
            fields = {
                column: source
                for (name, column), source in self._legacy.items()
                if name == old
            }
            self._forget_table(old)
            self._forget_table(new)
            self._legacy.update(
                {(new, column): source for column, source in fields.items()}
            )
        elif (
            kind == "add_column"
            and self.get_field(table, op["field"]["name"]) is not None
        ):
            self._remember_field(table, op["field"])
        elif kind == "drop_column":
            self._legacy.pop((table, op["field"]), None)
        elif kind == "rename_column" and exists:
            source = self._legacy.pop((table, op["old_name"]), None)
            self._legacy.pop((table, op["new_name"]), None)
            if source is not None:
                self._legacy[(table, op["new_name"])] = source
        elif kind == "alter_column" and exists:
            column = op.get("column") or op["old_field"]["name"]
            key = (table, column)
            changes = op.get("changes", {})
            if "type" in changes or "python_type" in changes:
                source = dict(self._legacy.get(key, {}))
                source["python_type"] = changes.get("python_type", changes.get("type"))
                if "db_type" in changes:
                    source["db_type"] = changes["db_type"]
                self._legacy[key] = source
            elif "column_type" in changes or "new_field" in op:
                self._legacy.pop(key, None)
            elif key in self._legacy and "db_type" in changes:
                self._legacy[key]["db_type"] = changes["db_type"]

    def _forget_table(self, table: str) -> None:
        self._legacy = {
            key: source for key, source in self._legacy.items() if key[0] != table
        }

    def _remember_field(self, table: str, field: dict[str, Any]) -> None:
        key = (table, field["name"])
        self._legacy.pop(key, None)
        if "column_type" not in field:
            self._legacy[key] = {
                key: field[key] for key in ("python_type", "db_type") if key in field
            }

    def to_snapshot(self) -> dict[str, Any]:
        snapshot: dict[str, Any] = json.loads(self._replay.snapshot_json())
        return snapshot


def resolve_alter_column(
    state: SchemaState, table: str, column: str, changes: dict[str, Any]
) -> dict[str, Any]:
    """Resolve an alteration, sharing the execute-mode unknown-column fallback."""
    op = {"type": "alter_column", "table": table, "column": column, "changes": changes}
    if state.get_field(table, column) is None:
        op["old_field"] = {
            "name": column,
            "column_type": changes.get("column_type", {"kind": "unknown"}),
            "db_type": None,
            "nullable": True,
            "primary_key": False,
            "unique": False,
            "default": None,
            "auto_increment": False,
        }
    return state.resolve_operation(op)


def _apply_module_ops(
    state: SchemaState, file: Path, ops: list[dict[str, Any]]
) -> None:
    for i, op in enumerate(ops, 1):
        try:
            state.apply_operation(op)
        except Exception as err:
            raise MigrationReplayError(
                f"{file.name}: operation {i} ({op.get('type', '?')}): {err}. "
                "Edit this historical migration file to correct the operation."
            ) from err


def _get_migration_dependency(module: Any) -> str | None:
    """Get the depends_on value from a migration module.

    Args:
        module: Loaded migration module

    Returns:
        Name of dependency migration or None
    """
    return getattr(module, "depends_on", None)


def _topological_sort_migrations(
    migration_files: list[Path],
) -> list[Path]:
    """Sort migrations in topological order based on depends_on.

    Args:
        migration_files: List of migration file paths

    Returns:
        Migrations sorted in dependency order

    Raises:
        ValueError: If circular dependency detected
    """
    # Build dependency graph
    modules: dict[str, tuple[Path, Any]] = {}
    for file in migration_files:
        module = load_migration_module(file)
        if module is not None:
            modules[file.stem] = (file, module)

    # Build adjacency list (name -> depends_on)
    dependencies: dict[str, str | None] = {}
    for name, (_, module) in modules.items():
        dependencies[name] = _get_migration_dependency(module)

    # Topological sort using Kahn's algorithm
    # Count incoming edges
    in_degree: dict[str, int] = {name: 0 for name in modules}
    for name, dep in dependencies.items():
        if dep is not None and dep in in_degree:
            in_degree[name] = 1  # Has one dependency

    # Start with nodes that have no dependencies
    queue = [name for name, degree in in_degree.items() if degree == 0]
    result: list[Path] = []

    while queue:
        # Sort queue to ensure deterministic order (by migration name)
        queue.sort()
        current = queue.pop(0)
        result.append(modules[current][0])

        # Find migrations that depend on current
        for name, dep in dependencies.items():
            if dep == current:
                in_degree[name] -= 1
                if in_degree[name] == 0:
                    queue.append(name)

    # Check for circular dependencies
    if len(result) != len(modules):
        applied = {f.stem for f in result}
        remaining = set(modules.keys()) - applied
        raise ValueError(f"Circular dependency detected in migrations: {remaining}")

    return result


def replay_migrations(migrations_dir: str = "migrations") -> dict[str, Any]:
    """Replay all migrations and return final schema as snapshot.

    Migrations are applied in topological order based on their depends_on field.

    Args:
        migrations_dir: Directory containing migration files

    Returns:
        Schema snapshot after replaying all migrations
    """
    state = SchemaState()
    migration_files = sorted(Path(migrations_dir).glob("[0-9]*.py"))

    # Sort migrations by dependencies
    sorted_files = _topological_sort_migrations(migration_files)

    for file in sorted_files:
        module = load_migration_module(file)
        if module is None:
            continue

        # Run upgrade() in "collect" mode
        if hasattr(module, "upgrade"):
            ctx = MigrationContext(mode="collect")
            module.upgrade(ctx)

            # Apply collected operations to virtual schema
            operations = ctx.get_collected_operations()
            if any(op_uses_legacy_fields(op) for op in operations):
                warnings.warn(
                    f"Migration file '{file.name}' uses the legacy field format "
                    "(python_type). Support will be removed in oxyde 1.0 — "
                    "run `oxyde migrations squash` to convert.",
                    FutureWarning,
                    stacklevel=2,
                )
            _apply_module_ops(state, file, operations)

    return state.to_snapshot()


__all__ = [
    "SchemaState",
    "MigrationReplayError",
    "resolve_alter_column",
    "replay_migrations",
    "_topological_sort_migrations",
]
