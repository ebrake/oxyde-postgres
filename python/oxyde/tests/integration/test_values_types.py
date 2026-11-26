"""Type parity between projected rows and hydrated models."""

from datetime import date, datetime, time, timedelta, timezone
from decimal import Decimal
from typing import Annotated
from uuid import UUID

import msgpack
import pytest

from oxyde import (
    Avg,
    Coalesce,
    Count,
    F,
    Field,
    Greatest,
    Max,
    Min,
    Model,
    RawSQL,
    Sum,
    atomic,
)
from oxyde.models.registry import register_table
from oxyde.queries.raw import execute_raw

from .conftest import (
    AllTypes,
    ArrayDoc,
    BytesModel,
    NullableTypes,
    TdModel,
    TemporalArray,
    Tenant,
    TenantItem,
)

UUID_VALUE = UUID("11111111-2222-4333-8444-555555555555")


@pytest.mark.asyncio
async def test_temporal_arrays_round_trip(db):
    durations = [timedelta(seconds=2, microseconds=3), timedelta(microseconds=-1)]
    dates = [date(2026, 11, 26)]
    times = [time(13, 4, 5, 6)]
    await TemporalArray.objects.create(
        using=db.name, durations=durations, dates=dates, times=times
    )
    [row] = await TemporalArray.objects.all(using=db.name)
    assert row.durations == durations
    assert row.dates == dates
    assert row.times == times
    [projected] = await TemporalArray.objects.values().all(using=db.name)
    assert_matches_model(projected, row)


def assert_matches_model(projected, model):
    for field, value in projected.items():
        attr = getattr(model, field)
        assert value == attr, field
        assert type(value) is type(attr), field
        if isinstance(value, list):
            assert [type(item) for item in value] == [type(item) for item in attr], (
                field
            )


@pytest.mark.asyncio
@pytest.mark.parametrize(
    ("model", "fields"),
    [
        (
            AllTypes,
            {
                "int_val": 42,
                "str_val": "text",
                "float_val": 1.5,
                "bool_val": True,
                "datetime_val": datetime(2026, 11, 26, 13, 4, 5, 6),
                "date_val": date(2026, 11, 26),
                "time_val": time(13, 4, 5, 6),
                "uuid_val": UUID_VALUE,
                "decimal_val": Decimal("12.3456"),
                "json_val": {"x": [1, True, "text"]},
                "str_list": ["first", "second"],
                "int_list": [1, 2],
                "decimal_list": [Decimal("1.23"), Decimal("-4.56")],
            },
        ),
        (NullableTypes, {"int_val": None}),
        (
            ArrayDoc,
            {
                "ids": [UUID_VALUE],
                "times": [datetime(2026, 11, 26, 13, 4, 5, 6)],
                "tags": ["first", None],
                "amounts": [Decimal("1.23")],
            },
        ),
        (BytesModel, {"data": b"\x00\xff"}),
    ],
)
async def test_values_match_hydrated_field_types(db, model, fields):
    await model.objects.create(using=db.name, **fields)
    [row] = await model.objects.all(using=db.name)
    [values] = await model.objects.values().all(using=db.name)
    assert values.keys() == model.model_fields.keys()
    assert_matches_model(values, row)


@pytest.mark.asyncio
async def test_typed_terminals_and_union(db):
    left = await AllTypes.objects.create(uuid_val=UUID_VALUE, using=db.name)
    other_uuid = UUID(int=20)
    right = await AllTypes.objects.create(uuid_val=other_uuid, using=db.name)

    query = AllTypes.objects.filter(id=left.id)
    assert await query.values("uuid_val").first(using=db.name) == {
        "uuid_val": UUID_VALUE
    }
    assert await query.values_list("id", "uuid_val").get(using=db.name) == (
        left.id,
        UUID_VALUE,
    )

    left_arm = query.values_list("uuid_val", flat=True)
    right_arm = AllTypes.objects.filter(id=right.id).values_list("uuid_val", flat=True)
    assert await left_arm.get(using=db.name) == UUID_VALUE
    assert set(await left_arm.union(right_arm).all(using=db.name)) == {
        UUID_VALUE,
        other_uuid,
    }
    empty = query.filter(id=-1)
    assert await empty.values().all(using=db.name) == []
    assert await empty.values_list("uuid_val", flat=True).first(using=db.name) is None


@pytest.mark.asyncio
async def test_typed_join_and_grouped_long_label(db):
    tenant = await Tenant.objects.create(
        uuid=UUID_VALUE, name="Acme", duration=timedelta(seconds=90), using=db.name
    )
    label = (
        "customer_facing_reporting_label_that_exceeds_the_postgresql_identifier_limit"
    )
    await TenantItem.objects.create(
        tenant_uuid=tenant.uuid,
        exists=timedelta(seconds=15),
        customer_facing_reporting_label_that_exceeds_the_postgresql_identifier_limit="Acme West",
        using=db.name,
    )

    joined = TenantItem.objects.join("tenant").values()
    [row] = await joined.all(using=db.name)
    assert row["tenant__uuid"] == UUID_VALUE
    assert row["tenant__duration"] == timedelta(seconds=90)
    assert row["exists"] == timedelta(seconds=15)
    async with atomic(using=db.name):
        assert await joined.all(using=db.name) == [row]

    group = (label, "tenant__uuid", "tenant__duration")
    [grouped] = await (
        TenantItem.objects.filter()
        .group_by(*group)
        .annotate(n=Count("*"))
        .values(*group, "n")
        .all(using=db.name)
    )
    assert grouped == {
        label: "Acme West",
        "tenant__uuid": UUID_VALUE,
        "tenant__duration": timedelta(seconds=90),
        "n": 1,
    }


@pytest.mark.asyncio
async def test_typed_summary_annotations_and_shortcuts(db):
    first = timedelta(microseconds=2)
    last = timedelta(seconds=120, microseconds=3)
    for duration in (first, last):
        await TdModel.objects.create(duration=duration, using=db.name)
    query = TdModel.objects.filter()
    expected = {
        "duration": first,
        "maximum": last,
        "total": first + last,
        "average": (first + last) / 2,
    }
    [row] = await (
        query.annotate(
            duration=Min("duration"),
            maximum=Max("duration"),
            total=Sum("duration"),
            average=Avg("duration"),
        )
        .values(*expected)
        .all(using=db.name)
    )
    assert row == expected
    assert await query.min("duration", using=db.name) == first
    assert await query.max("duration", using=db.name) == last
    assert await query.sum("duration", using=db.name) == expected["total"]
    assert await query.avg("duration", using=db.name) == expected["average"]

    for number, price, day in [(2, "1.25", 1), (3, "2.50", 2)]:
        await AllTypes.objects.create(
            int_val=number,
            float_val=number / 2,
            decimal_val=Decimal(price),
            date_val=date(2026, 11, day),
            using=db.name,
        )
    [row] = await (
        AllTypes.objects.annotate(
            total=Sum("int_val"),
            average=Avg("int_val"),
            price=Avg("decimal_val"),
            total_price=Sum("decimal_val"),
            float_sum=Sum("float_val"),
            float_avg=Avg("float_val"),
            latest=Max("date_val"),
        )
        .values(
            "total",
            "average",
            "price",
            "total_price",
            "float_sum",
            "float_avg",
            "latest",
        )
        .all(using=db.name)
    )
    assert row == {
        "total": 5,
        "average": 2.5,
        "price": Decimal("1.875"),
        "total_price": Decimal("3.75"),
        "float_sum": 2.5,
        "float_avg": 1.25,
        "latest": date(2026, 11, 2),
    }
    assert [type(value) for value in row.values()] == [
        int,
        float,
        Decimal,
        Decimal,
        float,
        float,
        date,
    ]
    assert type(await AllTypes.objects.sum("int_val", using=db.name)) is int
    assert type(await AllTypes.objects.avg("int_val", using=db.name)) is float
    assert await AllTypes.objects.sum("decimal_val", using=db.name) == Decimal("3.75")
    assert await AllTypes.objects.avg("decimal_val", using=db.name) == Decimal("1.875")

    await AllTypes.objects.create(int_val=2**63 - 1, bool_val=True, using=db.name)
    await AllTypes.objects.create(int_val=1, bool_val=True, using=db.name)
    overflow = Decimal(2**63)
    assert await AllTypes.objects.sum("int_val", using=db.name) == overflow + 5
    sums = await (
        AllTypes.objects.filter()
        .group_by("bool_val")
        .annotate(total=Sum("int_val"))
        .values_list("bool_val", "total")
        .all(using=db.name)
    )
    by_group = dict(sums)
    assert by_group == {False: 5, True: overflow}
    assert type(by_group[False]) is int
    assert type(by_group[True]) is Decimal


@pytest.mark.asyncio
async def test_scalar_aliases_preserve_uuid(db):
    await AllTypes.objects.create(uuid_val=UUID_VALUE, using=db.name)
    [uuids] = await (
        AllTypes.objects.annotate(
            copy=F("uuid_val"), first_uuid=Coalesce("uuid_val", None)
        )
        .values("copy", "first_uuid")
        .all(using=db.name)
    )
    assert uuids == {"copy": UUID_VALUE, "first_uuid": UUID_VALUE}


class TypedWireRow(Model):
    id: UUID = Field(db_pk=True)
    clock: time = Field(db_type="TIMETZ")
    clocks: list[time] = Field(db_type="TIMETZ[]")
    text_ids: list[Annotated[str, "wire"]] | None = Field(db_type="UUID[]")
    amount: Annotated[str, "wire"] | None = Field(db_type="NUMERIC(10,2)")
    backup: Decimal = Field(db_type="NUMERIC(12,2)")
    external_id: str = Field(db_type="UUID", db_column="typed_wire_row.external_uuid")

    class Meta:
        is_table = True
        table_name = "typed_wire_row"


@pytest.mark.asyncio
async def test_timetz_and_untyped_uuid_array_values_and_kinds(db):
    register_table(TypedWireRow, overwrite=True)
    await execute_raw(
        "CREATE TABLE typed_wire_row (id uuid PRIMARY KEY, clock timetz, "
        "clocks timetz[], text_ids uuid[], amount numeric(10,2), "
        'backup numeric(12,2), "typed_wire_row.external_uuid" uuid)',
        client=db,
    )
    await execute_raw(
        "INSERT INTO typed_wire_row VALUES ("
        "'11111111-2222-4333-8444-555555555555', '12:30:00.000001+02', "
        "ARRAY['12:30:00.000001+02'::timetz, NULL], "
        "ARRAY['11111111-2222-4333-8444-555555555555'::uuid], 1, 2, "
        "'11111111-2222-4333-8444-555555555555')",
        client=db,
    )

    aware_time = time(12, 30, 0, 1, tzinfo=timezone(timedelta(hours=2)))
    clocks = TypedWireRow.objects.values("clock", "clocks", "text_ids", "amount")
    [row] = await clocks.all(client=db)
    assert row == {
        "clock": aware_time,
        "clocks": [aware_time, None],
        "text_ids": [str(UUID_VALUE)],
        "amount": "1.00",
    }

    aggregated = TypedWireRow.objects.annotate(ids=RawSQL("array_agg(id)")).values(
        "ids"
    )
    assert await aggregated.all(client=db) == [{"ids": [UUID_VALUE]}]
    query_ir = aggregated._build_ir(for_models=False, result_kinds=True)
    assert msgpack.unpackb(await db.execute(query_ir)) == [
        ["ids"],
        [[[str(UUID_VALUE)]]],
        None,
        ["uuid[]"],
    ]

    [aliases] = await (
        TypedWireRow.objects.annotate(
            copy=F("amount"),
            pass_through=Coalesce("amount", "backup"),
            dotted_copy=F("external_id"),
            text_ids_copy=Coalesce("text_ids", []),
            widened=Greatest("amount", Decimal("1.5")),
        )
        .values("copy", "pass_through", "dotted_copy", "text_ids_copy", "widened")
        .all(client=db)
    )
    assert aliases == {
        "copy": "1.00",
        "pass_through": "1.00",
        "dotted_copy": str(UUID_VALUE),
        "text_ids_copy": [str(UUID_VALUE)],
        "widened": Decimal("1.5"),
    }
    assert await TypedWireRow.objects.min("typed_wire_row.amount", client=db) == "1.00"
    assert await TypedWireRow.objects.max("amount", client=db) == "1.00"
    [grouped] = await (
        TypedWireRow.objects.filter()
        .group_by("amount")
        .annotate(smallest=Min("typed_wire_row.amount"), largest=Max("amount"))
        .values("amount", "smallest", "largest")
        .all(client=db)
    )
    assert grouped == {"amount": "1.00", "smallest": "1.00", "largest": "1.00"}


@pytest.mark.asyncio
async def test_long_annotation_alias_keeps_wire_value(db):
    await AllTypes.objects.create(uuid_val=UUID_VALUE, using=db.name)
    alias = "é" * 32
    truncated = "é" * 31
    annotated = AllTypes.objects.annotate(**{alias: F("uuid_val")}).values(alias)
    assert await annotated.all(client=db) == [{truncated: str(UUID_VALUE)}]
    assert await annotated.values_list(alias, flat=True).all(client=db) == [None]
    assert await annotated.group_by(alias).all(client=db) == [
        {truncated: str(UUID_VALUE)}
    ]


class SpecialValues(Model):
    id: int = Field(db_pk=True)
    stamp: datetime
    day: date
    clock: time
    clocks: list[time] = Field(db_type="TIMETZ[]")
    amount: Decimal
    amounts: list[Decimal]
    duration: timedelta

    class Meta:
        is_table = True
        table_name = "special_values"


@pytest.mark.asyncio
async def test_special_temporal_and_numeric_values_are_preserved(db):
    register_table(SpecialValues, overwrite=True)
    await execute_raw(
        "CREATE TABLE special_values (id bigint PRIMARY KEY, stamp timestamp, day date, "
        "clock time, clocks timetz[], amount numeric, amounts numeric[], duration bigint)",
        client=db,
    )
    await execute_raw(
        "INSERT INTO special_values VALUES (1, 'infinity', '-infinity', '24:00', "
        "ARRAY['24:00+02'::timetz, '00:00+02'::timetz], 'NaN', "
        "ARRAY['Infinity'::numeric, '-Infinity'::numeric, NULL, 1.25], 9007199254740992)",
        client=db,
    )
    query = SpecialValues.objects.filter()
    [row] = await query.values(
        "stamp", "day", "clock", "clocks", "amount", "amounts"
    ).all(client=db)
    assert row["stamp"] == "infinity"
    assert row["day"] == "-infinity"
    assert row["clock"] == "24:00:00"
    assert row["clocks"] == [
        "24:00:00+02:00",
        time(0, tzinfo=timezone(timedelta(hours=2))),
    ]
    assert row["amount"].is_nan()
    assert row["amounts"] == [
        Decimal("Infinity"),
        Decimal("-Infinity"),
        None,
        Decimal("1.25"),
    ]
    assert (await query.sum("amount", client=db)).is_nan()
    assert (await query.avg("amount", client=db)).is_nan()
    [raw] = (
        await query.annotate(
            stamp_raw=RawSQL("'infinity'::timestamp"),
            day_raw=RawSQL("'-infinity'::date"),
            stamps=RawSQL("ARRAY['infinity'::timestamp, '2001-01-01'::timestamp]"),
            clocks_raw=RawSQL("ARRAY['24:00:00'::time, '00:00:00'::time]"),
            amounts_raw=RawSQL("ARRAY['Infinity'::numeric, '-Infinity'::numeric]"),
        )
        .values("stamp_raw", "day_raw", "stamps", "clocks_raw", "amounts_raw")
        .all(client=db)
    )
    assert raw == {
        "stamp_raw": "infinity",
        "day_raw": "-infinity",
        "stamps": ["infinity", datetime(2001, 1, 1)],
        "clocks_raw": ["24:00:00", time(0)],
        "amounts_raw": [Decimal("Infinity"), Decimal("-Infinity")],
    }

    for pk, micros in [(2, 2**53 + 1), (3, -(2**53 + 1))]:
        await execute_raw(
            "INSERT INTO special_values (id, duration, amount) VALUES ($1, $2, 12.50)",
            [pk, micros],
            client=db,
        )
    for pk, micros in [(1, 2**53), (2, 2**53 + 1), (3, -(2**53 + 1))]:
        subset = query.filter(id=pk)
        assert await subset.sum("duration", client=db) == timedelta(microseconds=micros)
        assert await subset.avg("duration", client=db) == timedelta(microseconds=micros)

    duration_sum = (
        query.filter(id=1)
        .annotate(total=Sum("duration"))
        .values_list("total", flat=True)
    )
    price_sum = (
        query.filter(id=2).annotate(total=Sum("amount")).values_list("total", flat=True)
    )
    totals = await duration_sum.union_all(price_sum).all(client=db)
    assert set(totals) == {Decimal(2**53), Decimal("12.50")}
    assert all(type(total) is Decimal for total in totals)
