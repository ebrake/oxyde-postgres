"""Differential state and SQL checks for generated, legacy and on-disk fixtures."""

from copy import deepcopy

from oxyde.migrations.context import MigrationContext
from oxyde.migrations.replay import SchemaState

from ._python_schema_state import _PythonMigrationContext, _PythonSchemaState


class _TracePythonState(_PythonSchemaState):
    def __init__(self):
        super().__init__()
        self.trace = []

    def apply_operation(self, op):
        super().apply_operation(op)
        self.trace.append(deepcopy(self.tables))


class _TraceRustState(SchemaState):
    def __init__(self):
        super().__init__()
        self.trace = []

    def apply_operation(self, op):
        super().apply_operation(op)
        self.trace.append(deepcopy(self.tables))


def assert_migration_parity(module, initial=None):
    """Compare every intermediate state and the execute SQL of one migration."""
    python = _TracePythonState()
    rust = _TraceRustState()
    for table in (initial or {}).get("tables", {}).values():
        op = {"type": "create_table", "table": deepcopy(table)}
        python.apply_operation(deepcopy(op))
        rust.apply_operation(deepcopy(op))
    collect = MigrationContext(mode="collect")
    module.upgrade(collect)
    for op in collect.get_collected_operations():
        python.apply_operation(deepcopy(op))
        rust.apply_operation(deepcopy(op))
        assert rust.tables == python.tables, op
    assert rust.to_snapshot() == python.to_snapshot()

    # Execute the actual migration entry point. Seed independent states to make
    # alterations read the same pre-migration and intra-migration definitions.
    python = _TracePythonState()
    rust = _TraceRustState()
    for table in (initial or {}).get("tables", {}).values():
        op = {"type": "create_table", "table": deepcopy(table)}
        python.apply_operation(deepcopy(op))
        rust.apply_operation(deepcopy(op))
    old_ctx = _PythonMigrationContext(mode="execute", schema_state=python)
    new_ctx = MigrationContext(mode="execute", schema_state=rust)
    old_error = new_error = None
    try:
        module.upgrade(old_ctx)
    except RuntimeError as error:
        old_error = str(error)
    try:
        module.upgrade(new_ctx)
    except RuntimeError as error:
        new_error = str(error)
    assert old_error == new_error
    assert new_ctx._sql_statements == old_ctx._sql_statements
    assert rust.trace == python.trace
    assert rust.tables == python.tables
    assert rust.to_snapshot() == python.to_snapshot()
    return {"version": 1, "tables": deepcopy(rust.tables)}


def assert_fixture_histories(root):
    """Check every valid migration history written by an existing fixture."""
    from oxyde.migrations.replay import _topological_sort_migrations
    from oxyde.migrations.utils import load_migration_module

    directories = {path.parent for path in root.rglob("[0-9]*.py")}
    for directory in directories:
        try:
            paths = _topological_sort_migrations(list(directory.glob("[0-9]*.py")))
        except ValueError:
            # Dependency-cycle fixtures are invalid histories by construction.
            continue
        initial = None
        for path in paths:
            module = load_migration_module(path)
            if module is not None and hasattr(module, "upgrade"):
                initial = assert_migration_parity(module, initial)
