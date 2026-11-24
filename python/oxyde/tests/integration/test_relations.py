"""Integration tests for joins and prefetch (FK, reverse FK, M2M)."""
from __future__ import annotations

import pytest

from oxyde import Field, Model, execute_raw
from oxyde.models.registry import register_table

from .conftest import Author, Post, RenamedChild, RenamedParent


class ArrayKeyParent(Model):
    id: int | None = Field(default=None, db_pk=True, db_column="parent_pk")
    tags: list[str] = Field(db_unique=True)

    class Meta:
        is_table = True
        table_name = "array_key_parents"


class ArrayKeyChild(Model):
    id: int | None = Field(default=None, db_pk=True)
    parent: ArrayKeyParent | None = Field(default=None, db_fk="tags")
    parent_tags: list[str] | None = None

    class Meta:
        is_table = True
        table_name = "array_key_children"


class TestJoin:
    @pytest.mark.asyncio
    async def test_join_fk(self, db):
        """Join Post → Author via FK."""
        posts = await Post.objects.join("author").order_by("id").all(using=db.name)
        assert len(posts) == 6

        assert posts[0].author is not None
        assert posts[0].author.name == "Alice"
        assert posts[2].author.name == "Bob"

    @pytest.mark.asyncio
    async def test_related_model_with_db_column_renames(self, db):
        """Joined rows hydrate a related model whose pk and list field set
        db_column (result columns are aliased by field name)."""
        rust = await RenamedParent.objects.create(
            name="rust", labels=["a", "b"], using=db.name
        )
        go = await RenamedParent.objects.create(name="go", labels=None, using=db.name)
        await RenamedChild.objects.create(name="r1", parent_id=rust.id, using=db.name)
        await RenamedChild.objects.create(name="r2", parent_id=rust.id, using=db.name)
        await RenamedChild.objects.create(name="g1", parent_id=go.id, using=db.name)

        children = await (
            RenamedChild.objects.join("parent").order_by("id").all(using=db.name)
        )
        assert [(c.name, c.parent.id, c.parent.labels) for c in children] == [
            ("r1", rust.id, ["a", "b"]),
            ("r2", rust.id, ["a", "b"]),
            ("g1", go.id, None),
        ]

        # Filtering through the FK joins the parent the same way.
        children = await (
            RenamedChild.objects.filter(parent__name="rust")
            .order_by("id")
            .all(using=db.name)
        )
        assert [c.name for c in children] == ["r1", "r2"]
        assert children[0].parent.labels == ["a", "b"]

    @pytest.mark.asyncio
    async def test_join_deduplicates_related_rows_referenced_by_an_array_key(self, db):
        register_table(ArrayKeyParent)
        register_table(ArrayKeyChild)
        # Declare the FK inline to avoid PostgreSQL's polymorphic array
        # comparison during ALTER TABLE's existing-row validation.
        await execute_raw(
            "CREATE TABLE array_key_parents "
            "(parent_pk BIGSERIAL PRIMARY KEY, tags TEXT[] NOT NULL UNIQUE)",
            using=db.name,
        )
        await execute_raw(
            "CREATE TABLE array_key_children "
            "(id BIGSERIAL PRIMARY KEY, parent_tags TEXT[] REFERENCES array_key_parents (tags))",
            using=db.name,
        )
        parent = await ArrayKeyParent.objects.create(
            tags=["rust", "python"], using=db.name
        )
        for _ in range(2):
            await ArrayKeyChild.objects.create(parent_tags=parent.tags, using=db.name)
        await ArrayKeyChild.objects.create(parent_tags=None, using=db.name)

        children = await (
            ArrayKeyChild.objects.join("parent").order_by("id").all(using=db.name)
        )
        assert len(children) == 3
        related = children[0].parent
        assert related is not None
        assert (related.id, related.tags) == (parent.id, parent.tags)
        assert children[1].parent is related
        assert children[2].parent is None


class TestPrefetch:
    @pytest.mark.asyncio
    async def test_prefetch_one_to_many(self, db):
        """Prefetch Author → Posts (one-to-many)."""
        authors = await (
            Author.objects.prefetch("posts").order_by("id").all(using=db.name)
        )
        assert len(authors) == 3

        alice = authors[0]
        assert alice.name == "Alice"
        assert len(alice.posts) == 2
        assert {p.title for p in alice.posts} == {"Rust Patterns", "Async Python"}

        charlie = authors[2]
        assert len(charlie.posts) == 2

    @pytest.mark.asyncio
    async def test_prefetch_with_filter(self, db):
        """Prefetch comments only for filtered posts."""
        posts = await (
            Post.objects.filter(id=1).prefetch("comments").all(using=db.name)
        )
        assert len(posts) == 1
        assert len(posts[0].comments) == 2
        bodies = {c.body for c in posts[0].comments}
        assert bodies == {"Great read!", "Thanks for sharing"}

    @pytest.mark.asyncio
    async def test_prefetch_m2m(self, db):
        """Prefetch Post → Tags via M2M through PostTag."""
        posts = await (
            Post.objects.filter(id__in=[1, 2, 3])
            .prefetch("tags")
            .order_by("id")
            .all(using=db.name)
        )
        assert len(posts) == 3

        rust_patterns = posts[0]
        tag_names = {t.name for t in rust_patterns.tags}
        assert tag_names == {"Python", "Rust"}

        async_python = posts[1]
        assert {t.name for t in async_python.tags} == {"Python"}

        quantum = posts[2]
        assert {t.name for t in quantum.tags} == {"SQL"}
