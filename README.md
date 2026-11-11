# Oxyde for PostgreSQL

Oxyde for PostgreSQL is a PostgreSQL-focused fork of [Oxyde ORM](https://github.com/mr-fatalyst/oxyde): a type-safe, Pydantic-centric asynchronous ORM with a high-performance Rust core designed for clarity, speed, and reliability. It builds on Oxyde's great work and optimizes it for PostgreSQL with bug fixes and PostgreSQL-specific features.

**Requires PostgreSQL 18+ and Python 3.10+.** The Rust core checks the server version when connecting. Both `postgres://` and `postgresql://` connection URLs are supported.

## Features

- Pydantic v2 models with validation, type hints, and serialization.
- Familiar `Model.objects.filter()` queries, joins, aggregates, and bulk operations.
- Native PostgreSQL arrays, JSONB, enums, UUIDs, decimals, `RETURNING`, and row locks.
- Array and JSON lookups: `contains`, `contained_by`, and `overlap`.
- Async transactions with nested savepoints and public transaction-depth reporting.
- Schema migrations with PostgreSQL advisory locking and transactional DDL.
- Rust SQL generation, connection pooling, and execution, with Python 3.15 wheel support.
- Cache-disabled unnamed statements for PostgreSQL/PgBouncer, including cold custom-type decoding without replaying queries.

The initial fork release is **0.1.0**, currently in preparation. [First-release notes](docs/changelog.md) list every addition and its source.

## Installation

After the first release is published:

```bash
pip install oxyde-postgres
```

The distribution names are `oxyde-postgres` and `oxyde-postgres-core`. Python imports remain `oxyde`, the native module remains `_oxyde_core`, and the CLI remains `oxyde`. Use a separate virtual environment from upstream Oxyde because the packages share these import names.

To build this checkout:

```bash
uv venv --python 3.13
source .venv/bin/activate
uv pip install maturin
maturin develop --manifest-path crates/oxyde-core-py/Cargo.toml
uv pip install -e './python[dev]'
```

## Quick start

```python
import asyncio
from oxyde import Field, Model, db
from oxyde.db.schema import create_tables

class User(Model):
    id: int | None = Field(default=None, db_pk=True)
    name: str
    email: str = Field(db_unique=True)
    tags: list[str] = Field(default_factory=list)

    class Meta:
        is_table = True

async def main():
    await db.init(default="postgresql://user:password@localhost:5432/app")
    try:
        connection = await db.get_connection()
        await create_tables(connection)
        await User.objects.create(name="Alice", email="alice@example.com", tags=["python"])
        users = await User.objects.filter(tags__contains=["python"]).all()
        print(users)
    finally:
        await db.close()

asyncio.run(main())
```

For managed schema changes, run `oxyde init`, `oxyde makemigrations`, and `oxyde migrate`. See the [documentation](docs/index.md), [connection guide](docs/guide/connections.md), and [contributor guide](.github/CONTRIBUTING.md).

## Credit and license

Oxyde for PostgreSQL is based on [Oxyde](https://github.com/mr-fatalyst/oxyde), created by Nikita Ryzhenkov and its contributors. The original MIT [license](LICENSE) is preserved. The vendored SQLx PostgreSQL driver retains its MIT and Apache-2.0 licenses.
