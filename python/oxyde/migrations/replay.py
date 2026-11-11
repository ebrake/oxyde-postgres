"""Replay migrations in memory to build schema state."""

from __future__ import annotations

import json
import warnings
from copy import deepcopy
from pathlib import Path
from typing import Any

from oxyde.migrations.context import MigrationContext
from oxyde.migrations.utils import (
    _iter_field_dicts,
    load_migration_module,
    normalize_field_dict,
    normalize_op_fields,
    op_uses_legacy_fields,
)


class SchemaState:
    """Schema replay backed by Rust; Python normalizes legacy field types.

    ``tables`` is a decoded view cached until the next operation. Mutations must
    go through ``apply_operation`` so the Rust state remains authoritative.
    """

    def __init__(self) -> None:
        from oxyde.core import SchemaReplay

        self._replay = SchemaReplay()
        self._tables: dict[str, dict[str, Any]] | None = None

    @property
    def tables(self) -> dict[str, dict[str, Any]]:
        if self._tables is None:
            self._tables = json.loads(self._replay.snapshot_json())["tables"]
            for table in self._tables.values():
                for field in table["fields"]:
                    if field.pop("__oxyde_legacy_field", False):
                        field.pop("column_type", None)
        return self._tables

    def _normalize_operation(self, op: dict[str, Any]) -> dict[str, Any]:
        # Mark synthesized types so the intermediate view keeps the historical
        # field shape. Final snapshots always carry a normalized column_type.
        op = deepcopy(op)
        for field in _iter_field_dicts(op):
            if "column_type" not in field:
                field["__oxyde_legacy_field"] = True
        op = normalize_op_fields(op)
        if op.get("type") == "alter_column" and "column" in op:
            changes = dict(op.get("changes", {}))
            table = self.tables.get(op["table"], {})
            current: dict[str, Any] | None = next(
                (f for f in table.get("fields", []) if f["name"] == op["column"]), None
            )
            if current is not None:
                legacy = "type" in changes or "python_type" in changes
                if legacy or (
                    "column_type" not in current and "column_type" not in changes
                ):
                    source = {**current, **changes}
                    if "type" in changes:
                        source["python_type"] = changes["type"]
                    if "python_type" in changes:
                        source["python_type"] = changes["python_type"]
                    source.pop("column_type", None)
                    changes["column_type"] = normalize_field_dict(source)["column_type"]
                    changes["__oxyde_legacy_field"] = True
                    if legacy:
                        changes["python_type"] = source["python_type"]
                elif "column_type" in changes:
                    changes["__oxyde_legacy_field"] = False
            op["changes"] = changes
        return op

    def resolve_operation(self, op: dict[str, Any]) -> dict[str, Any]:
        """Resolve collect-mode changes against the current schema for SQL."""
        resolved: dict[str, Any] = json.loads(
            self._replay.resolve(json.dumps(self._normalize_operation(op)))
        )
        return resolved

    def apply_operation(self, op: dict[str, Any]) -> None:
        self._replay.apply(json.dumps(self._normalize_operation(op)))
        self._tables = None

    def to_snapshot(self) -> dict[str, Any]:
        tables = deepcopy(self.tables)
        for table in tables.values():
            table["fields"] = [normalize_field_dict(f) for f in table["fields"]]
        return {"version": 1, "tables": tables}


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
            for op in operations:
                state.apply_operation(op)

    return state.to_snapshot()


__all__ = ["SchemaState", "replay_migrations", "_topological_sort_migrations"]
