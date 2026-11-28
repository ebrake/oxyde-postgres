"""PostgreSQL LISTEN/NOTIFY integration tests."""

from __future__ import annotations

import asyncio
import gc
import struct
import uuid
from collections.abc import AsyncGenerator, AsyncIterator
from contextlib import asynccontextmanager, closing, suppress
from dataclasses import dataclass, field
from pathlib import Path
from urllib.parse import parse_qsl, urlencode, urlsplit

import psycopg2
import pytest
import pytest_asyncio

from oxyde import db, execute_raw
from oxyde.core import wrapper

pytestmark = pytest.mark.asyncio


def channel(prefix):
    return f"{prefix}_{uuid.uuid4().hex}"


@pytest_asyncio.fixture
async def database(postgres_url):
    name = f"listen_{uuid.uuid4().hex}"
    async with db.connect(
        postgres_url, name=name, settings=db.PoolSettings(max_connections=4)
    ) as database:
        yield database


def proxy_url(postgres_url: str, port: int, *, plaintext: bool = False) -> str:
    url = urlsplit(postgres_url)
    userinfo, at_sign, _ = url.netloc.rpartition("@")
    replaced = {"host", "hostaddr", "port"}
    if plaintext:
        replaced.add("sslmode")
    query = [
        (key, value)
        for key, value in parse_qsl(url.query, keep_blank_values=True)
        if key not in replaced
    ]
    query.append(("hostaddr", "127.0.0.1"))
    if plaintext:
        query.append(("sslmode", "disable"))
    return url._replace(
        netloc=f"{userinfo}{at_sign}127.0.0.1:{port}", query=urlencode(query)
    ).geturl()


def upstream_endpoint(postgres_url: str, *, plaintext: bool) -> tuple[str, int]:
    options = {"sslmode": "disable", "gssencmode": "disable"} if plaintext else {}
    try:
        with closing(psycopg2.connect(postgres_url, **options)) as connection:
            info = connection.info
            return info.dsn_parameters.get("hostaddr") or info.host, info.port
    except psycopg2.OperationalError as error:
        message = str(error).lower()
        if plaintext and (
            "no encryption" in message or "ssl connection is required" in message
        ):
            pytest.skip(f"PostgreSQL rejected a plaintext connection: {error}")
        raise


@dataclass
class ProxyConnection:
    sent_listen: bool = False
    client_closed: asyncio.Event = field(default_factory=asyncio.Event)


@dataclass
class PostgresProxy:
    url: str = ""
    refuse: asyncio.Event = field(default_factory=asyncio.Event)
    stall_listen: bool = False
    inject_on_listen: str | None = None
    connections: list[ProxyConnection] = field(default_factory=list)


async def read_frame(
    reader: asyncio.StreamReader, *, startup: bool = False
) -> tuple[bytes, bytes, bytes]:
    kind = b"" if startup else await reader.readexactly(1)
    length = await reader.readexactly(4)
    body = await reader.readexactly(int.from_bytes(length, "big") - len(length))
    return kind, length, body


@asynccontextmanager
async def run_postgres_proxy(
    postgres_url: str, *, plaintext: bool = False
) -> AsyncGenerator[PostgresProxy]:
    host, port = await asyncio.to_thread(
        upstream_endpoint, postgres_url, plaintext=plaintext
    )
    proxy = PostgresProxy()

    async def forward(
        reader: asyncio.StreamReader, writer: asyncio.StreamWriter
    ) -> None:
        with suppress(ConnectionError):
            while data := await reader.read(65536):
                writer.write(data)
                await writer.drain()

    async def handle(
        reader: asyncio.StreamReader, writer: asyncio.StreamWriter
    ) -> None:
        connection = ProxyConnection()
        proxy.connections.append(connection)
        try:
            if proxy.refuse.is_set():
                return
            if host.startswith("/"):
                upstream_reader, upstream_writer = await asyncio.open_unix_connection(
                    Path(host) / f".s.PGSQL.{port}"
                )
            else:
                upstream_reader, upstream_writer = await asyncio.open_connection(
                    host, port
                )

            notification = None

            async def send_to_postgres() -> None:
                nonlocal notification
                if not plaintext:
                    await forward(reader, upstream_writer)
                    connection.client_closed.set()
                    return

                startup = True
                with suppress(ConnectionError, asyncio.IncompleteReadError):
                    while True:
                        kind, length, body = await read_frame(reader, startup=startup)
                        startup = False
                        if kind == b"Q" and body.startswith(b'LISTEN "'):
                            connection.sent_listen = True
                            if proxy.inject_on_listen is not None:
                                topic = body.split(b'"', 2)[1]
                                payload = proxy.inject_on_listen.encode()
                                contents = (
                                    struct.pack("!I", 1)
                                    + topic
                                    + b"\0"
                                    + payload
                                    + b"\0"
                                )
                                notification = (
                                    b"A"
                                    + struct.pack("!I", 4 + len(contents))
                                    + contents
                                )
                        if connection.sent_listen and proxy.stall_listen:
                            continue  # Keep reading so a closed client socket is still observed.
                        upstream_writer.write(kind + length + body)
                        await upstream_writer.drain()
                connection.client_closed.set()

            async def send_to_client() -> None:
                nonlocal notification
                if not plaintext:
                    await forward(upstream_reader, writer)
                    return

                listen_completed = False
                with suppress(ConnectionError, asyncio.IncompleteReadError):
                    while True:
                        kind, length, body = await read_frame(upstream_reader)
                        if kind == b"C" and body == b"LISTEN\0":
                            listen_completed = True
                        elif kind == b"Z":
                            # SQLx drains through ReadyForQuery when executing LISTEN.
                            if listen_completed and notification is not None:
                                writer.write(notification)
                                notification = None
                            listen_completed = False
                        writer.write(kind + length + body)
                        await writer.drain()

            try:
                async with asyncio.TaskGroup() as pipes:
                    outgoing = pipes.create_task(send_to_postgres())
                    incoming = pipes.create_task(send_to_client())
                    _, pending = await asyncio.wait(
                        (outgoing, incoming), return_when=asyncio.FIRST_COMPLETED
                    )
                    for task in pending:
                        task.cancel()
            finally:
                upstream_writer.close()
                with suppress(ConnectionError):
                    await upstream_writer.wait_closed()
        finally:
            writer.close()
            with suppress(ConnectionError):
                await writer.wait_closed()

    async with asyncio.TaskGroup() as clients:
        server = await asyncio.start_server(
            lambda reader, writer: clients.create_task(handle(reader, writer)),
            "127.0.0.1",
        )
        async with server:
            proxy.url = proxy_url(
                postgres_url, server.sockets[0].getsockname()[1], plaintext=plaintext
            )
            try:
                yield proxy
            finally:
                server.close()
                server.abort_clients()


@pytest_asyncio.fixture
async def postgres_proxy(postgres_url: str) -> AsyncIterator[PostgresProxy]:
    async with run_postgres_proxy(postgres_url) as proxy:
        yield proxy


@pytest_asyncio.fixture
async def plaintext_postgres_proxy(postgres_url: str) -> AsyncIterator[PostgresProxy]:
    async with run_postgres_proxy(postgres_url, plaintext=True) as proxy:
        yield proxy


async def publish(database, channel, payload="hint"):
    return await execute_raw(
        "SELECT pg_notify($1, $2), pg_backend_pid() AS pid",
        [channel, payload],
        client=database,
    )


async def receive(listener):
    return await asyncio.wait_for(listener.recv(), 5)


async def receive_reconnect_probe(database, topic, listener):
    pending = asyncio.create_task(listener.recv())
    try:
        async with asyncio.timeout(5):
            while not pending.done():
                await publish(database, topic, "probe")
                await asyncio.sleep(0.05)
            return await pending
    finally:
        pending.cancel()
        await asyncio.gather(pending, return_exceptions=True)


async def test_multiple_channels_and_broadcast(database):
    longest = channel("a" * 30)
    assert len(longest.encode("utf-8")) == 63
    channels = [channel('Quoted"Name'), channel("更新"), longest, channel("é" * 14)]
    async with (
        db.listen(*channels, using=database.name) as first,
        db.listen(*channels, using=database.name) as second,
    ):
        for topic in channels:
            pid = (await publish(database, topic, "run-1"))[0]["pid"]
            for listener in (first, second):
                notification = await receive(listener)
                assert notification == db.Notification(topic, "run-1", pid)


async def test_notifications_follow_transaction_commit(database):
    topic = channel("transaction")
    async with db.listen(topic, using=database.name) as listener:
        pending = asyncio.create_task(listener.recv())
        try:
            async with db.atomic(using=database.name):
                await execute_raw(
                    "SELECT pg_notify($1, $2)",
                    [topic, "commit"],
                    using=database.name,
                )
                done, _ = await asyncio.wait({pending}, timeout=0.1)
                assert not done
            assert (await asyncio.wait_for(pending, 5)).payload == "commit"
        finally:
            pending.cancel()
            await asyncio.gather(pending, return_exceptions=True)

        with pytest.raises(ValueError):
            async with db.atomic(using=database.name):
                await execute_raw(
                    "SELECT pg_notify($1, $2)",
                    [topic, "rollback"],
                    using=database.name,
                )
                raise ValueError("rollback")
        await publish(database, topic, "barrier")
        assert (await receive(listener)).payload == "barrier"


async def test_close_interrupts_receive_and_iteration(database):
    async with db.listen(channel("close"), using=database.name) as listener:
        pending = asyncio.create_task(listener.__anext__())
        await asyncio.sleep(0)
        with pytest.raises(RuntimeError, match="already pending"):
            await listener.recv()

        await asyncio.wait_for(listener.close(), 5)
        with pytest.raises(StopAsyncIteration):
            await asyncio.wait_for(pending, 5)
        await listener.close()
        with pytest.raises(db.ListenerClosedError):
            await listener.recv()
        assert [item async for item in listener] == []


@pytest.mark.parametrize("operation", ["close", "replace", "close_all"])
async def test_named_database_shutdown_closes_listener(
    database, postgres_url, operation
):
    async with db.listen(channel("shutdown"), using=database.name) as listener:
        pending = asyncio.create_task(listener.__anext__())
        await asyncio.sleep(0)
        if operation == "replace":
            await asyncio.wait_for(
                wrapper.init_pool_overwrite(database.name, postgres_url, None), 5
            )
        elif operation == "close_all":
            await asyncio.wait_for(db.close(), 5)
        else:
            await asyncio.wait_for(database.disconnect(), 5)

        with pytest.raises(StopAsyncIteration):
            await asyncio.wait_for(pending, 5)
        with pytest.raises(db.ListenerClosedError):
            await listener.recv()


async def listener_pids(database, application_name):
    rows = await execute_raw(
        "SELECT pid FROM pg_stat_activity "
        "WHERE application_name = $1 AND pid <> pg_backend_pid()",
        [application_name],
        client=database,
    )
    return [row["pid"] for row in rows]


async def test_listener_does_not_consume_application_pool_capacity(postgres_url):
    name = channel("capacity")
    topic = channel("dedicated")
    async with db.connect(
        postgres_url,
        name=name,
        settings=db.PoolSettings(max_connections=1, pg_application_name=name),
    ) as database:
        handle = await wrapper.listen(name, [channel("abandoned")])
        pids = await listener_pids(database, name)
        assert len(pids) == 1
        del handle
        gc.collect()
        async with asyncio.timeout(5):
            while pids[0] in await listener_pids(database, name):
                await asyncio.sleep(0.05)

        async with db.listen(topic, using=name) as listener:
            rows = await asyncio.wait_for(
                execute_raw("SELECT 1 AS value", client=database), 5
            )
            assert rows[0]["value"] == 1
            await publish(database, topic)
            assert (await receive(listener)).payload == "hint"


async def test_cancelled_receive_can_receive_again(database):
    topic = channel("cancel")
    async with db.listen(topic, using=database.name) as listener:
        for _ in range(20):
            pending = asyncio.create_task(listener.recv())
            await asyncio.sleep(0)
            pending.cancel()
            with pytest.raises(asyncio.CancelledError):
                await pending
        await publish(database, topic, "still-usable")
        assert (await receive(listener)).payload == "still-usable"


async def test_backend_termination_reconnects(postgres_url):
    application_name = channel("oxyde-listen")
    topic = channel("terminate")
    async with (
        db.connect(
            postgres_url,
            name=application_name,
            settings=db.PoolSettings(
                max_connections=1, pg_application_name=application_name
            ),
        ) as database,
        db.listen(topic, using=application_name) as listener,
    ):
        pids = await listener_pids(database, application_name)
        assert len(pids) == 1
        old_pid = pids[0]
        rows = await execute_raw(
            "SELECT pg_terminate_backend($1::int) AS terminated",
            [old_pid],
            client=database,
        )
        assert rows[0]["terminated"]

        assert (
            await receive_reconnect_probe(database, topic, listener)
        ).payload == "probe"

        pids = await listener_pids(database, application_name)
        assert any(pid != old_pid for pid in pids)


async def test_listener_reconnect_io_errors_are_bounded(
    database, postgres_proxy: PostgresProxy
) -> None:
    name = channel("reconnect_proxy")
    topic = channel("reconnect")
    settings = db.PoolSettings(
        max_connections=1, pg_application_name=name, acquire_timeout=0.5
    )
    async with (
        db.connect(
            postgres_proxy.url, name=name, settings=settings
        ) as proxied_database,
        db.listen(topic, using=name) as listener,
    ):
        [pid] = await listener_pids(proxied_database, name)
        postgres_proxy.refuse.set()
        rows = await execute_raw(
            "SELECT pg_terminate_backend($1::int, 5000) AS terminated",
            [pid],
            client=database,
        )
        assert rows[0]["terminated"]

        with pytest.raises(wrapper.DatabaseError):
            await asyncio.wait_for(listener.recv(), 2)

        postgres_proxy.refuse.clear()
        assert (
            await receive_reconnect_probe(database, topic, listener)
        ).payload == "probe"


async def test_reconnect_delivers_notification_buffered_during_listen(
    database, plaintext_postgres_proxy: PostgresProxy
) -> None:
    proxy = plaintext_postgres_proxy
    name = channel("buffered_proxy")
    topic = channel("reconnect")
    settings = db.PoolSettings(
        max_connections=1, pg_application_name=name, acquire_timeout=0.5
    )
    async with (
        db.connect(proxy.url, name=name, settings=settings) as proxied_database,
        db.listen(topic, using=name) as listener,
    ):
        [pid] = await listener_pids(proxied_database, name)
        proxy.refuse.set()
        rows = await execute_raw(
            "SELECT pg_terminate_backend($1::int, 5000) AS terminated",
            [pid],
            client=database,
        )
        assert rows[0]["terminated"]
        with pytest.raises(wrapper.DatabaseError):
            await receive(listener)

        proxy.inject_on_listen = "buffered"
        proxy.refuse.clear()
        notification = await receive(listener)
        assert (notification.channel, notification.payload) == (topic, "buffered")


async def test_initial_subscription_is_bounded(
    plaintext_postgres_proxy: PostgresProxy,
) -> None:
    proxy = plaintext_postgres_proxy
    name = channel("initial_listen")
    settings = db.PoolSettings(acquire_timeout=0.5)

    async def subscribe() -> None:
        async with db.listen(channel("stall"), using=name):
            pytest.fail("stalled LISTEN completed")

    async with db.connect(proxy.url, name=name, settings=settings):
        proxy.stall_listen = True
        with pytest.raises(wrapper.DatabaseError, match="pool timed out"):
            await asyncio.wait_for(subscribe(), 3)
        [connection] = [
            connection for connection in proxy.connections if connection.sent_listen
        ]
        await asyncio.wait_for(connection.client_closed.wait(), 2)


async def test_close_releases_stalled_listener_socket(
    plaintext_postgres_proxy: PostgresProxy,
) -> None:
    proxy = plaintext_postgres_proxy
    name = channel("stalled_close")
    async with (
        db.connect(proxy.url, name=name),
        db.listen(channel("stall"), using=name) as listener,
    ):
        [connection] = [
            connection for connection in proxy.connections if connection.sent_listen
        ]
        proxy.stall_listen = True
        await asyncio.wait_for(listener.close(), 3)
        await asyncio.wait_for(connection.client_closed.wait(), 2)


async def test_proxy_url_uses_proxy_endpoint() -> None:
    original = (
        "postgresql://user:pass@example.com:5432/test"
        "?host=other.example.com&hostaddr=192.0.2.20&port=5555&sslmode=require"
    )
    url = urlsplit(proxy_url(original, 9876))
    assert url.netloc == "user:pass@127.0.0.1:9876"
    assert parse_qsl(url.query) == [("sslmode", "require"), ("hostaddr", "127.0.0.1")]
    plaintext_url = urlsplit(proxy_url(original, 9876, plaintext=True))
    assert parse_qsl(plaintext_url.query) == [
        ("hostaddr", "127.0.0.1"),
        ("sslmode", "disable"),
    ]


@pytest_asyncio.fixture
async def listener_role(database) -> AsyncIterator[tuple[str, str]]:
    [privileges] = await execute_raw(
        "SELECT rolsuper OR rolcreaterole AS can_create_role "
        "FROM pg_roles WHERE rolname = current_user",
        client=database,
    )
    if not privileges["can_create_role"]:
        pytest.skip(
            "requires CREATEROLE (or superuser) to create a temporary login role"
        )

    role = channel("listener_role")
    password = uuid.uuid4().hex
    await execute_raw(
        f"CREATE ROLE {role} LOGIN PASSWORD '{password}'", client=database
    )
    try:
        try:
            await execute_raw(
                f"GRANT {role} TO CURRENT_USER WITH INHERIT TRUE, SET FALSE",
                client=database,
            )
        except wrapper.DatabaseError:
            [privileges] = await execute_raw(
                "SELECT pg_has_role($1::text, 'USAGE') "
                "OR pg_has_role('pg_signal_backend', 'USAGE') AS can_signal",
                [role],
                client=database,
            )
            if not privileges["can_signal"]:
                pytest.skip("cannot signal the temporary login role's backend")
        yield role, password
    finally:
        await execute_raw(f"DROP ROLE {role}", client=database)


@pytest.mark.parametrize(
    "retryable", [True, False], ids=["pool-timeout", "login-denied"]
)
async def test_listener_reconnect_failure(
    database, postgres_url, listener_role, retryable
):
    role, password = listener_role
    url = urlsplit(postgres_url)
    host = url.netloc.rsplit("@", 1)[-1]
    credentials = {
        "user",
        "username",
        "password",
        "passfile",
        "sslcert",
        "ssl-cert",
        "sslkey",
        "ssl-key",
        "sslpassword",
    }
    query = urlencode(
        [
            (key, value)
            for key, value in parse_qsl(url.query, keep_blank_values=True)
            if key.lower() not in credentials
        ]
    )
    listener_url = url._replace(
        netloc=f"{role}:{password}@{host}", query=query
    ).geturl()
    topic = channel("reconnect")
    settings = db.PoolSettings(
        max_connections=1, pg_application_name=role, acquire_timeout=0.5
    )
    async with (
        db.connect(listener_url, name=role, settings=settings) as listener_database,
        db.listen(topic, using=role) as listener,
    ):
        [pid] = await listener_pids(listener_database, role)
        restriction = "CONNECTION LIMIT 0" if retryable else "NOLOGIN"
        await execute_raw(f"ALTER ROLE {role} {restriction}", client=database)
        rows = await execute_raw(
            "SELECT pg_terminate_backend($1::int, 5000) AS terminated",
            [pid],
            client=database,
        )
        assert rows[0]["terminated"]

        message = "pool timed out" if retryable else "is not permitted to log in"
        with pytest.raises(wrapper.DatabaseError, match=message) as failure:
            await receive(listener)

        if retryable:
            await execute_raw(f"ALTER ROLE {role} CONNECTION LIMIT -1", client=database)
            notification = await receive_reconnect_probe(database, topic, listener)
            assert notification.payload == "probe"
        else:
            with pytest.raises(db.ListenerClosedError) as closed:
                await receive(listener)
            assert str(closed.value) == str(failure.value)
            assert isinstance(closed.value, RuntimeError)
            assert not isinstance(closed.value, wrapper.DatabaseError)
            assert str(closed.value.__cause__) == str(failure.value)

            async with asyncio.timeout(5):
                with pytest.raises(db.ListenerClosedError) as closed:
                    async for _ in listener:
                        pytest.fail("received a notification after terminal failure")
            assert str(closed.value) == str(failure.value)


@pytest.mark.parametrize(
    "channels", [(), ("",), ("a\0b",), ("a" * 64,), ("é" * 32,), ("x", "x")]
)
async def test_invalid_channels(database, channels):
    with pytest.raises(ValueError, match="Invalid listener channels"):
        async with db.listen(*channels, using=database.name):
            pytest.fail("invalid channels accepted")
