"""Restore Python values from the driver's column-wise MessagePack tags."""

from datetime import date, datetime, time, timedelta
from decimal import Decimal, InvalidOperation
from typing import Any
from uuid import UUID

from pydantic import TypeAdapter, ValidationError

_ADAPTERS: dict[str, TypeAdapter[list[Any]]] = {
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

_RESULT_KINDS = frozenset(_ADAPTERS) | {"decimal", "decimal[]"}


def _decimal_column(values: list[Any]) -> list[Any]:
    try:
        return [None if value is None else Decimal(value) for value in values]
    except InvalidOperation, TypeError, ValueError:
        converted = []
        for value in values:
            try:
                converted.append(None if value is None else Decimal(value))
            except InvalidOperation, TypeError, ValueError:
                converted.append(value)
        return converted


def convert_column(kind: str, values: list[Any]) -> list[Any]:
    """Convert a column in batch and preserve values Python cannot represent."""
    if kind == "decimal":
        return _decimal_column(values)
    if kind == "decimal[]":
        return [
            _decimal_column(value) if isinstance(value, list) else value
            for value in values
        ]
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
                converted.append(value)
        return converted


def convert_rows(
    rows: list[list[Any]], kinds: list[str | None], keep_text: set[int]
) -> None:
    """Convert the tagged columns in place."""
    for index, kind in enumerate(kinds):
        if kind is not None and index not in keep_text:
            values = convert_column(kind, [row[index] for row in rows])
            for row, value in zip(rows, values):
                row[index] = value
