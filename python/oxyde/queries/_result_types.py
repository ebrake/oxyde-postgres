"""Restore Python values from the driver's column-wise MessagePack tags."""

from datetime import date, datetime, time, timedelta
from decimal import Decimal
from typing import Any
from uuid import UUID

from pydantic import ConfigDict, StrictFloat, StrictInt, TypeAdapter, ValidationError

_DECIMAL_CONFIG = ConfigDict(allow_inf_nan=True)

_ADAPTERS: dict[str, TypeAdapter[list[Any]]] = {
    # Overflow or non-finite NUMERIC results tag the entire column as decimal.
    # Keep native integer/float results and decode ISO durations in mixed columns.
    "decimal": TypeAdapter(
        list[StrictInt | StrictFloat | Decimal | timedelta | None],
        config=_DECIMAL_CONFIG,
    ),
    "decimal[]": TypeAdapter(list[list[Decimal | None] | None], config=_DECIMAL_CONFIG),
    "uuid": TypeAdapter(list[UUID | None]),
    "uuid[]": TypeAdapter(list[list[UUID | None] | None]),
    "datetime": TypeAdapter(list[datetime | None]),
    "datetime[]": TypeAdapter(list[list[datetime | None] | None]),
    "date": TypeAdapter(list[date | None]),
    "date[]": TypeAdapter(list[list[date | None] | None]),
    "time": TypeAdapter(list[time | None]),
    "time[]": TypeAdapter(list[list[time | None] | None]),
    "timedelta": TypeAdapter(list[timedelta | None]),
    "timedelta[]": TypeAdapter(list[list[timedelta | None] | None]),
}

_RESULT_KINDS = frozenset(_ADAPTERS)


def convert_column(kind: str, values: list[Any]) -> list[Any]:
    """Convert a column in batch and preserve values Python cannot represent."""
    if kind not in _RESULT_KINDS:
        raise ValueError(f"Unknown result kind: {kind!r}")

    adapter = _ADAPTERS[kind]
    try:
        return adapter.validate_python(values)
    except ValidationError:
        if kind.endswith("[]"):
            return [
                convert_column(kind[:-2], value) if isinstance(value, list) else value
                for value in values
            ]
        converted = []
        for value in values:
            try:
                converted.append(adapter.validate_python([value])[0])
            except ValidationError:
                # PostgreSQL also accepts TIMETZ offsets with second precision.
                # Python 3.15's fromisoformat wraps 24:00 to midnight.
                if (
                    kind == "time"
                    and isinstance(value, str)
                    and not value.startswith("24:")
                ):
                    try:
                        value = time.fromisoformat(value)
                    except ValueError:
                        pass
                converted.append(value)
        return converted


def convert_rows(rows: list[list[Any]], kinds: list[str | None]) -> None:
    """Convert the tagged columns in place."""
    for index, kind in enumerate(kinds):
        if kind is not None:
            values = convert_column(kind, [row[index] for row in rows])
            for row, value in zip(rows, values):
                row[index] = value
