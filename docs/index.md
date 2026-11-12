# Oxyde for PostgreSQL

PostgreSQL-focused fork of [Oxyde](https://github.com/mr-fatalyst/oxyde), requiring **PostgreSQL 18+** and **Python 3.13+**. Version **0.3.2** adds expression and operator-class indexes (see [Models](guide/models.md#expression-indexes)); 0.3.1 added scalar expressions to annotations and updates, see the [expression guide](guide/expressions.md) and [release notes](changelog.md).


<p align="center">
  <img src="https://raw.githubusercontent.com/mr-fatalyst/oxyde/master/logo.png" alt="Logo" width="200">
</p>

<p align="center"> <b>Oxyde ORM</b> is a type-safe, Pydantic-centric asynchronous ORM with a high-performance Rust core designed for clarity, speed, and reliability. </p>

<p align="center"> Inspired by the elegance of <a href="https://www.djangoproject.com/">Django’s ORM</a>, Oxyde focuses on explicitness over magic, providing a modern developer-friendly workflow with predictable behavior and strong typing throughout. </p>

<p align="center">
  <img src="https://img.shields.io/github/license/ebrake/oxyde-postgres" alt="License">
  <img src="https://github.com/ebrake/oxyde-postgres/actions/workflows/test.yml/badge.svg" alt="Tests">
  <img src="https://img.shields.io/pypi/v/oxyde-postgres" alt="PyPI version">
  <img src="https://img.shields.io/pypi/pyversions/oxyde-postgres" alt="Python versions">
  <img src="https://static.pepy.tech/badge/oxyde-postgres" alt="PyPI Downloads">
</p>

---

!!! warning "Heads up!"
    Oxyde for PostgreSQL is under active development. The API may evolve between minor versions. Report fork issues in [ebrake/oxyde-postgres](https://github.com/ebrake/oxyde-postgres/issues).

## Quick Links

<div class="grid cards" markdown>

-   :material-download:{ .lg .middle } **Installation**

    ---

    Install Oxyde and set up your environment

    [:octicons-arrow-right-24: Getting started](getting-started/installation.md)

-   :material-rocket-launch:{ .lg .middle } **Quick Start**

    ---

    Build your first app in 5 minutes

    [:octicons-arrow-right-24: Quick start](getting-started/quickstart.md)

-   :material-book-open-variant:{ .lg .middle } **Guide**

    ---

    Learn Oxyde step by step

    [:octicons-arrow-right-24: User guide](guide/models.md)

-   :material-api:{ .lg .middle } **API Reference**

    ---

    Complete API documentation

    [:octicons-arrow-right-24: Cheatsheet](cheatsheet.md)

</div>

## Features

- **Django-style API** — Familiar `Model.objects.filter()` syntax
- **Pydantic v2 models** — Full validation, type hints, serialization
- **Async-first** — Built for modern async Python with `asyncio`
- **Rust performance** — SQL generation and execution in native Rust
- **PostgreSQL 18+** — Native arrays, JSONB, enums, row locks, and `RETURNING`
- **[Scalar expressions](guide/expressions.md)** — `Coalesce`, `Greatest`, `Concat`, `Value`, `RawSQL`, and `F` arithmetic in annotations and updates
- **[Typed containment](guide/filtering.md#array-lookups)** — `contains`, `contained_by`, and `overlap` for arrays and JSON arrays
- **[NULL-aware uniqueness](guide/models.md#unique-indexes-and-nulls)** — `NULLS NOT DISTINCT` for unique fields and indexes
- **Transactions** — `transaction.atomic()` context manager with savepoints
- **[Migrations](guide/migrations.md#schema-replay)** — Rust schema replay, advisory locking, and Django-style migration commands
- **[Generated stubs](guide/models.md#generated-type-stubs)** — Typed query methods with mypy, pyright, and ty coverage
- **[Transaction depth](guide/transactions.md#inspecting-transaction-depth)** — Inspect the current task's nested `atomic()` blocks
- **[PgBouncer support](guide/connections.md#postgresql-settings)** — Cache-disabled unnamed statements and deferred custom-type resolution

## Why Oxyde?

### Performance

Oxyde's Rust core handles SQL generation and query execution, releasing Python's GIL during database I/O. This enables true parallelism for database operations.

**Upstream Oxyde benchmarks vs popular Python ORMs** (avg ops/sec, higher is better; these are not measurements of this fork):

| Database   | Oxyde | Tortoise | Piccolo | Django | SQLAlchemy | SQLModel | Peewee |
|------------|-------|----------|---------|--------|------------|----------|--------|
| PostgreSQL | 1,475 | 888      | 932     | 736    | 445        | 431      | 80     |

[:octicons-arrow-right-24: Full benchmark report](advanced/benchmarks.md)

### Type Safety

Built on Pydantic v2, Oxyde provides full type checking for models and queries. Your IDE understands your database schema.

### Familiar API

If you know Django ORM, you know Oxyde. The QuerySet API follows Django conventions:

```python
# Django
User.objects.filter(age__gte=18).exclude(status="banned").order_by("-created_at")

# Oxyde (identical)
await User.objects.filter(age__gte=18).exclude(status="banned").order_by("-created_at").all()
```

### Async Native

No sync wrappers or thread pools. Oxyde is async from the ground up:

```python
from oxyde.db import transaction

async with transaction.atomic():
    user = await User.objects.create(name="Alice")
    await Profile.objects.create(user_id=user.id)
```

## Database Support

| Database   | Min Version | Status | Notes |
|------------|-------------|--------|-------|
| PostgreSQL | 18+ | Full | RETURNING, UPSERT, FOR UPDATE/SHARE, JSONB, Arrays |


## Ecosystem

### Oxyde Admin

Upstream Oxyde offers an auto-generated admin panel. Its compatibility with this fork must be checked separately because both distributions use the `oxyde` import namespace.

- Automatic CRUD, search, filters, export
- FastAPI, Litestar, Sanic, Quart, Falcon
- Theming, authentication, bulk operations

[:octicons-arrow-right-24: GitHub](https://github.com/mr-fatalyst/oxyde-admin)
