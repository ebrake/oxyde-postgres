"""Type declarations and wrapped functions must retain their stub interfaces."""

from __future__ import annotations

import functools
from enum import Enum
from typing import TypeAlias, TypeVar

from oxyde import Field, Model

T = TypeVar("T")
Ids = list[int]
JsonDict: TypeAlias = dict[str, int]
type ItemIds = list[int]
Status = Enum("Status", "A B")


def first(values: list[T]) -> T:
    return values[0]


def cached(value: int) -> int:
    return value


# Annotated rebinding is rejected as a redeclaration by mypy and pyright.
cached = functools.cache(cached)  # ty: ignore[invalid-assignment]


def accept_status(status: Status) -> Status:
    return status


def accept_json(value: JsonDict) -> JsonDict:
    return value


def accept_ids(value: Ids) -> ItemIds:
    return value


class Entry(Model):
    id: int | None = Field(default=None, db_pk=True)

    class Meta:
        is_table = True
