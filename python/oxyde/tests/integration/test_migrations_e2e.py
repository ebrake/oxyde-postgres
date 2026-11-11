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
from pathlib import Path

import pytest
import pytest_asyncio

from oxyde import AsyncDatabase, Field, Index, Model, disconnect_all
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


async def _column_exists(
    db_name: str, table: str, column: str, dialect: str
) -> bool:
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
    path = generate_migration_file(ops, migrations_dir=tmp_path, name=name)
    from types import SimpleNamespace

    from oxyde.migrations.utils import load_migration_module
    from oxyde.tests.unit.replay_parity import assert_migration_parity

    module = load_migration_module(path)
    assert module is not None
    replayed = assert_migration_parity(module, old)
    assert_migration_parity(SimpleNamespace(upgrade=module.downgrade), replayed)
    return path


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
            nickname: str | None = Field(
                default=None, db_nullable=True, max_length=100
            )

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
        assert not await _column_exists(
            database.name, tbl, "deprecated", dialect
        )
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

        _write_migration(
            tmp_path, [], [AuthorV1, BookV1], dialect, f"initial_{suffix}"
        )
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
            nickname: str | None = Field(
                default=None, db_nullable=True, max_length=100
            )

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
