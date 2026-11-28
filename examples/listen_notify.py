"""Receive PostgreSQL notifications and periodically check application state.

Run with DATABASE_URL=postgresql://... python examples/listen_notify.py.
From another session, run SELECT pg_notify('run_changed', 'run-123').
Replace reconcile() with reads of your authoritative application state.
"""

from __future__ import annotations

import asyncio
import os

from oxyde import db, execute_raw


async def reconcile() -> None:
    rows = await execute_raw("SELECT current_timestamp AS checked_at")
    print("Reconciled state:", rows[0]["checked_at"])


async def main() -> None:
    async with (
        db.connect(
            os.environ["DATABASE_URL"], settings=db.PoolSettings(max_connections=4)
        ),
        db.listen("run_changed") as listener,
    ):
        await reconcile()
        pending = asyncio.create_task(listener.recv())
        try:
            while True:
                done, _ = await asyncio.wait({pending}, timeout=30)
                if done:
                    notification = pending.result()
                    print("Wake-up hint:", notification.payload)
                    pending = asyncio.create_task(listener.recv())
                await reconcile()
        finally:
            pending.cancel()
            await asyncio.gather(pending, return_exceptions=True)


if __name__ == "__main__":
    asyncio.run(main())
