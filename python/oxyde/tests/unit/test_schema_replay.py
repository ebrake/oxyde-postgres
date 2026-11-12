"""Replay parity over deterministic generated operations and legacy files."""

from __future__ import annotations

import json
import random
from copy import deepcopy
from types import SimpleNamespace

import pytest
from typer.testing import CliRunner

from oxyde.core import SchemaReplay, migration_to_sql
from oxyde.migrations.replay import SchemaState
from oxyde.migrations.utils import load_migration_module

from ._python_schema_state import SchemaState as _PythonSchemaState
from .replay_parity import assert_migration_parity, canonical_snapshot


def _generated_migration(seed):
    rng = random.Random(seed)

    def upgrade(ctx):
        ctx.create_enum_type("status", ["draft"])
        ctx.create_table(
            "items",
            [
                {
                    "nullable": True,
                    "unique": False,
                    "name": "id",
                    "column_type": {"kind": "big_integer"},
                    "primary_key": True,
                },
                {
                    "primary_key": False,
                    "unique": False,
                    "name": "value",
                    "column_type": {"kind": "text"},
                    "nullable": True,
                },
                {
                    "nullable": True,
                    "primary_key": False,
                    "unique": False,
                    "name": "status",
                    "column_type": {
                        "kind": "array",
                        "item": {
                            "kind": "array",
                            "item": {
                                "kind": "enum",
                                "name": "status",
                                "values": ["draft"],
                            },
                        },
                    },
                },
            ],
        )
        ctx.create_table(
            "parent",
            [
                {
                    "nullable": True,
                    "primary_key": False,
                    "unique": False,
                    "name": "id",
                    "column_type": {"kind": "big_integer"},
                }
            ],
        )
        ctx.create_index(
            "items",
            {
                "unique": False,
                "name": "idx",
                "fields": ["value"],
                "where": "  value IS NOT NULL  ",
            },
        )
        ctx.add_foreign_key("items", "fk", ["id"], "parent", ["id"])
        ctx.add_check("items", "ck", "id > 0")
        ctx.add_column(
            "items",
            {
                "nullable": True,
                "primary_key": False,
                "unique": False,
                "name": "temporary",
                "column_type": {"kind": "boolean"},
            },
        )
        ctx.rename_column("items", "value", "label")
        for _ in range(12):
            ctx.alter_column(
                "items",
                "label",
                **rng.choice(
                    [
                        {"nullable": True, "default": "'a'"},
                        {"nullable": False, "default": None},
                        {"unique": True, "nulls_not_distinct": True},
                        {"unique": True, "nulls_not_distinct": False},
                        {"unique": False, "max_length": 50},
                        {"column_type": {"kind": "string", "length": 80}},
                        {"column_type": {"kind": "text"}, "db_type": None},
                    ]
                ),
            )
        ctx.add_enum_value("status", "published")
        ctx.add_enum_value("status", "published")
        ctx.alter_enum_type(
            "status", ["draft", "published"], ["published", "draft", "archived"]
        )
        ctx.drop_check("items", "ck")
        ctx.drop_foreign_key("items", "fk")
        ctx.drop_index("items", "idx")
        ctx.drop_column("items", "temporary")
        ctx.rename_table("items", "renamed")
        ctx.execute("SELECT 1")
        ctx.drop_table("parent")
        ctx.drop_enum_type("unused")

    # Each execute/collect run must see the same generated choices.
    def deterministic(ctx):
        rng.seed(seed)
        upgrade(ctx)

    return SimpleNamespace(upgrade=deterministic)


@pytest.mark.parametrize("seed", range(40))
def test_generated_sequences(seed):
    assert_migration_parity(_generated_migration(seed))


def test_drop_column_takes_its_indexes_with_it(tmp_path):
    """A migration that drops only the column replays like PostgreSQL runs it.

    PostgreSQL drops every index that uses a dropped column. This is outside
    the differential corpus on purpose: the frozen 0.2.2 reference keeps those
    indexes, and generated migrations always drop them first.
    """
    from oxyde.migrations.replay import replay_migrations

    (tmp_path / "0001_create.py").write_text(
        """def upgrade(ctx):
    ctx.create_table("users", [
        {"name": "id", "column_type": {"kind": "big_integer"}, "primary_key": True, "nullable": False, "unique": False},
        {"name": "email", "column_type": {"kind": "text"}, "primary_key": False, "nullable": True, "unique": False},
    ])
    ctx.create_index("users", {"name": "ix_email", "fields": ["email"], "unique": False})
    ctx.create_index("users", {"name": "ix_id_email", "fields": ["id", "email"], "unique": False})
    ctx.create_index("users", {"name": "ix_lower", "unique": False, "fields": [
        {"expr": {"type": "func", "name": "lower", "args": [{"type": "column", "name": "email"}]}},
    ]})
    ctx.create_index("users", {"name": "ix_id", "fields": ["id"], "unique": False})
"""
    )
    (tmp_path / "0002_drop_email.py").write_text(
        """depends_on = "0001_create"
def upgrade(ctx):
    ctx.drop_column("users", "email")
"""
    )

    users = replay_migrations(str(tmp_path))["tables"]["users"]
    assert [field["name"] for field in users["fields"]] == ["id"]
    assert [index["name"] for index in users["indexes"]] == ["ix_id"]


@pytest.mark.parametrize("type_key", ["type", "python_type"])
def test_legacy_migration_files(tmp_path, type_key):
    sources = [
        """def upgrade(ctx):
    ctx.create_table("old", [
        {"name": "id", "python_type": "int", "primary_key": True, "nullable": False, "unique": False},
        {"name": "price", "python_type": "Decimal", "max_digits": 10, "decimal_places": 2, "nullable": True, "primary_key": False, "unique": False},
        {"name": "name", "python_type": "str", "nullable": True, "max_length": 20, "primary_key": False, "unique": False},
    ])
    ctx.create_index("old", {"name": "idx", "fields": ["name"], "unique": False})
""",
        f"""depends_on = "0001_legacy"
def upgrade(ctx):
    ctx.alter_column("old", "name", {type_key}="str", max_length=64)
    ctx.alter_column("old", "price", max_digits=12, decimal_places=4)
    ctx.alter_column("old", "name", db_type="VARCHAR(80)", nullable=False)
    ctx.rename_table("old", "new")
    ctx.alter_column("new", "name", column_type={{"kind": "text"}}, db_type=None)
    ctx.add_column("new", {{"name": "extra", "python_type": "str", "nullable": True, "primary_key": False, "unique": False}})
    ctx.drop_column("new", "extra")
""",
    ]
    snapshot = None
    for number, source in enumerate(sources, 1):
        path = tmp_path / f"{number:04}_legacy.py"
        path.write_text(source)
        snapshot = assert_migration_parity(load_migration_module(path), snapshot)


def test_missing_keys_and_noop_targets():
    operations = [
        {
            "type": "create_table",
            "table": {
                "indexes": [],
                "name": "t",
                "fields": [
                    {
                        "nullable": True,
                        "primary_key": False,
                        "unique": False,
                        "name": "x",
                        "python_type": "str",
                        "custom_metadata": 123,
                    },
                ],
            },
        },
        {
            "type": "create_index",
            "table": "t",
            "index": {"unique": False, "name": "idx", "fields": ["x"]},
        },
        {
            "type": "add_foreign_key",
            "table": "t",
            "fk": {
                "name": "fk",
                "columns": ["x"],
                "ref_table": "other",
                "ref_columns": ["x"],
            },
        },
        {
            "type": "add_check",
            "table": "t",
            "check": {"name": "ck", "expression": "x <> ''"},
        },
        {
            "type": "alter_column",
            "table": "t",
            "column": "absent",
            "changes": {"nullable": True},
        },
        {"type": "drop_table", "name": "absent"},
    ]
    for kind in ("drop_index", "drop_foreign_key", "drop_check"):
        operations.append({"type": kind, "table": "absent", "name": "absent"})
    python, rust = _PythonSchemaState(), SchemaState()
    for op in operations:
        python.apply_operation(deepcopy(op))
        rust.apply_operation(deepcopy(op))
    assert rust.to_snapshot() == canonical_snapshot(python.to_snapshot())


def test_cache_and_snapshot_independence():
    state = SchemaState()
    state.apply_operation(
        {"type": "create_table", "table": {"name": "t", "fields": [], "indexes": []}}
    )
    first = state.tables
    assert state.tables == first
    assert state.tables is not first
    snapshot = state.to_snapshot()
    snapshot["tables"]["t"]["fields"].append({"name": "unrelated"})
    assert state.tables["t"]["fields"] == []
    state.apply_operation({"type": "rename_table", "old_name": "t", "new_name": "u"})
    assert state.tables is not first
    assert "u" in state.tables


def test_binding_resolved_and_collect_alterations():
    replay = SchemaReplay()
    replay.apply(
        json.dumps(
            {
                "type": "create_table",
                "table": {
                    "indexes": [],
                    "name": "t",
                    "fields": [
                        {
                            "primary_key": False,
                            "unique": False,
                            "name": "x",
                            "column_type": {"kind": "text"},
                            "nullable": True,
                        }
                    ],
                },
            }
        )
    )
    collect = {
        "type": "alter_column",
        "table": "t",
        "column": "x",
        "changes": {"nullable": False},
    }
    resolved = json.loads(replay.resolve(json.dumps(collect)))
    assert resolved["old_field"]["nullable"] is True
    assert resolved["new_field"]["nullable"] is False
    assert migration_to_sql(json.dumps([resolved]), "postgres") == [
        'ALTER TABLE "t" ALTER COLUMN "x" SET NOT NULL'
    ]
    replay.apply(json.dumps(resolved))
    assert (
        json.loads(replay.snapshot_json())["tables"]["t"]["fields"][0]["nullable"]
        is False
    )
    with pytest.raises(ValueError, match="Unknown column"):
        replay.resolve(json.dumps({**collect, "column": "missing"}))
    with pytest.raises(RuntimeError, match="resolved against schema"):
        migration_to_sql(json.dumps([collect]), "postgres")


def test_sqlmigrate_resolves_history_and_intra_migration_changes(tmp_path, monkeypatch):
    from oxyde.cli import migrations
    from oxyde.cli.migrations import app

    (tmp_path / "0001_initial.py").write_text("""def upgrade(ctx):
    ctx.create_table("items", [{"name": "value", "python_type": "str", "nullable": True, "primary_key": False, "unique": False}])
""")
    (tmp_path / "0002_change.py").write_text("""depends_on = "0001_initial"
def upgrade(ctx):
    ctx.rename_table("items", "renamed")
    ctx.alter_column("renamed", "value", nullable=False)
    ctx.alter_column("renamed", "value", nullable=True)
""")
    # A lexically earlier file depends on the target: replay must use dependency order.
    (tmp_path / "0000_future.py").write_text("""depends_on = "0002_change"
def upgrade(ctx):
    ctx.drop_table("renamed")
""")
    monkeypatch.setattr(
        migrations,
        "load_config_or_exit",
        lambda: SimpleNamespace(migrations_dir=str(tmp_path), dialect="postgres"),
    )
    result = CliRunner().invoke(app, ["sqlmigrate", "0002_change"])
    assert result.exit_code == 0, result.output
    assert 'RENAME TO "renamed"' in result.output
    assert 'ALTER COLUMN "value" SET NOT NULL' in result.output
    assert 'ALTER COLUMN "value" DROP NOT NULL' in result.output


def _integration_fixture_cases():
    """Enumerate the integration suite's migration-building setups.

    Run their synchronous setup and migration writers without database actions.
    This keeps every existing fixture (and its parameter combinations) in the
    differential corpus even on machines without PostgreSQL 18.
    """
    import ast
    import inspect
    from itertools import product

    from oxyde.tests.integration import test_migrations_e2e as fixtures

    tree = ast.parse(inspect.getsource(fixtures))
    for cls in tree.body:
        if not isinstance(cls, ast.ClassDef):
            continue
        for method in cls.body:
            if not isinstance(method, ast.AsyncFunctionDef):
                continue
            writes = [
                node
                for node in method.body
                if isinstance(node, ast.Expr)
                and isinstance(node.value, ast.Call)
                and isinstance(node.value.func, ast.Name)
                and node.value.func.id == "_write_migration"
            ]
            if not writes:
                continue
            marks = getattr(
                getattr(getattr(fixtures, cls.name), method.name), "pytestmark", []
            )
            params = [mark.args for mark in marks if mark.name == "parametrize"]
            for values in product(*(choices for _, choices in params)):
                arguments = dict(zip((name for name, _ in params), values))
                body = [
                    node
                    for node in method.body
                    if isinstance(node, (ast.Assign, ast.ClassDef)) or node in writes
                ]
                # Every writer must be retained: fail if fixtures evolve beyond
                # this deliberately small setup-only extraction.
                all_writes = [
                    node
                    for node in ast.walk(method)
                    if isinstance(node, ast.Call)
                    and isinstance(node.func, ast.Name)
                    and node.func.id == "_write_migration"
                ]
                assert len(writes) == len(all_writes)
                source = ast.Module(body=body, type_ignores=[])
                label = f"{cls.name}.{method.name}-{arguments}"
                yield pytest.param(fixtures, source, arguments, len(writes), id=label)


@pytest.mark.parametrize(
    "fixtures,source,arguments,expected", list(_integration_fixture_cases())
)
def test_existing_integration_migration_fixtures(
    tmp_path, fixtures, source, arguments, expected
):
    import ast

    namespace = {
        **vars(fixtures),
        **arguments,
        "empty_db": (None, "postgres"),
        "tmp_path": tmp_path,
    }
    exec(
        compile(ast.fix_missing_locations(source), fixtures.__file__, "exec"), namespace
    )
    assert len(list(tmp_path.glob("[0-9]*.py"))) == expected
    from .replay_parity import assert_fixture_histories

    assert_fixture_histories(tmp_path)


def test_existing_legacy_history_fixtures(tmp_path):
    from .replay_parity import assert_fixture_histories
    from .test_migrations_squash import LEGACY_0001, LEGACY_0002, LEGACY_IDENTITY

    for filename, source in [
        ("0001_users.py", LEGACY_0001),
        ("0002_age.py", LEGACY_0002),
        ("0003_identity.py", LEGACY_IDENTITY),
    ]:
        (tmp_path / filename).write_text(source)
    assert_fixture_histories(tmp_path)


def test_enum_field_references_accept_legacy_fields():
    state = SchemaState()
    state.apply_operation(
        {
            "type": "create_table",
            "table": {
                "indexes": [],
                "name": "t",
                "fields": [
                    {
                        "nullable": True,
                        "primary_key": False,
                        "unique": False,
                        "name": "state",
                        "column_type": {
                            "kind": "enum",
                            "name": "status",
                            "values": ["draft"],
                        },
                    }
                ],
            },
        }
    )
    op = {
        "indexes": [],
        "type": "add_enum_value",
        "name": "status",
        "value": "published",
        "fields": [
            {
                "table": "t",
                "field": {
                    "nullable": True,
                    "primary_key": False,
                    "unique": False,
                    "name": "state",
                    "python_type": "str",
                },
            }
        ],
    }
    resolved = state.resolve_operation(op)
    assert resolved["fields"][0]["field"]["column_type"] == {"kind": "string"}
    state.apply_operation(op)
    assert state.tables["t"]["fields"][0]["column_type"]["values"] == [
        "draft",
        "published",
    ]


def test_nested_enum_rewrites_preserve_spec_metadata():
    python, rust = _PythonSchemaState(), SchemaState()
    operations = [
        {
            "type": "create_table",
            "table": {
                "indexes": [],
                "name": "t",
                "fields": [
                    {
                        "nullable": True,
                        "primary_key": False,
                        "unique": False,
                        "name": "states",
                        "column_type": {
                            "kind": "array",
                            "dimensions": 2,
                            "item": {
                                "kind": "array",
                                "item": {
                                    "kind": "enum",
                                    "name": "status",
                                    "values": [],
                                },
                            },
                        },
                    }
                ],
            },
        },
        {"type": "add_enum_value", "name": "status", "value": "draft"},
        {
            "type": "alter_enum_type",
            "name": "status",
            "old_values": ["draft"],
            "new_values": [],
        },
    ]
    for op in operations:
        python.apply_operation(deepcopy(op))
        rust.apply_operation(deepcopy(op))
    assert rust.to_snapshot() == canonical_snapshot(python.to_snapshot())
