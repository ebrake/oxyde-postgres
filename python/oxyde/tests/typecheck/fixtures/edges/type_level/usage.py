from typing import assert_type

from module import (
    Entry,
    Ids,
    ItemIds,
    JsonDict,
    Status,
    accept_ids,
    accept_json,
    accept_status,
    cached,
    first,
)

assert_type(first([1]), int)
assert_type(cached(1), int)


def check_types(status: Status, payload: JsonDict, ids: Ids) -> None:
    assert_type(accept_status(status), Status)
    assert_type(accept_json(payload), dict[str, int])
    result: ItemIds = accept_ids(ids)
    assert_type(result, list[int])


async def fetch_entry() -> Entry:
    return await Entry.objects.get(id=1)
