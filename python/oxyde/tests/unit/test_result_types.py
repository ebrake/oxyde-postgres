"""Conversion of tagged MessagePack query results."""

from datetime import date, datetime, time, timedelta, timezone
from decimal import Decimal
from typing import Annotated
from uuid import UUID

import msgpack
import pytest

from oxyde import Avg, Coalesce, F, Field, Greatest, Max, Min, Model, RawSQL, Sum
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
    [seconds_offset] = convert_column("time", ["12:30:00+02:00:30"])
    assert seconds_offset.utcoffset() == timedelta(hours=2, seconds=30)
    assert convert_column("time", ["24:00:00", "24:00:00+02:00"]) == [
        "24:00:00",
        "24:00:00+02:00",
    ]
    assert convert_column("decimal", [1, "1.5", "invalid"]) == [
        1,
        Decimal("1.5"),
        "invalid",
    ]
    assert type(convert_column("decimal", [1])[0]) is int
    mixed = convert_column("decimal", [1.25, "Infinity", "P0DT0.000001S", "2.50"])
    assert mixed == [
        1.25,
        Decimal("Infinity"),
        timedelta(microseconds=1),
        Decimal("2.50"),
    ]
    assert type(mixed[0]) is float
    with pytest.raises(ValueError, match="Unknown result kind"):
        convert_column("unsupported", [])


def test_convert_rows_honors_tag_positions():
    rows = [[UUID_TEXT, UUID_TEXT, "3.00", "untagged"], [None, None, None, "unchanged"]]
    convert_rows(rows, ["uuid", None, "decimal", None])
    assert rows == [
        [UUID_VALUE, UUID_TEXT, Decimal("3.00"), "untagged"],
        [None, None, None, "unchanged"],
    ]


class ResultRecord(Model):
    id: UUID = Field(db_pk=True)
    external_id: str = Field(db_type="UUID", db_column="external_uuid")
    external_ids: list[str] = Field(default_factory=list, db_type="UUID[]")
    nullable_ids: (
        Annotated[list[Annotated[str | None, "array item"]], "array"] | None
    ) = Field(default=None, db_type="UUID[]")
    nullable_id: Annotated[str, "scalar"] | None = Field(default=None, db_type="UUID")
    optional_first: Annotated[str | None, "scalar"] = Field(
        default=None, db_type="UUID"
    )
    number: int = 0
    fraction: float = 0.0
    price: Decimal = Decimal(0)
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
        ["uuid", None, "timedelta"],
    ]
    values = ResultRecord.objects.values("id", "external_id", "duration")
    row = {
        "id": UUID_VALUE,
        "external_id": UUID_TEXT,
        "duration": timedelta(seconds=2.5),
    }
    stub = StubExecuteClient([payload] * 10 + [[["id"], []]])
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
    assert await values.fetch_one(stub) == row
    assert await ResultRecord.objects.values_list("id").fetch_one(stub) == (UUID_VALUE,)
    assert (
        await ResultRecord.objects.values_list("id", flat=True).fetch_one(stub)
        == UUID_VALUE
    )
    assert (
        await ResultRecord.objects.values_list("id", flat=True).fetch_one(stub) is None
    )
    assert all(call["result_kinds"] is True for call in stub.calls)
    expected_refs = {
        "external_uuid",
        "external_ids",
        "nullable_ids",
        "nullable_id",
        "optional_first",
    }
    assert all(set(call["text_columns"]) == expected_refs for call in stub.calls)


@pytest.mark.asyncio
@pytest.mark.parametrize(
    ("annotation", "expected", "kind"),
    [
        (F("external_id"), UUID_TEXT, None),
        (F("external_uuid"), UUID_TEXT, None),
        (Coalesce("external_id", None), UUID_TEXT, None),
        (Greatest("id", Coalesce(F("external_id"), None)), UUID_TEXT, None),
        (Coalesce("external_id", RawSQL("NULL::uuid")), UUID_VALUE, "uuid"),
        (Min("external_id"), UUID_TEXT, None),
        (Max("external_id"), UUID_TEXT, None),
        (Max("id"), UUID_VALUE, "uuid"),
    ],
)
async def test_projected_annotations_follow_core_tags(annotation, expected, kind):
    payload = [["alias"], [[UUID_TEXT]], None, [kind]]
    stub = StubExecuteClient([payload, payload])
    query = ResultRecord.objects.annotate(alias=annotation)
    assert await query.values("alias").all(client=stub) == [{"alias": expected}]
    assert await query.values_list("alias", flat=True).all(client=stub) == [expected]


@pytest.mark.asyncio
async def test_terminal_min_and_max_keep_text_for_the_input_field():
    stub = StubExecuteClient(
        [
            [["_min"], [[UUID_TEXT]], None, None],
            [["_max"], [[UUID_TEXT]], None, [None]],
            [["_max"], [[UUID_TEXT]], None, ["uuid"]],
        ]
    )
    assert await ResultRecord.objects.min("external_id", client=stub) == UUID_TEXT
    assert await ResultRecord.objects.max("external_id", client=stub) == UUID_TEXT
    assert await ResultRecord.objects.max("id", client=stub) == UUID_VALUE


@pytest.mark.asyncio
@pytest.mark.parametrize(
    ("field", "wire"),
    [("external_ids", [UUID_TEXT]), ("nullable_ids", [UUID_TEXT, None])],
)
async def test_array_of_strings_keeps_text_in_values_modes(field, wire):
    payload = [[field], [[wire]], None, None]
    alias_payload = [["alias"], [[wire]], None, None]
    stub = StubExecuteClient([payload, payload, alias_payload])
    assert await ResultRecord.objects.values(field).all(client=stub) == [{field: wire}]
    assert await ResultRecord.objects.values_list(field, flat=True).all(
        client=stub
    ) == [wire]
    query = ResultRecord.objects.annotate(alias=Coalesce(field, []))
    assert await query.values("alias").all(client=stub) == [{"alias": wire}]


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
        related["kinds"] = ["uuid", None, "timedelta"]
        payload.append(None)
    stub = StubExecuteClient([payload])
    rows = await ResultLink.objects.join("record").values().all(client=stub)
    [call] = stub.calls
    [join] = call["joins"]
    assert f"{join['alias']}.external_uuid" in call["text_columns"]
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
@pytest.mark.parametrize(
    ("aggregate", "field", "wire", "kind", "expected"),
    [
        (
            Sum,
            "duration",
            "P0DT1.500001S",
            "timedelta",
            timedelta(microseconds=1_500_001),
        ),
        (
            Avg,
            "duration",
            "-P104249DT85654.740993S",
            "timedelta",
            timedelta(microseconds=-(2**53 + 1)),
        ),
        (Sum, "number", 3, None, 3),
        (
            Sum,
            "number",
            "9223372036854775808",
            "decimal",
            Decimal("9223372036854775808"),
        ),
        (Avg, "number", 1.5, None, 1.5),
        (Sum, "fraction", 3.5, None, 3.5),
        (Avg, "fraction", 1.75, None, 1.75),
        (Sum, "price", "3.75", "decimal", Decimal("3.75")),
        (Avg, "price", "1.875", "decimal", Decimal("1.875")),
    ],
)
async def test_aggregate_results_use_core_encoding(
    aggregate, field, wire, kind, expected
):
    method = "sum" if aggregate is Sum else "avg"
    payload = [["value"], [[wire]], None, [kind]]
    stub = StubExecuteClient([payload] * 3 + [[[f"_{method}"], [[wire]], None, [kind]]])
    query = ResultRecord.objects.annotate(value=aggregate(field))
    [row] = await query.values("value").all(client=stub)
    [(item,)] = await query.values_list("value").all(client=stub)
    [flat] = await query.values_list("value", flat=True).all(client=stub)
    terminal = await getattr(ResultRecord.objects, method)(field, client=stub)
    for actual in (row["value"], item, flat, terminal):
        assert actual == expected
        assert type(actual) is type(expected)
    assert all(call["result_kinds"] is True for call in stub.calls)


@pytest.mark.asyncio
async def test_legacy_terminal_max_decodes_duration_seconds():
    stub = StubExecuteClient([[["_max"], [[1.25]]]])
    assert await ResultRecord.objects.max("duration", client=stub) == timedelta(
        seconds=1.25
    )


@pytest.mark.asyncio
@pytest.mark.parametrize("kinds_tail", [[], [None, [None]]])
async def test_untagged_values_stay_raw_and_legacy_terminal_sum_converts(kinds_tail):
    payload = [["total"], [["1500000"]], *kinds_tail]
    stub = StubExecuteClient([payload, payload, payload, [["_sum"], [["1500000"]]]])
    query = ResultRecord.objects.annotate(total=Sum("duration"))
    total = timedelta(seconds=1.5)
    assert await query.values("total").all(client=stub) == [{"total": "1500000"}]
    assert await query.values_list("total").all(client=stub) == [("1500000",)]
    assert await query.values_list("total", flat=True).all(client=stub) == ["1500000"]
    assert await ResultRecord.objects.sum("duration", client=stub) == total


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
    assert all(
        "result_kinds" not in call and "text_columns" not in call for call in stub.calls
    )


@pytest.mark.asyncio
async def test_values_list_uses_the_last_position_for_repeated_labels():
    payload = [
        ["id", "external_uuid", "id"],
        [[str(UUID(int=1)), UUID_TEXT, UUID_TEXT]],
        None,
        ["uuid", None, "uuid"],
    ]
    stub = StubExecuteClient([payload] * 4)
    assert await ResultRecord.objects.values_list().all(client=stub) == [
        (UUID_VALUE, UUID_TEXT)
    ]
    assert await ResultRecord.objects.values_list("id", flat=True).all(client=stub) == [
        UUID_VALUE
    ]
    assert await ResultRecord.objects.values_list("duration", flat=True).all(
        client=stub
    ) == [None]
    assert await ResultRecord.objects.values_list("duration", "id").all(
        client=stub
    ) == [(None, UUID_VALUE)]


@pytest.mark.asyncio
async def test_values_list_flat_requires_a_single_result_field():
    stub = StubExecuteClient([[["id", "duration"], [[UUID_TEXT, 2.5]]]])
    with pytest.raises(ValueError, match="requires exactly one field"):
        await ResultRecord.objects.values_list(flat=True).all(client=stub)
