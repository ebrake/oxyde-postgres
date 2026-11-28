"""Integration tests for pool settings (TLS, backend-specific options)."""

from __future__ import annotations

import uuid

import pytest
import pytest_asyncio

from oxyde import AsyncDatabase, PoolSettings, db, disconnect_all
from oxyde.core.wrapper import DatabaseError
from oxyde.queries.raw import execute_raw

from .conftest import _DIALECTS


@pytest_asyncio.fixture(params=_DIALECTS)
async def pool_db(request, postgres_database_url):
    """Bare database connection (no tables/seed) for pool settings tests."""
    url = postgres_database_url
    database = AsyncDatabase(
        url, name=f"pool_{request.param}_{uuid.uuid4().hex[:6]}", overwrite=True
    )
    yield database
    await disconnect_all()


class TestPoolSettings:
    @pytest.mark.asyncio
    async def test_ssl_mode(self, pool_db):
        settings = PoolSettings(ssl_mode="require")
        query = "SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()"

        pool_db.settings = settings
        try:
            await pool_db.connect()
        except RuntimeError as e:
            if "does not support TLS" in str(e):
                pytest.skip("Server does not support TLS")
            raise

        rows = await execute_raw(query, using=pool_db.name)

        if pool_db.backend == "postgres":
            assert rows[0]["ssl"] is True
        else:
            assert rows[0]["Value"] != ""

    @pytest.mark.asyncio
    async def test_pg_application_name(self, pool_db):
        if not pool_db.url.startswith("postgres"):
            pytest.skip("PostgreSQL-only setting")

        pool_db.settings = PoolSettings(pg_application_name="oxyde-test")
        await pool_db.connect()

        rows = await execute_raw(
            "SELECT application_name FROM pg_stat_activity "
            "WHERE pid = pg_backend_pid()",
            using=pool_db.name,
        )
        assert rows[0]["application_name"] == "oxyde-test"

    @pytest.mark.asyncio
    async def test_test_before_acquire(self, pool_db):
        pool_db.settings = PoolSettings(test_before_acquire=True)
        await pool_db.connect()

        # If test_before_acquire works, the pool pings before returning
        # connections. Verify the pool is functional.
        rows = await execute_raw("SELECT 1 AS ok", using=pool_db.name)
        assert rows[0]["ok"] == 1


@pytest.mark.asyncio
async def test_query_pool_acquire_timeout_raises_database_error(postgres_url):
    name = f"timeout_{uuid.uuid4().hex}"
    settings = PoolSettings(max_connections=1, acquire_timeout=0.5)
    async with db.connect(postgres_url, name=name, settings=settings) as database:
        async with db.atomic(using=name):
            # An explicit database client bypasses the transaction and needs a pool slot.
            with pytest.raises(DatabaseError, match="pool timed out") as error:
                await execute_raw("SELECT 1", client=database)
            assert isinstance(error.value, RuntimeError)
