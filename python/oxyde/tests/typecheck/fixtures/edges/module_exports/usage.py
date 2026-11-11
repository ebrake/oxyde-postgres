from decimal import Decimal
from typing import Any, assert_type

from module import (
    DEFAULT_API_FEED_SCHEDULE_INTERVAL_MINUTES,
    DYNAMIC,
    EMPTY,
    ENABLED,
    INTERVALS,
    INFERRED_LIMIT,
    LABEL,
    LIMIT,
    PAIR,
    PRICE,
    RATIO,
    UNSPECIFIED_ENUM_VALUE,
    ExportedHelper,
    Feed,
    SupportingClass,
)

assert_type(UNSPECIFIED_ENUM_VALUE, int)
assert_type(LABEL, str)
assert_type(RATIO, float)
assert_type(ENABLED, bool)
assert_type(EMPTY, None)
assert_type(INTERVALS, list[int])
assert_type(PAIR, tuple[int, str])
assert_type(PRICE, Decimal)
assert_type(DYNAMIC, Any)
interval: int = DEFAULT_API_FEED_SCHEDULE_INTERVAL_MINUTES
assert_type(ExportedHelper().label(), str)
assert_type(SupportingClass().label(), str)
limit: int = LIMIT
inferred_limit: int = INFERRED_LIMIT


async def fetch_feed() -> Feed:
    return await Feed.objects.get(id=1)
