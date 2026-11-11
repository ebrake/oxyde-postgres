"""Regression coverage for the Rust replay review findings."""

import json
from types import SimpleNamespace
from unittest.mock import Mock

import pytest
from typer.testing import CliRunner

from oxyde.core import migration_compute_diff, migration_to_sql
from oxyde.migrations.context import MigrationContext
from oxyde.migrations.replay import SchemaState, replay_migrations
from oxyde.migrations.utils import normalize_op_fields


def field(name="x", **changes):
    return dict(
        name=name,
        column_type={"kind": "text"},
        nullable=True,
        primary_key=False,
        unique=False,
        **changes,
    )


def create(state, fields=None, name="t"):
    state.apply_operation(
        {
            "type": "create_table",
            "table": {"name": name, "fields": fields or [field()], "indexes": []},
        }
    )


def test_serialized_diff_member_declaration_order():
    state = SchemaState()
    create(state)
    ops = json.loads(
        migration_compute_diff(
            json.dumps({"version": 1, "tables": {}}), json.dumps(state.to_snapshot())
        )
    )
    member = ops[0]["table"]["fields"][0]
    assert next(iter(member)) == "name"
    assert next(iter(member["column_type"])) == "kind"


@pytest.mark.parametrize("target", ["sql", "replay"])
def test_missing_required_nullable_rejected(target):
    member = field()
    del member["nullable"]
    op = {
        "type": "create_table",
        "table": {"name": "t", "fields": [member], "indexes": []},
    }
    with pytest.raises(ValueError, match="nullable"):
        if target == "sql":
            migration_to_sql(json.dumps([op]), "postgres")
        else:
            SchemaState().apply_operation(op)


def test_enum_values_required():
    with pytest.raises(ValueError, match="values"):
        create(
            SchemaState(),
            [{**field(), "column_type": {"kind": "enum", "name": "status"}}],
        )


def test_truthy_nulls_not_distinct():
    ctx = MigrationContext(mode="execute")
    ctx.create_table("t", [field()])
    ctx.alter_column("t", "x", unique=True, nulls_not_distinct=1)
    assert any("UNIQUE NULLS NOT DISTINCT" in sql for sql in ctx._sql_statements)


def test_partial_enum_reference_normalization():
    op = {
        "type": "add_enum_value",
        "name": "status",
        "value": "done",
        "fields": [{"table": "t"}],
    }
    assert normalize_op_fields(op) == op


def test_tables_are_independent_read_only_snapshots():
    state = SchemaState()
    create(state)
    state.tables["t"]["fields"].clear()
    assert len(state.tables["t"]["fields"]) == 1
    with pytest.raises(AttributeError):
        setattr(state, "tables", {})


def test_missing_core_is_clear_and_context_is_lazy(monkeypatch):
    import oxyde.core

    monkeypatch.setattr(oxyde.core, "SchemaReplay", Mock())
    MigrationContext(mode="collect")
    MigrationContext(mode="execute")
    with pytest.raises(RuntimeError, match=r"0.3.*core>=0.3.0"):
        SchemaState()


def test_alterations_do_not_serialize_snapshots():
    state = SchemaState()
    create(state)
    replay = state._replay

    class Spy:
        def __getattr__(self, name):
            assert name != "snapshot_json", "alter_column serialized the whole schema"
            return getattr(replay, name)

    state._replay = Spy()
    for length in range(10):
        state.apply_operation(
            {
                "type": "alter_column",
                "table": "t",
                "column": "x",
                "changes": {"max_length": length},
            }
        )


def test_legacy_type_rederivation_and_canonical_override():
    state = SchemaState()
    legacy = {k: v for k, v in field().items() if k != "column_type"}
    create(state, [{**legacy, "python_type": "str", "max_length": 50}])

    def alter(**changes):
        state.apply_operation(
            {"type": "alter_column", "table": "t", "column": "x", "changes": changes}
        )
        return state.to_snapshot()["tables"]["t"]["fields"][0]["column_type"]

    assert alter(max_length=100) == {"kind": "string", "length": 100}
    assert alter(column_type={"kind": "text"}) == {"kind": "text"}
    assert alter(max_length=120) == {"kind": "text"}
    assert alter(type="str") == {"kind": "string", "length": 120}


def test_history_error_locates_file_operation_and_type(tmp_path):
    (tmp_path / "0001_bad.py").write_text(
        'def upgrade(ctx):\n    ctx.drop_table("missing")\n    ctx.add_column("t", {"name": "bad"})\n'
    )
    with pytest.raises(ValueError, match=r"0001_bad.py: operation 2 \(add_column\)"):
        replay_migrations(str(tmp_path))


def test_sqlmigrate_without_alter_does_not_replay(tmp_path, monkeypatch):
    import oxyde.cli.migrations as cli
    import oxyde.migrations.executor as executor

    (tmp_path / "0001_sql.py").write_text(
        'def upgrade(ctx):\n    ctx.drop_table("t")\n'
    )
    monkeypatch.setattr(
        cli,
        "load_config_or_exit",
        lambda: SimpleNamespace(migrations_dir=str(tmp_path), dialect="postgres"),
    )
    monkeypatch.setattr(
        executor,
        "replay_migrations_up_to",
        Mock(side_effect=AssertionError("unnecessary replay")),
    )
    result = CliRunner().invoke(cli.app, ["sqlmigrate", "0001_sql"])
    assert result.exit_code == 0, result.output


def test_sqlmigrate_missing_column_matches_execute_fallback(tmp_path, monkeypatch):
    import oxyde.cli.migrations as cli

    (tmp_path / "0001_alter.py").write_text(
        'def upgrade(ctx):\n    ctx.alter_column("untracked", "x", nullable=False)\n'
    )
    monkeypatch.setattr(
        cli,
        "load_config_or_exit",
        lambda: SimpleNamespace(migrations_dir=str(tmp_path), dialect="postgres"),
    )
    ctx = MigrationContext(mode="execute")
    ctx.alter_column("untracked", "x", nullable=False)
    result = CliRunner().invoke(cli.app, ["sqlmigrate", "0001_alter"])
    assert result.exit_code == 0, result.output
    assert all(sql in result.output for sql in ctx._sql_statements)


def test_rename_then_alter_uses_current_state():
    ctx = MigrationContext(mode="execute")
    ctx.create_table("t", [{**field(), "nullable": False}])
    ctx.rename_column("t", "x", "renamed")
    ctx.alter_column("t", "renamed", nullable=True)
    assert (
        ctx._sql_statements[-1]
        == 'ALTER TABLE "t" ALTER COLUMN "renamed" DROP NOT NULL'
    )


def legacy_field(name="x"):
    member = field(name)
    del member["column_type"]
    return {**member, "python_type": "str", "max_length": 50}


def alter(state, table="t", column="x", **changes):
    state.apply_operation(
        {"type": "alter_column", "table": table, "column": column, "changes": changes}
    )


def test_legacy_tracking_rekeys_and_removes_fields():
    state = SchemaState()
    create(state, [legacy_field()])
    state.apply_operation({"type": "rename_table", "old_name": "t", "new_name": "u"})
    state.apply_operation(
        {"type": "rename_column", "table": "u", "old_name": "x", "new_name": "y"}
    )
    alter(state, "u", "y", max_length=100)
    member = state.get_field("u", "y")
    assert member is not None
    assert member["column_type"] == {"kind": "string", "length": 100}
    assert set(state._legacy) == {("u", "y")}
    state.apply_operation({"type": "drop_column", "table": "u", "field": "y"})
    assert not state._legacy
    state.apply_operation({"type": "add_column", "table": "u", "field": legacy_field()})
    state.apply_operation({"type": "drop_table", "name": "u"})
    assert not state._legacy


def test_legacy_tracking_ignores_missing_targets_and_failed_operations():
    state = SchemaState()
    state.apply_operation(
        {"type": "add_column", "table": "absent", "field": legacy_field()}
    )
    alter(state, "absent", type="str")
    state.apply_operation(
        {"type": "rename_column", "table": "absent", "old_name": "x", "new_name": "y"}
    )
    assert not state._legacy
    create(state, [legacy_field()])
    before = state.to_snapshot()
    provenance = dict(state._legacy)
    with pytest.raises(ValueError, match="expected a boolean"):
        alter(state, nullable="invalid", column_type={"kind": "text"})
    assert state.to_snapshot() == before
    assert state._legacy == provenance
    state.apply_operation(
        {"type": "rename_column", "table": "t", "old_name": "missing", "new_name": "x"}
    )
    state.apply_operation(
        {"type": "rename_table", "old_name": "missing", "new_name": "t"}
    )
    assert state._legacy == provenance


def test_replacing_table_clears_legacy_status():
    state = SchemaState()
    create(state, [legacy_field()])
    create(state)
    alter(state, max_length=100)
    member = state.get_field("t", "x")
    assert member is not None
    assert member["column_type"] == {"kind": "text"}
    assert not state._legacy


def test_rename_table_overwrite_clears_destination_legacy_status():
    state = SchemaState()
    create(state, [legacy_field()], name="destination")
    create(state, name="source")
    state.apply_operation(
        {"type": "rename_table", "old_name": "source", "new_name": "destination"}
    )
    alter(state, "destination", max_length=100)
    member = state.get_field("destination", "x")
    assert member is not None
    assert member["column_type"] == {"kind": "text"}
    assert not state._legacy


def test_execute_legacy_alterations_keep_deriving_types():
    ctx = MigrationContext(mode="execute")
    ctx.create_table("t", [legacy_field()])
    ctx.alter_column("t", "x", max_length=100)
    ctx.alter_column("t", "x", max_length=120)
    assert ctx._schema_state is not None
    member = ctx._schema_state.get_field("t", "x")
    assert member is not None
    assert member["column_type"] == {
        "kind": "string",
        "length": 120,
    }
    assert "TYPE VARCHAR(120)" in ctx._sql_statements[-1]


@pytest.mark.parametrize("entry", ["replay", "executor", "squash"])
def test_located_errors_shared_by_replay_callers(tmp_path, entry):
    from oxyde.migrations.executor import replay_migrations_up_to
    from oxyde.migrations.replay import MigrationReplayError
    from oxyde.migrations.squash import squash_migrations

    bad = tmp_path / "0001_bad.py"
    bad.write_text('def upgrade(ctx):\n    ctx.add_column("t", {"name": "bad"})\n')
    with pytest.raises(
        MigrationReplayError,
        match=r"0001_bad.py: operation 1 \(add_column\).+Edit this historical",
    ):
        if entry == "replay":
            replay_migrations(str(tmp_path))
        elif entry == "executor":
            replay_migrations_up_to(str(tmp_path), bad, include_target=True)
        else:
            squash_migrations(tmp_path)
    assert bad.exists()


@pytest.mark.parametrize(
    "command", ["makemigrations", "migrate", "sqlmigrate", "squash"]
)
def test_cli_reports_replay_errors_without_traceback(tmp_path, monkeypatch, command):
    from unittest.mock import AsyncMock
    import oxyde.cli.migrations as cli
    import oxyde.migrations as migrations
    from oxyde.migrations.replay import MigrationReplayError

    (tmp_path / "0001_bad.py").write_text(
        'def upgrade(ctx):\n    ctx.add_column("t", {"name": "bad"})\n'
    )
    (tmp_path / "0002_alter.py").write_text(
        'depends_on = "0001_bad"\ndef upgrade(ctx):\n    ctx.alter_column("t", "x", nullable=False)\n'
    )
    config = SimpleNamespace(
        migrations_dir=str(tmp_path),
        dialect="postgres",
        models=["models"],
        databases={"default": "postgresql://unused"},
    )
    monkeypatch.setattr(cli, "load_config_or_exit", lambda: config)
    monkeypatch.setattr(cli, "import_models", lambda modules: 1)
    monkeypatch.setattr(
        migrations,
        "extract_current_schema",
        lambda **kwargs: {"version": 1, "tables": {}},
    )
    monkeypatch.setattr(
        cli,
        "init_databases",
        AsyncMock(
            side_effect=MigrationReplayError(
                "0001_bad.py: operation 1 (add_column): bad field. Edit this historical migration file."
            )
        ),
    )
    args = {
        "makemigrations": ["makemigrations"],
        "migrate": ["migrate"],
        "sqlmigrate": ["sqlmigrate", "0002_alter"],
        "squash": ["migrations", "squash", "--yes"],
    }[command]
    result = CliRunner().invoke(cli.app, args, color=True)
    assert result.exit_code == 1, result.output
    assert "0001_bad.py: operation 1 (add_column)" in result.output
    assert "Traceback" not in result.output
    assert "\x1b[31m" in result.output
