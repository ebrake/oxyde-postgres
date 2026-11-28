# Database Connections

Oxyde for PostgreSQL requires **PostgreSQL 18 or newer**. The Rust core checks `server_version_num` before registering a new pool. All APIs use PostgreSQL connection pools; both URL schemes below are accepted.

## Quick Start

```python
from oxyde import db

# Initialize
await db.init(default="postgresql://localhost/mydb")

# Use models
users = await User.objects.all()

# Close
await db.close()
```

## Connection URLs

| Database | URL Format |
|----------|------------|
| PostgreSQL | `postgresql://user:pass@host:5432/database` |
| PostgreSQL | `postgres://user:pass@host:5432/database` |

## High-Level API

### db.init()

Initialize one or more database connections:

```python
from oxyde import db, PoolSettings

# Single database
await db.init(default="postgresql://localhost/mydb")

# Multiple databases
await db.init(
    default="postgresql://localhost/main",
    analytics="postgresql://localhost/analytics",
    cache="postgresql://localhost/app",
)

# With custom settings
await db.init(
    default="postgresql://localhost/mydb",
    settings=PoolSettings(max_connections=20),
)
```

### db.close()

Close all connections gracefully:

```python
await db.close()
```

This rolls back any active transactions before closing.

### db.connect()

Context manager for scripts and tests:

```python
async with db.connect("postgresql://localhost/app") as conn:
    users = await User.objects.all()
# Connection closed automatically
```

With custom name:

```python
async with db.connect("postgresql://localhost/app", name="test") as conn:
    users = await User.objects.all(using="test")
```

### db.lifespan()

FastAPI integration:

```python
from fastapi import FastAPI
from oxyde import db

app = FastAPI(
    lifespan=db.lifespan(
        default="postgresql://localhost/mydb",
        settings=PoolSettings(max_connections=50),
    )
)

@app.get("/users")
async def get_users():
    return await User.objects.all()
```

## AsyncDatabase

Low-level connection wrapper:

```python
from oxyde import AsyncDatabase, PoolSettings

database = AsyncDatabase(
    "postgresql://localhost/mydb",
    name="default",
    settings=PoolSettings(max_connections=20),
)

# Manual lifecycle
await database.connect()
# ... use database ...
await database.disconnect()

# Or as context manager
async with database:
    users = await User.objects.all()
```

## Pool Settings

Configure connection pool behavior:

```python
from oxyde import PoolSettings
from datetime import timedelta

settings = PoolSettings(
    # Pool size
    max_connections=20,           # Maximum pool size
    min_connections=5,            # Minimum idle connections

    # Timeouts
    acquire_timeout=30.0,         # Max wait for connection (seconds)
    idle_timeout=600.0,           # Close idle connections after (seconds)
    max_lifetime=1800.0,          # Max connection age (seconds)

    # Health check
    test_before_acquire=True,     # Ping before using connection

    # Transaction cleanup (background task)
    transaction_timeout=300,      # Max transaction age (seconds)
    transaction_cleanup_interval=60,  # Cleanup check interval (seconds)
)
```

### TLS Settings

Configure TLS for PostgreSQL connections:

```python
settings = PoolSettings(
    # TLS mode (controls both requirement and certificate verification)
    #   PG:    "disable", "allow", "prefer", "require", "verify-ca", "verify-full"
    ssl_mode="require",

    # CA certificate (for verify-ca / verify-full modes)
    ssl_root_cert="/path/to/ca.pem",

    # Client certificate + key (for mTLS)
    ssl_client_cert="/path/to/client-cert.pem",
    ssl_client_key="/path/to/client-key.pem",
)
```

!!! note "Server certificate verification"
    `ssl_mode="verify-ca"` and `ssl_mode="verify-full"` trust the operating
    system's certificate store by default. `verify-full` also checks the
    server hostname. Set `ssl_root_cert` to trust an additional custom CA.
    On Alpine and other musl-based images, install the `ca-certificates`
    package to provide the system trust store. `ssl_mode="require"` encrypts
    without verifying the certificate.

### PostgreSQL Settings

```python
settings = PoolSettings(
    pg_application_name="my-app",       # Visible in pg_stat_activity
    pg_statement_cache_capacity=200,    # Prepared statement cache size (default: 100)
)
```

With the statement cache enabled, SQLx invalidates cached plans that become
stale after schema changes and retries the query outside transactions. Inside a
transaction, SQLx returns the original error and the transaction must be
rolled back.

Set `pg_statement_cache_capacity=0` to execute ordinary parameterized queries
with unnamed statements. Parse, Bind, Describe portal, Execute, and Close are
sent in a single batch ending in one Sync. Queries use the current result shape
after schema changes without accumulating named statements in PostgreSQL or
PgBouncer. The single batch also supports PgBouncer transaction pooling.
Explicitly prepared statement objects retain their existing preparation path.

Built-in results and custom types already known to the connection stream normally.
On the first binary result containing an unknown custom type, the driver buffers
that result until its execution reaches ReadyForQuery, then resolves the type and
its array elements or composite fields on the same connection before exposing
rows. This cold path adds catalog round trips, delays the first row, and uses
memory proportional to that result. Later reads reuse the connection's type
cache. Metadata loading never replays the original query or mutation; a metadata
failure after an autocommit mutation can occur after the write has committed.

## Schema Management

Create or drop all registered model tables programmatically — no migration files needed. Useful for tests, scripts, and rapid prototyping.

```python
from oxyde import create_tables, drop_tables

# Create all tables
await create_tables(database)

# Drop all tables (handles FK constraints per dialect)
await drop_tables(database)
```

`create_tables()` generates dialect-aware DDL via the migration diff engine and executes it. `drop_tables()` uses PostgreSQL `CASCADE` to handle foreign key dependencies.

!!! tip "For production"
    Use [migrations](migrations.md) for production schema management. `create_tables()` / `drop_tables()` are best suited for tests and throwaway environments.

## Multiple Databases

### Configuration

```python
await db.init(
    default="postgresql://localhost/main",
    analytics="postgresql://localhost/analytics",
    archive="postgresql://localhost/archive",
)
```

### Using Specific Database

```python
# Default database
users = await User.objects.all()

# Specific database
events = await Event.objects.all(using="analytics")
old_users = await LegacyUser.objects.all(using="legacy")
```

### Transactions Across Databases

Each database has separate transactions:

```python
from oxyde.db import transaction

# Transaction on default database
async with transaction.atomic():
    await User.objects.create(name="Alice")

# Transaction on analytics database
async with transaction.atomic(using="analytics"):
    await Event.objects.create(type="signup")
```

## Connection Registry

### Get Connection by Name

```python
from oxyde import get_connection

conn = await get_connection("default")
print(conn.connected)  # True
```

### Register Custom Connection

```python
from oxyde import AsyncDatabase, register_connection

database = AsyncDatabase("postgresql://localhost/custom", name="custom")
register_connection(database)
await database.connect()

# Now available as "custom"
users = await User.objects.all(using="custom")
```

### Disconnect All

```python
from oxyde import disconnect_all

await disconnect_all()  # Close all registered connections
```

## PostgreSQL Notifications

`db.listen()` subscribes before entering the context. Publish from another task or database session.

```python
from oxyde import db, execute_raw

async def watch():
    async with db.listen("run_changed", using="default") as listener:
        async for notification in listener:
            print(notification.channel, notification.payload, notification.pid)

async def publish():
    await execute_raw("SELECT pg_notify($1, $2)", ["run_changed", "run-123"])
```

Each immutable `db.Notification` has a `channel` and `payload` (strings) and the publisher's backend `pid` (an integer).
Use `recv()` or async iteration, with one pending receive per listener. Calling `close()` or
closing or replacing the named pool ends iteration and causes `recv()` to raise `db.ListenerClosedError`.

The listener reconnects automatically after a lost connection, backing off between attempts.
If it cannot reconnect within the pool's acquire timeout, `recv()` raises `DatabaseError`,
the listener stays open, and the next receive retries. Other transient errors, such as a server
shutdown or a read-only standby rejecting `LISTEN`, have the same behavior.
A terminal receive error also raises `DatabaseError` and closes the listener.
Later calls to `recv()` or async iteration raise `db.ListenerClosedError` with the original error message.

Each listener uses one extra connection outside the named pool's limit. It inherits the URL, TLS, and
connection settings and runs independently of `atomic()`.
`LISTEN` requires a direct PostgreSQL URL or a PgBouncer session-mode URL. Channel names are case-sensitive like `pg_notify()`.
Pass at least one unique name of 1–63 UTF-8 bytes with no NUL. Invalid names raise `ValueError`.

!!! note
    Notifications are wake-up hints. Notifications sent while the listener is reconnecting are lost.
    Subscribe before reading state and reconcile periodically. For periodic timeouts, keep one
    pending receive task and wait with `asyncio.wait()`. Cancelling a receive can drop a notification.
    See [the runnable example](https://github.com/ebrake/oxyde-postgres/blob/main/examples/listen_notify.py).

## Error Handling

```python
from oxyde import db
from oxyde.exceptions import ManagerError

try:
    await db.init(default="postgresql://invalid-host/db")
except Exception as e:
    print(f"Connection failed: {e}")
```

## Best Practices

### 1. Use Context Managers for Scripts

```python
async def main():
    async with db.connect("postgresql://localhost/app"):
        # Automatic cleanup on exit
        await run_app()
```

### 2. Use Lifespan for Web Apps

```python
app = FastAPI(lifespan=db.lifespan(default="postgresql://..."))
```

### 3. Configure Pool Size Based on Workload

```python
# Web API: many short connections
PoolSettings(max_connections=50, min_connections=10)

# Background worker: fewer long connections
PoolSettings(max_connections=10, min_connections=2)

```

### 4. Set Appropriate Timeouts

```python
# Production
PoolSettings(
    acquire_timeout=30,      # Don't wait forever
    idle_timeout=300,        # Close idle after 5 min
    max_lifetime=3600,       # Refresh connections hourly
)
```

## Advanced: Multiple Databases

### Per-Database Settings

```python
from oxyde import AsyncDatabase, PoolSettings

# Main database: high concurrency
main_db = AsyncDatabase(
    "postgresql://localhost/main",
    name="default",
    settings=PoolSettings(
        max_connections=50,
        min_connections=10,
    ),
)

# Analytics: read-heavy, fewer connections
analytics_db = AsyncDatabase(
    "postgresql://localhost/analytics",
    name="analytics",
    settings=PoolSettings(
        max_connections=10,
        min_connections=2,
    ),
)

await main_db.connect()
await analytics_db.connect()
```

### Read Replicas

```python
await db.init(
    default="postgresql://primary/db",
    replica="postgresql://replica/db",
)

async def get_user(user_id: int):
    # Read from replica
    return await User.objects.get(id=user_id, using="replica")

async def update_user(user_id: int, **data):
    # Write to primary
    await User.objects.filter(id=user_id).update(**data)
```

### Cross-Database Operations

```python
async def sync_user_to_analytics(user_id: int):
    # Read from main
    user = await User.objects.get(id=user_id)

    # Write to analytics
    await UserProfile.objects.create(
        user_id=user.id,
        name=user.name,
        using="analytics"
    )
```

### Dynamic Tenant Connections

```python
from oxyde import AsyncDatabase

class TenantConnectionPool:
    def __init__(self):
        self._connections: dict[int, AsyncDatabase] = {}

    async def get_connection(self, tenant_id: int) -> AsyncDatabase:
        if tenant_id not in self._connections:
            tenant = await Tenant.objects.get(id=tenant_id)
            conn = AsyncDatabase(
                tenant.database_url,
                name=f"tenant_{tenant_id}",
            )
            await conn.connect()
            self._connections[tenant_id] = conn
        return self._connections[tenant_id]

    async def close_all(self):
        for conn in self._connections.values():
            await conn.disconnect()
        self._connections.clear()

tenant_pool = TenantConnectionPool()
```

### FastAPI with Multiple Databases

```python
from fastapi import FastAPI, Depends, Request
from oxyde import db, PoolSettings

app = FastAPI(
    lifespan=db.lifespan(
        default="postgresql://localhost/main",
        analytics="postgresql://localhost/analytics",
        settings=PoolSettings(max_connections=20),
    )
)

@app.get("/users")
async def get_users():
    return await User.objects.all()

@app.get("/events")
async def get_events():
    return await Event.objects.all(using="analytics")
```

### Testing with Multiple Databases

```python
import pytest

@pytest.fixture
async def test_dbs():
    await db.init(
        default="postgresql://localhost/app",
        analytics="postgresql://localhost/app",
    )
    yield
    await db.close()

@pytest.mark.asyncio
async def test_cross_db(test_dbs):
    user = await User.objects.create(name="Test")
    await Event.objects.create(
        type="test",
        user_id=user.id,
        using="analytics"
    )

    events = await Event.objects.filter(user_id=user.id).all(using="analytics")
    assert len(events) == 1
```

## Next Steps

- [Queries](queries.md) — Query your models
- [Transactions](transactions.md) — Transaction handling
