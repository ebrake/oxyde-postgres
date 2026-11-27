"""Foreign-key synchronization on application input and writes."""

from __future__ import annotations

from uuid import UUID, uuid4

import pytest
from pydantic import BaseModel, ValidationError

from oxyde import Field, Model
from oxyde.exceptions import FieldError
from oxyde.models.db_schema import _validate_db_row
from oxyde.models.registry import register_table
from oxyde.models.serializers import _prepare_fk_columns
from oxyde.models.utils import _AggregateFieldsSet
from oxyde.queries import Count
from oxyde.tests.helpers import StubExecuteClient


class Author(Model):
    id: int | None = Field(default=None, db_pk=True)
    name: str = "Author"

    class Meta:
        is_table = True


class Post(Model):
    id: int | None = Field(default=None, db_pk=True)
    title: str = "Post"
    author: Author | None = None

    class Meta:
        is_table = True


class FactoryAuthor(Model):
    id: UUID = Field(default_factory=uuid4, db_pk=True)
    code: str = Field(default="author", db_unique=True)
    name: str = "Author"
    group_id: int | None = None

    class Meta:
        is_table = True


class FactoryPost(Model):
    id: int | None = Field(default=None, db_pk=True)
    author: FactoryAuthor | None = None
    reviewer: FactoryAuthor | None = Field(default=None, db_fk="code")

    class Meta:
        is_table = True


def post_row(author_id):
    return {"columns": ["id", "title", "author_id"], "rows": [[1, "Post", author_id]]}


def factory_author_row(author):
    return {
        "columns": ["id", "code", "name"],
        "rows": [[str(author.id), author.code, author.name]],
    }


def assert_factory_author_keys_available(author):
    assert {"id", "code"} <= author.model_fields_set
    post = FactoryPost(author=author, reviewer=author)
    assert (post.author_id, post.reviewer_code) == (author.id, author.code)


def test_default_factory_pk_requires_provenance():
    author = FactoryAuthor()
    post = FactoryPost(author=author)
    assert post.author_id is None
    with pytest.raises(ValueError, match="save the related object first"):
        _prepare_fk_columns(post, None)

    author._mark_pk_persisted()
    _prepare_fk_columns(post, None)
    assert post.author_id == author.id
    assert FactoryPost(author=author).author_id == author.id


@pytest.mark.asyncio
async def test_default_non_pk_target_requires_provenance():
    author = FactoryAuthor()
    post = FactoryPost(reviewer=author)
    assert post.reviewer_code is None
    with pytest.raises(ValueError, match="save the related object first"):
        _prepare_fk_columns(post, None)

    stub = StubExecuteClient([factory_author_row(author)])
    await FactoryAuthor.objects.create(instance=author, client=stub)
    _prepare_fk_columns(post, None)
    assert post.reviewer_code == author.code


def test_required_fk_rejects_default_factory_key():
    class RequiredFactoryPost(Model):
        author: FactoryAuthor

        class Meta:
            is_table = True

    with pytest.raises(ValidationError, match="author_id"):
        RequiredFactoryPost(author=FactoryAuthor())


def test_partially_loaded_default_target_is_unavailable():
    author = _validate_db_row(FactoryAuthor, {"id": str(uuid4())})
    assert author.code == "author"
    post = FactoryPost(author=author, reviewer=author)
    assert post.author_id == author.id
    assert post.reviewer_code is None
    with pytest.raises(ValueError, match="save the related object first"):
        _prepare_fk_columns(post, None)


def test_constructor_sets_fk_for_related_instance():
    post = Post(author=Author(id=9))
    assert post.author_id == 9
    assert "author_id" in post.model_fields_set

    unsaved = Post(author=Author())
    assert unsaved.author_id is None
    assert "author_id" not in unsaved.model_fields_set


def test_constructor_sets_required_fk_for_related_instance():
    class RequiredPost(Model):
        author: Author

        class Meta:
            is_table = True

    assert RequiredPost(author=Author(id=9)).author_id == 9
    with_explicit_id = RequiredPost(author=Author(), author_id=9)
    assert with_explicit_id.author_id == 9
    assert with_explicit_id.author is None

    author = Author(id=9)
    with_coerced_id = RequiredPost(author=author, author_id="9")
    assert with_coerced_id.author_id == 9
    assert with_coerced_id.author is author


def test_fk_uses_target_field_and_column_overrides():
    class Tenant(Model):
        id: int = Field(db_pk=True)
        uuid: UUID = Field(db_unique=True)

        class Meta:
            is_table = True

    class Resource(Model):
        tenant: Tenant | None = Field(default=None, db_fk="uuid")
        owner: Author | None = Field(default=None, db_column="owner_key")
        reviewer: Author | None = None
        reviewer_id: int | None = Field(default=None, db_column="reviewer_key")

        class Meta:
            is_table = True

    tenant = Tenant(id=2, uuid=uuid4())
    author = Author(id=9)
    resource = Resource(tenant=tenant, owner=author, reviewer=author)
    assert resource.tenant_uuid == tenant.uuid
    assert resource.owner_key == 9
    assert resource.reviewer_id == 9
    resource.reviewer_id = 10
    assert resource.reviewer is None


@pytest.mark.parametrize("author_id", [None, 10])
def test_constructor_prefers_explicit_fk(author_id):
    post = Post(author=Author(id=9), author_id=author_id)
    assert post.author_id == author_id
    assert post.author is None


def test_explicit_null_fk_clears_unsaved_relation():
    author = Author()
    for post in (
        Post(author=author, author_id=None),
        Post.model_construct(author=author, author_id=None),
        Post(author=author).model_copy(update={"author_id": None}),
        Post(author_id=8).model_copy(update={"author": author, "author_id": None}),
    ):
        assert post.author_id is None
        assert post.author is None
        assert "author_id" in post.model_fields_set


def test_nested_input_sets_fk():
    post = Post.model_validate({"author": {"id": 9}})
    assert post.author_id == 9
    assert "author_id" in post.model_fields_set
    assert Post.model_validate({"author": {"id": 9}, "author_id": 10}).author_id == 10
    assert Post.model_validate_json('{"author":{"id":9}}').author_id == 9
    assert (
        Post.model_validate_json('{"author":{"id":9},"author_id":10}').author_id == 10
    )


def test_model_construct_sets_fk_and_tracks_injected_field():
    fields_set = {"author"}
    post = Post.model_construct(_fields_set=fields_set, author=Author(id=9))
    assert post.author_id == 9
    assert post.model_fields_set == {"author", "author_id"}
    assert fields_set == {"author"}
    conflicting = Post.model_construct(author=Author(id=9), author_id=10)
    assert conflicting.author_id == 10
    assert conflicting.author is None


def test_model_copy_sets_fk_from_update():
    post = Post()
    update = {"author": Author(id=10)}
    copied = post.model_copy(update=update)
    assert copied.author_id == 10
    assert "author_id" in copied.model_fields_set
    assert "author_id" not in update
    assert post.author_id is None
    conflicting = post.model_copy(update={**update, "author_id": 11})
    assert conflicting.author_id == 11
    assert conflicting.author is None

    column_only = copied.model_copy(update={"author_id": 11})
    assert column_only.author_id == 11
    assert column_only.author is None
    assert copied.author is update["author"]
    assert copied.model_copy(update={"author_id": 10}).author is copied.author


@pytest.mark.asyncio
async def test_copy_with_unsaved_relation_replaces_existing_fk():
    original = Post(author_id=8)
    author = Author()
    copied = original.model_copy(update={"author": author})
    assert copied.author is author
    assert copied.author_id is None
    assert "author_id" in copied.model_fields_set
    assert original.author_id == 8

    stub = StubExecuteClient([{"inserted_ids": [9]}, post_row(9)])
    with pytest.raises(ValueError, match="save the related object first"):
        await copied.save(client=stub)
    assert stub.calls == []

    await author.save(client=stub)
    await copied.save(client=stub)
    assert stub.calls[1]["values"]["author_id"] == author.id == 9
    assert copied.author is author


def test_assignment_uses_last_write():
    post = Post()
    author = Author(id=9)
    post.author = author
    assert post.author_id == 9
    assert "author_id" in post.model_fields_set

    post.author_id = 9
    assert post.author is author
    post.author_id = 10
    assert post.author is None
    post.author = author
    post.author = None
    assert post.author_id == 9
    post.author = author
    post.author_id = None
    assert post.author is None


@pytest.mark.asyncio
@pytest.mark.parametrize("returning", [True, False], ids=["returning", "ids-only"])
async def test_save_inserts_assigned_fk(returning):
    post = Post()
    other = Author(id=9)
    post.author = other
    result = post_row(9) if returning else {"inserted_ids": [1]}
    stub = StubExecuteClient([result])
    await post.save(client=stub)
    assert stub.calls[0]["op"] == "insert"
    assert stub.calls[0]["values"]["author_id"] == 9
    assert post.author is other
    assert post.author_id == other.id


@pytest.mark.asyncio
@pytest.mark.parametrize("update_fields", [None, ["author"]])
async def test_save_updates_assigned_fk(update_fields):
    post = Post(id=1, author_id=8)
    other = Author(id=9)
    post.author = other
    stub = StubExecuteClient([post_row(9)])
    await post.save(client=stub, update_fields=update_fields)
    expected = {"author_id": 9} if update_fields else {"author_id": 9, "title": "Post"}
    assert stub.calls[0]["op"] == "update"
    assert stub.calls[0]["values"] == expected
    assert post.author is other
    assert post.author_id == other.id


@pytest.mark.asyncio
@pytest.mark.parametrize("pk", [None, 1], ids=["insert", "update"])
async def test_save_drops_relation_when_returned_fk_changes(pk):
    post = Post(id=pk, author=Author(id=9))
    await post.save(client=StubExecuteClient([post_row(10)]))
    assert post.author_id == 10
    assert post.author is None


@pytest.mark.asyncio
async def test_clear_unsaved_relation_by_nulling_fk_column():
    post = Post(id=1)
    post.author = Author()
    post.author_id = None
    assert post.author is None
    stub = StubExecuteClient([post_row(None)])
    await post.save(update_fields=["author"], client=stub)
    assert stub.calls[0]["values"] == {"author_id": None}


@pytest.mark.asyncio
@pytest.mark.parametrize("constructed", [True, False])
async def test_explicit_fk_column_wins_over_unsaved_relation(constructed):
    author = Author()
    if constructed:
        post = Post(author=author, author_id=8)
        assert post.author is None
    else:
        post = Post.model_validate({"author": author, "author_id": 8})
        assert post.author is author

    stub = StubExecuteClient([post_row(8)])
    await post.save(client=stub)
    assert stub.calls[0]["values"]["author_id"] == 8
    assert post.author_id == 8
    assert post.author is None


@pytest.mark.asyncio
async def test_unsaved_related_object_blocks_only_writes_to_its_column():
    post = Post(id=1, author_id=8)
    post.author = Author()
    assert post.author_id is None
    stub = StubExecuteClient([post_row(8)])
    with pytest.raises(ValueError, match="save the related object first"):
        await post.save(client=stub)
    assert stub.calls == []

    await post.save(client=stub, update_fields=["title"])
    assert stub.calls[0]["values"] == {"title": "Post"}


@pytest.mark.asyncio
async def test_save_fills_fk_after_related_object_is_saved():
    author = Author()
    post = Post()
    post.author = author
    stub = StubExecuteClient([{"inserted_ids": [9]}, post_row(9)])
    await author.save(client=stub)
    await post.save(client=stub)
    assert stub.calls[1]["values"]["author_id"] == 9


@pytest.mark.asyncio
async def test_save_fills_fk_for_validated_related_instance():
    post = Post.model_validate({"author": Author(id=9)})
    stub = StubExecuteClient([post_row(9)])
    await post.save(client=stub)
    assert stub.calls[0]["values"]["author_id"] == 9


@pytest.mark.asyncio
async def test_bulk_create_writes_fk_for_dicts_and_instances():
    author = Author(id=9)
    post = Post.model_validate({"author": author})
    stub = StubExecuteClient([{"inserted_ids": [1, 2]}])
    await Post.objects.bulk_create([{"author": author}, post], client=stub)
    assert [row["author_id"] for row in stub.calls[0]["bulk_values"]] == [9, 9]


@pytest.mark.asyncio
async def test_bulk_update_resolves_fk_name_to_column():
    assigned = Post(id=1, author_id=8)
    assigned.author = Author(id=9)
    loaded = Post(id=2, author_id=10)
    stub = StubExecuteClient([{"affected": 2}])
    affected = await Post.objects.bulk_update(
        [assigned, loaded], ["author"], client=stub
    )
    assert affected == 2
    assert stub.calls[0]["bulk_update"]["rows"] == [
        {"filters": {"id": 1}, "values": {"author_id": 9}},
        {"filters": {"id": 2}, "values": {"author_id": 10}},
    ]


@pytest.mark.asyncio
@pytest.mark.parametrize(
    "operation",
    [
        lambda post, client: Post.objects.create(instance=post, client=client),
        lambda post, client: Post.objects.bulk_create([post], client=client),
        lambda post, client: Post.objects.bulk_update(
            [post], ["author"], client=client
        ),
    ],
    ids=["create", "bulk_create", "bulk_update"],
)
async def test_mutations_reject_unsaved_related_object(operation):
    post = Post(id=1, author=Author())
    stub = StubExecuteClient([])
    with pytest.raises(ValueError, match="save the related object first"):
        await operation(post, stub)
    assert stub.calls == []


@pytest.mark.asyncio
async def test_annotated_results_cannot_supply_related_keys():
    stub = StubExecuteClient([(["id"], [[9]])])
    (author,) = await Author.objects.annotate(id=Count("id")).all(client=stub)
    assert isinstance(author.model_fields_set, _AggregateFieldsSet)
    assert author.id == 9 and author.model_fields_set == {"id"}
    post = Post()
    post.author = author
    assert post.author is author
    assert post.author_id is None
    with pytest.raises(ValueError, match="save the related object first"):
        await post.save(client=stub)
    assert len(stub.calls) == 1


@pytest.mark.asyncio
async def test_bulk_update_rejects_virtual_collections():
    class PostWithRelations(Model):
        id: int = Field(db_pk=True)
        replies: list[Post] = Field(default_factory=list, db_reverse_fk="parent_id")
        related_posts: list[Post] = Field(default_factory=list, db_m2m=True)

        class Meta:
            is_table = True

    post = PostWithRelations(id=1)
    stub = StubExecuteClient([])
    for field in ("replies", "related_posts"):
        with pytest.raises(FieldError, match="virtual relation field"):
            await PostWithRelations.objects.bulk_update([post], [field], client=stub)
    assert stub.calls == []


@pytest.mark.asyncio
async def test_update_or_create_updates_related_instance():
    stub = StubExecuteClient(
        [[{"id": 1, "title": "Post", "author_id": 8}], post_row(9)]
    )
    post, created = await Post.objects.update_or_create(
        id=1, defaults={"author": Author(id=9)}, client=stub
    )
    assert created is False
    assert post.author_id == 9
    assert stub.calls[1]["values"] == {"author_id": 9}


@pytest.mark.asyncio
@pytest.mark.parametrize("in_place", [False, True], ids=["new-instance", "in-place"])
@pytest.mark.parametrize("returning", [False, True], ids=["ids-only", "returning"])
async def test_insert_instance_preserves_key_provenance(in_place, returning):
    author = FactoryAuthor()
    assert not author.model_fields_set
    result = (
        factory_author_row(author) if returning else {"inserted_ids": [str(author.id)]}
    )
    stub = StubExecuteClient([result])
    saved, inserted = await FactoryAuthor.objects.filter()._insert_instance(
        author, using=None, client=stub, in_place=in_place
    )
    assert inserted
    assert (saved is author) == in_place
    assert saved.id == author.id
    assert_factory_author_keys_available(saved)


@pytest.mark.asyncio
async def test_create_records_defaulted_keys():
    author = FactoryAuthor()
    stub = StubExecuteClient([factory_author_row(author)])
    created = await FactoryAuthor.objects.create(name=author.name, client=stub)
    assert created.id == author.id
    assert_factory_author_keys_available(created)


@pytest.mark.asyncio
async def test_insert_affected_count_records_submitted_keys():
    author = FactoryAuthor()
    stub = StubExecuteClient([{"affected": 1}])
    assert await FactoryAuthor.objects.create(instance=author, client=stub) is author
    assert_factory_author_keys_available(author)


@pytest.mark.asyncio
@pytest.mark.parametrize("existing", [False, True], ids=["create", "found"])
@pytest.mark.parametrize("method", ["get_or_create", "update_or_create"])
async def test_get_or_write_returns_keys_with_provenance(method, existing):
    author = FactoryAuthor()
    row = factory_author_row(author)
    select = [row["columns"], row["rows"] if existing else []]
    stub = StubExecuteClient([select, row])
    operation = getattr(FactoryAuthor.objects, method)
    saved, created = await operation(
        name=author.name, defaults={"name": author.name}, client=stub
    )
    assert created is not existing
    assert saved.id == author.id
    assert_factory_author_keys_available(saved)
    expected_ops = ["select"]
    if not existing:
        expected_ops.append("insert")
    elif method == "update_or_create":
        expected_ops.append("update")
    assert [call["op"] for call in stub.calls] == expected_ops


@pytest.mark.asyncio
@pytest.mark.parametrize("inserted", [False, True], ids=["conflict", "new"])
async def test_default_key_upsert_records_key_provenance(inserted):
    author = FactoryAuthor()
    assert not author.model_fields_set
    row = factory_author_row(author)
    row["columns"].append("__oxyde_inserted")
    row["rows"][0].append(inserted)
    stub = StubExecuteClient([row])
    await author.save(client=stub)
    assert stub.calls[0]["on_conflict"]["action"] == "update"
    assert_factory_author_keys_available(author)


@pytest.mark.asyncio
@pytest.mark.parametrize("ids_returned", [False, True], ids=["affected", "ids"])
async def test_bulk_create_records_defaulted_key_provenance(ids_returned):
    author = FactoryAuthor()
    assert not author.model_fields_set
    result = {"inserted_ids": [str(author.id)]} if ids_returned else {"affected": 1}
    stub = StubExecuteClient([result])
    assert await FactoryAuthor.objects.bulk_create([author], client=stub) == [author]
    assert_factory_author_keys_available(author)


@pytest.mark.asyncio
async def test_bulk_create_partial_result_marks_only_confirmed_rows():
    missing, confirmed = FactoryAuthor(), FactoryAuthor()
    stub = StubExecuteClient([{"affected": 1, "inserted_ids": [str(confirmed.id)]}])
    await FactoryAuthor.objects.bulk_create([missing, confirmed], client=stub)
    assert not missing.model_fields_set
    assert_factory_author_keys_available(confirmed)


@pytest.mark.asyncio
async def test_bulk_create_affected_count_records_columns_with_missing_key_slot():
    author = FactoryAuthor()
    stub = StubExecuteClient([{"affected": 1, "inserted_ids": [None]}])
    await FactoryAuthor.objects.bulk_create([author], client=stub)
    assert "id" not in author.model_fields_set
    assert FactoryPost(reviewer=author).reviewer_code == author.code


@pytest.mark.asyncio
async def test_bulk_update_records_written_non_pk_keys():
    author = FactoryAuthor(id=uuid4())
    assert "code" not in author.model_fields_set
    stub = StubExecuteClient([{"affected": 1}])
    assert await FactoryAuthor.objects.bulk_update([author], ["code"], client=stub) == 1
    assert FactoryPost(reviewer=author).reviewer_code == author.code


@pytest.mark.asyncio
@pytest.mark.parametrize("bulk", [True, False], ids=["bulk", "single"])
async def test_create_with_no_pk_marks_confirmed_non_pk_target(bulk):
    class Publisher(Model):
        code: str = Field(default="publisher", db_unique=True)

        class Meta:
            is_table = True

    class Publication(Model):
        publisher: Publisher | None = Field(default=None, db_fk="code")

        class Meta:
            is_table = True

    publisher = Publisher()
    stub = StubExecuteClient([{"affected": 1}])
    if bulk:
        assert await Publisher.objects.bulk_create([publisher], client=stub) == [
            publisher
        ]
    else:
        assert (
            await Publisher.objects.create(instance=publisher, client=stub) is publisher
        )
    assert not stub.calls[0].get("returning")
    assert "pk_column" not in stub.calls[0]
    assert Publication(publisher=publisher).publisher_code == publisher.code


@pytest.mark.asyncio
async def test_deduplicated_join_records_related_key_provenance():
    register_table(FactoryAuthor)
    author = FactoryAuthor()
    key = str(author.id)
    stub = StubExecuteClient(
        [
            [
                ["id"],
                [[1], [2]],
                {
                    "author": {
                        "columns": ["id", "code", "name"],
                        "data": {key: [key, author.code, author.name]},
                        "refs": [key, key],
                    }
                },
            ]
        ]
    )
    first, second = (
        await FactoryPost.objects.join("author").select("id").all(client=stub)
    )
    assert first.author is second.author
    assert_factory_author_keys_available(first.author)


@pytest.mark.asyncio
@pytest.mark.parametrize("many_to_many", [False, True], ids=["reverse-fk", "m2m"])
async def test_prefetch_records_related_key_provenance(many_to_many):
    class FactoryGroup(Model):
        id: int = Field(db_pk=True)
        name: str = "group"
        authors: list[FactoryAuthor] = Field(db_reverse_fk="group_id")
        members: list[FactoryAuthor] = Field(
            db_m2m=True, db_through="FactoryMembership"
        )

        class Meta:
            is_table = True

    class FactoryMembership(Model):
        group: FactoryGroup | None = None
        author: FactoryAuthor | None = None

        class Meta:
            is_table = True

    register_table(FactoryAuthor)
    register_table(FactoryMembership)
    author = FactoryAuthor()
    key = str(author.id)
    responses = [[["id"], [[1]]]]
    if many_to_many:
        responses.append([["group_id", "author_id"], [[1, key]]])
    responses.append(
        [["id", "code", "name", "group_id"], [[key, author.code, author.name, 1]]]
    )
    relation = "members" if many_to_many else "authors"
    (group,) = await FactoryGroup.objects.prefetch(relation).all(
        client=StubExecuteClient(responses)
    )
    (loaded,) = getattr(group, relation)
    assert_factory_author_keys_available(loaded)

    collection = getattr(group, relation)
    saved_row = {"columns": ["id", "name"], "rows": [[1, "group"]]}
    await group.save(client=StubExecuteClient([saved_row]))
    assert getattr(group, relation) is collection


@pytest.mark.asyncio
async def test_update_returning_and_save_record_returned_key_provenance():
    seed = FactoryAuthor()
    row = factory_author_row(seed)
    stub = StubExecuteClient([row, row])
    (returned,) = await FactoryAuthor.objects.filter(id=seed.id).update(
        name=seed.name, returning=True, client=stub
    )
    assert_factory_author_keys_available(returned)

    partial = FactoryAuthor(id=seed.id)
    assert "code" not in partial.model_fields_set
    await partial.save(update_fields=["name"], client=stub)
    assert_factory_author_keys_available(partial)


def test_custom_init_model_syncs_fk_and_hydrates():
    class CustomPost(Model):
        title: str
        author: Author | None = None

        def __init__(self, **data):
            data["title"] = data["title"].upper()
            super().__init__(**data)

        class Meta:
            is_table = True

    assert CustomPost(title="Post", author=Author(id=9)).author_id == 9
    hydrated = _validate_db_row(CustomPost, {"title": "Post", "author_id": 10})
    assert (hydrated.title, hydrated.author_id, hydrated.author) == ("POST", 10, None)


def test_custom_init_mixin_syncs_fk_and_hydrates():
    class AddTitle(BaseModel):
        def __init__(self, **data):
            data["title"] = data["title"].upper()
            super().__init__(**data)

    class CustomPost(Model, AddTitle):
        title: str
        author: Author | None = None

        class Meta:
            is_table = True

    assert CustomPost.__pydantic_custom_init__ is True
    assert CustomPost(title="Post", author=Author(id=9)).author_id == 9
    hydrated = _validate_db_row(CustomPost, {"title": "Post", "author_id": 10})
    assert (hydrated.title, hydrated.author_id, hydrated.author) == ("POST", 10, None)
