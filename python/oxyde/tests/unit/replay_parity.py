"""Compare final canonical snapshots against the unmodified 0.2.2 replay."""

import json
from copy import deepcopy

from oxyde.migrations.context import MigrationContext
from oxyde.migrations.replay import SchemaState, replay_migrations

from ._python_schema_state import SchemaState as _PythonSchemaState


def canonical_snapshot(snapshot):
    """Use the serialized contract to discard legacy metadata and fill defaults."""
    from oxyde.core import SchemaReplay
    from oxyde.migrations.utils import normalize_op_fields

    replay = SchemaReplay()
    for table in snapshot["tables"].values():
        replay.apply(
            json.dumps(normalize_op_fields({"type": "create_table", "table": table}))
        )
    return json.loads(replay.snapshot_json())


def assert_migration_parity(module, initial=None):
    """Compare final schema only; execute-mode state updates changed in 0.3."""
    python, rust = _PythonSchemaState(), SchemaState()
    for table in (initial or {}).get("tables", {}).values():
        op = {"type": "create_table", "table": deepcopy(table)}
        python.apply_operation(deepcopy(op))
        rust.apply_operation(op)
    collect = MigrationContext(mode="collect")
    module.upgrade(collect)
    for op in collect.get_collected_operations():
        python.apply_operation(deepcopy(op))
        rust.apply_operation(op)
    assert rust.to_snapshot() == canonical_snapshot(python.to_snapshot())
    return rust.to_snapshot()


def assert_fixture_histories(root):
    """Strictly replay existing files without rewriting any historical dict."""
    for directory in {path.parent for path in root.rglob("[0-9]*.py")}:
        replay_migrations(str(directory))
