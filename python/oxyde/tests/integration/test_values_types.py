"""Type parity between projected rows and hydrated models."""

from datetime import date, time, timedelta

import pytest

from .conftest import TemporalArray


@pytest.mark.asyncio
async def test_temporal_arrays_round_trip(db):
    fields = {
        "durations": [timedelta(seconds=2, microseconds=3), timedelta(microseconds=-1)],
        "dates": [date(2026, 11, 26)],
        "times": [time(13, 4, 5, 6)],
    }
    await TemporalArray.objects.create(using=db.name, **fields)
    [row] = await TemporalArray.objects.all(using=db.name)
    for field, value in fields.items():
        assert getattr(row, field) == value
