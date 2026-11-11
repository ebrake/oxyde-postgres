"""Edge: async generators in functions, support classes, and model methods."""

from __future__ import annotations

from collections.abc import AsyncGenerator, AsyncIterator
from contextlib import asynccontextmanager

from oxyde import Field, Model


class Event(Model):
    id: int | None = Field(default=None, db_pk=True)
    kind: str = Field(default="created")

    async def stream_kinds(self) -> AsyncIterator[str]:
        yield self.kind

    class Meta:
        is_table = True


class EventFeed:
    async def __aiter__(self) -> AsyncIterator[Event]:
        yield Event(kind="created")


@asynccontextmanager
async def audited_mutation(actor: str) -> AsyncGenerator[None]:
    yield


async def iter_events(limit: int = 10) -> AsyncGenerator[Event]:
    for _ in range(limit):
        yield Event(kind="created")


async def count_events() -> int:
    async def nested() -> AsyncIterator[Event]:
        yield Event(kind="created")

    return len([event async for event in nested()])
