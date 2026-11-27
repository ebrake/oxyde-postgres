"""Native SELECT rows and the MessagePack-only client adapter."""

from datetime import date, datetime, time, timedelta, timezone
from decimal import Decimal
from enum import StrEnum
from typing import Annotated, Literal
from uuid import UUID

import pytest
import pytest_asyncio
from pydantic import BeforeValidator, Json, field_validator
from pydantic_core import core_schema

from oxyde import Field, Model, atomic
from oxyde.core.ir import build_delete_ir
from oxyde.core.types import TYPE_REGISTRY, TypeDescriptor
from oxyde.db import AsyncTransaction
from oxyde.models.registry import register_table
from oxyde.queries._result_set import fetch_result
from oxyde.queries.raw import execute_raw

from .conftest import AllTypes


class BytesOnlyClient:
    def __init__(self, client):
        self.client = client

    async def execute(self, ir):
        return await self.client.execute(ir)


class NativeResultChild(Model):
    id: int = Field(db_pk=True)
    parent: AllTypes | None = None
    stamp: datetime = Field(db_type="TIMESTAMPTZ")
    elapsed: timedelta

    class Meta:
        is_table = True
        table_name = "native_result_child"


def assert_typed_equal(native, wire):
    assert type(native) is type(wire)
    assert native == wire
    if isinstance(native, Model):
        for key in type(native).model_fields:
            assert_typed_equal(getattr(native, key), getattr(wire, key))
    elif isinstance(native, dict):
        for key, value in native.items():
            assert_typed_equal(value, wire[key])
    elif isinstance(native, (list, tuple)):
        for left, right in zip(native, wire, strict=True):
            assert_typed_equal(left, right)
    elif isinstance(native, (datetime, time)):
        assert type(native.tzinfo) is type(wire.tzinfo)


@pytest.mark.asyncio
async def test_native_select_matches_messagepack_models_values_and_joins(db):
    register_table(NativeResultChild, overwrite=True)
    token = UUID(int=50)
    parent = await AllTypes.objects.create(
        using=db.name,
        int_val=7,
        str_val="typed",
        float_val=1.5,
        bool_val=True,
        datetime_val=datetime(2026, 11, 26, 13, 4, 5, 6),
        date_val=date(2026, 11, 26),
        time_val=time(13, 4, 5, 6),
        uuid_val=token,
        decimal_val=Decimal("1.2300"),
        json_val={"x": [1, "text", None]},
        str_list=["one", "two"],
        int_list=[1, 2],
        decimal_list=[Decimal("1.23"), Decimal("-4.56")],
    )
    await execute_raw(
        "CREATE TABLE native_result_child (id bigint PRIMARY KEY, parent_id bigint, "
        "stamp timestamptz, elapsed bigint)",
        client=db,
    )
    await execute_raw(
        "INSERT INTO native_result_child VALUES "
        "(1, $1, '2026-11-26 13:04:05.000006+02', 1234567), "
        "(2, $1, '2026-11-26 13:04:05.000006+02', 2000000), "
        "(3, NULL, '2026-11-26 13:04:05.000006+02', 0)",
        [parent.id],
        client=db,
    )

    wire = BytesOnlyClient(db)
    all_types = AllTypes.objects.filter(id=parent.id)
    joined = NativeResultChild.objects.join("parent").order_by("id")
    for query in (
        all_types,
        all_types.values(),
        all_types.values_list("uuid_val", "decimal_val"),
        all_types.values_list("uuid_val", flat=True),
        joined,
        joined.values(),
        joined.values_list(),
        joined.values_list("stamp", "elapsed"),
    ):
        assert_typed_equal(await query.all(client=db), await query.all(client=wire))

    for client in (db, wire):
        first, sibling, orphan = await joined.all(client=client)
        assert first.parent is not None
        assert first.parent is sibling.parent
        assert first.parent.uuid_val == token
        assert orphan.parent is None
        assert first.stamp.tzinfo is timezone.utc


@pytest.mark.asyncio
async def test_select_uses_native_pool_transaction_and_atomic_context(db, monkeypatch):
    saved = await AllTypes.objects.create(using=db.name, int_val=17)

    async def fail_if_bytes(*_args, **_kwargs):
        pytest.fail("SELECT unexpectedly used execute()")

    monkeypatch.setattr(type(db), "execute", fail_if_bytes)
    monkeypatch.setattr(AsyncTransaction, "execute", fail_if_bytes)
    query = AllTypes.objects.filter(id=saved.id).values_list("int_val", flat=True)
    assert await query.all(client=db) == [17]
    async with AsyncTransaction(db) as transaction:
        assert await query.all(client=transaction) == [17]
    async with atomic(using=db.name) as context:
        assert await query.all(client=context) == [17]
        assert await query.all(using=db.name) == [17]


class WireAmount(StrEnum):
    EXACT = "1.2300"


def require_text(value):
    assert type(value) is str
    return value


class RegisteredUUID(UUID):
    @classmethod
    def __get_pydantic_core_schema__(cls, _source, _handler):
        return core_schema.no_info_plain_validator_function(
            lambda value: cls(require_text(value))
        )


def require_uuid(value):
    assert type(value) is UUID
    return value


class WireModel(Model):
    id: int = Field(db_pk=True)
    external: str = Field(db_type="UUID")
    amount: WireAmount = Field(db_type="NUMERIC(10,4)")
    fixed: Literal["1.2300"] = Field(db_type="NUMERIC(10,4)")
    json_amount: Json[Decimal] = Field(db_type="NUMERIC(10,4)")
    date_text: str = Field(db_type="DATE")
    timestamp_text: str = Field(db_type="TIMESTAMP")
    custom: RegisteredUUID = Field(db_type="UUID")
    native: Annotated[UUID, BeforeValidator(require_uuid)]
    elapsed: timedelta
    elapsed_items: list[timedelta]

    @field_validator("external", "amount", "fixed", mode="before")
    @classmethod
    def validate_wire_fields(cls, value):
        return require_text(value)

    class Meta:
        is_table = True
        table_name = "native_result_wire"


@pytest_asyncio.fixture
async def wire_model_row(db, monkeypatch):
    monkeypatch.setitem(TYPE_REGISTRY, RegisteredUUID, TypeDescriptor("uuid", str))
    register_table(WireModel, overwrite=True)
    await execute_raw(
        "CREATE TABLE native_result_wire (id bigint PRIMARY KEY, external uuid, "
        "amount numeric(10,4), fixed numeric(10,4), custom uuid, native uuid, "
        "elapsed bigint, elapsed_items bigint[], json_amount numeric(10,4), "
        "date_text date, timestamp_text timestamp)",
        client=db,
    )
    micros = 2**53 + 1
    await execute_raw(
        "INSERT INTO native_result_wire VALUES "
        "(1, $1::uuid, 1.23, 1.23, $1::uuid, $1::uuid, $2, ARRAY[$2, -$2], "
        "1.23, '2024-01-02', '2024-01-03 04:05:06')",
        [str(UUID(int=7)), micros],
        client=db,
    )
    return micros


@pytest.mark.asyncio
async def test_wire_model_fields_and_native_before_validators(db, wire_model_row):
    micros = wire_model_row
    for client in (db, BytesOnlyClient(db)):
        [row] = await WireModel.objects.all(client=client)
        assert row.external == str(UUID(int=7))
        assert row.amount is WireAmount.EXACT
        assert row.fixed == "1.2300"
        assert_typed_equal(row.json_amount, Decimal("1.2300"))
        assert row.custom == RegisteredUUID(int=7)
        assert row.native == UUID(int=7)
    [row] = await WireModel.objects.all(client=db)
    assert row.elapsed == timedelta(microseconds=micros)
    assert row.elapsed_items == [
        timedelta(microseconds=micros),
        timedelta(microseconds=-micros),
    ]


@pytest.mark.asyncio
@pytest.mark.usefixtures("wire_model_row")
async def test_union_preserves_first_projection_wire_values(db):
    base = WireModel.objects.query()
    fields = tuple(
        name for name in WireModel.model_fields if name not in {"external", "native"}
    )
    query = base.select("external", "native", *fields).union_all(
        base.select("native", "external", *fields)
    )
    projection = base.select("external").union_all(base.select("native"))
    token = str(UUID(int=7))
    for client in (db, BytesOnlyClient(db)):
        rows = await query.all(client=client)
        assert_typed_equal([row.external for row in rows], [token, token])
        assert_typed_equal(
            await projection.values("external").all(client=client),
            [{"external": token}, {"external": token}],
        )
        assert_typed_equal(
            await projection.values_list("external", flat=True).all(client=client),
            [token, token],
        )


@pytest.mark.asyncio
@pytest.mark.usefixtures("wire_model_row")
async def test_union_decodes_widened_wire_columns(db):
    base = WireModel.objects.query()
    fields = tuple(
        name
        for name in WireModel.model_fields
        if name not in {"date_text", "timestamp_text"}
    )
    query = (
        base.select("date_text", "timestamp_text", *fields)
        .union_all(base.select("timestamp_text", "date_text", *fields))
        .order_by("date_text")
    )
    projection = (
        base.select("date_text")
        .union_all(base.select("timestamp_text"))
        .order_by("date_text")
    )
    expected = ["2024-01-02T00:00:00", "2024-01-03T04:05:06"]
    for client in (db, BytesOnlyClient(db)):
        rows = await query.all(client=client)
        assert_typed_equal([row.date_text for row in rows], expected)
        assert_typed_equal(
            await projection.values_list("date_text", flat=True).all(client=client),
            expected,
        )


@pytest.mark.asyncio
async def test_raw_result_rows_keep_wire_fallbacks_and_duplicate_keys(db):
    query = {
        "proto": 1,
        "op": "raw",
        "table": "",
        "result_kinds": True,
        "sql": """
            SELECT 1 AS label, 2 AS label, '0001-01-02 BC'::date AS bc,
                '10000-01-02'::timestamp AS future, 'infinity'::date AS infinity,
                '24:00'::time AS midnight, '13:30:00+02:00:01'::timetz AS offset,
                '1.2300'::numeric AS scale, ARRAY[[1, 2], [3, 4]] AS matrix,
                ARRAY['2026-11-28'::date, 'infinity', NULL] AS dates
            FROM generate_series(1, 2)
        """,
    }
    native = await fetch_result(db, query)
    wire = await fetch_result(BytesOnlyClient(db), query)
    assert len(native) == len(wire) == 2
    assert native.columns == wire.columns
    rows = native.dicts(native.columns)
    assert_typed_equal(rows, wire.dicts(wire.columns))
    assert rows[0]["label"] == 2
    assert rows[0]["matrix"] is None
    assert rows[0]["dates"] == [date(2026, 11, 28), "infinity", None]
    assert rows[0]["scale"].as_tuple().exponent == -4
    assert rows[0]["offset"].utcoffset() == timedelta(hours=2, seconds=1)
    assert list(rows[0])[2] is list(rows[1])[2]
    assert native.tuples([0, 1, None]) == [(1, 2, None)] * 2
    assert native.column(0) == [1, 1]
    assert native.relation("missing") is None
    with pytest.raises(ValueError, match="requires a SELECT or Raw operation"):
        await db.execute_rows(build_delete_ir(table="all_types"))
