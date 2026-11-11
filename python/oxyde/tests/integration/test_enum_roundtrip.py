from __future__ import annotations

from enum import Enum

import pytest
import pytest_asyncio

from oxyde import AsyncDatabase, Field, Model, disconnect_all
from oxyde.db.schema import create_tables
from oxyde.models.registry import clear_registry, register_table
from oxyde.queries.raw import execute_raw


class LiveStatus(str, Enum):
    DRAFT = "draft"
    PUBLISHED = "published"
    ARCHIVED = "archived"


class EnumRoundTrip(Model):
    id: int | None = Field(default=None, db_pk=True)
    status: LiveStatus = Field(default=LiveStatus.DRAFT)
    labels: list[LiveStatus] | None = Field(default=None, db_nullable=True)

    class Meta:
        is_table = True
        table_name = "enum_roundtrip_test"


@pytest_asyncio.fixture(params=["postgres"])
async def enum_db(request, postgres_database_url):
    dialect = request.param
    url = postgres_database_url

    clear_registry()
    register_table(EnumRoundTrip, overwrite=True)

    database = AsyncDatabase(
        url, name=f"enum_roundtrip_{dialect}", overwrite=True
    )
    await database.connect()

    try:
        await _drop_enum_roundtrip_schema(database, dialect)
        await create_tables(database)
        yield database
    finally:
        await _drop_enum_roundtrip_schema(database, dialect)
        await disconnect_all()
        clear_registry()


async def _drop_enum_roundtrip_schema(database: AsyncDatabase, dialect: str) -> None:
    await execute_raw(
        "DROP TABLE IF EXISTS enum_roundtrip_test CASCADE", client=database
    )
    await execute_raw("DROP TYPE IF EXISTS live_status_enum CASCADE", client=database)


class TestEnumRoundTrip:
    @pytest.mark.asyncio
    async def test_create_filter_update_fetch_scalar_and_array(self, enum_db):
        created = await EnumRoundTrip.objects.create(
            status=LiveStatus.DRAFT,
            labels=[LiveStatus.DRAFT, LiveStatus.PUBLISHED],
            using=enum_db.name,
        )

        fetched = await EnumRoundTrip.objects.get(id=created.id, using=enum_db.name)
        assert fetched.status is LiveStatus.DRAFT
        assert fetched.labels == [LiveStatus.DRAFT, LiveStatus.PUBLISHED]

        matched = await EnumRoundTrip.objects.filter(
            status=LiveStatus.DRAFT
        ).get(using=enum_db.name)
        assert matched.id == created.id

        affected = await EnumRoundTrip.objects.filter(id=created.id).update(
            status="published",
            labels=["published", LiveStatus.ARCHIVED],
            using=enum_db.name,
        )
        assert affected == 1

        updated = await EnumRoundTrip.objects.get(id=created.id, using=enum_db.name)
        assert updated.status is LiveStatus.PUBLISHED
        assert updated.labels == [LiveStatus.PUBLISHED, LiveStatus.ARCHIVED]
