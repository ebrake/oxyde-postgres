"""PostgreSQL notification hints on a dedicated database connection."""

from __future__ import annotations

from collections.abc import AsyncGenerator
from contextlib import asynccontextmanager
from dataclasses import dataclass
from importlib import import_module
from typing import Any

from oxyde.core import wrapper

from .registry import get_connection

try:
    _ListenerFailed: type[RuntimeError] = import_module(
        "oxyde._oxyde_core"
    )._ListenerFailed
except ImportError, AttributeError:

    class _ListenerFailed(RuntimeError):  # type: ignore[no-redef]
        """Fallback marker when the native exception is unavailable."""


@dataclass(frozen=True)
class Notification:
    """A PostgreSQL notification and the publishing backend's process ID."""

    channel: str
    payload: str
    pid: int


class ListenerClosedError(RuntimeError):
    """The listener closed explicitly, with its named pool, or after a terminal receive error."""


class NotificationListener:
    """Single-consumer listener created with :func:`listen`."""

    def __init__(self, handle: Any) -> None:
        self._handle = handle
        self._receiving = False

    async def _receive(self) -> Notification | None:
        if self._receiving:
            raise RuntimeError("A receive is already pending on this listener")
        self._receiving = True
        try:
            value = await self._handle.recv()
            return None if value is None else Notification(*value)
        except _ListenerFailed as error:
            raise ListenerClosedError(str(error)) from error
        finally:
            self._receiving = False

    async def recv(self) -> Notification:
        """Wait for a hint. Raise DatabaseError if this receive fails.

        Raise ListenerClosedError after explicit or pool closure, or after a previous
        receive failed and closed the listener.
        """
        notification = await self._receive()
        if notification is None:
            raise ListenerClosedError("Notification listener is closed")
        return notification

    def __aiter__(self) -> NotificationListener:
        return self

    async def __anext__(self) -> Notification:
        notification = await self._receive()
        if notification is None:
            raise StopAsyncIteration
        return notification

    async def close(self) -> None:
        """Interrupt pending receives and close the listener. Safe to repeat."""
        await self._handle.close()


@asynccontextmanager
async def listen(
    *channels: str, using: str = "default"
) -> AsyncGenerator[NotificationListener]:
    """Subscribe before yielding on a dedicated PostgreSQL connection.

    See PostgreSQL Notifications in the connection guide for usage and reconnect behavior.
    """
    database = await get_connection(using)
    handle = await wrapper.listen(database.name, list(channels))
    listener = NotificationListener(handle)
    try:
        yield listener
    finally:
        await listener.close()
