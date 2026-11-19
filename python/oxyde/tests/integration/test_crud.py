"""Integration tests for CRUD operations."""

from __future__ import annotations

from contextlib import nullcontext

import pytest

from oxyde import F, Field, Model, atomic
from oxyde.exceptions import (
    IntegrityError,
    MultipleObjectsReturned,
    NotFoundError,
    UniqueViolationError,
)
from oxyde.queries import Count, execute_raw
from oxyde.queries.insert import InsertQuery

from .conftest import (
    AliasCollision,
    AliasCollisionChild,
    AliasDocument,
    Author,
    ExcludedNamed,
    FactoryKeyed,
    Post,
    Product,
    Singleton,
    create_author,
)


@pytest.mark.asyncio
async def test_json_aliases_round_trip_through_select_join_and_writes(db):
    await execute_raw(
        """INSERT INTO alias_collisions (row_pk, external_ref, name, document)
           VALUES (9, 'ref', 'stored', '{"externalCode":"stored"}')""",
        using=db.name,
    )
    stored = await AliasCollision.objects.get(id=9, using=db.name)
    assert stored.document == AliasDocument(externalCode="stored")
    child = await AliasCollisionChild.objects.create(parent_id=9, using=db.name)
    joined = (
        await AliasCollisionChild.objects.join("parent")
        .filter(id=child.id)
        .get(using=db.name)
    )
    assert joined.parent is not None
    assert joined.parent.document == stored.document

    inserted = await AliasCollision.objects.create(
        name="created", document=AliasDocument(externalCode="created"), using=db.name
    )
    assert inserted.document == AliasDocument(externalCode="created")
    assert await execute_raw(
        "SELECT document FROM alias_collisions WHERE row_pk = $1",
        [inserted.id],
        using=db.name,
    ) == [{"document": {"externalCode": "created"}}]

    stored.document = AliasDocument(externalCode="saved")
    await stored.save(using=db.name, update_fields=["document"])
    assert await execute_raw(
        "SELECT document FROM alias_collisions WHERE row_pk = 9", using=db.name
    ) == [{"document": {"externalCode": "saved"}}]

    stored.document = AliasDocument(externalCode="bulk")
    await AliasCollision.objects.bulk_update([stored], ["document"], using=db.name)
    assert await execute_raw(
        "SELECT document FROM alias_collisions WHERE row_pk = 9", using=db.name
    ) == [{"document": {"externalCode": "bulk"}}]


@pytest.mark.asyncio
async def test_conflict_expression_reads_the_existing_row(db):
    post = await Post.objects.get(id=1, using=db.name)
    query = (
        InsertQuery(Post)
        .values(
            id=post.id,
            title="proposed",
            author_id=1,
            category_id=1,
            views=999,
            body="",
            published=False,
        )
        .on_conflict_update(["title"], values={"views": F("views") + 1})
    )
    await query.execute(db)
    result = await Post.objects.get(id=1, using=db.name)
    assert (result.title, result.views) == ("proposed", post.views + 1)


@pytest.mark.asyncio
@pytest.mark.parametrize("in_transaction", [False, True])
async def test_bulk_trigger_only_confirms_rows_that_were_inserted(db, in_transaction):
    class GeneratedIntKey(Model):
        id: int | None = Field(default=None, db_pk=True, db_type="INTEGER")
        name: str

        class Meta:
            is_table = True
            table_name = "generated_int_keys"

    await execute_raw(
        "CREATE TABLE generated_int_keys (id SERIAL PRIMARY KEY, name text NOT NULL)",
        using=db.name,
    )
    await execute_raw(
        """CREATE FUNCTION skip_named_row() RETURNS trigger LANGUAGE plpgsql AS $$
           BEGIN
             IF NEW.name = 'skip' THEN RETURN NULL; END IF;
             RETURN NEW;
           END $$""",
        using=db.name,
    )
    for table in ("factory_keyed", "generated_int_keys"):
        await execute_raw(
            f"CREATE TRIGGER skip_first BEFORE INSERT ON {table} "
            "FOR EACH ROW EXECUTE FUNCTION skip_named_row()",
            using=db.name,
        )

    suppressed, written = FactoryKeyed(name="skip"), FactoryKeyed(name="write")
    skipped_int, written_int = (
        GeneratedIntKey(name="skip"),
        GeneratedIntKey(name="write"),
    )
    context = atomic(using=db.name) if in_transaction else nullcontext()
    async with context:
        await FactoryKeyed.objects.bulk_create([suppressed, written], using=db.name)
        with pytest.warns(RuntimeWarning, match="could not match generated keys"):
            await GeneratedIntKey.objects.bulk_create(
                [skipped_int, written_int], using=db.name
            )
        extra = GeneratedIntKey(name="extra")
        await GeneratedIntKey.objects.bulk_create([extra], using=db.name)

    assert "id" not in suppressed.model_fields_set
    assert "id" in written.model_fields_set
    assert skipped_int.id is None and written_int.id is None
    assert isinstance(extra.id, int)
    assert not await FactoryKeyed.objects.filter(id=suppressed.id).exists(using=db.name)
    await execute_raw("DROP TRIGGER skip_first ON factory_keyed", using=db.name)
    await suppressed.save(using=db.name)
    assert await FactoryKeyed.objects.filter(id=suppressed.id).exists(using=db.name)


@pytest.mark.asyncio
@pytest.mark.parametrize("path", ["select", "join", "create", "update"])
async def test_rows_validate_by_field_name_when_alias_collides(db, path):
    if path == "create":
        instance = await AliasCollision.objects.create(name="created", using=db.name)
        assert instance.id is not None
        assert (instance.external_id, instance.name) == ("ref", "created")
        return

    await execute_raw(
        "INSERT INTO alias_collisions (row_pk, external_ref, name) "
        "VALUES (5, 'abc', 'original')",
        using=db.name,
    )
    if path == "update":
        instance = AliasCollision(id=5, name="updated")
        instance.external_id = "abc"
        await instance.save(using=db.name)
        expected_name = "updated"
    elif path == "join":
        await execute_raw(
            "INSERT INTO alias_collision_children (parent_id) VALUES (5)",
            using=db.name,
        )
        child = await AliasCollisionChild.objects.join("parent").get(using=db.name)
        instance = child.parent
        expected_name = "original"
    else:
        instance = await AliasCollision.objects.get(id=5, using=db.name)
        expected_name = "original"
    assert instance is not None
    assert (instance.id, instance.external_id, instance.name) == (
        5,
        "abc",
        expected_name,
    )


class TestCreate:
    @pytest.mark.asyncio
    async def test_create_single(self, db):
        author = await Author.objects.create(
            name="Diana", email="diana@test.com", using=db.name
        )
        assert author.name == "Diana"
        assert author.email == "diana@test.com"
        assert author.id is not None

    @pytest.mark.asyncio
    async def test_create_with_defaults(self, db):
        """Fields with defaults should get default values when not passed."""
        post = await Post.objects.create(
            title="Default Test", author_id=1, using=db.name
        )
        assert post.title == "Default Test"
        assert post.body == ""
        assert post.views == 0
        assert post.published == False

    @pytest.mark.asyncio
    async def test_create_sends_default_factory_pk(self, db):
        """default_factory pk must reach the INSERT, not become NULL.

        The column has no server-side default, so if the serializer drops the
        generated UUID this fails with a not-null violation.
        """
        obj = await FactoryKeyed.objects.create(name="Alice", using=db.name)
        assert obj.id is not None
        fetched = await FactoryKeyed.objects.get(id=obj.id, using=db.name)
        assert fetched.name == "Alice"
        # Plain default= must be stored too, not just shown on the instance
        assert fetched.status == "active"

    @pytest.mark.asyncio
    async def test_create_with_null_fk(self, db):
        """Nullable FK can be None."""
        post = await Post.objects.create(
            title="No Category", author_id=1, using=db.name
        )
        assert post.category_id is None

    @pytest.mark.asyncio
    async def test_duplicate_unique_raises_typed_integrity_error(self, db):
        """Constraint violations are classified by the driver (sqlx ErrorKind),
        so all dialects raise UniqueViolationError — a subclass of
        IntegrityError — without any error-message matching."""
        await Author.objects.create(
            name="Dup", email="dup@test.com", using=db.name
        )
        with pytest.raises(UniqueViolationError) as exc_info:
            await Author.objects.create(
                name="Dup2", email="dup@test.com", using=db.name
            )
        assert isinstance(exc_info.value, IntegrityError)


class TestBulkCreate:
    @pytest.mark.asyncio
    async def test_bulk_create(self, db):
        authors = [
            Author(name="X", email="x@test.com"),
            Author(name="Y", email="y@test.com"),
        ]
        created = await Author.objects.bulk_create(authors, using=db.name)
        assert len(created) == 2
        assert created[0].name == "X"
        assert created[1].name == "Y"

        count = await Author.objects.count(using=db.name)
        assert count == 5  # 3 seed + 2 new

    @pytest.mark.asyncio
    async def test_bulk_create_empty(self, db):
        created = await Author.objects.bulk_create([], using=db.name)
        assert created == []

    @pytest.mark.asyncio
    async def test_save_after_bulk_create_with_default_factory_pk(self, db):
        obj = FactoryKeyed(name="c")
        pk = obj.id
        await FactoryKeyed.objects.bulk_create([obj], using=db.name)
        obj.name = "d"
        await obj.save(using=db.name)

        assert await FactoryKeyed.objects.count(using=db.name) == 1
        fetched = await FactoryKeyed.objects.get(id=pk, using=db.name)
        assert fetched.name == "d"


class TestGet:
    @pytest.mark.asyncio
    async def test_get_found(self, db):
        author = await Author.objects.get(id=1, using=db.name)
        assert author.name == "Alice"
        assert author.email == "alice@test.com"

    @pytest.mark.asyncio
    async def test_get_not_found(self, db):
        with pytest.raises(NotFoundError):
            await Author.objects.get(id=999, using=db.name)

    @pytest.mark.asyncio
    async def test_get_multiple(self, db):
        with pytest.raises(MultipleObjectsReturned):
            await Author.objects.get(active=True, using=db.name)

    @pytest.mark.asyncio
    async def test_get_or_none_found(self, db):
        author = await Author.objects.get_or_none(id=1, using=db.name)
        assert author is not None
        assert author.name == "Alice"

    @pytest.mark.asyncio
    async def test_get_or_none_not_found(self, db):
        author = await Author.objects.get_or_none(id=999, using=db.name)
        assert author is None

    @pytest.mark.asyncio
    async def test_get_or_create_existing(self, db):
        author, created = await Author.objects.get_or_create(
            email="alice@test.com",
            defaults={"name": "Alice Clone"},
            using=db.name,
        )
        assert created is False
        assert author.name == "Alice"

    @pytest.mark.asyncio
    async def test_get_or_create_new(self, db):
        author, created = await Author.objects.get_or_create(
            email="new@test.com",
            defaults={"name": "New Author"},
            using=db.name,
        )
        assert created is True
        assert author.name == "New Author"
        assert author.email == "new@test.com"


class TestUpdateOrCreate:
    @pytest.mark.asyncio
    async def test_update_or_create_existing_changed(self, db):
        author, created = await Author.objects.update_or_create(
            email="alice@test.com",
            defaults={"name": "Alice Updated"},
            using=db.name,
        )
        assert created is False
        assert author.name == "Alice Updated"

        refreshed = await Author.objects.get(email="alice@test.com", using=db.name)
        assert refreshed.name == "Alice Updated"

    @pytest.mark.asyncio
    async def test_update_or_create_new(self, db):
        author, created = await Author.objects.update_or_create(
            email="created@test.com",
            defaults={"name": "Created Author"},
            using=db.name,
        )
        assert created is True
        assert author.name == "Created Author"
        assert author.email == "created@test.com"


class TestSave:
    @pytest.mark.asyncio
    async def test_save_insert(self, db):
        author = Author(name="SavedNew", email="saved@test.com")
        saved = await author.save(using=db.name)
        assert saved.id is not None

        fetched = await Author.objects.get(id=saved.id, using=db.name)
        assert fetched.name == "SavedNew"

    @pytest.mark.asyncio
    async def test_save_twice_with_default_factory_pk(self, db):
        obj = FactoryKeyed(name="a")
        pk = obj.id
        await obj.save(using=db.name)
        obj.name = "b"
        await obj.save(using=db.name)

        assert await FactoryKeyed.objects.count(using=db.name) == 1
        fetched = await FactoryKeyed.objects.get(id=pk, using=db.name)
        assert fetched.name == "b"

    @pytest.mark.asyncio
    async def test_save_scalar_default_pk_updates_on_conflict(self, db, monkeypatch):
        outcomes: list[bool] = []

        async def post_save(self, *, is_create, update_fields=None):
            outcomes.append(is_create)

        monkeypatch.setattr(Singleton, "post_save", post_save)
        await Singleton(theme="a").save(using=db.name)
        updated = await Singleton(theme="b").save(using=db.name)

        assert updated.id == 1
        assert outcomes == [True, False]
        assert await Singleton.objects.count(using=db.name) == 1
        assert (await Singleton.objects.get(id=1, using=db.name)).theme == "b"

    @pytest.mark.asyncio
    async def test_save_retries_after_trigger_suppresses_default_pk_upsert(
        self, db, monkeypatch
    ):
        outcomes: list[bool] = []

        async def pre_save(self, *, is_create, update_fields=None):
            self.id = 2

        async def post_save(self, *, is_create, update_fields=None):
            outcomes.append(is_create)

        monkeypatch.setattr(Singleton, "pre_save", pre_save)
        monkeypatch.setattr(Singleton, "post_save", post_save)
        await execute_raw(
            "CREATE FUNCTION suppress_singleton() RETURNS trigger LANGUAGE plpgsql "
            "AS $$ BEGIN RETURN NULL; END; $$",
            using=db.name,
        )
        await execute_raw(
            "CREATE TRIGGER suppress_insert BEFORE INSERT ON singletons "
            "FOR EACH ROW EXECUTE FUNCTION suppress_singleton()",
            using=db.name,
        )

        obj = Singleton(theme="a")
        await obj.save(using=db.name)
        assert await Singleton.objects.count(using=db.name) == 0
        assert "id" not in obj.model_fields_set

        await execute_raw("DROP TRIGGER suppress_insert ON singletons", using=db.name)
        await obj.save(using=db.name)

        assert outcomes == [False, True]
        assert (await Singleton.objects.get(id=2, using=db.name)).theme == "a"

    @pytest.mark.asyncio
    async def test_save_without_updates_retries_after_trigger_suppresses_insert(
        self, db, monkeypatch
    ):
        outcomes: list[bool] = []

        async def post_save(self, *, is_create, update_fields=None):
            outcomes.append(is_create)

        monkeypatch.setattr(Singleton, "post_save", post_save)
        await execute_raw(
            "CREATE FUNCTION suppress_singleton() RETURNS trigger LANGUAGE plpgsql "
            "AS $$ BEGIN RETURN NULL; END; $$",
            using=db.name,
        )
        await execute_raw(
            "CREATE TRIGGER suppress_insert BEFORE INSERT ON singletons "
            "FOR EACH ROW EXECUTE FUNCTION suppress_singleton()",
            using=db.name,
        )

        obj = Singleton(theme="a")
        await obj.save(using=db.name, update_fields=[])
        assert await Singleton.objects.count(using=db.name) == 0
        assert "id" not in obj.model_fields_set

        await execute_raw("DROP TRIGGER suppress_insert ON singletons", using=db.name)
        await obj.save(using=db.name)

        assert outcomes == [False, True]
        assert (await Singleton.objects.get(id=1, using=db.name)).theme == "a"

    @pytest.mark.asyncio
    async def test_save_default_pk_upserts_table_named_excluded(self, db, monkeypatch):
        outcomes: list[bool] = []

        async def post_save(self, *, is_create, update_fields=None):
            outcomes.append(is_create)

        monkeypatch.setattr(ExcludedNamed, "post_save", post_save)
        await ExcludedNamed(name="a").save(using=db.name)
        await ExcludedNamed(name="b").save(using=db.name)

        assert outcomes == [True, False]
        assert await ExcludedNamed.objects.count(using=db.name) == 1
        assert (await ExcludedNamed.objects.get(id=1, using=db.name)).name == "b"

    @pytest.mark.asyncio
    async def test_save_default_pk_preserves_server_default_on_insert(self, db):
        obj = Singleton(theme="a")
        await obj.save(using=db.name, update_fields=["note"])
        assert obj.note == "server"

        updated = Singleton(theme="b")
        await updated.save(using=db.name, update_fields=["note"])
        assert updated.note is None
        assert updated.theme == "a"

    @pytest.mark.asyncio
    async def test_save_default_pk_with_empty_update_fields_leaves_row_untouched(
        self, db
    ):
        await Singleton(theme="original").save(using=db.name)
        await Singleton(theme="changed").save(using=db.name, update_fields=[])

        assert (await Singleton.objects.get(id=1, using=db.name)).theme == "original"

    @pytest.mark.asyncio
    async def test_save_partially_selected_model_updates_original_row(self, db):
        original = await FactoryKeyed.objects.create(
            name="a", status="archived", using=db.name
        )
        selected = (
            await FactoryKeyed.objects.filter().select("name").first(using=db.name)
        )
        assert selected is not None

        assert selected.id == original.id
        selected.name = "b"
        await selected.save(using=db.name)

        assert await FactoryKeyed.objects.count(using=db.name) == 1
        fetched = await FactoryKeyed.objects.get(id=original.id, using=db.name)
        assert fetched.name == "b"
        assert fetched.status == "archived"

    @pytest.mark.asyncio
    async def test_save_updates_instance_passed_to_create(self, db):
        original_count = await Author.objects.count(using=db.name)
        author = Author(name="x", email="in-place@example.com")
        created = await Author.objects.create(instance=author, using=db.name)

        assert created is author
        assert author.id is not None
        author.name = "y"
        await author.save(using=db.name)

        assert await Author.objects.count(using=db.name) == original_count + 1
        fetched = await Author.objects.get(id=author.id, using=db.name)
        assert fetched.name == "y"

    @pytest.mark.asyncio
    async def test_save_update(self, db):
        author = await Author.objects.get(id=1, using=db.name)
        author.name = "Alice Updated"
        await author.save(using=db.name)

        refreshed = await Author.objects.get(id=1, using=db.name)
        assert refreshed.name == "Alice Updated"


@pytest.mark.asyncio
async def test_union_with_aggregate_branch_runs(db):
    await Product.objects.create(price=2.0, quantity=3, using=db.name)
    query = Product.objects.filter()
    union = query.select("quantity").union_all(
        query.select("quantity").annotate(product_count=Count("id"))
    )

    rows = await union.all(using=db.name)

    assert len(rows) == await Product.objects.count(using=db.name) + 1


@pytest.mark.asyncio
async def test_bulk_insert_skips_conflicting_rows(db):
    from oxyde.queries.insert import InsertQuery

    await Singleton(theme="kept").save(using=db.name)
    query = (
        InsertQuery(Singleton)
        .bulk_values([{"id": 1, "theme": "dup"}, {"id": 2, "theme": "new"}])
        .on_conflict_do_nothing()
    )
    await query.execute(db)

    rows = await Singleton.objects.filter().order_by("id").all(using=db.name)
    assert [(row.id, row.theme) for row in rows] == [(1, "kept"), (2, "new")]


@pytest.mark.asyncio
async def test_union_with_values_list_uses_the_same_projection_in_both_branches(db):
    query = Author.objects.filter().select("name")
    names = await query.union(query).values_list("name", flat=True).all(using=db.name)

    assert "Alice" in names


class TestDelete:
    @pytest.mark.asyncio
    async def test_delete_instance(self, db):
        # Create an author with no FK dependencies so delete succeeds
        author = await create_author(db, name="ToDelete")
        author_id = author.id

        result = await author.delete(using=db.name)
        assert result >= 1

        deleted = await Author.objects.get_or_none(id=author_id, using=db.name)
        assert deleted is None

    @pytest.mark.asyncio
    async def test_delete_queryset(self, db):
        deleted = await Post.objects.filter(published=False).delete(using=db.name)
        assert deleted == 2  # Draft Post + Unpublished

        remaining = await Post.objects.count(using=db.name)
        assert remaining == 4


class TestRefresh:
    @pytest.mark.asyncio
    async def test_refresh(self, db):
        author = await Author.objects.get(id=1, using=db.name)
        assert author.name == "Alice"

        # Modify directly in DB (placeholder syntax differs per dialect)
        if db.url.startswith("postgres"):
            sql = "UPDATE authors SET name = $1 WHERE id = $2"
        else:
            sql = "UPDATE authors SET name = ? WHERE id = ?"
        await execute_raw(sql, ["Alice Modified", 1], using=db.name)

        # Instance still has old value
        assert author.name == "Alice"

        # Refresh reloads from DB
        await author.refresh(using=db.name)
        assert author.name == "Alice Modified"


class TestUpdate:
    @pytest.mark.asyncio
    async def test_update_queryset(self, db):
        result = await Post.objects.filter(published=True).update(
            views=0, using=db.name
        )
        assert result == 4  # 4 published posts

        post = await Post.objects.get(id=1, using=db.name)
        assert post.views == 0

    @pytest.mark.asyncio
    async def test_update_returning(self, db):
        result = await Post.objects.filter(id=1).update(
            views=999, returning=True, using=db.name
        )
        assert len(result) == 1
        assert result[0].views == 999
        assert result[0].title == "Rust Patterns"

    @pytest.mark.asyncio
    async def test_update_with_f_expression(self, db):
        await Post.objects.filter(id=1).update(views=F("views") + 1, using=db.name)
        post = await Post.objects.get(id=1, using=db.name)
        assert post.views == 121  # 120 + 1

    @pytest.mark.asyncio
    async def test_increment(self, db):
        result = await Post.objects.filter(id=1).increment("views", by=5, using=db.name)
        assert result >= 1

        post = await Post.objects.get(id=1, using=db.name)
        assert post.views == 125  # 120 + 5


class TestComputedField:
    """Computed fields must be excluded from INSERT/UPDATE payloads."""

    @pytest.mark.asyncio
    async def test_create_with_computed_field(self, db):
        product = await Product.objects.create(
            price=10.0, quantity=3, using=db.name
        )
        assert product.id is not None
        assert product.price == 10.0
        assert product.quantity == 3
        assert product.total == 30.0

    @pytest.mark.asyncio
    async def test_bulk_create_with_computed_field(self, db):
        products = [
            Product(price=5.0, quantity=2),
            Product(price=20.0, quantity=4),
        ]
        created = await Product.objects.bulk_create(products, using=db.name)
        assert len(created) == 2
        assert created[0].total == 10.0
        assert created[1].total == 80.0

    @pytest.mark.asyncio
    async def test_select_with_computed_field(self, db):
        await Product.objects.create(price=7.0, quantity=5, using=db.name)
        product = await Product.objects.first(using=db.name)
        assert product is not None
        assert product.total == 35.0
