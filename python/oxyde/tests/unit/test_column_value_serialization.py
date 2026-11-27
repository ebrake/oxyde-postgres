"""Check that core write-value serialization matches the Python serializer."""

from __future__ import annotations

from datetime import date, datetime, time, timedelta, timezone
from decimal import Decimal
from enum import Enum, IntEnum, StrEnum
from types import MappingProxyType
from typing import Any
from uuid import UUID

import pytest

from oxyde import Field, Model
from oxyde._msgpack import msgpack
from oxyde.core.types import TYPE_REGISTRY, TypeDescriptor
from oxyde.db.pool import _msgpack_encoder
from oxyde.models.metadata import db_column_for
from oxyde.models.registry import registered_tables
from oxyde.queries.base import _field_columns, _serialize_column_values
from oxyde.queries.expressions import F, Value, _serialize_value_for_ir
from oxyde.queries.insert import InsertQuery
from oxyde.tests.helpers import StubExecuteClient


ROW_ID = UUID("12345678-1234-5678-9abc-def012345678")
CREATED_AT = datetime(2026, 11, 27, 14, 15, 16, 700, tzinfo=timezone.utc)


class WriteModel(Model):
    id: UUID = Field(db_pk=True, db_column="row_id")
    title: str = Field(db_column="display_title")
    created_at: datetime
    amount: Decimal
    token: UUID

    class Meta:
        is_table = True


class ColumnRef(F, Enum):
    """A reusable column reference; F takes precedence over Enum."""

    ID = "id"


def _fallback(value: Any) -> Any:
    return _serialize_value_for_ir(value, WriteModel)


def test_write_values_match_python_ir_and_wire_bytes(monkeypatch):
    class State(Enum):
        READY = "ready"
        DATED = CREATED_AT

    class Count(IntEnum):
        ONE = 1

    class Label(StrEnum):
        TITLE = "title"

    class ListEnum(list, Enum):
        DATES = [CREATED_AT]

    class Text(str):
        pass

    class CustomUUID(UUID):
        pass

    class RegisteredUUID(UUID):
        pass

    monkeypatch.setitem(
        TYPE_REGISTRY, RegisteredUUID, TypeDescriptor("uuid", lambda value: value.hex)
    )

    values = {
        "title": "new title",
        "null": None,
        "boolean": True,
        "int": 17,
        "big_int": 2**63,
        "negative_int": -(2**62),
        "float": 3.25,
        "bytes": b"binary",
        "bytearray": bytearray(b"mutable"),
        "naive_datetime": CREATED_AT.replace(tzinfo=None),
        "created_at": CREATED_AT,
        "date": date(2026, 11, 27),
        "time": time(9, 30),
        "time_with_offset": time(9, 30, tzinfo=timezone(timedelta(hours=-3))),
        "timedelta": timedelta(days=-3, microseconds=1),
        "id": ROW_ID,
        "uuid_subclass": RegisteredUUID(str(ROW_ID)),
        "amount": Decimal("1.50"),
        "decimal_exponent": Decimal("1E+2"),
        "decimal_nan": Decimal("NaN"),
        "enum": State.READY,
        "enum_with_datetime": State.DATED,
        "int_enum": Count.ONE,
        "str_enum": Label.TITLE,
        "list_enum": ListEnum.DATES,
        "str_subclass": Text("text"),
        "list": [CREATED_AT, ROW_ID, F("title")],
        "tuple": (CREATED_AT, ROW_ID),
        "dict": {"dt": CREATED_AT, "uuid": ROW_ID, "field": F("title")},
        "expression": F("amount") + 1,
        "literal": Value("title"),
        "column_ref": ColumnRef.ID,
    }
    fallback = _fallback
    expected = {
        db_column_for(WriteModel, field): fallback(value)
        for field, value in values.items()
    }
    columns = _field_columns(WriteModel)
    actual = _serialize_column_values(values, columns, fallback)

    assert actual == expected
    assert actual["column_ref"] == {"__expr__": {"type": "column", "name": "row_id"}}
    assert actual["enum_with_datetime"] is CREATED_AT
    assert actual["str_subclass"] is values["str_subclass"]
    assert msgpack.packb(actual, default=_msgpack_encoder) == msgpack.packb(
        expected, default=_msgpack_encoder
    )

    unregistered_uuid = CustomUUID(str(ROW_ID))
    payload = _serialize_column_values({"id": unregistered_uuid}, columns, fallback)
    assert payload["row_id"] is fallback(unregistered_uuid) is unregistered_uuid


def test_foreign_key_field_maps_to_current_column():
    class Author(Model):
        id: int = Field(db_pk=True)

        class Meta:
            is_table = True

    class Article(Model):
        id: int = Field(db_pk=True)
        author: Author

        class Meta:
            is_table = True

    registered_tables()
    assert _field_columns(Article) == {"author": "author_id"}
    assert (
        _serialize_column_values(
            {"author": 7}, _field_columns(Article), _serialize_value_for_ir
        )
        == {db_column_for(Article, "author"): 7}
        == {"author_id": 7}
    )


def test_fallback_error_and_duplicate_column_order():
    error = ValueError("invalid write value")

    def fail(_value):
        raise error

    values = {"title": object(), "row_id": 1, "display_title": "last value"}
    columns = _field_columns(WriteModel)

    assert list(_serialize_column_values(values, columns, fail).items()) == [
        ("display_title", "last value"),
        ("row_id", 1),
    ]
    with pytest.raises(ValueError) as caught:
        _serialize_column_values(values, columns, fail, map_first=False)
    assert caught.value is error
    with pytest.raises(ValueError) as caught:
        _serialize_column_values({"title": object()}, columns, fail)
    assert caught.value is error


@pytest.mark.asyncio
async def test_insert_and_update_send_serialized_values_to_client():
    values: dict[str, Any] = {
        "title": "a title",
        "created_at": CREATED_AT,
        "amount": Decimal("1.50"),
        "token": ROW_ID,
    }
    expected = {
        "display_title": "a title",
        "created_at": CREATED_AT.isoformat(),
        "amount": "1.50",
        "token": str(ROW_ID),
    }
    client = StubExecuteClient(
        [{"affected": 1, "inserted_ids": [str(ROW_ID)]}, {"affected": 1}]
    )

    await WriteModel.objects.create(id=ROW_ID, **values, client=client)
    await WriteModel.objects.filter(id=ROW_ID).update(**values, client=client)

    inserted, updated = client.calls
    assert inserted["values"] == {"row_id": str(ROW_ID), **expected}
    assert updated["values"] == expected


def test_conflict_update_serializes_and_maps_explicit_values():
    query = (
        InsertQuery(WriteModel)
        .values(id=ROW_ID)
        .on_conflict_update([], {"title": F("title"), "amount": Decimal("1.50")})
    )

    assert query.to_ir()["on_conflict"]["update_values"] == {
        "display_title": _serialize_value_for_ir(F("title"), WriteModel),
        "amount": "1.50",
    }


@pytest.mark.asyncio
async def test_bulk_update_serializes_values_and_retains_uuid_filter():
    obj = WriteModel(
        id=ROW_ID,
        title="updated",
        created_at=CREATED_AT,
        amount=Decimal("1.50"),
        token=ROW_ID,
    )
    obj.model_fields_set.remove("title")
    client = StubExecuteClient([{"affected": 1}])

    affected = await WriteModel.objects.bulk_update(
        [obj], ["title", "created_at", "amount", "token"], client=client
    )

    assert affected == 1
    assert "title" in obj.model_fields_set
    assert "display_title" not in obj.model_fields_set
    (row,) = client.calls[0]["bulk_update"]["rows"]
    assert row == {
        "filters": {"row_id": ROW_ID},
        "values": {
            "display_title": "updated",
            "created_at": CREATED_AT.isoformat(),
            "amount": "1.50",
            "token": str(ROW_ID),
        },
    }
    assert msgpack.unpackb(msgpack.packb(row, default=_msgpack_encoder), raw=False) == {
        **row,
        "filters": {"row_id": str(ROW_ID)},
    }


def test_column_map_is_cached_and_holds_only_renamed_fields():
    columns = _field_columns(WriteModel)

    assert columns == {"id": "row_id", "title": "display_title"}
    assert _field_columns(WriteModel) is columns


def test_non_dict_mapping_is_accepted():
    values = MappingProxyType({"title": "a title", "amount": Decimal("1.50")})

    assert _serialize_column_values(values, _field_columns(WriteModel), _fallback) == {
        "display_title": "a title",
        "amount": "1.50",
    }


def test_write_serialization_respects_overridden_items():
    class FilteredRow(dict):
        def items(self):
            return {key: value for key, value in super().items() if key != "amount"}.items()

    row = FilteredRow(title="a title", amount=Decimal("1.50"))
    expected = {"display_title": "a title"}

    assert _serialize_column_values(row, _field_columns(WriteModel), _fallback) == expected
    assert InsertQuery(WriteModel).bulk_values([row]).to_ir()["bulk_values"] == [expected]


@pytest.mark.parametrize(
    "value",
    [Decimal("1.50"), CREATED_AT, ROW_ID, "text", 7],
    ids=lambda value: type(value).__name__,
)
@pytest.mark.parametrize("map_first", [True, False])
def test_registered_serializer_replaces_builtin(monkeypatch, value, map_first):
    builtin = TYPE_REGISTRY[type(value)]
    monkeypatch.setitem(
        TYPE_REGISTRY,
        type(value),
        TypeDescriptor(builtin.category, lambda item: f"custom:{item}"),
    )

    payload = _serialize_column_values(
        {"amount": value}, _field_columns(WriteModel), _fallback, map_first=map_first
    )

    assert payload == {"amount": f"custom:{value}"} == {"amount": _fallback(value)}


def test_removed_builtin_serializer_leaves_value_unchanged(monkeypatch):
    monkeypatch.delitem(TYPE_REGISTRY, Decimal)
    amount = Decimal("1.50")

    payload = _serialize_column_values(
        {"amount": amount}, _field_columns(WriteModel), _fallback
    )

    assert payload["amount"] is amount


@pytest.mark.asyncio
async def test_update_applies_registered_decimal_serializer(monkeypatch):
    monkeypatch.setitem(
        TYPE_REGISTRY,
        Decimal,
        TypeDescriptor("numeric", lambda value: str(value.normalize())),
    )
    client = StubExecuteClient([{"affected": 1}])

    await WriteModel.objects.filter(id=ROW_ID).update(
        amount=Decimal("1.50"), client=client
    )

    assert client.calls[0]["values"] == {"amount": "1.5"}


@pytest.mark.asyncio
async def test_update_copies_column_named_by_f_enum():
    client = StubExecuteClient([{"affected": 1}])

    await WriteModel.objects.filter(id=ROW_ID).update(
        token=ColumnRef.ID, client=client
    )

    assert client.calls[0]["values"] == {
        "token": {"__expr__": {"type": "column", "name": "row_id"}}
    }
