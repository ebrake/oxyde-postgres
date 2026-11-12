"""End-to-end migration pipeline tests (without a real database).

Flow per test:
    Model(before) → extract_current_schema → old_snapshot
    Model(after)  → extract_current_schema → new_snapshot
    migration_compute_diff → operations
    generate_migration_file → on-disk .py
    load_migration_module + ctx.upgrade(collect) → ops replayed
    migration_to_sql → SQL for each dialect

Catches composition bugs that unit layers miss — e.g. the drop_fk bug
where Python/Rust payload shapes diverged silently.
"""

from __future__ import annotations

import json
from copy import deepcopy
from decimal import Decimal
from enum import Enum
from pathlib import Path

import pytest

from oxyde import Field, Model
from oxyde.core import migration_compute_diff, migration_to_sql
from oxyde.migrations.context import MigrationContext
from oxyde.migrations.extract import extract_current_schema
from oxyde.migrations.generator import generate_migration_file
from oxyde.migrations.replay import SchemaState, replay_migrations
from oxyde.migrations.utils import load_migration_module
from oxyde.models.registry import clear_registry

from .replay_parity import assert_migration_parity

ALL_DIALECTS = ["postgres"]
POSTGRES_DIALECTS = ["postgres"]
PARTIAL_INDEX_DIALECTS = ["postgres"]


class PublishState(Enum):
    DRAFT = "draft"
    PUBLISHED = "published"


def _snapshot_from_models(models: list[type[Model]], dialect: str) -> dict:
    """Register given models fresh and return their schema snapshot."""
    clear_registry()
    for model in models:
        from oxyde.models.registry import register_table

        register_table(model, overwrite=True)
    return extract_current_schema(dialect=dialect)


def _run_pipeline(
    old_models: list[type[Model]],
    new_models: list[type[Model]],
    dialect: str,
    tmp_path: Path,
    name: str,
) -> tuple[list[dict], list[str], list[str]]:
    """Run full pipeline and return (ops, upgrade_sql, downgrade_sql)."""
    old = _snapshot_from_models(old_models, dialect)
    new = _snapshot_from_models(new_models, dialect)

    ops_json = migration_compute_diff(json.dumps(old), json.dumps(new))
    ops = json.loads(ops_json)
    assert ops, "diff produced no operations — test setup is wrong"

    filepath = generate_migration_file(ops, migrations_dir=tmp_path, name=name)
    module = load_migration_module(filepath)
    assert module is not None
    replayed = assert_migration_parity(module, old)
    from types import SimpleNamespace
    assert_migration_parity(SimpleNamespace(upgrade=module.downgrade), replayed)

    up_ctx = MigrationContext(mode="collect", dialect=dialect)
    module.upgrade(up_ctx)
    up_sql = migration_to_sql(
        json.dumps(up_ctx.get_collected_operations()), dialect
    )

    down_ctx = MigrationContext(mode="collect", dialect=dialect)
    module.downgrade(down_ctx)
    down_ops = down_ctx.get_collected_operations()
    # downgrade may contain a TODO comment instead of ops — only render if non-empty
    down_sql = (
        migration_to_sql(json.dumps(down_ops), dialect) if down_ops else []
    )

    return ops, up_sql, down_sql


def _enum_snapshot(values: list[str]) -> dict:
    return {
        "version": 1,
        "tables": {
            "articles": {
                "name": "articles",
                "fields": [
                    {
                        "name": "id",
                        "column_type": {"kind": "big_integer"},
                        "db_type": None,
                        "nullable": False,
                        "primary_key": True,
                        "unique": False,
                        "default": None,
                        "auto_increment": False,
                    },
                    {
                        "name": "state",
                        "column_type": {
                            "kind": "enum",
                            "name": "publish_state_enum",
                            "values": values,
                        },
                        "db_type": None,
                        "nullable": False,
                        "primary_key": False,
                        "unique": False,
                        "default": None,
                        "auto_increment": False,
                    },
                ],
                "indexes": [],
                "foreign_keys": [],
                "checks": [],
                "comment": None,
            }
        },
    }


class TestCreateTablePipeline:
    @pytest.mark.parametrize("dialect", ALL_DIALECTS)
    def test_create_table_from_scratch(self, tmp_path, dialect):
        class User(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str
            active: bool = True

            class Meta:
                is_table = True
                table_name = "users"

        ops, up_sql, down_sql = _run_pipeline(
            [], [User], dialect, tmp_path, "create_users"
        )

        assert any(op["type"] == "create_table" for op in ops)
        assert any("CREATE TABLE" in s.upper() and "users" in s for s in up_sql)
        assert any("DROP TABLE" in s.upper() and "users" in s for s in down_sql)


class TestEnumPipeline:
    @pytest.mark.parametrize("dialect", ALL_DIALECTS)
    def test_create_table_with_enum(self, tmp_path, dialect):
        class Article(Model):
            id: int | None = Field(default=None, db_pk=True)
            state: PublishState = Field(default=PublishState.DRAFT)

            class Meta:
                is_table = True
                table_name = "articles"

        ops, up_sql, down_sql = _run_pipeline(
            [], [Article], dialect, tmp_path, "create_articles"
        )

        assert [op["type"] for op in ops[:2]] == ["create_enum_type", "create_table"]
        assert up_sql[0] == (
            'CREATE TYPE "publish_state_enum" AS ENUM ('
            "'draft', 'published')"
        )
        assert any('"state" "publish_state_enum" NOT NULL' in s for s in up_sql)
        assert down_sql[-1] == 'DROP TYPE "publish_state_enum"'

    def test_add_enum_value_diff_emits_enum_op_only(self):
        old = _enum_snapshot(["draft", "published"])
        new = _enum_snapshot(["draft", "published", "archived"])

        ops_json = migration_compute_diff(json.dumps(old), json.dumps(new))
        ops = json.loads(ops_json)

        assert len(ops) == 1
        op = ops[0]
        assert op["type"] == "add_enum_value"
        assert op["name"] == "publish_state_enum"
        assert op["value"] == "archived"
        assert op["fields"][0]["table"] == "articles"
        assert op["fields"][0]["field"]["name"] == "state"
        assert migration_to_sql(ops_json, "postgres") == [
            'ALTER TYPE "publish_state_enum" ADD VALUE IF NOT EXISTS \'archived\''
        ]


    def test_enum_value_removal_requires_manual_migration(self):
        old = _enum_snapshot(["draft", "published"])
        new = _enum_snapshot(["draft"])

        ops_json = migration_compute_diff(json.dumps(old), json.dumps(new))
        ops = json.loads(ops_json)

        assert ops == [
            {
                "type": "alter_enum_type",
                "name": "publish_state_enum",
                "old_values": ["draft", "published"],
                "new_values": ["draft"],
            }
        ]

    def test_manual_enum_migration_file_contains_replay_marker(self, tmp_path):
        old = _enum_snapshot(["draft", "published"])
        new = _enum_snapshot(["draft"])
        ops = json.loads(migration_compute_diff(json.dumps(old), json.dumps(new)))

        empty = {"version": 1, "tables": {}}
        create_ops = json.loads(migration_compute_diff(json.dumps(empty), json.dumps(old)))
        generate_migration_file(
            create_ops,
            migrations_dir=tmp_path,
            name="create_articles",
        )
        filepath = generate_migration_file(
            ops,
            migrations_dir=tmp_path,
            name="alter_publish_state",
        )
        assert_migration_parity(load_migration_module(filepath), old)
        content = filepath.read_text()

        assert "Manual enum migration required for publish_state_enum" in content
        assert "ctx.alter_enum_type(" in content
        assert "ctx.require_manual(" in content
        assert "raise RuntimeError" not in content
        assert "old_values=[" in content
        assert "new_values=[" in content

        replayed = replay_migrations(str(tmp_path))
        state_field = replayed["tables"]["articles"]["fields"][1]
        assert state_field["column_type"]["values"] == ["draft"]


class TestAddColumnPipeline:
    @pytest.mark.parametrize("dialect", ALL_DIALECTS)
    def test_add_column(self, tmp_path, dialect):
        class UserV1(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str

            class Meta:
                is_table = True
                table_name = "users"

        class UserV2(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str
            nickname: str | None = Field(default=None, db_nullable=True)

            class Meta:
                is_table = True
                table_name = "users"

        ops, up_sql, down_sql = _run_pipeline(
            [UserV1], [UserV2], dialect, tmp_path, "add_nickname"
        )

        assert any(op["type"] == "add_column" for op in ops)
        assert any("ADD" in s.upper() and "nickname" in s for s in up_sql)
        assert any("nickname" in s for s in down_sql)


class TestDropColumnPipeline:
    @pytest.mark.parametrize("dialect", ALL_DIALECTS)
    def test_drop_column(self, tmp_path, dialect):
        class UserV1(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str
            deprecated: str | None = Field(default=None, db_nullable=True)

            class Meta:
                is_table = True
                table_name = "users"

        class UserV2(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str

            class Meta:
                is_table = True
                table_name = "users"

        ops, up_sql, down_sql = _run_pipeline(
            [UserV1], [UserV2], dialect, tmp_path, "drop_deprecated"
        )

        assert any(op["type"] == "drop_column" for op in ops)
        assert any("DROP" in s.upper() and "deprecated" in s for s in up_sql)
        # Reverse must re-add the column (field_def persisted in downgrade)
        assert any("ADD" in s.upper() and "deprecated" in s for s in down_sql)


class TestDropForeignKeyPipeline:
    """Exact scenario from the bug report.

    Model Book(author: Author) → Book(author_id: int) keeps the column
    but removes the FK relationship; makemigrations + sqlmigrate must succeed.
    """

    @pytest.mark.parametrize("dialect", POSTGRES_DIALECTS)
    def test_drop_fk_keeping_column(self, tmp_path, dialect):
        class AuthorV1(Model):
            id: int | None = Field(default=None, db_pk=True)
            name: str

            class Meta:
                is_table = True
                table_name = "authors"

        class BookV1(Model):
            id: int | None = Field(default=None, db_pk=True)
            title: str
            author: AuthorV1 | None = Field(default=None, db_on_delete="CASCADE")

            class Meta:
                is_table = True
                table_name = "books"

        class AuthorV2(Model):
            id: int | None = Field(default=None, db_pk=True)
            name: str

            class Meta:
                is_table = True
                table_name = "authors"

        class BookV2(Model):
            id: int | None = Field(default=None, db_pk=True)
            title: str
            author_id: int | None = Field(default=None, db_nullable=True)

            class Meta:
                is_table = True
                table_name = "books"

        ops, up_sql, down_sql = _run_pipeline(
            [AuthorV1, BookV1],
            [AuthorV2, BookV2],
            dialect,
            tmp_path,
            "drop_author_fk",
        )

        assert any(op["type"] == "drop_foreign_key" for op in ops)
        assert any(
            "DROP" in s.upper() and "fk_books_author_id" in s for s in up_sql
        )
        # Reverse must re-add the FK, not just mention its name
        assert any(
            "ADD" in s.upper() and "fk_books_author_id" in s for s in down_sql
        )


class TestAlterColumnDowngradePipeline:
    """A plain alter_column rolls back: its downgrade undoes the change."""

    @staticmethod
    def _coded(unique: bool = False, nullable: bool = False) -> type[Model]:
        class Coded(Model):
            id: int | None = Field(default=None, db_pk=True)
            code: str | None = Field(default=None, max_length=20, db_unique=unique)

            class Meta:
                is_table = True
                table_name = "coded"

        if not nullable:
            Coded._db_meta.field_metadata["code"].nullable = False
        return Coded

    @pytest.mark.parametrize(
        "before,after,up_fragment,down_fragment",
        [
            (
                {"unique": False},
                {"unique": True},
                'ADD CONSTRAINT "coded_code_key" UNIQUE ("code")',
                'DROP CONSTRAINT "coded_code_key"',
            ),
            (
                {"unique": True},
                {"unique": False},
                'DROP CONSTRAINT "coded_code_key"',
                'ADD CONSTRAINT "coded_code_key" UNIQUE ("code")',
            ),
            (
                {"nullable": False},
                {"nullable": True},
                'ALTER COLUMN "code" DROP NOT NULL',
                'ALTER COLUMN "code" SET NOT NULL',
            ),
            (
                {"nullable": True},
                {"nullable": False},
                'ALTER COLUMN "code" SET NOT NULL',
                'ALTER COLUMN "code" DROP NOT NULL',
            ),
        ],
    )
    def test_change_and_its_rollback(
        self, tmp_path, before, after, up_fragment, down_fragment
    ):
        old = _snapshot_from_models([self._coded(**before)], "postgres")
        new = _snapshot_from_models([self._coded(**after)], "postgres")
        ops = json.loads(migration_compute_diff(json.dumps(old), json.dumps(new)))
        assert [op["type"] for op in ops] == ["alter_column"]
        path = generate_migration_file(ops, migrations_dir=tmp_path, name="alter_coded")
        module = load_migration_module(path)
        assert module is not None

        def statements(run, schema):
            # Execute mode resolves the alteration against the schema it
            # starts from, as `migrate` and `downgrade` do.
            state = SchemaState()
            for table in deepcopy(schema["tables"]).values():
                state.apply_operation({"type": "create_table", "table": table})
            ctx = MigrationContext(mode="execute", schema_state=state)
            run(ctx)
            return ctx._sql_statements

        up_sql = statements(module.upgrade, old)
        down_sql = statements(module.downgrade, new)
        assert len(up_sql) == 1 and up_fragment in up_sql[0], up_sql
        # Without a downgrade, rolling back would silently leave the change.
        assert len(down_sql) == 1 and down_fragment in down_sql[0], down_sql


class TestNullsDistinctPipeline:
    @pytest.mark.parametrize(
        "old_unique,old_nulls,new_unique,new_nulls",
        [
            (True, None, True, False),
            (True, False, True, True),
            (False, None, True, False),
            (True, False, False, None),
        ],
    )
    def test_unique_column_roundtrip(
        self, tmp_path, old_unique, old_nulls, new_unique, new_nulls
    ):
        class Before(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str | None = Field(
                default=None,
                db_column="contact_email",
                db_unique=old_unique,
                db_nulls_distinct=old_nulls,
            )

            class Meta:
                is_table = True
                table_name = "contacts"

        class After(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str | None = Field(
                default=None,
                db_column="contact_email",
                db_unique=new_unique,
                db_nulls_distinct=new_nulls,
            )

            class Meta:
                is_table = True
                table_name = "contacts"

        old = _snapshot_from_models([Before], "postgres")
        new = _snapshot_from_models([After], "postgres")
        initial = json.loads(
            migration_compute_diff(
                json.dumps({"version": 1, "tables": {}}), json.dumps(old)
            )
        )
        generate_migration_file(initial, migrations_dir=tmp_path, name="initial")
        ops = json.loads(migration_compute_diff(json.dumps(old), json.dumps(new)))
        assert [op["type"] for op in ops] == ["alter_column"]
        path = generate_migration_file(
            ops, migrations_dir=tmp_path, name="nulls_distinct"
        )
        module = load_migration_module(path)
        assert module is not None
        assert_migration_parity(module, old)
        from types import SimpleNamespace
        assert_migration_parity(SimpleNamespace(upgrade=module.downgrade), new)

        state = SchemaState()
        for table in deepcopy(old["tables"]).values():
            state.apply_operation({"type": "create_table", "table": table})
        up = MigrationContext(mode="execute", schema_state=state)
        module.upgrade(up)
        assert len(up._sql_statements) == (2 if old_unique and new_unique else 1)
        if new_unique:
            assert ("UNIQUE NULLS NOT DISTINCT" in up._sql_statements[-1]) is (
                new_nulls is False
            )

        replayed = replay_migrations(tmp_path)
        assert (
            json.loads(migration_compute_diff(json.dumps(replayed), json.dumps(new)))
            == []
        )
        for table in deepcopy(replayed["tables"]).values():
            state.apply_operation({"type": "create_table", "table": table})
        down = MigrationContext(mode="execute", schema_state=state)
        module.downgrade(down)
        assert len(down._sql_statements) == (2 if old_unique and new_unique else 1)
        if old_unique:
            assert ("UNIQUE NULLS NOT DISTINCT" in down._sql_statements[-1]) is (
                old_nulls is False
            )

        collect = MigrationContext(mode="collect")
        module.downgrade(collect)
        for op in collect.get_collected_operations():
            state.apply_operation(op)
        restored = state.to_snapshot()
        assert (
            json.loads(migration_compute_diff(json.dumps(restored), json.dumps(old)))
            == []
        )
        if old_nulls is not False:
            assert (
                "nulls_not_distinct" not in restored["tables"]["contacts"]["fields"][1]
            )

    @pytest.mark.parametrize("enabled", [True, False])
    def test_index_roundtrip(self, tmp_path, enabled):
        from oxyde import Index

        class Before(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str | None = None

            class Meta:
                is_table = True
                table_name = "users"
                indexes = [
                    Index(
                        ["email"],
                        unique=True,
                        nulls_distinct=None if enabled else False,
                    )
                ]

        class After(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str | None = None

            class Meta:
                is_table = True
                table_name = "users"
                indexes = [
                    Index(
                        ["email"],
                        unique=True,
                        nulls_distinct=False if enabled else True,
                    )
                ]

        ops, up_sql, down_sql = _run_pipeline(
            [Before], [After], "postgres", tmp_path, "index_nulls_distinct"
        )
        assert [op["type"] for op in ops] == ["drop_index", "create_index"]
        assert ("NULLS NOT DISTINCT" in up_sql[-1]) is enabled
        assert ("NULLS NOT DISTINCT" in down_sql[-1]) is not enabled

    def test_default_and_true_produce_no_changes(self):
        from oxyde import Index

        class Defaults(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str | None = Field(default=None, db_unique=True)

            class Meta:
                is_table = True
                table_name = "users"
                indexes = [Index(["email"], unique=True)]

        old = _snapshot_from_models([Defaults], "postgres")
        Defaults._db_meta.field_metadata["email"].nulls_distinct = True
        Defaults._db_meta.indexes[0].nulls_distinct = True
        new = _snapshot_from_models([Defaults], "postgres")
        assert old == new
        assert (
            json.loads(migration_compute_diff(json.dumps(old), json.dumps(new))) == []
        )


class TestIndexPipeline:
    @pytest.mark.parametrize("dialect", ALL_DIALECTS)
    def test_create_index(self, tmp_path, dialect):
        from oxyde import Index

        class UserV1(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str

            class Meta:
                is_table = True
                table_name = "users"

        class UserV2(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str

            class Meta:
                is_table = True
                table_name = "users"
                indexes = [Index(fields=["email"], name="idx_users_email")]

        ops, up_sql, down_sql = _run_pipeline(
            [UserV1], [UserV2], dialect, tmp_path, "add_email_index"
        )

        assert any(op["type"] == "create_index" for op in ops)
        assert any(
            "CREATE" in s.upper() and "idx_users_email" in s for s in up_sql
        )
        assert any("idx_users_email" in s for s in down_sql)

    @pytest.mark.parametrize("dialect", PARTIAL_INDEX_DIALECTS)
    def test_create_partial_index_preserves_where(self, tmp_path, dialect):
        from oxyde import Index

        class UserV1(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str
            deleted_at: str | None = Field(default=None)

            class Meta:
                is_table = True
                table_name = "users"

        class UserV2(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str
            deleted_at: str | None = Field(default=None)

            class Meta:
                is_table = True
                table_name = "users"
                indexes = [
                    Index(
                        fields=["email"],
                        name="idx_users_active_email",
                        unique=True,
                        where="  deleted_at IS NULL  ",
                    )
                ]

        ops, up_sql, down_sql = _run_pipeline(
            [UserV1], [UserV2], dialect, tmp_path, "add_active_email_index"
        )

        create_ops = [op for op in ops if op["type"] == "create_index"]
        assert create_ops[0]["index"]["where"] == "deleted_at IS NULL"
        assert any("WHERE deleted_at IS NULL" in s for s in up_sql)
        assert any("idx_users_active_email" in s for s in down_sql)

    @pytest.mark.parametrize("dialect", ALL_DIALECTS)
    def test_drop_index(self, tmp_path, dialect):
        from oxyde import Index

        class UserV1(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str

            class Meta:
                is_table = True
                table_name = "users"
                indexes = [Index(fields=["email"], name="idx_users_email")]

        class UserV2(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str

            class Meta:
                is_table = True
                table_name = "users"

        ops, up_sql, down_sql = _run_pipeline(
            [UserV1], [UserV2], dialect, tmp_path, "drop_email_index"
        )

        assert any(op["type"] == "drop_index" for op in ops)
        assert any("DROP" in s.upper() and "idx_users_email" in s for s in up_sql)
        # Reverse re-creates index
        assert any(
            "CREATE" in s.upper() and "idx_users_email" in s for s in down_sql
        )


class TestExpressionIndexPipeline:
    """Expression / operator-class indexes through the full pipeline."""

    @staticmethod
    def _models():
        from oxyde import Coalesce, Index, Lower

        class AccountV1(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str = Field(db_column="email_address")
            name: str
            price: Decimal = Decimal("0")

            class Meta:
                is_table = True
                table_name = "accounts"

        class AccountV2(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str = Field(db_column="email_address")
            name: str
            price: Decimal = Decimal("0")

            class Meta:
                is_table = True
                table_name = "accounts"
                indexes = [
                    Index((Lower("email"),), unique=True, name="ux_accounts_email_lower"),
                    Index(
                        ("name",),
                        opclasses=("text_pattern_ops",),
                        name="ix_accounts_name_pattern",
                    ),
                    Index(
                        (Coalesce("price", Decimal("1.50")),),
                        name="ix_accounts_price_default",
                    ),
                ]

        return AccountV1, AccountV2

    def test_create_renders_expressions_and_survives_replay(self, tmp_path):
        v1, v2 = self._models()
        ops, up_sql, down_sql = _run_pipeline(
            [v1], [v2], "postgres", tmp_path, "add_expression_indexes"
        )

        assert [op["type"] for op in ops] == ["create_index"] * 3
        sql = "\n".join(up_sql)
        # field name -> db_column mapping happens at extraction time
        assert 'CREATE UNIQUE INDEX "ux_accounts_email_lower" ON "accounts" ((LOWER("email_address")))' in sql
        assert '("name" text_pattern_ops)' in sql
        assert 'COALESCE("price", 1.50)' in sql
        assert "$" not in sql
        # The migration file contains plain literals, so it loads and the
        # downgrade drops every index it created.
        assert sum("DROP INDEX" in s.upper() for s in down_sql) == 3

        # Replaying the written migration on top of the old schema reproduces
        # the model snapshot: a fresh diff is empty.
        module = load_migration_module(next(tmp_path.glob("[0-9]*.py")))
        replayed = assert_migration_parity(module, _snapshot_from_models([v1], "postgres"))
        current = _snapshot_from_models([v2], "postgres")
        assert json.loads(
            migration_compute_diff(json.dumps(replayed), json.dumps(current))
        ) == []

    def test_changing_the_expression_recreates_the_index(self, tmp_path):
        from oxyde import Index, Lower, Upper

        def make(func):
            class Account(Model):
                id: int | None = Field(default=None, db_pk=True)
                email: str

                class Meta:
                    is_table = True
                    table_name = "accounts"
                    indexes = [Index((func("email"),), name="ix_accounts_email_case")]

            return Account

        ops, up_sql, down_sql = _run_pipeline(
            [make(Lower)], [make(Upper)], "postgres", tmp_path, "swap_case"
        )
        assert [op["type"] for op in ops] == ["drop_index", "create_index"]
        assert "UPPER" in up_sql[-1]
        # The downgrade recreates the old definition from the stored index_def.
        assert "LOWER" in down_sql[-1]

    def test_dropping_an_expression_index_is_reversible(self, tmp_path):
        v1, v2 = self._models()
        ops, up_sql, down_sql = _run_pipeline(
            [v2], [v1], "postgres", tmp_path, "drop_expression_indexes"
        )
        assert [op["type"] for op in ops] == ["drop_index"] * 3
        recreated = "\n".join(down_sql)
        assert 'LOWER("email_address")' in recreated
        assert "text_pattern_ops" in recreated

    def test_invalid_hand_written_key_fails_loudly(self):
        state = SchemaState()
        state.apply_operation(
            {
                "type": "create_table",
                "table": {
                    "name": "t",
                    "fields": [],
                    "indexes": [],
                    "foreign_keys": [],
                    "checks": [],
                },
            }
        )
        with pytest.raises(Exception, match="operator class"):
            state.apply_operation(
                {
                    "type": "create_index",
                    "table": "t",
                    "index": {
                        "name": "ix",
                        "fields": [{"column": "a", "opclass": "x; DROP TABLE t"}],
                        "unique": False,
                        "method": None,
                    },
                }
            )

    def test_generator_tolerates_drop_index_without_definition(self, tmp_path):
        ops = [{"type": "drop_index", "table": "accounts", "name": "ix_old"}]
        path = generate_migration_file(ops, migrations_dir=tmp_path, name="drop_old")
        text = Path(path).read_text()
        assert "TODO: Reverse drop_index for accounts.ix_old" in text


class TestCyclicForeignKeys:
    """Cycles only matter when the affected tables are actually being created
    or dropped. For unchanged tables the topo-sort is irrelevant. The diff
    planner must therefore restrict its topo-sort to the create/drop subset,
    not the entire snapshot.
    """

    @pytest.mark.parametrize("dialect", ALL_DIALECTS)
    def test_noop_diff_on_cyclic_schema(self, dialect):
        class Company(Model):
            id: int | None = Field(default=None, db_pk=True)
            user: User | None = Field(default=None, db_on_delete="NO ACTION")

            class Meta:
                is_table = True
                table_name = "companies"

        class User(Model):
            id: int | None = Field(default=None, db_pk=True)
            company: Company | None = Field(default=None, db_on_delete="NO ACTION")

            class Meta:
                is_table = True
                table_name = "users"

        snap = _snapshot_from_models([Company, User], dialect)
        ops_json = migration_compute_diff(json.dumps(snap), json.dumps(snap))
        ops = json.loads(ops_json)
        assert ops == [], (
            f"no-op diff on cyclic schema must produce no operations, got {ops}"
        )

    @pytest.mark.parametrize("dialect", ALL_DIALECTS)
    def test_add_column_in_cyclic_schema(self, tmp_path, dialect):
        class CompanyV1(Model):
            id: int | None = Field(default=None, db_pk=True)
            user: UserV1 | None = Field(default=None, db_on_delete="NO ACTION")

            class Meta:
                is_table = True
                table_name = "companies"

        class UserV1(Model):
            id: int | None = Field(default=None, db_pk=True)
            company: CompanyV1 | None = Field(default=None, db_on_delete="NO ACTION")

            class Meta:
                is_table = True
                table_name = "users"

        class CompanyV2(Model):
            id: int | None = Field(default=None, db_pk=True)
            user: UserV2 | None = Field(default=None, db_on_delete="NO ACTION")
            name: str | None = Field(default=None, db_nullable=True)

            class Meta:
                is_table = True
                table_name = "companies"

        class UserV2(Model):
            id: int | None = Field(default=None, db_pk=True)
            company: CompanyV2 | None = Field(default=None, db_on_delete="NO ACTION")

            class Meta:
                is_table = True
                table_name = "users"

        ops, up_sql, _ = _run_pipeline(
            [CompanyV1, UserV1],
            [CompanyV2, UserV2],
            dialect,
            tmp_path,
            "add_company_name",
        )

        add_columns = [op for op in ops if op["type"] == "add_column"]
        assert len(add_columns) == 1, (
            f"expected exactly one AddColumn, got {ops}"
        )
        assert add_columns[0]["table"] == "companies"
        assert add_columns[0]["field"]["name"] == "name"
        assert any(
            "ADD" in s.upper() and "name" in s.lower() for s in up_sql
        )

    @pytest.mark.parametrize("dialect", ALL_DIALECTS)
    def test_create_table_referencing_cyclic_subset(self, tmp_path, dialect):
        class CompanyV1(Model):
            id: int | None = Field(default=None, db_pk=True)
            user: UserV1 | None = Field(default=None, db_on_delete="NO ACTION")

            class Meta:
                is_table = True
                table_name = "companies"

        class UserV1(Model):
            id: int | None = Field(default=None, db_pk=True)
            company: CompanyV1 | None = Field(default=None, db_on_delete="NO ACTION")

            class Meta:
                is_table = True
                table_name = "users"

        class CompanyV2(Model):
            id: int | None = Field(default=None, db_pk=True)
            user: UserV2 | None = Field(default=None, db_on_delete="NO ACTION")

            class Meta:
                is_table = True
                table_name = "companies"

        class UserV2(Model):
            id: int | None = Field(default=None, db_pk=True)
            company: CompanyV2 | None = Field(default=None, db_on_delete="NO ACTION")

            class Meta:
                is_table = True
                table_name = "users"

        class OrderV2(Model):
            id: int | None = Field(default=None, db_pk=True)
            user: UserV2 | None = Field(default=None, db_on_delete="NO ACTION")

            class Meta:
                is_table = True
                table_name = "orders"

        ops, up_sql, _ = _run_pipeline(
            [CompanyV1, UserV1],
            [CompanyV2, UserV2, OrderV2],
            dialect,
            tmp_path,
            "add_orders_table",
        )

        create_tables = [
            op["table"]["name"] for op in ops if op["type"] == "create_table"
        ]
        assert create_tables == ["orders"], (
            f"expected exactly one CreateTable(orders), got {ops}"
        )
        assert any("CREATE TABLE" in s.upper() and "orders" in s for s in up_sql)
