"""PostgreSQL 18 integration fixtures with isolated schemas."""
from __future__ import annotations

import uuid
from datetime import date, datetime, time, timedelta
from decimal import Decimal
from typing import Annotated, Any
from uuid import UUID

import pytest
import pytest_asyncio
from pydantic import computed_field

from oxyde import AsyncDatabase, Field, Model, disconnect_all
from oxyde.db.schema import create_tables
from oxyde.migrations.utils import detect_dialect
from oxyde.models.registry import clear_registry, register_table
from oxyde.models.utils import _unpack_annotated, _unwrap_optional
from oxyde.queries.raw import execute_raw

_DIALECTS = ["postgres"]

# ── Models ─────────────────────────────────────────────────────────────


class Event(Model):
    id: int | None = Field(default=None, db_pk=True)
    title: str = Field(max_length=200)
    created_at: datetime

    class Meta:
        is_table = True
        table_name = "events"


class AliasedEvent(Model):
    """Model with db_column != field_name (for P3 bug regression test)."""

    id: int | None = Field(default=None, db_pk=True)
    title: str = Field(max_length=200, db_column="event_title")
    created: datetime = Field(db_column="created_at")

    class Meta:
        is_table = True
        table_name = "aliased_events"


class Author(Model):
    id: int | None = Field(default=None, db_pk=True)
    name: str = Field(max_length=100)
    email: str = Field(db_unique=True, db_type="VARCHAR(255)")
    active: bool = Field(default=True)
    posts: list[Post] = Field(db_reverse_fk="author_id")

    class Meta:
        is_table = True
        table_name = "authors"


class Category(Model):
    id: int | None = Field(default=None, db_pk=True)
    name: str = Field(max_length=50, db_unique=True, db_type="VARCHAR(50)")

    class Meta:
        is_table = True
        table_name = "categories"


class Post(Model):
    id: int | None = Field(default=None, db_pk=True)
    title: str = Field(max_length=200)
    body: str = Field(default="")
    author: Author | None = None
    category: Category | None = Field(default=None, db_nullable=True)
    views: int = Field(default=0)
    published: bool = Field(default=False)
    comments: list[Comment] = Field(db_reverse_fk="post_id")
    tags: list[Tag] = Field(db_m2m=True, db_through="PostTag")

    class Meta:
        is_table = True
        table_name = "posts"


class Comment(Model):
    id: int | None = Field(default=None, db_pk=True)
    post: Post | None = None
    body: str = Field(default="")

    class Meta:
        is_table = True
        table_name = "comments"


class Tag(Model):
    id: int | None = Field(default=None, db_pk=True)
    name: str = Field(max_length=50, db_unique=True, db_type="VARCHAR(50)")

    class Meta:
        is_table = True
        table_name = "tags"


class PostTag(Model):
    id: int | None = Field(default=None, db_pk=True)
    post: Post | None = None
    tag: Tag | None = None

    class Meta:
        is_table = True
        table_name = "post_tags"


class AllTypes(Model):
    id: int | None = Field(default=None, db_pk=True)
    int_val: int = Field(default=0)
    str_val: str = Field(default="")
    float_val: float = Field(default=0.0)
    bool_val: bool = Field(default=False)
    datetime_val: datetime | None = Field(default=None, db_nullable=True)
    date_val: date | None = Field(default=None, db_nullable=True)
    time_val: time | None = Field(default=None, db_nullable=True)
    uuid_val: UUID | None = Field(default=None, db_nullable=True)
    decimal_val: Decimal | None = Field(
        default=None, db_nullable=True, max_digits=20, decimal_places=10
    )
    json_val: dict | None = Field(default=None, db_nullable=True)
    str_list: list[Annotated[str, Field(max_length=100)]] | None = Field(
        default=None, db_nullable=True
    )
    int_list: list[int] | None = Field(default=None, db_nullable=True)
    decimal_list: list[Annotated[Decimal, Field(max_digits=10, decimal_places=2)]] | None = Field(
        default=None, db_nullable=True
    )

    class Meta:
        is_table = True
        table_name = "all_types"


class NullableTypes(Model):
    id: int | None = Field(default=None, db_pk=True)
    int_val: int | None = Field(default=None, db_nullable=True)
    str_val: str | None = Field(default=None, db_nullable=True)
    float_val: float | None = Field(default=None, db_nullable=True)
    bool_val: bool | None = Field(default=None, db_nullable=True)
    datetime_val: datetime | None = Field(default=None, db_nullable=True)
    date_val: date | None = Field(default=None, db_nullable=True)
    time_val: time | None = Field(default=None, db_nullable=True)
    uuid_val: UUID | None = Field(default=None, db_nullable=True)
    decimal_val: Decimal | None = Field(
        default=None, db_nullable=True, max_digits=20, decimal_places=10
    )
    json_val: dict | None = Field(default=None, db_nullable=True)

    class Meta:
        is_table = True
        table_name = "nullable_types"


class BytesModel(Model):
    id: int | None = Field(default=None, db_pk=True)
    data: bytes | None = Field(default=None, db_nullable=True)

    class Meta:
        is_table = True
        table_name = "bytes_model"


class TdModel(Model):
    id: int | None = Field(default=None, db_pk=True)
    duration: timedelta | None = Field(default=None, db_nullable=True)

    class Meta:
        is_table = True
        table_name = "td_model"


class Product(Model):
    """Model with @computed_field for regression test."""

    id: int | None = Field(default=None, db_pk=True)
    price: float = Field(default=0.0)
    quantity: int = Field(default=1)

    @computed_field
    @property
    def total(self) -> float:
        return self.price * self.quantity

    class Meta:
        is_table = True
        table_name = "products"


class FactoryKeyed(Model):
    """UUID pk from default_factory — no server-side default on the column."""

    id: UUID | None = Field(default_factory=uuid.uuid4, db_pk=True)
    name: str = Field(max_length=100)
    status: str = Field(default="active", max_length=20)

    class Meta:
        is_table = True
        table_name = "factory_keyed"


class RenamedParent(Model):
    """Related model whose pk and list field both set db_column."""

    id: int | None = Field(default=None, db_pk=True, db_column="parent_pk")
    name: str = Field(default="", max_length=50)
    labels: list[str] | None = Field(
        default=None, db_nullable=True, db_column="label_list"
    )

    class Meta:
        is_table = True
        table_name = "renamed_parents"


class RenamedChild(Model):
    id: int | None = Field(default=None, db_pk=True)
    parent: RenamedParent | None = Field(default=None, db_nullable=True)
    name: str = Field(default="", max_length=50)

    class Meta:
        is_table = True
        table_name = "renamed_children"


class ConvertedValues(Model):
    """Fields whose values are converted to the column's kind when bound."""

    id: int | None = Field(default=None, db_pk=True)
    name: str = Field(default="", max_length=50)
    # Naive TIMESTAMP[] on PostgreSQL: aware elements are stored as UTC
    times: list[datetime] | None = Field(default=None, db_nullable=True)
    # PostgreSQL JSONB[]
    items: list[dict | str] | None = Field(default=None, db_nullable=True)
    mixed: list[dict | int | bool] | None = Field(default=None, db_nullable=True)
    ratios: list[float] | None = Field(default=None, db_nullable=True)
    amounts: list[Decimal] | None = Field(default=None, db_nullable=True)
    # A union element type: PostgreSQL TEXT[]
    labels: list[str | int] | None = Field(default=None, db_nullable=True)
    # A str field stored as JSON
    body: str | None = Field(default=None, db_nullable=True, db_type="JSON")

    class Meta:
        is_table = True
        table_name = "converted_values"


class ArrayDoc(Model):
    """Array lookup targets: typed arrays plus JSON columns holding arrays."""

    id: int | None = Field(default=None, db_pk=True)
    name: str = Field(default="", max_length=50)
    tags: list[str | None] | None = Field(default=None, db_nullable=True)
    scores: list[int] | None = Field(default=None, db_nullable=True)
    amounts: list[Annotated[Decimal, Field(max_digits=10, decimal_places=2)]] | None = Field(
        default=None, db_nullable=True
    )
    ids: list[UUID] | None = Field(default=None, db_nullable=True)
    # Naive TIMESTAMP[]: aware elements are stored (and matched) as UTC
    times: list[datetime] | None = Field(default=None, db_nullable=True)
    # PostgreSQL JSONB[]
    items: list[dict | str] | None = Field(default=None, db_nullable=True)
    # JSONB on PostgreSQL
    payload: dict | list | None = Field(default=None, db_nullable=True)
    # Plain json on PostgreSQL (no equality operator without a jsonb cast)
    raw: list[Any] | dict | None = Field(default=None, db_nullable=True, db_type="JSON")

    class Meta:
        is_table = True
        table_name = "array_docs"


class ArrayNote(Model):
    """Joins to ArrayDoc; its own `tags` column is text, not an array."""

    id: int | None = Field(default=None, db_pk=True)
    doc: ArrayDoc | None = Field(default=None, db_nullable=True)
    tags: str = Field(default="", max_length=50)
    labels: list[str] | None = Field(
        default=None, db_nullable=True, db_column="label_list"
    )

    class Meta:
        is_table = True
        table_name = "array_notes"


class ArrayShadow(Model):
    """Array columns named like the aliases and columns array lookups use
    internally."""

    id: int | None = Field(default=None, db_pk=True)
    name: str = Field(default="", max_length=50)
    v: list[int | None] | None = Field(
        default=None, db_nullable=True, db_column="__oxyde_v"
    )
    i: list[int | None] | None = Field(
        default=None, db_nullable=True, db_column="oxyde_v"
    )
    j: list[str | None] | None = Field(
        default=None, db_nullable=True, db_column="oxyde_j"
    )
    # JSONB on PostgreSQL, whose lookups walk elements under their own alias
    c: dict | list | None = Field(default=None, db_nullable=True, db_column="oxyde_c")
    val: list[int | None] | None = Field(
        default=None, db_nullable=True, db_column="value"
    )
    kind: dict | list | None = Field(default=None, db_nullable=True, db_column="type")

    class Meta:
        is_table = True
        table_name = "array_shadows"


class ArrayShadowJson(Model):
    """A JSON column named like the PostgreSQL element alias."""

    id: int | None = Field(default=None, db_pk=True)
    name: str = Field(default="", max_length=50)
    v: dict | list | None = Field(
        default=None, db_nullable=True, db_column="__oxyde_v"
    )

    class Meta:
        is_table = True
        table_name = "array_shadow_jsons"


ALL_MODELS = [
    Event, AliasedEvent, Author, Category, Post, Comment, Tag, PostTag,
    AllTypes, NullableTypes, BytesModel, TdModel, Product, FactoryKeyed,
    RenamedParent, RenamedChild, ConvertedValues,
    ArrayDoc, ArrayNote, ArrayShadow, ArrayShadowJson,
]


# ── Table management ───────────────────────────────────────────────────

async def _ensure_tables(database: AsyncDatabase) -> None:
    """Create tables in the fresh PostgreSQL schema.

    Each fixture gets its own PostgreSQL schema.

    Clears registry and re-registers ALL_MODELS right before schema extraction.
    Other test suites (smoke) may register models with same table_name but
    different schema. Registry is keyed by class path, not table_name, so both
    survive and last-write-wins in extract_current_schema. Clear first to avoid.
    """
    clear_registry()
    for model in ALL_MODELS:
        register_table(model, overwrite=True)

    await create_tables(database)


async def _fix_pg_sequences(database: AsyncDatabase) -> None:
    """Reset PG sequences after explicit-ID inserts."""
    for model in ALL_MODELS:
        # Only integer pks have a serial sequence (UUID pks: FactoryKeyed)
        base_hint, _ = _unpack_annotated(model.model_fields["id"].annotation)
        pk_type, _ = _unwrap_optional(base_hint)
        if pk_type is not int:
            continue
        table = model.Meta.table_name
        pk = model._db_meta.pk_column or "id"
        await execute_raw(
            f"SELECT setval(pg_get_serial_sequence('{table}', '{pk}'), "
            f"GREATEST(COALESCE((SELECT MAX({pk}) FROM {table}), 1), 1))",
            using=database.name,
        )


# ── Seed data ──────────────────────────────────────────────────────────

MAIN_SEED = [
    "INSERT INTO authors (id, name, email, active) VALUES "
    "(1, 'Alice', 'alice@test.com', true), "
    "(2, 'Bob', 'bob@test.com', true), "
    "(3, 'Charlie', 'charlie@test.com', false)",
    "INSERT INTO categories (id, name) VALUES (1, 'Tech'), (2, 'Science')",
    "INSERT INTO posts (id, title, body, author_id, category_id, views, published) VALUES "
    "(1, 'Rust Patterns', '', 1, 1, 120, true), "
    "(2, 'Async Python', '', 1, 1, 35, true), "
    "(3, 'Quantum Computing', '', 2, 2, 80, true), "
    "(4, 'Draft Post', '', 2, NULL, 0, false), "
    "(5, 'ML Basics', '', 3, 2, 200, true), "
    "(6, 'Unpublished', '', 3, NULL, 0, false)",
    "INSERT INTO comments (id, post_id, body) VALUES "
    "(1, 1, 'Great read!'), "
    "(2, 1, 'Thanks for sharing'), "
    "(3, 3, 'Interesting topic')",
    "INSERT INTO tags (id, name) VALUES (1, 'Python'), (2, 'Rust'), (3, 'SQL')",
    "INSERT INTO post_tags (post_id, tag_id) VALUES (1, 2), (1, 1), (2, 1), (3, 3)",
]

EVENT_SEED = [
    "INSERT INTO events (title, created_at) VALUES "
    "('Morning A', '2026-03-14 09:34:18'), "
    "('Morning B', '2026-03-14 09:34:18'), "
    "('Morning C', '2026-03-14 09:34:18'), "
    "('Midnight', '2026-03-15 00:00:00'), "
    "('Next Day', '2026-03-16 00:00:00')",
]

ALIASED_EVENT_SEED = [
    "INSERT INTO aliased_events (event_title, created_at) VALUES "
    "('Morning A', '2026-03-14 09:34:18'), "
    "('Midnight', '2026-03-15 00:00:00')",
]


async def _seed(database: AsyncDatabase, statements: list[str]) -> None:
    for sql in statements:
        await execute_raw(sql, using=database.name)
    if detect_dialect(database.url) == "postgres":
        await _fix_pg_sequences(database)


# ── Fixtures ───────────────────────────────────────────────────────────


@pytest.fixture(autouse=True)
def _register_models():
    """Re-register models (may be cleared by unit test cleanup)."""
    for model in ALL_MODELS:
        register_table(model, overwrite=True)


@pytest_asyncio.fixture(params=_DIALECTS)
async def db(request, postgres_database_url):
    """Fresh DB with main seed data, parametrized by dialect."""
    url = postgres_database_url
    database = AsyncDatabase(
        url, name=f"test_{request.param}", overwrite=True
    )
    await database.connect()
    await _ensure_tables(database)
    await _seed(database, MAIN_SEED)

    yield database
    await disconnect_all()


@pytest_asyncio.fixture(params=_DIALECTS)
async def event_db(request, postgres_database_url):
    """Fresh DB with event seed data for datetime filtering tests."""
    url = postgres_database_url
    database = AsyncDatabase(
        url, name=f"evt_{request.param}", overwrite=True
    )
    await database.connect()
    await _ensure_tables(database)
    await _seed(database, EVENT_SEED)

    yield database
    await disconnect_all()


@pytest_asyncio.fixture(params=_DIALECTS)
async def aliased_db(request, postgres_database_url):
    """Fresh DB with aliased_events seed for db_column remapping tests."""
    url = postgres_database_url
    database = AsyncDatabase(
        url, name=f"alias_{request.param}", overwrite=True
    )
    await database.connect()
    await _ensure_tables(database)
    await _seed(database, ALIASED_EVENT_SEED)

    yield database
    await disconnect_all()


# ── Factories ──────────────────────────────────────────────────────────


async def create_author(db, **overrides):
    """Create an Author with sensible defaults."""
    defaults = {"name": "Test Author", "email": f"{uuid.uuid4().hex}@test.com"}
    defaults.update(overrides)
    return await Author.objects.create(**defaults, using=db.name)


async def create_post(db, *, author_id, **overrides):
    """Create a Post with sensible defaults."""
    defaults = {"title": "Test Post", "author_id": author_id}
    defaults.update(overrides)
    return await Post.objects.create(**defaults, using=db.name)


async def create_tag(db, **overrides):
    """Create a Tag with a unique name."""
    defaults = {"name": f"tag_{uuid.uuid4().hex[:8]}"}
    defaults.update(overrides)
    return await Tag.objects.create(**defaults, using=db.name)
