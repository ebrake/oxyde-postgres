from __future__ import annotations

from contextlib import AbstractAsyncContextManager
from typing import assert_type

from module import Event, EventFeed, audited_mutation, count_events, iter_events


async def main(event: Event) -> None:
    async with audited_mutation("admin") as result:
        assert_type(result, None)

    manager: AbstractAsyncContextManager[None] = audited_mutation("admin")
    async with manager:
        pass

    async for item in iter_events():
        assert_type(item, Event)
    async for kind in event.stream_kinds():
        assert_type(kind, str)
    async for item in EventFeed():
        assert_type(item, Event)

    assert_type(await count_events(), int)
    assert_type(await Event.objects.create(_skip_hooks=True, kind="created"), Event)
