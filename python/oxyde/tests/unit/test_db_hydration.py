"""Regression coverage for database schemas and insert confirmations."""

from __future__ import annotations

from types import SimpleNamespace
from uuid import UUID

import pytest
from pydantic import AliasChoices, AliasPath, PrivateAttr, model_validator
from pydantic_core import core_schema

from oxyde import Field, Model
from oxyde.models.db_schema import _dump_db_fields, _validate_db_row
from oxyde.queries.insert import InsertQuery
from oxyde.tests.helpers import StubExecuteClient


class HiddenKey(Model):
    id: int = Field(default=1, db_pk=True, db_column="row_id", exclude=True)
    name: str
    note: str | None = None

    class Meta:
        is_table = True


def test_db_schema_uses_field_names_through_json_or_python():
    class Branched(Model):
        id: int = Field(default=1, db_pk=True, alias="key")
        name: str = Field(default="default", alias="label")
        model_config = {"serialize_by_alias": True}

        class Meta:
            is_table = True

        @classmethod
        def __get_pydantic_core_schema__(cls, source, handler):
            schema = super().__get_pydantic_core_schema__(source, handler)
            return core_schema.json_or_python_schema(schema, schema)

    instance = _validate_db_row(Branched, {"id": 2, "name": "row"})
    assert _dump_db_fields(instance) == {"id": 2, "name": "row"}

    instance = _validate_db_row(Branched, {"key": 2, "label": "alias"})
    assert (instance.id, instance.name) == (1, "default")
    assert instance.model_fields_set == set()


def test_db_schema_rejects_outer_union():
    class Branched(Model):
        id: int = Field(db_pk=True)

        class Meta:
            is_table = True

        @classmethod
        def __get_pydantic_core_schema__(cls, source, handler):
            schema = super().__get_pydantic_core_schema__(source, handler)
            return core_schema.union_schema([schema, core_schema.none_schema()])

    with pytest.raises(TypeError, match="Branched.*union"):
        _validate_db_row(Branched, {"id": 2})


@pytest.mark.parametrize("operation", ["create", "save", "bulk"])
@pytest.mark.asyncio
async def test_insert_uses_excluded_key(operation):
    instance = HiddenKey(name="test")
    returned = {"columns": ["row_id", "name"], "rows": [[1, "test"]]}
    stub = StubExecuteClient(
        [{"inserted_ids": [1]} if operation == "bulk" else returned]
    )

    if operation == "create":
        await HiddenKey.objects.create(instance=instance, client=stub)
    elif operation == "save":
        await instance.save(client=stub)
        assert stub.calls[0]["on_conflict"]["update_columns"] == ["name"]
    else:
        await HiddenKey.objects.bulk_create([instance], client=stub)

    values = (
        stub.calls[0]["bulk_values"][0]
        if operation == "bulk"
        else stub.calls[0]["values"]
    )
    assert values["row_id"] == 1
    assert "id" in instance.model_fields_set
    assert "id" not in instance.model_dump()


@pytest.mark.parametrize(
    ("data", "options", "explicit"),
    [
        ({"payload": {}}, {}, False),
        ({"payload": {"id": 9}}, {}, True),
        ({"payload": {}, "alternate_id": 9}, {}, True),
        (SimpleNamespace(payload={}), {"from_attributes": True}, False),
        (SimpleNamespace(payload={"id": 9}), {"from_attributes": True}, True),
        ({"alternate_id": 9}, {"by_alias": False, "by_name": True}, False),
    ],
)
@pytest.mark.asyncio
async def test_pk_provenance_uses_the_key_pydantic_received(data, options, explicit):
    class PathKey(Model):
        id: int = Field(
            default=1,
            db_pk=True,
            validation_alias=AliasChoices(AliasPath("payload", "id"), "alternate_id"),
        )
        name: str = "test"

        class Meta:
            is_table = True

        def model_post_init(self, _context, /):
            self.id += 1

    instance = PathKey.model_validate(data, **options)
    assert ("id" in instance.model_fields_set) is explicit
    stub = StubExecuteClient(
        [{"columns": ["id", "name"], "rows": [[instance.id, "test"]]}]
    )
    await instance.save(client=stub)
    assert stub.calls[0]["op"] == ("update" if explicit else "insert")


def test_db_schema_rebuild_and_recursive_nested_defaults():
    class Recursive(Model):
        id: int = Field(default=1, db_pk=True, alias="key")
        descendant: Recursive | None = None

        class Meta:
            is_table = True

        def model_post_init(self, _context, /):
            self.id += 1

    child = _validate_db_row(Recursive, {"id": 4, "descendant": {"key": 8}}).descendant
    assert child is not None and child.id == 9
    Recursive.model_rebuild(force=True)
    child = _validate_db_row(Recursive, {"id": 4, "descendant": {"key": 8}}).descendant
    assert child is not None and child.id == 9
    application = Recursive(key=5, descendant={})
    assert application.descendant is not None
    assert "id" not in application.descendant.model_fields_set


@pytest.mark.asyncio
async def test_refresh_captures_mutated_private_container_and_preserves_untouched_state():
    factories = []

    def factory():
        factories.append(None)
        return {"nested": []}

    class Cached(Model):
        id: int | None = Field(default=None, db_pk=True)
        name: str
        _cache: dict = PrivateAttr(default_factory=factory)
        _local: list = PrivateAttr(default_factory=list)

        class Meta:
            is_table = True

        def model_post_init(self, _context, /):
            self._cache["nested"].append(self.id)

        @model_validator(mode="after")
        def derive_id(self):
            self._cache["id"] = self.id
            return self

    instance = Cached(name="test")
    instance._local.append("keep")
    stub = StubExecuteClient([{"columns": ["id", "name"], "rows": [[42, "test"]]}])
    await Cached.objects.create(instance=instance, client=stub)
    assert instance._cache == {"nested": [42], "id": 42}
    assert instance._local == ["keep"]
    assert len(factories) == 2


def test_on_conflict_rejects_overlap_after_column_mapping():
    query = (
        InsertQuery(HiddenKey)
        .values(id=2, name="test")
        .on_conflict_update(["id"], values={"row_id": 2})
    )
    with pytest.raises(ValueError, match="multiple assignments.*row_id"):
        query.to_ir()


@pytest.mark.asyncio
@pytest.mark.parametrize("reverse", [False, True])
async def test_bulk_create_submits_mixed_column_shapes_in_one_call(reverse):
    first = HiddenKey.model_construct(_fields_set={"name"}, id=20, name="first")
    second = HiddenKey.model_construct(
        _fields_set={"name", "note"}, id=21, name="second", note="included"
    )
    ordered = [second, first] if reverse else [first, second]
    stub = StubExecuteClient([{"inserted_ids": [obj.id for obj in ordered]}])

    await HiddenKey.objects.bulk_create(ordered, client=stub)

    assert all("id" in obj.model_fields_set for obj in ordered)
    (call,) = stub.calls
    assert ["note" in row for row in call["bulk_values"]] == [reverse, not reverse]


@pytest.mark.asyncio
async def test_bulk_create_applies_rewritten_key():
    instance = HiddenKey(name="test")
    stub = StubExecuteClient([{"inserted_ids": [42]}])

    await HiddenKey.objects.bulk_create([instance], client=stub)

    assert instance.id == 42
    assert "id" in instance.model_fields_set
    assert stub.calls[0]["bulk_values"][0]["row_id"] == 1


@pytest.mark.asyncio
async def test_bulk_create_matches_only_confirmed_default_keys():
    class Keyed(Model):
        id: UUID = Field(default=UUID(int=1), db_pk=True)
        name: str

        class Meta:
            is_table = True

    first = Keyed(name="skip")
    second = Keyed(name="keep")
    object.__setattr__(second, "id", UUID(int=2))
    stub = StubExecuteClient([{"affected": 1, "inserted_ids": [str(second.id)]}])
    await Keyed.objects.bulk_create([first, second], client=stub)

    assert first.id == UUID(int=1) and "id" not in first.model_fields_set
    assert second.id == UUID(int=2) and "id" in second.model_fields_set
