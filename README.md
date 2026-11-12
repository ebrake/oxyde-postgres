# Oxyde for PostgreSQL

Oxyde for PostgreSQL is a PostgreSQL-focused fork of [Oxyde ORM](https://github.com/mr-fatalyst/oxyde): a type-safe, Pydantic-centric asynchronous ORM with a high-performance Rust core designed for clarity, speed, and reliability. It builds on Oxyde's great work and optimizes it for PostgreSQL with bug fixes and PostgreSQL-specific features.

**Requires PostgreSQL 18+ and Python 3.13+.** The Rust core checks the server version when connecting. Both `postgres://` and `postgresql://` connection URLs are supported.

## Features

- Pydantic v2 models with validation, type hints, and serialization.
- Familiar `Model.objects.filter()` queries, joins, aggregates, and bulk operations.
- Native PostgreSQL arrays, JSONB, enums, UUIDs, decimals, `RETURNING`, and row locks.
- Array and JSON lookups: `contains`, `contained_by`, and `overlap`.
- Async transactions with nested savepoints and public transaction-depth reporting.
- Schema migrations with PostgreSQL advisory locking and transactional DDL.
- Rust SQL generation, connection pooling, and execution, with Python 3.15 wheel support.
- Cache-disabled unnamed statements for PostgreSQL/PgBouncer, including cold custom-type decoding without replaying queries.

Version **0.3.2** adds expression and operator-class indexes (`Index((Lower("email"),), name=...)`, `Index(..., opclasses=(...))`) and the `Lower` / `Upper` expressions. Version 0.3.1 added scalar expressions (`Coalesce`, `Greatest`, `Concat`, `Value`, `RawSQL`, and `F` arithmetic) to annotations and updates, with typed NULL, enum, and array handling. The [expression guide](https://github.com/ebrake/oxyde-postgres/blob/main/docs/guide/expressions.md) covers usage, and the [release notes](https://github.com/ebrake/oxyde-postgres/blob/main/docs/changelog.md) describe compatibility changes.

The fork also includes [typed array and JSON containment](https://github.com/ebrake/oxyde-postgres/blob/main/docs/guide/filtering.md#array-lookups), [NULLS NOT DISTINCT constraints](https://github.com/ebrake/oxyde-postgres/blob/main/docs/guide/models.md#unique-indexes-and-nulls), [Rust migration replay](https://github.com/ebrake/oxyde-postgres/blob/main/docs/guide/migrations.md#schema-replay), [transaction depth](https://github.com/ebrake/oxyde-postgres/blob/main/docs/guide/transactions.md#inspecting-transaction-depth), and [complete generated type stubs](https://github.com/ebrake/oxyde-postgres/blob/main/docs/guide/models.md#generated-type-stubs). The [connection guide](https://github.com/ebrake/oxyde-postgres/blob/main/docs/guide/connections.md#postgresql-settings) explains the patched driver's statement-cache and PgBouncer behavior.

## Installation

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
        await User.objects.create(
            name="Alice", email="alice@example.com", tags=["python"]
        )
        users = await User.objects.filter(tags__contains=["python"]).all()
        print(users)
    finally:
        await db.close()


asyncio.run(main())
```

For managed schema changes, run `oxyde init`, `oxyde makemigrations`, and `oxyde migrate`. See the [documentation](https://github.com/ebrake/oxyde-postgres/blob/main/docs/index.md), [connection guide](https://github.com/ebrake/oxyde-postgres/blob/main/docs/guide/connections.md), and [contributor guide](https://github.com/ebrake/oxyde-postgres/blob/main/.github/CONTRIBUTING.md).

## Credit and license

Oxyde for PostgreSQL is based on [Oxyde](https://github.com/mr-fatalyst/oxyde), created by Nikita Ryzhenkov and its contributors. The original MIT [license](https://github.com/ebrake/oxyde-postgres/blob/main/LICENSE) is preserved. The vendored SQLx PostgreSQL driver retains its MIT and Apache-2.0 licenses.
