"""Public module declarations must survive even when models don't use them."""

from __future__ import annotations

from dataclasses import dataclass
from decimal import Decimal
from enum import Enum
from typing import Final
from uuid import UUID

from oxyde import Field, Model
from sibling import DEFAULT_API_FEED_SCHEDULE_INTERVAL_MINUTES
from sibling import ExportedHelper as ExportedHelper

UNSPECIFIED_ENUM_VALUE = -1
LABEL = "unspecified"
RATIO = 1.5
ENABLED = True
EMPTY = None
INTERVALS = [5, 10]
PAIR = (1, "one")
LIMIT: Final[int] = 10
INFERRED_LIMIT: Final = 20
PRICE: Decimal = Decimal("1.5")
DYNAMIC = object()


class _Kind(str, Enum):
    DEFAULT = "default"


@dataclass
class SupportingClass:
    kind: _Kind = _Kind.DEFAULT

    def label(self) -> str:
        return self.kind.value


class Feed(Model):
    id: int | None = Field(default=None, db_pk=True)

    class Meta:
        is_table = True
