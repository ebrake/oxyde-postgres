"""Type parity between projected rows and hydrated models."""

from datetime import date, datetime, time, timedelta
from decimal import Decimal
from uuid import UUID

import pytest

from oxyde import Avg, Coalesce, Count, F, Max, Min, Sum, atomic

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
            decimal_val=Decimal(price),
            date_val=date(2026, 11, day),
            using=db.name,
        )
    [row] = await (
        AllTypes.objects.annotate(
            total=Sum("int_val"),
            average=Avg("int_val"),
            price=Avg("decimal_val"),
            latest=Max("date_val"),
        )
        .values("total", "average", "price", "latest")
        .all(using=db.name)
    )
    assert row == {
        "total": 5,
        "average": 2.5,
        "price": Decimal("1.875"),
        "latest": date(2026, 11, 2),
    }
    assert [type(row[key]) for key in ("total", "average", "price", "latest")] == [
        int,
        float,
        Decimal,
        date,
    ]


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
