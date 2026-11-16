"""Declared extensions across models, replay, generation, and table helpers."""

from __future__ import annotations

import json
from unittest.mock import AsyncMock, Mock

import pytest

from oxyde import AsyncDatabase, Field, Index, Model
from oxyde.core import migration_compute_diff, migration_to_sql
from oxyde.db.schema import create_tables, drop_tables
from oxyde.migrations.context import MigrationContext
from oxyde.migrations.extract import extract_current_schema
from oxyde.migrations.generator import generate_migration_file
from oxyde.migrations.replay import SchemaState, replay_migrations
from oxyde.migrations.squash import squash_migrations
from oxyde.migrations.utils import load_migration_module


def extension_model(required=("pg_trgm",), *, trigram=False):
    class Doc(Model):
        id: int | None = Field(default=None, db_pk=True)
        title: str

        class Meta:
            is_table = True
            table_name = "extension_docs"
            extensions = required
            indexes = (
                [
                    Index(
                        ("title",),
                        method="gin",
                        opclasses=("gin_trgm_ops",),
                        name="ix_extension_docs_title",
                    )
                ]
                if trigram
                else []
            )

    return Doc


@pytest.mark.parametrize("value", ["pg_trgm", None, {"pg_trgm"}, 42, ("pg_trgm", 1)])
def test_meta_extensions_rejects_invalid_shapes(value):
    with pytest.raises(TypeError, match="Meta.extensions"):
        extension_model(value)


def test_meta_extensions_deduplicates_and_snapshot_sorts_union():
    doc = extension_model(["uuid-ossp", "pg_trgm", "uuid-ossp"])
    assert doc._db_meta.extensions == ("uuid-ossp", "pg_trgm")
    assert "extensions" not in doc._db_meta.extra

    class Other(Model):
        id: int = Field(db_pk=True)

        class Meta:
            is_table = True
            extensions = ("hstore", "pg_trgm")

    assert extract_current_schema()["extensions"] == ["hstore", "pg_trgm", "uuid-ossp"]


@pytest.mark.parametrize("required", [(), ("pg_trgm",)])
def test_extensions_on_non_table_model_is_an_error(required):
    with pytest.raises(TypeError, match="table model"):

        class Abstract(Model):
            class Meta:
                extensions = required


def test_no_extension_key_when_not_declared():
    extension_model(())
    assert "extensions" not in extract_current_schema()


@pytest.mark.parametrize(
    "name", ["", "PG_TRGM", 'bad"name', "pg trgm", "1bad", "a" * 64]
)
def test_rust_validates_extension_names_at_class_definition(name):
    with pytest.raises(
        ValueError, match=r"Doc\.Meta\.extensions: invalid extension name"
    ):
        extension_model((name,))


def test_known_extension_is_inferred_during_extraction():
    extension_model((), trigram=True)
    assert extract_current_schema()["extensions"] == ["pg_trgm"]


def test_upgrade_from_032_manual_extension_history(tmp_path):
    extension_model((), trigram=True)
    target = extract_current_schema()
    legacy = {key: value for key, value in target.items() if key != "extensions"}
    ops = json.loads(
        migration_compute_diff(
            json.dumps({"version": 1, "tables": {}}), json.dumps(legacy)
        )
    )
    path = generate_migration_file(ops, migrations_dir=tmp_path, name="legacy")
    source = path.read_text()
    source = source.replace(
        "def upgrade(ctx):",
        'def upgrade(ctx):\n    ctx.execute("CREATE EXTENSION IF NOT EXISTS pg_trgm")',
    )
    assert "ctx.execute" in source
    path.write_text(source)
    replayed = replay_migrations(str(tmp_path))
    assert "extensions" not in replayed
    assert json.loads(
        migration_compute_diff(json.dumps(replayed), json.dumps(target))
    ) == [{"type": "ensure_extension", "name": "pg_trgm"}]


@pytest.mark.parametrize(
    "method", ["ensure_extension", "remove_extension_requirement", "drop_extension"]
)
def test_extension_context_validates_names_before_collecting(method):
    ctx = MigrationContext(mode="collect")
    with pytest.raises(ValueError, match="invalid extension name"):
        getattr(ctx, method)("BAD")
    assert ctx.get_collected_operations() == []


@pytest.mark.parametrize(
    "db_type,expected",
    [
        ("vector(3)", "vector"),
        ("public.hstore", "hstore"),
        ("citext[]", "citext"),
        ('"geometry"(Point,4326)', "postgis"),
    ],
)
def test_extension_types_are_inferred_during_extraction(db_type, expected):
    class Custom(Model):
        id: int = Field(db_pk=True)
        value: str = Field(db_type=db_type)

        class Meta:
            is_table = True

    assert extract_current_schema()["extensions"] == [expected]


@pytest.mark.parametrize("db_type", ['"Vector"', 'public."Vector"', '"my.vector"'])
def test_quoted_custom_types_do_not_infer_extensions(db_type):
    class Custom(Model):
        id: int = Field(db_pk=True)
        value: str = Field(db_type=db_type)

        class Meta:
            is_table = True

    snapshot = extract_current_schema()
    assert snapshot.get("extensions", []) == []


@pytest.mark.parametrize(
    "ops,expected",
    [
        ([{"type": "ensure_extension", "name": "uuid-ossp"}], "extension_uuid_ossp"),
        (
            [
                {"type": "ensure_extension", "name": "pg_trgm"},
                {
                    "type": "create_table",
                    "table": {
                        "name": "docs",
                        "fields": [],
                        "indexes": [],
                        "foreign_keys": [],
                        "checks": [],
                    },
                },
            ],
            "create_docs_table",
        ),
        (
            [
                {"type": "ensure_extension", "name": "pg_trgm"},
                {
                    "type": "create_index",
                    "table": "docs",
                    "index": {"name": "ix", "fields": ["title"], "unique": False},
                },
            ],
            "migration",
        ),
        (
            [{"type": "add_enum_value", "name": "Status", "value": "in progress/a"}],
            "add_in_progress_a_to_status_enum",
        ),
    ],
)
def test_generated_migration_names_are_safe_and_skip_extensions(
    tmp_path, ops, expected
):
    path = generate_migration_file(ops, migrations_dir=tmp_path)
    assert path.name == f"0001_{expected}.py"
    assert load_migration_module(path) is not None


@pytest.mark.parametrize(
    "kind,reverse",
    [
        ("ensure_extension", "remove_extension_requirement"),
        ("remove_extension_requirement", "ensure_extension"),
        ("drop_extension", "ensure_extension"),
    ],
)
def test_extension_generation_and_downgrade(tmp_path, kind, reverse):
    path = generate_migration_file(
        [{"type": kind, "name": "pg_trgm"}], migrations_dir=tmp_path
    )
    assert "extension_pg_trgm" in path.name
    module = load_migration_module(path)
    assert module is not None
    up, down = MigrationContext(mode="collect"), MigrationContext(mode="collect")
    module.upgrade(up)
    module.downgrade(down)
    assert up.get_collected_operations() == [{"type": kind, "name": "pg_trgm"}]
    assert down.get_collected_operations() == [{"type": reverse, "name": "pg_trgm"}]
    if reverse == "remove_extension_requirement":
        assert (
            migration_to_sql(json.dumps(down.get_collected_operations()), "postgres")
            == []
        )


def test_squash_preserves_extension_requirements(tmp_path):
    extension_model(trigram=True)
    target = extract_current_schema()
    ops = json.loads(
        migration_compute_diff(
            json.dumps({"version": 1, "tables": {}}), json.dumps(target)
        )
    )
    generate_migration_file(ops, migrations_dir=tmp_path, name="initial")
    result = squash_migrations(tmp_path)
    assert result.new_file is not None
    replayed = replay_migrations(str(tmp_path))
    assert replayed["extensions"] == ["pg_trgm"]
    assert (
        json.loads(migration_compute_diff(json.dumps(replayed), json.dumps(target)))
        == []
    )
    assert "ctx.ensure_extension('pg_trgm')" in result.new_file.read_text()


def test_extension_execute_context_updates_replay_without_sql_for_removal():
    state = SchemaState()
    ctx = MigrationContext(schema_state=state)
    ctx.ensure_extension("pg_trgm")
    assert state.to_snapshot()["extensions"] == ["pg_trgm"]
    ctx.remove_extension_requirement("pg_trgm")
    assert "extensions" not in state.to_snapshot()
    ctx.ensure_extension("pg_trgm")
    ctx.drop_extension("pg_trgm")
    assert "extensions" not in state.to_snapshot()


@pytest.mark.asyncio
async def test_create_tables_installs_extensions_first_and_drop_preserves_them(
    monkeypatch,
):
    extension_model(trigram=True)
    execute = AsyncMock()
    monkeypatch.setattr("oxyde.db.schema.execute_raw", execute)
    database = Mock(spec=AsyncDatabase)
    database.url = "postgresql://localhost/test"
    await create_tables(database)
    statements = [call.args[0] for call in execute.await_args_list]
    assert statements[0] == 'CREATE EXTENSION IF NOT EXISTS "pg_trgm"'
    assert statements[1].startswith('CREATE TABLE "extension_docs"')
    assert any("gin_trgm_ops" in sql for sql in statements[2:])
    execute.reset_mock()
    await drop_tables(database)
    execute.assert_awaited_once_with(
        "DROP TABLE IF EXISTS extension_docs CASCADE", client=database
    )
