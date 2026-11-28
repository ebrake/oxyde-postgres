"""Validation and Python iterator contracts without a database."""

from dataclasses import FrozenInstanceError
from unittest.mock import AsyncMock

import pytest

from oxyde import db
from oxyde.core import wrapper


@pytest.mark.asyncio
@pytest.mark.parametrize(
    "channels", [(), ("",), ("a\0b",), ("a" * 64,), ("é" * 32,), ("x", "x")]
)
async def test_invalid_channels_before_pool_resolution(channels):
    with pytest.raises(ValueError, match="Invalid listener channels"):
        await wrapper.listen("unregistered", list(channels))


@pytest.mark.asyncio
async def test_non_string_channel():
    with pytest.raises(TypeError):
        wrapper.listen("unregistered", [123])


def test_notification_is_immutable():
    notification = db.Notification("channel", "payload", 12)
    with pytest.raises(FrozenInstanceError):
        notification.payload = "changed"


@pytest.mark.asyncio
async def test_operational_error_is_not_reported_as_end_of_iteration():
    handle = AsyncMock()
    handle.recv.side_effect = RuntimeError("database failed")
    listener = db.NotificationListener(handle)
    with pytest.raises(RuntimeError, match="database failed"):
        await listener.__anext__()
    handle.recv.side_effect = None
    handle.recv.return_value = None
    with pytest.raises(StopAsyncIteration):
        await listener.__anext__()
    with pytest.raises(db.ListenerClosedError):
        await listener.recv()
