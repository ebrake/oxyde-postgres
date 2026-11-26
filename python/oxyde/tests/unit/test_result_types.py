"""Conversion of tagged MessagePack query results."""

from datetime import date, datetime, time, timedelta, timezone
from decimal import Decimal
from uuid import UUID

import msgpack
import pytest

from oxyde import Avg, Field, Model, Sum
from oxyde.models.registry import register_table
from oxyde.queries._result_types import _RESULT_KINDS, convert_column, convert_rows
from oxyde.tests.helpers import StubExecuteClient

UUID_TEXT = "11111111-2222-4333-8444-555555555555"
UUID_VALUE = UUID(UUID_TEXT)

CASES = {
    "uuid": (UUID_TEXT, UUID_VALUE),
    "decimal": ("12.50", Decimal("12.50")),
    "datetime": (
        "2026-11-26T13:04:05+00:00",
        datetime(2026, 11, 26, 13, 4, 5, tzinfo=timezone.utc),
    ),
    "date": ("2026-11-26", date(2026, 11, 26)),
    "time": ("13:04:05.000001", time(13, 4, 5, 1)),
    "timedelta": (1.500001, timedelta(seconds=1, microseconds=500001)),
}


def test_pinned_result_kinds():
    assert _RESULT_KINDS == set(CASES) | {f"{kind}[]" for kind in CASES}


@pytest.mark.parametrize("kind", CASES)
def test_each_kind_converts_scalars_and_arrays(kind):
    wire, value = CASES[kind]
    result = convert_column(kind, [wire, None])
    assert result == [value, None]
    assert type(result[0]) is type(value)
    arrays = convert_column(f"{kind}[]", [[wire, None], None, []])
    assert arrays == [[value, None], None, []]
    assert type(arrays[0][0]) is type(value)


def test_non_finite_decimals_and_unrepresentable_dates():
    nan, positive, negative = convert_column(
        "decimal", ["NaN", "Infinity", "-Infinity"]
    )
    assert nan.is_nan()
    assert positive == Decimal("Infinity")
    assert negative == Decimal("-Infinity")
    assert convert_column("date", ["infinity", None, "2026-11-26"]) == [
        "infinity",
        None,
        date(2026, 11, 26),
    ]
    assert convert_column("date[]", [["infinity", "2026-11-26"]]) == [
        ["infinity", date(2026, 11, 26)],
    ]
    with pytest.raises(ValueError, match="Unknown result kind"):
        convert_column("unsupported", [])


def test_convert_rows_honors_positions_and_text_overrides():
    rows = [[UUID_TEXT, UUID_TEXT, "3.00", "untagged"], [None, None, None, "unchanged"]]
    convert_rows(rows, ["uuid", "uuid", "decimal", None], {1})
    assert rows == [
        [UUID_VALUE, UUID_TEXT, Decimal("3.00"), "untagged"],
        [None, None, None, "unchanged"],
    ]


class ResultRecord(Model):
    id: UUID = Field(db_pk=True)
    external_id: str = Field(db_type="UUID", db_column="external_uuid")
    duration: timedelta

    class Meta:
        is_table = True


class ResultLink(Model):
    id: int = Field(db_pk=True)
    record: ResultRecord | None = None

    class Meta:
        is_table = True


@pytest.mark.asyncio
async def test_tagged_values_through_execution_entry_points():
    payload = [
        ["id", "external_uuid", "duration"],
        [[UUID_TEXT, UUID_TEXT, 2.5]],
        None,
        ["uuid", "uuid", "timedelta"],
    ]
    values = ResultRecord.objects.values("id", "external_id", "duration")
    row = {
        "id": UUID_VALUE,
        "external_id": UUID_TEXT,
        "duration": timedelta(seconds=2.5),
    }
    stub = StubExecuteClient([payload] * 7)
    assert await values.all(client=stub) == [row]
    assert await values.fetch_rows(stub) == [row]
    assert await values.first(client=stub) == row
    assert await values.last(client=stub) == row
    assert await values.filter(id=UUID_VALUE).get(client=stub) == row
    assert await ResultRecord.objects.values_list("id", "external_id").all(
        client=stub
    ) == [(UUID_VALUE, UUID_TEXT)]
    assert await ResultRecord.objects.values_list("id", flat=True).all(client=stub) == [
        UUID_VALUE
    ]
    assert all(call["result_kinds"] is True for call in stub.calls)


@pytest.mark.asyncio
@pytest.mark.parametrize("tagged", [False, True])
async def test_relation_kinds_convert_data_before_flattening(tagged):
    register_table(ResultRecord, overwrite=True)
    related = {
        "columns": ["id", "external_id", "duration"],
        "data": {UUID_TEXT: [UUID_TEXT, UUID_TEXT, 2.5]},
        "refs": [UUID_TEXT, UUID_TEXT, None],
    }
    payload = [["id"], [[3], [4], [5]], {"record": related}]
    if tagged:
        related["kinds"] = ["uuid", "uuid", "timedelta"]
        payload.append(None)
    stub = StubExecuteClient([payload])
    rows = await ResultLink.objects.join("record").values().all(client=stub)
    for row, pk in zip(rows[:2], [3, 4], strict=True):
        assert row == {
            "id": pk,
            "record__id": UUID_VALUE if tagged else UUID_TEXT,
            "record__external_id": UUID_TEXT,
            "record__duration": timedelta(seconds=2.5) if tagged else 2.5,
        }
    assert rows[2] == {
        "id": 5,
        "record__id": None,
        "record__external_id": None,
        "record__duration": None,
    }


@pytest.mark.asyncio
async def test_aggregate_annotations_and_shortcuts_share_coercion():
    stub = StubExecuteClient(
        [
            [
                ["external_id", "avg_duration"],
                [["1000001", "500000.5"]],
                None,
                ["decimal", "decimal"],
            ],
            [["_sum"], [["1000001"]], None, ["decimal"]],
            [["_avg"], [["500000.5"]], None, ["decimal"]],
            [["_sum"], [["1000001"]]],
            [["_max"], [[1.25]]],
        ]
    )
    rows = await (
        ResultRecord.objects.annotate(
            external_id=Sum("duration"), avg_duration=Avg("duration")
        )
        .values("external_id", "avg_duration")
        .all(client=stub)
    )
    total = timedelta(microseconds=1_000_001)
    average = timedelta(microseconds=500_000.5)
    assert rows == [{"external_id": total, "avg_duration": average}]
    assert await ResultRecord.objects.sum("duration", client=stub) == total
    assert await ResultRecord.objects.avg("duration", client=stub) == average
    assert await ResultRecord.objects.sum("duration", client=stub) == total
    assert await ResultRecord.objects.max("duration", client=stub) == timedelta(
        seconds=1.25
    )
    assert all(call["result_kinds"] is True for call in stub.calls)


@pytest.mark.asyncio
async def test_raw_model_count_and_exists_do_not_request_kinds():
    payload = [["id", "external_uuid", "duration"], [[UUID_TEXT, UUID_TEXT, 2.5]]]
    stub = StubExecuteClient(
        [payload, payload, [["_count"], [[1]]], [["exists"], [[True]]]]
    )
    query = ResultRecord.objects.filter()
    assert await query.fetch_msgpack(stub) == msgpack.packb(payload)
    [model] = await query.fetch_models(stub)
    assert model.id == UUID_VALUE
    assert await query.count(client=stub) == 1
    assert await query.exists(client=stub) is True
    assert all("result_kinds" not in call for call in stub.calls)
