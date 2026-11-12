"""End-to-end migration tests against real databases via testcontainers.

Each test:
    1. Builds one or two migration files in tmp_path.
    2. Applies them against a fresh DB (PostgreSQL 18).
    3. Inspects the resulting schema via information_schema and pg_catalog.

Table names are suffixed with a per-test UUID so parallel test runs against the
PostgreSQL server do not collide.
"""

from __future__ import annotations

import uuid
from enum import Enum
from pathlib import Path

import pytest
import pytest_asyncio

from oxyde import AsyncDatabase, Coalesce, Field, Index, Lower, Model, disconnect_all
from oxyde.db.schema import create_tables
from oxyde.exceptions import UniqueViolationError
from oxyde.migrations import (
    apply_migrations,
    extract_current_schema,
    generate_migration_file,
    rollback_migrations,
)
from oxyde.models.registry import clear_registry, register_table
from oxyde.queries.raw import execute_raw


@pytest_asyncio.fixture(params=["postgres"])
async def empty_db(request, postgres_database_url):
    """Fresh DB with no tables — unlike `db` fixture that seeds ALL_MODELS."""
    url = postgres_database_url
    db_name = f"mig_{request.param}_{uuid.uuid4().hex[:8]}"
    database = AsyncDatabase(url, name=db_name, overwrite=True)
    await database.connect()

    clear_registry()

    yield database, request.param
    await disconnect_all()


async def _table_exists(db_name: str, table: str, dialect: str) -> bool:
    rows = await execute_raw(
        "SELECT 1 FROM information_schema.tables WHERE table_name = $1",
        [table],
        using=db_name,
    )
    return bool(rows)


async def _column_exists(db_name: str, table: str, column: str, dialect: str) -> bool:
    rows = await execute_raw(
        "SELECT 1 FROM information_schema.columns "
        "WHERE table_name = $1 AND column_name = $2",
        [table, column],
        using=db_name,
    )
    return bool(rows)


async def _fk_exists(db_name: str, fk_name: str, dialect: str) -> bool:
    """Check if a named FK constraint exists. PostgreSQL."""
    rows = await execute_raw(
        "SELECT 1 FROM pg_constraint WHERE conname = $1",
        [fk_name],
        using=db_name,
    )
    return bool(rows)


async def _index_exists(db_name: str, index_name: str, dialect: str) -> bool:
    rows = await execute_raw(
        "SELECT 1 FROM pg_indexes WHERE indexname = $1",
        [index_name],
        using=db_name,
    )
    return bool(rows)


def _write_migration(
    tmp_path: Path,
    old_models: list[type[Model]],
    new_models: list[type[Model]],
    dialect: str,
    name: str,
) -> Path:
    import json

    from oxyde.core import migration_compute_diff

    clear_registry()
    for m in old_models:
        register_table(m, overwrite=True)
    old = extract_current_schema(dialect=dialect)

    clear_registry()
    for m in new_models:
        register_table(m, overwrite=True)
    new = extract_current_schema(dialect=dialect)

    ops_json = migration_compute_diff(json.dumps(old), json.dumps(new))
    ops = json.loads(ops_json)
    return generate_migration_file(ops, migrations_dir=tmp_path, name=name)


def _diff_ops(
    old_model: type[Model],
    new_model: type[Model],
    dialect: str,
    only: str | None = None,
) -> list[dict]:
    """Ops the core diffs between two single-model schemas (as makemigrations).

    Kept out of the test bodies: `test_schema_replay` replays only the
    assignments and `_write_migration` calls of each fixture, without a
    database.
    """
    import json

    from oxyde.core import migration_compute_diff

    schemas = []
    for current in (old_model, new_model):
        clear_registry()
        register_table(current, overwrite=True)
        schemas.append(extract_current_schema(dialect=dialect))
    ops = json.loads(
        migration_compute_diff(json.dumps(schemas[0]), json.dumps(schemas[1]))
    )
    return ops if only is None else [op for op in ops if op["type"] == only]


async def _assert_relations_exist(db_name: str, expected: set[str]) -> None:
    """Every name is a relation (table, index or sequence) in the schema."""
    wanted = sorted(expected)
    placeholders = ", ".join(f"${i}" for i in range(1, len(wanted) + 1))
    rows = await execute_raw(
        f"SELECT relname FROM pg_class WHERE relname IN ({placeholders})",
        wanted,
        using=db_name,
    )
    names = {row["relname"] for row in rows}
    assert expected <= names, (expected - names, names)


# Two versions of one enum (same name, so the same PostgreSQL type) for the
# "member added" migration. Module level, so model annotations resolve.
_StatusV1 = Enum("Status", {"A": "a", "B": "b"})
_StatusV2 = Enum("Status", {"A": "a", "B": "b", "C": "c"})


class TestNullsDistinctE2E:
    @pytest.mark.asyncio
    @pytest.mark.parametrize(
        "mode", ["column", "index", "partial_index", "default", "explicit_true"]
    )
    async def test_create_tables_null_uniqueness(self, empty_db, mode):
        database, _ = empty_db

        class Contact(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str | None = Field(
                default=None,
                db_unique=mode in ("column", "default", "explicit_true"),
                db_nulls_distinct=False
                if mode == "column"
                else True
                if mode == "explicit_true"
                else None,
            )

            class Meta:
                is_table = True
                table_name = "null_contacts"
                indexes = (
                    [
                        Index(
                            ["email"],
                            unique=True,
                            nulls_distinct=False,
                            method="btree",
                            where="email IS NULL" if mode == "partial_index" else None,
                        )
                    ]
                    if mode in ("index", "partial_index")
                    else []
                )

        await create_tables(database)
        await Contact.objects.create(email=None, using=database.name)
        if mode in ("column", "index", "partial_index"):
            with pytest.raises(UniqueViolationError):
                await Contact.objects.create(email=None, using=database.name)
            assert await Contact.objects.count(using=database.name) == 1
        else:
            await Contact.objects.create(email=None, using=database.name)
            assert await Contact.objects.count(using=database.name) == 2

    @pytest.mark.asyncio
    @pytest.mark.parametrize("kind", ["column", "index"])
    @pytest.mark.parametrize("enabled", [True, False])
    async def test_toggle_forward_and_reverse(self, empty_db, tmp_path, kind, enabled):
        database, dialect = empty_db
        tbl = f"contacts_{uuid.uuid4().hex[:8]}"
        index_name = f"{tbl}_email_key" if kind == "column" else f"{tbl}_email_idx"

        class Before(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str | None = Field(
                default=None,
                db_unique=kind == "column",
                db_nulls_distinct=False if kind == "column" and not enabled else None,
            )

            class Meta:
                is_table = True
                table_name = tbl
                indexes = (
                    [
                        Index(
                            ["email"],
                            unique=True,
                            nulls_distinct=None if enabled else False,
                        )
                    ]
                    if kind == "index"
                    else []
                )

        class After(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str | None = Field(
                default=None,
                db_unique=kind == "column",
                db_nulls_distinct=False if kind == "column" and enabled else None,
            )

            class Meta:
                is_table = True
                table_name = tbl
                indexes = (
                    [
                        Index(
                            ["email"],
                            unique=True,
                            nulls_distinct=False if enabled else None,
                        )
                    ]
                    if kind == "index"
                    else []
                )

        _write_migration(tmp_path, [], [Before], dialect, "initial")
        await apply_migrations(migrations_dir=str(tmp_path), db_alias=database.name)
        _write_migration(tmp_path, [Before], [After], dialect, "toggle_nulls")
        await apply_migrations(migrations_dir=str(tmp_path), db_alias=database.name)

        async def assert_null_behavior(expected):
            rows = await execute_raw(
                "SELECT i.indnullsnotdistinct AS enabled FROM pg_index i "
                "JOIN pg_class c ON c.oid = i.indexrelid WHERE c.relname = $1",
                [index_name],
                using=database.name,
            )
            assert rows == [{"enabled": expected}]
            await After.objects.create(email=None, using=database.name)
            if expected:
                with pytest.raises(UniqueViolationError):
                    await After.objects.create(email=None, using=database.name)
            else:
                await After.objects.create(email=None, using=database.name)
            await After.objects.filter(email__isnull=True).delete(using=database.name)

        await assert_null_behavior(enabled)
        await rollback_migrations(
            steps=1, migrations_dir=str(tmp_path), db_alias=database.name
        )
        await assert_null_behavior(not enabled)


class TestCreateTableE2E:
    @pytest.mark.asyncio
    async def test_create_and_drop_table(self, empty_db, tmp_path):
        database, dialect = empty_db
        suffix = uuid.uuid4().hex[:8]
        tbl = f"users_{suffix}"

        class UserV1(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str = Field(max_length=255)

            class Meta:
                is_table = True
                table_name = tbl

        _write_migration(tmp_path, [], [UserV1], dialect, f"create_{suffix}")
        await apply_migrations(migrations_dir=str(tmp_path), db_alias=database.name)
        assert await _table_exists(database.name, tbl, dialect)

        _write_migration(tmp_path, [UserV1], [], dialect, f"drop_{suffix}")
        await apply_migrations(migrations_dir=str(tmp_path), db_alias=database.name)
        assert not await _table_exists(database.name, tbl, dialect)


class TestAddDropColumnE2E:
    @pytest.mark.asyncio
    async def test_add_column(self, empty_db, tmp_path):
        database, dialect = empty_db
        suffix = uuid.uuid4().hex[:8]
        tbl = f"users_{suffix}"

        class UserV1(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str = Field(max_length=255)

            class Meta:
                is_table = True
                table_name = tbl

        class UserV2(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str = Field(max_length=255)
            nickname: str | None = Field(default=None, db_nullable=True, max_length=100)

            class Meta:
                is_table = True
                table_name = tbl

        _write_migration(tmp_path, [], [UserV1], dialect, f"create_{suffix}")
        _write_migration(tmp_path, [UserV1], [UserV2], dialect, f"add_nick_{suffix}")

        await apply_migrations(migrations_dir=str(tmp_path), db_alias=database.name)
        assert await _column_exists(database.name, tbl, "nickname", dialect)

    @pytest.mark.asyncio
    async def test_drop_column(self, empty_db, tmp_path):
        database, dialect = empty_db
        suffix = uuid.uuid4().hex[:8]
        tbl = f"users_{suffix}"

        class UserV1(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str = Field(max_length=255)
            deprecated: str | None = Field(
                default=None, db_nullable=True, max_length=100
            )

            class Meta:
                is_table = True
                table_name = tbl

        class UserV2(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str = Field(max_length=255)

            class Meta:
                is_table = True
                table_name = tbl

        _write_migration(tmp_path, [], [UserV1], dialect, f"create_{suffix}")
        _write_migration(
            tmp_path, [UserV1], [UserV2], dialect, f"drop_deprecated_{suffix}"
        )

        pass

        await apply_migrations(migrations_dir=str(tmp_path), db_alias=database.name)
        assert not await _column_exists(database.name, tbl, "deprecated", dialect)
        assert await _column_exists(database.name, tbl, "email", dialect)


class TestIndexE2E:
    @pytest.mark.asyncio
    async def test_create_and_drop_index(self, empty_db, tmp_path):
        database, dialect = empty_db
        suffix = uuid.uuid4().hex[:8]
        tbl = f"users_{suffix}"
        idx_name = f"idx_{tbl}_email"

        class UserV1(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str = Field(max_length=255)

            class Meta:
                is_table = True
                table_name = tbl

        class UserV2(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str = Field(max_length=255)

            class Meta:
                is_table = True
                table_name = tbl
                indexes = [Index(fields=["email"], name=idx_name)]

        _write_migration(tmp_path, [], [UserV1], dialect, f"create_{suffix}")
        _write_migration(tmp_path, [UserV1], [UserV2], dialect, f"add_idx_{suffix}")
        _write_migration(tmp_path, [UserV2], [UserV1], dialect, f"drop_idx_{suffix}")

        # Apply first two — index should exist
        await apply_migrations(
            migrations_dir=str(tmp_path),
            db_alias=database.name,
            target="0002",
        )
        assert await _index_exists(database.name, idx_name, dialect)

        # Apply the third — index gone
        await apply_migrations(migrations_dir=str(tmp_path), db_alias=database.name)
        assert not await _index_exists(database.name, idx_name, dialect)


class TestExpressionIndexE2E:
    """Expression / operator-class indexes against a real PostgreSQL.

    PostgreSQL stores a normalized expression (``lower((email)::text)``), so the
    catalog text is only checked for fragments. The EXPLAIN checks are the real
    proof: a naturally written query must pick the index.
    """

    @staticmethod
    async def _indexdef(db_name: str, index_name: str) -> str:
        rows = await execute_raw(
            "SELECT indexdef FROM pg_indexes WHERE indexname = $1",
            [index_name],
            using=db_name,
        )
        return rows[0]["indexdef"]

    @staticmethod
    async def _plan(db_name: str, query: str) -> str:
        from oxyde import atomic

        # SET LOCAL needs the transaction's connection; it discourages seq scans
        # so the empty table still shows whether the index is usable.
        async with atomic(using=db_name):
            await execute_raw("SET LOCAL enable_seqscan = off", using=db_name)
            rows = await execute_raw(f"EXPLAIN {query}", using=db_name)
        return "\n".join(str(next(iter(row.values()))) for row in rows)

    @classmethod
    async def _assert_plan_uses(cls, db_name: str, query: str, index_name: str) -> None:
        plan = await cls._plan(db_name, query)
        assert index_name in plan, plan

    @staticmethod
    def _assert_no_pending_diff(
        tmp_path: Path, model: type[Model], dialect: str
    ) -> None:
        import json

        from oxyde.core import migration_compute_diff
        from oxyde.migrations.replay import replay_migrations

        clear_registry()
        register_table(model, overwrite=True)
        current = extract_current_schema(dialect=dialect)
        replayed = replay_migrations(str(tmp_path))
        assert (
            json.loads(
                migration_compute_diff(json.dumps(replayed), json.dumps(current))
            )
            == []
        )

    @pytest.mark.asyncio
    async def test_indexes_are_created_used_and_dropped(self, empty_db, tmp_path):
        database, dialect = empty_db
        suffix = uuid.uuid4().hex[:8]
        tbl = f"accounts_{suffix}"
        ix_lower = f"ux_{suffix}_email_lower"
        ix_qty = f"ix_{suffix}_qty_default"
        ix_name = f"ix_{suffix}_name_pattern"

        class V1(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str = Field(max_length=255, db_column="email_address")
            name: str = Field(max_length=100)
            qty: int = 0

            class Meta:
                is_table = True
                table_name = tbl

        class V2(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str = Field(max_length=255, db_column="email_address")
            name: str = Field(max_length=100)
            qty: int = 0

            class Meta:
                is_table = True
                table_name = tbl
                indexes = [
                    Index((Lower("email"),), unique=True, name=ix_lower),
                    Index((Coalesce("qty", 1),), name=ix_qty),
                    Index(("name",), opclasses=("text_pattern_ops",), name=ix_name),
                ]

        _write_migration(tmp_path, [], [V1], dialect, f"create_{suffix}")
        _write_migration(tmp_path, [V1], [V2], dialect, f"add_idx_{suffix}")
        await apply_migrations(migrations_dir=str(tmp_path), db_alias=database.name)

        assert "lower(" in await self._indexdef(database.name, ix_lower)
        assert "COALESCE(" in await self._indexdef(database.name, ix_qty)
        assert "text_pattern_ops" in await self._indexdef(database.name, ix_name)

        await self._assert_plan_uses(
            database.name,
            f"SELECT * FROM {tbl} WHERE lower(email_address) = 'x'",
            ix_lower,
        )
        await self._assert_plan_uses(
            database.name, f"SELECT * FROM {tbl} WHERE COALESCE(qty, 1) = 5", ix_qty
        )
        await self._assert_plan_uses(
            database.name, f"SELECT * FROM {tbl} WHERE name LIKE 'abc%'", ix_name
        )

        # The unique expression index enforces case-insensitive uniqueness.
        await execute_raw(
            f"INSERT INTO {tbl} (email_address, name) VALUES ($1, $2)",
            ["A@x.io", "a"],
            using=database.name,
        )
        # execute_raw surfaces the core's own exception type, so match the message.
        with pytest.raises(Exception, match="duplicate key value violates unique"):
            await execute_raw(
                f"INSERT INTO {tbl} (email_address, name) VALUES ($1, $2)",
                ["a@X.io", "b"],
                using=database.name,
            )

        # No pending changes: a new diff against the replayed history is empty.
        self._assert_no_pending_diff(tmp_path, V2, dialect)

        # Downgrade drops them.
        await rollback_migrations(
            migrations_dir=str(tmp_path), db_alias=database.name, steps=1
        )
        for name in (ix_lower, ix_qty, ix_name):
            assert not await _index_exists(database.name, name, dialect)

    @pytest.mark.asyncio
    @pytest.mark.parametrize("inline", [True, False])
    @pytest.mark.parametrize("fallback", [[1, 2], []])
    async def test_array_and_binary_literals_survive_generation_and_replay(
        self, empty_db, tmp_path, inline, fallback
    ):
        database, dialect = empty_db
        suffix = uuid.uuid4().hex[:8]
        tbl = f"literals_{suffix}"
        ix_tags, ix_data = f"ix_tags_{suffix}", f"ix_data_{suffix}"
        binary = b"\x00\xff'\\fallback" if fallback else b""

        class V1(Model):
            id: int | None = Field(default=None, db_pk=True)
            tags: list[int] | None = Field(default=None, db_column="tag_ids")
            data: bytes | None = None

            class Meta:
                is_table = True
                table_name = tbl

        class V2(Model):
            id: int | None = Field(default=None, db_pk=True)
            tags: list[int] | None = Field(default=None, db_column="tag_ids")
            data: bytes | None = None

            class Meta:
                is_table = True
                table_name = tbl
                indexes = [
                    Index((Coalesce("tags", fallback),), name=ix_tags),
                    Index((Coalesce("data", binary),), name=ix_data),
                ]

        if inline:
            _write_migration(tmp_path, [], [V2], dialect, f"create_{suffix}")
        else:
            _write_migration(tmp_path, [], [V1], dialect, f"create_{suffix}")
            _write_migration(tmp_path, [V1], [V2], dialect, f"indexes_{suffix}")
        await apply_migrations(migrations_dir=str(tmp_path), db_alias=database.name)
        self._assert_no_pending_diff(tmp_path, V2, dialect)

        array_sql = "ARRAY[1,2]::bigint[]" if fallback else "'{}'::bigint[]"
        binary_sql = f"'\\x{binary.hex()}'::bytea"
        await self._assert_plan_uses(
            database.name,
            f"SELECT * FROM {tbl} WHERE COALESCE(tag_ids, {array_sql}) = {array_sql}",
            ix_tags,
        )
        await self._assert_plan_uses(
            database.name,
            f"SELECT * FROM {tbl} WHERE COALESCE(data, {binary_sql}) = {binary_sql}",
            ix_data,
        )
        await rollback_migrations(
            migrations_dir=str(tmp_path), db_alias=database.name, steps=1
        )
        assert not await _index_exists(database.name, ix_tags, dialect)
        assert not await _index_exists(database.name, ix_data, dialect)

    @pytest.mark.asyncio
    async def test_unique_opclass_index_precedes_a_new_foreign_key(
        self, empty_db, tmp_path
    ):
        database, dialect = empty_db
        suffix = uuid.uuid4().hex[:8]
        parent_table, child_table = f"parent_{suffix}", f"child_{suffix}"
        ix_code = f"ux_code_{suffix}"

        class ParentV1(Model):
            id: int | None = Field(default=None, db_pk=True)
            code: str

            class Meta:
                is_table = True
                table_name = parent_table

        class ParentV2(Model):
            id: int | None = Field(default=None, db_pk=True)
            code: str

            class Meta:
                is_table = True
                table_name = parent_table
                indexes = [
                    Index(
                        ("code",),
                        unique=True,
                        opclasses=("text_pattern_ops",),
                        name=ix_code,
                    )
                ]

        class Child(Model):
            id: int | None = Field(default=None, db_pk=True)
            parent: ParentV2 = Field(db_fk="code")

            class Meta:
                is_table = True
                table_name = child_table

        _write_migration(tmp_path, [], [ParentV1], dialect, f"create_{suffix}")
        _write_migration(
            tmp_path, [ParentV1], [ParentV2, Child], dialect, f"child_{suffix}"
        )
        await apply_migrations(migrations_dir=str(tmp_path), db_alias=database.name)
        assert await _fk_exists(database.name, f"fk_{child_table}_parent_code", dialect)
        await rollback_migrations(
            migrations_dir=str(tmp_path), db_alias=database.name, steps=1
        )
        assert not await _table_exists(database.name, child_table, dialect)
        assert not await _index_exists(database.name, ix_code, dialect)

    @pytest.mark.asyncio
    async def test_enum_member_added_keeps_expression_indexes(self, empty_db, tmp_path):
        database, dialect = empty_db
        suffix = uuid.uuid4().hex[:8]
        tbl = f"tagged_{suffix}"
        ix_status = f"ix_{suffix}_status"
        ix_tags = f"ix_{suffix}_tags"

        class V1(Model):
            id: int | None = Field(default=None, db_pk=True)
            status: _StatusV1 | None = None
            tags: list[_StatusV1] | None = None

            class Meta:
                is_table = True
                table_name = tbl
                indexes = [
                    Index((Coalesce("status", _StatusV1("a")),), name=ix_status),
                    Index((Coalesce("tags", []),), name=ix_tags),
                ]

        class V2(Model):
            id: int | None = Field(default=None, db_pk=True)
            status: _StatusV2 | None = None
            tags: list[_StatusV2] | None = None

            class Meta:
                is_table = True
                table_name = tbl
                indexes = [
                    Index((Coalesce("status", _StatusV2("a")),), name=ix_status),
                    Index((Coalesce("tags", []),), name=ix_tags),
                ]

        v1, v2 = V1, V2

        ops = _diff_ops(v1, v2, dialect)
        # The enum grew. The indexes did not change: no drop/create pair.
        assert [op["type"] for op in ops] == ["add_enum_value"]

        _write_migration(tmp_path, [], [v1], dialect, f"create_{suffix}")
        _write_migration(tmp_path, [v1], [v2], dialect, f"add_member_{suffix}")
        await apply_migrations(migrations_dir=str(tmp_path), db_alias=database.name)
        assert await _index_exists(database.name, ix_status, dialect)
        assert await _index_exists(database.name, ix_tags, dialect)
        self._assert_no_pending_diff(tmp_path, v2, dialect)

    @pytest.mark.asyncio
    async def test_dropping_a_column_needs_no_separate_index_drop(
        self, empty_db, tmp_path
    ):
        database, dialect = empty_db
        suffix = uuid.uuid4().hex[:8]
        tbl = f"people_{suffix}"
        ix_lower = f"ix_{suffix}_email_lower"

        class V1(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str = Field(max_length=255)
            name: str = Field(max_length=100)

            class Meta:
                is_table = True
                table_name = tbl
                indexes = [Index((Lower("email"),), name=ix_lower)]

        class V2(Model):
            id: int | None = Field(default=None, db_pk=True)
            name: str = Field(max_length=100)

            class Meta:
                is_table = True
                table_name = tbl

        _write_migration(tmp_path, [], [V1], dialect, f"create_{suffix}")
        await apply_migrations(migrations_dir=str(tmp_path), db_alias=database.name)
        assert await _index_exists(database.name, ix_lower, dialect)

        # A migration written by hand drops the column and nothing else.
        column_only = _diff_ops(V1, V2, dialect, only="drop_column")
        assert len(column_only) == 1
        generate_migration_file(
            column_only, migrations_dir=tmp_path, name=f"drop_email_{suffix}"
        )
        await apply_migrations(migrations_dir=str(tmp_path), db_alias=database.name)

        # PostgreSQL dropped the index together with the column ...
        assert not await _column_exists(database.name, tbl, "email", dialect)
        assert not await _index_exists(database.name, ix_lower, dialect)
        # ... and the replayed state agrees, so nothing is left to diff.
        self._assert_no_pending_diff(tmp_path, V2, dialect)

    @pytest.mark.asyncio
    async def test_implicit_relation_names_match_what_postgres_creates(
        self, empty_db, tmp_path
    ):
        database, dialect = empty_db
        tbl = "a" * 60

        class Long(Model):
            id: int | None = Field(default=None, db_pk=True)
            code: str = Field(max_length=20, db_unique=True)

            class Meta:
                is_table = True
                table_name = tbl

        _write_migration(tmp_path, [], [Long], dialect, "create_long")
        await apply_migrations(migrations_dir=str(tmp_path), db_alias=database.name)

        # PostgreSQL shortens the longer name and keeps the suffix.
        await _assert_relations_exist(
            database.name,
            {"a" * 58 + "_pkey", "a" * 54 + "_code_key", "a" * 56 + "_id_seq"},
        )

        # So an explicit index with one of those names is caught before migrate.
        for taken in ("a" * 58 + "_pkey", "a" * 54 + "_code_key", "a" * 56 + "_id_seq"):

            class Clash(Model):
                id: int | None = Field(default=None, db_pk=True)
                code: str = Field(max_length=20, db_unique=True)

                class Meta:
                    is_table = True
                    table_name = tbl
                    indexes = [Index(("code",), name=taken)]

            clear_registry()
            register_table(Clash, overwrite=True)
            with pytest.raises(ValueError, match="clashes with"):
                extract_current_schema(dialect=dialect)

    @pytest.mark.asyncio
    async def test_alter_unique_finds_the_constraint_create_table_made(
        self, empty_db, tmp_path
    ):
        database, dialect = empty_db
        tbl = "a" * 60
        # What PostgreSQL names an inline UNIQUE here: the longer name is
        # shortened and the `_key` suffix is kept.
        constraint = "a" * 54 + "_code_key"

        class V1(Model):
            id: int | None = Field(default=None, db_pk=True)
            code: str = Field(max_length=20, db_unique=True)

            class Meta:
                is_table = True
                table_name = tbl

        class V2(Model):
            id: int | None = Field(default=None, db_pk=True)
            code: str = Field(max_length=20)

            class Meta:
                is_table = True
                table_name = tbl

        class V3(Model):
            id: int | None = Field(default=None, db_pk=True)
            code: str = Field(max_length=20, db_unique=True)

            class Meta:
                is_table = True
                table_name = tbl

        _write_migration(tmp_path, [], [V1], dialect, "create_long_unique")
        _write_migration(tmp_path, [V1], [V2], dialect, "drop_unique")
        _write_migration(tmp_path, [V2], [V3], dialect, "add_unique")

        # The drop must name the constraint CREATE TABLE made, or this fails
        # with "constraint ... does not exist".
        await apply_migrations(migrations_dir=str(tmp_path), db_alias=database.name)
        # The add then recreated it under the same name PostgreSQL chose.
        await _assert_relations_exist(database.name, {constraint})


class TestDropForeignKeyBugRegressionE2E:
    """The exact scenario from the bug report, driven end-to-end against a real DB.

    PostgreSQL supports ALTER TABLE DROP CONSTRAINT.
    """

    @pytest.mark.asyncio
    async def test_remove_fk_keep_column(self, empty_db, tmp_path):
        database, dialect = empty_db
        suffix = uuid.uuid4().hex[:8]
        authors = f"authors_{suffix}"
        books = f"books_{suffix}"
        fk_name = f"fk_{books}_author_id"

        class AuthorV1(Model):
            id: int | None = Field(default=None, db_pk=True)
            name: str = Field(max_length=100)

            class Meta:
                is_table = True
                table_name = authors

        class BookV1(Model):
            id: int | None = Field(default=None, db_pk=True)
            title: str = Field(max_length=200)
            author: AuthorV1 | None = Field(default=None, db_on_delete="CASCADE")

            class Meta:
                is_table = True
                table_name = books

        class AuthorV2(Model):
            id: int | None = Field(default=None, db_pk=True)
            name: str = Field(max_length=100)

            class Meta:
                is_table = True
                table_name = authors

        class BookV2(Model):
            id: int | None = Field(default=None, db_pk=True)
            title: str = Field(max_length=200)
            author_id: int | None = Field(default=None, db_nullable=True)

            class Meta:
                is_table = True
                table_name = books

        _write_migration(tmp_path, [], [AuthorV1, BookV1], dialect, f"initial_{suffix}")
        _write_migration(
            tmp_path,
            [AuthorV1, BookV1],
            [AuthorV2, BookV2],
            dialect,
            f"drop_fk_{suffix}",
        )

        # First migration creates tables + FK
        await apply_migrations(
            migrations_dir=str(tmp_path),
            db_alias=database.name,
            target="0001",
        )
        assert await _fk_exists(database.name, fk_name, dialect)

        # Second migration drops the FK — this is where the bug manifested
        await apply_migrations(migrations_dir=str(tmp_path), db_alias=database.name)
        assert not await _fk_exists(database.name, fk_name, dialect)
        assert await _column_exists(database.name, books, "author_id", dialect)


class TestUpgradeDowngradeSymmetryE2E:
    @pytest.mark.asyncio
    async def test_rollback_restores_state(self, empty_db, tmp_path):
        database, dialect = empty_db
        suffix = uuid.uuid4().hex[:8]
        tbl = f"users_{suffix}"

        class UserV1(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str = Field(max_length=255)

            class Meta:
                is_table = True
                table_name = tbl

        class UserV2(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str = Field(max_length=255)
            nickname: str | None = Field(default=None, db_nullable=True, max_length=100)

            class Meta:
                is_table = True
                table_name = tbl

        _write_migration(tmp_path, [], [UserV1], dialect, f"create_{suffix}")
        _write_migration(tmp_path, [UserV1], [UserV2], dialect, f"add_nick_{suffix}")

        await apply_migrations(migrations_dir=str(tmp_path), db_alias=database.name)
        assert await _column_exists(database.name, tbl, "nickname", dialect)

        await rollback_migrations(
            steps=1, migrations_dir=str(tmp_path), db_alias=database.name
        )
        assert not await _column_exists(database.name, tbl, "nickname", dialect)
        assert await _column_exists(database.name, tbl, "email", dialect)


class TestIndexNamesAndRenamesE2E:
    """PostgreSQL's identifier truncation and column renames under indexes."""

    @pytest.mark.asyncio
    async def test_overlong_index_name_is_created_and_dropped_by_full_name(
        self, empty_db, tmp_path
    ):
        # PostgreSQL truncates identifiers to 63 bytes in every statement, so a
        # DROP INDEX by the full name must hit the truncated index. This is the
        # premise for accepting long names (the Rust check only rejects names
        # that collide after truncation).
        database, dialect = empty_db
        suffix = uuid.uuid4().hex[:8]
        tbl = f"long_{suffix}"
        full_name = f"ix_{suffix}_" + "x" * 70
        stored_name = full_name.encode()[:63].decode()

        class V1(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str = Field(max_length=255)

            class Meta:
                is_table = True
                table_name = tbl

        class V2(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str = Field(max_length=255)

            class Meta:
                is_table = True
                table_name = tbl
                indexes = [Index((Lower("email"),), name=full_name)]

        _write_migration(tmp_path, [], [V1], dialect, f"create_{suffix}")
        _write_migration(tmp_path, [V1], [V2], dialect, f"add_long_{suffix}")
        await apply_migrations(migrations_dir=str(tmp_path), db_alias=database.name)
        assert await _index_exists(database.name, stored_name, dialect)

        await rollback_migrations(
            migrations_dir=str(tmp_path), db_alias=database.name, steps=1
        )
        assert not await _index_exists(database.name, stored_name, dialect)

    @pytest.mark.asyncio
    async def test_rename_column_keeps_expression_index_in_sync(
        self, empty_db, tmp_path
    ):
        database, dialect = empty_db
        suffix = uuid.uuid4().hex[:8]
        tbl = f"ren_{suffix}"
        ix = f"ix_{suffix}_lower"

        class Before(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str = Field(max_length=255)

            class Meta:
                is_table = True
                table_name = tbl
                indexes = [Index((Lower("email"),), name=ix)]

        class After(Model):
            id: int | None = Field(default=None, db_pk=True)
            mail: str = Field(max_length=255)

            class Meta:
                is_table = True
                table_name = tbl
                indexes = [Index((Lower("mail"),), name=ix)]

        _write_migration(tmp_path, [], [Before], dialect, f"create_{suffix}")
        # Hand-written rename, as a user would author it: no index operations.
        generate_migration_file(
            [
                {
                    "type": "rename_column",
                    "table": tbl,
                    "old_name": "email",
                    "new_name": "mail",
                }
            ],
            migrations_dir=tmp_path,
            name=f"rename_{suffix}",
        )
        await apply_migrations(migrations_dir=str(tmp_path), db_alias=database.name)
        assert await _index_exists(database.name, ix, dialect)

        # Replay follows the rename into the expression, so the next
        # makemigrations sees no change (no spurious DROP/CREATE INDEX).
        TestExpressionIndexE2E._assert_no_pending_diff(tmp_path, After, dialect)
