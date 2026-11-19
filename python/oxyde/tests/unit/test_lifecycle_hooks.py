"""Tests for Model lifecycle hooks: pre_save, post_save, pre_delete, post_delete."""

from __future__ import annotations

from typing import Any, Self
from uuid import UUID, uuid4

import pytest
from pydantic import AliasChoices, AliasPath, PrivateAttr, model_validator

from oxyde import Field, Model
from oxyde.exceptions import ManagerError
from oxyde.tests.helpers import StubExecuteClient


class TestPreSaveHook:
    """Test pre_save lifecycle hook."""

    @pytest.mark.asyncio
    async def test_pre_save_called_on_create(self):
        """Test pre_save is called when creating a new instance."""
        hook_calls: list[dict[str, Any]] = []

        class HookedModel(Model):
            id: int | None = Field(default=None, db_pk=True)
            name: str

            class Meta:
                is_table = True

            async def pre_save(
                self, *, is_create: bool, update_fields: set[str] | None = None
            ) -> None:
                hook_calls.append(
                    {
                        "is_create": is_create,
                        "update_fields": update_fields,
                        "name": self.name,
                    }
                )

        stub = StubExecuteClient([{"affected": 1, "inserted_ids": [1]}])
        instance = HookedModel(name="Alice")
        await instance.save(client=stub)

        assert len(hook_calls) == 1
        assert hook_calls[0]["is_create"] is True
        assert hook_calls[0]["update_fields"] is None
        assert hook_calls[0]["name"] == "Alice"

    @pytest.mark.asyncio
    async def test_pre_save_called_on_update(self):
        """Test pre_save is called when updating an existing instance."""
        hook_calls: list[dict[str, Any]] = []

        class HookedModel(Model):
            id: int | None = Field(default=None, db_pk=True)
            name: str

            class Meta:
                is_table = True

            async def pre_save(
                self, *, is_create: bool, update_fields: set[str] | None = None
            ) -> None:
                hook_calls.append(
                    {
                        "is_create": is_create,
                        "update_fields": update_fields,
                    }
                )

        stub = StubExecuteClient([{"columns": ["id", "name"], "rows": [[1, "Bob"]]}])
        instance = HookedModel(id=1, name="Bob")
        await instance.save(client=stub)

        assert len(hook_calls) == 1
        assert hook_calls[0]["is_create"] is False
        assert hook_calls[0]["update_fields"] is None

    @pytest.mark.asyncio
    async def test_pre_save_receives_update_fields(self):
        """Test pre_save receives update_fields when specified."""
        hook_calls: list[dict[str, Any]] = []

        class HookedModel(Model):
            id: int | None = Field(default=None, db_pk=True)
            name: str
            email: str | None = None

            class Meta:
                is_table = True

            async def pre_save(
                self, *, is_create: bool, update_fields: set[str] | None = None
            ) -> None:
                hook_calls.append(
                    {
                        "is_create": is_create,
                        "update_fields": update_fields,
                    }
                )

        stub = StubExecuteClient(
            [
                {
                    "columns": ["id", "name", "email"],
                    "rows": [[1, "Charlie", "c@example.com"]],
                }
            ]
        )
        instance = HookedModel(id=1, name="Charlie", email="c@example.com")
        await instance.save(client=stub, update_fields=["name"])

        assert len(hook_calls) == 1
        assert hook_calls[0]["update_fields"] == {"name"}

    @pytest.mark.asyncio
    async def test_pre_save_can_modify_instance(self):
        """Test pre_save can modify instance fields before save."""

        class HookedModel(Model):
            id: int | None = Field(default=None, db_pk=True)
            name: str
            slug: str | None = None

            class Meta:
                is_table = True

            async def pre_save(
                self, *, is_create: bool, update_fields: set[str] | None = None
            ) -> None:
                # Auto-generate slug from name
                self.slug = self.name.lower().replace(" ", "-")

        stub = StubExecuteClient([{"affected": 1, "inserted_ids": [1]}])
        instance = HookedModel(name="Hello World")
        await instance.save(client=stub)

        assert instance.slug == "hello-world"
        # Check that slug was included in the insert
        assert "slug" in stub.calls[0]["values"]
        assert stub.calls[0]["values"]["slug"] == "hello-world"


class TestPostSaveHook:
    """Test post_save lifecycle hook."""

    @pytest.mark.asyncio
    @pytest.mark.parametrize("fields", [[], ["id"]])
    async def test_default_pk_conflict_without_updates(self, fields):
        outcomes: list[bool] = []

        class HookedModel(Model):
            id: int = Field(default=1, db_pk=True)
            name: str

            class Meta:
                is_table = True

            async def post_save(
                self, *, is_create: bool, update_fields: set[str] | None = None
            ) -> None:
                outcomes.append(is_create)

        obj = HookedModel(name="Alice")
        stub = StubExecuteClient(
            [{"rows": []}, {"columns": ["id", "name"], "rows": [[1, "Alice"]]}]
        )

        await obj.save(client=stub, update_fields=fields)

        assert stub.calls[0]["on_conflict"] == {"columns": ["id"], "action": "nothing"}
        assert outcomes == [False]
        # No row came back (conflict, or a trigger suppressed the insert), so
        # the default key stays retryable instead of being marked persisted.
        assert "id" not in obj.model_fields_set

        await obj.save(client=stub)
        assert [call["op"] for call in stub.calls] == ["insert", "insert"]
        assert stub.calls[1]["on_conflict"]["action"] == "update"
        assert "id" in obj.model_fields_set

    @pytest.mark.asyncio
    @pytest.mark.parametrize("inserted", [False, True])
    async def test_default_pk_post_save_uses_returned_outcome(self, inserted):
        outcomes: list[bool] = []

        class HookedModel(Model, extra="forbid"):
            id: int = Field(default=1, db_pk=True)
            name: str

            class Meta:
                is_table = True

            async def post_save(
                self, *, is_create: bool, update_fields: set[str] | None = None
            ) -> None:
                outcomes.append(is_create)

        stub = StubExecuteClient(
            [
                {
                    "columns": ["id", "name", "__oxyde_inserted"],
                    "rows": [[1, "Alice", inserted]],
                }
            ]
        )
        obj = HookedModel(name="Alice")

        await obj.save(client=stub)

        assert outcomes == [inserted]
        assert not hasattr(obj, "__oxyde_inserted")

    @pytest.mark.asyncio
    @pytest.mark.parametrize("update_fields", [None, []])
    async def test_default_pk_upsert_without_returned_row_retries(self, update_fields):
        outcomes: list[bool] = []

        class HookedModel(Model):
            id: int = Field(default=1, db_pk=True)
            name: str

            class Meta:
                is_table = True

            async def pre_save(
                self, *, is_create: bool, update_fields: set[str] | None = None
            ) -> None:
                self.id = 2

            async def post_save(
                self, *, is_create: bool, update_fields: set[str] | None = None
            ) -> None:
                outcomes.append(is_create)

        obj = HookedModel(name="Alice")
        columns, row = ["id", "name"], [2, "Alice"]
        action = "nothing" if update_fields == [] else "update"
        if action == "update":
            columns.append("__oxyde_inserted")
            row.append(True)
        stub = StubExecuteClient([{"rows": []}, {"columns": columns, "rows": [row]}])

        await obj.save(client=stub, update_fields=update_fields)
        assert "id" not in obj.model_fields_set
        await obj.save(client=stub, update_fields=update_fields)

        assert [call["on_conflict"]["action"] for call in stub.calls] == [
            action,
            action,
        ]
        assert obj.id == 2
        assert "id" in obj.model_fields_set
        assert outcomes == [False, True]

    @pytest.mark.asyncio
    @pytest.mark.parametrize("default_key", [None, 1])
    async def test_failed_pre_save_leaves_key_retryable(self, default_key):
        fail = True

        class HookedModel(Model):
            id: int | None = Field(default=default_key, db_pk=True)
            name: str

            class Meta:
                is_table = True

            async def pre_save(
                self, *, is_create: bool, update_fields: set[str] | None = None
            ) -> None:
                assert is_create
                self.id = 2
                if fail:
                    raise RuntimeError("pre_save failed")

        obj = HookedModel(name="Alice")
        stub = StubExecuteClient(
            [
                {
                    "columns": ["id", "name", "__oxyde_inserted"],
                    "rows": [[2, "Alice", True]],
                }
            ]
        )
        with pytest.raises(RuntimeError, match="pre_save failed"):
            await obj.save(client=stub)

        assert not stub.calls
        assert obj.id == 2
        assert "id" not in obj.model_fields_set

        fail = False
        await obj.save(client=stub)

        assert stub.calls[0]["op"] == "insert"
        assert stub.calls[0]["on_conflict"]["action"] == "update"
        assert "id" in obj.model_fields_set

    @pytest.mark.asyncio
    @pytest.mark.parametrize("default_key", [None, 1])
    async def test_failed_insert_leaves_pre_save_key_retryable(self, default_key):
        class HookedModel(Model):
            id: int | None = Field(default=default_key, db_pk=True)
            name: str

            class Meta:
                is_table = True

            async def pre_save(
                self, *, is_create: bool, update_fields: set[str] | None = None
            ) -> None:
                assert is_create
                self.id = 2

        obj = HookedModel(name="Alice")
        failing = StubExecuteClient([])
        with pytest.raises(ManagerError, match="stub payloads exhausted"):
            await obj.save(client=failing)

        assert obj.id == 2
        assert "id" not in obj.model_fields_set
        stub = StubExecuteClient(
            [
                {
                    "columns": ["id", "name", "__oxyde_inserted"],
                    "rows": [[2, "Alice", True]],
                }
            ]
        )
        await obj.save(client=stub)

        assert stub.calls[0]["op"] == "insert"
        assert stub.calls[0]["on_conflict"]["action"] == "update"
        assert "id" in obj.model_fields_set

    @pytest.mark.asyncio
    @pytest.mark.parametrize(
        ("values", "fields_set", "expected_id", "explicit"),
        [
            ({}, None, 2, False),
            ({"id": 5}, None, 6, True),
            ({"key": 5}, None, 6, True),
            ({"alternate": 5}, None, 6, True),
            ({"payload": {"key": 5}}, None, 6, True),
            ({"payload": {}}, None, 2, False),
            ({"id": 5}, {"name"}, 6, False),
            ({}, {"id", "name"}, 2, True),
        ],
    )
    async def test_constructed_key_provenance(
        self, values, fields_set, expected_id, explicit
    ):
        class ConstructedKey(Model):
            id: int = Field(
                default=1,
                db_pk=True,
                alias="key",
                validation_alias=AliasChoices("alternate", AliasPath("payload", "key")),
            )
            name: str

            class Meta:
                is_table = True

            def model_post_init(self, _context, /):
                self.id += 1

        obj = ConstructedKey.model_construct(fields_set, name="Alice", **values)
        assert obj.id == expected_id
        assert ("id" in obj.model_fields_set) is explicit

        stub = StubExecuteClient(
            [{"columns": ["id", "name"], "rows": [[obj.id, obj.name]]}]
        )
        await obj.save(client=stub)

        (call,) = stub.calls
        if explicit:
            assert call["op"] == "update"
        else:
            assert call["op"] == "insert"
            assert call["on_conflict"]["action"] == "update"

    @pytest.mark.asyncio
    async def test_default_pk_upsert_outcome_alias_avoids_model_columns(self):
        outcomes: list[bool] = []

        class HookedModel(Model, extra="forbid"):
            id: int = Field(default=1, db_pk=True)
            name: str = Field(db_column="__oxyde_inserted")
            note: str = Field(default="", db_column="__oxyde_inserted_1")

            class Meta:
                is_table = True

            async def post_save(
                self, *, is_create: bool, update_fields: set[str] | None = None
            ) -> None:
                outcomes.append(is_create)

        obj = HookedModel(name="Alice")
        stub = StubExecuteClient(
            [
                {
                    "columns": [
                        "id",
                        "__oxyde_inserted",
                        "__oxyde_inserted_1",
                        "__oxyde_inserted_2",
                    ],
                    "rows": [[1, "Alice", "persisted", True]],
                }
            ]
        )

        await obj.save(client=stub)

        assert stub.calls[0]["on_conflict"]["inserted_alias"] == "__oxyde_inserted_2"
        assert (obj.name, obj.note) == ("Alice", "persisted")
        assert outcomes == [True]

    @pytest.mark.asyncio
    async def test_default_pk_outcome_reads_appended_flag_column(self):
        outcomes: list[bool] = []

        class HookedModel(Model, extra="ignore"):
            id: int = Field(default=1, db_pk=True)
            name: str

            class Meta:
                is_table = True

            async def post_save(
                self, *, is_create: bool, update_fields: set[str] | None = None
            ) -> None:
                outcomes.append(is_create)

        # An unmapped table column shares the flag's name; the builder appends
        # the real flag after RETURNING *, so it is the last column.
        stub = StubExecuteClient(
            [
                {
                    "columns": ["id", "__oxyde_inserted", "name", "__oxyde_inserted"],
                    "rows": [[1, True, "Alice", False]],
                }
            ]
        )
        obj = HookedModel(name="Alice")

        await obj.save(client=stub)

        assert outcomes == [False]
        assert obj.name == "Alice"

    @pytest.mark.asyncio
    async def test_in_place_refresh_keeps_factory_private_state(self):
        class HookedModel(Model):
            id: int | None = Field(default=None, db_pk=True)
            name: str
            _token: UUID = PrivateAttr(default_factory=uuid4)

            class Meta:
                is_table = True

        obj = HookedModel(name="Alice")
        token = obj._token
        stub = StubExecuteClient(
            [
                {"columns": ["id", "name"], "rows": [[42, "Alice"]]},
                {"affected": 1, "inserted_ids": [43]},
            ]
        )

        await HookedModel.objects.create(instance=obj, client=stub)
        assert (obj.id, obj._token) == (42, token)

        other = HookedModel(name="Bob")
        other_token = other._token
        await other.save(client=stub)
        assert (other.id, other._token) == (43, other_token)

    @pytest.mark.asyncio
    async def test_in_place_create_updates_validator_derived_private_state(self):
        states: list[tuple[str, str | None]] = []

        class HookedModel(Model):
            id: int | None = Field(default=None, db_pk=True)
            name: str
            _key: str = PrivateAttr()
            _local: str = PrivateAttr()
            _context: str | None = PrivateAttr(default=None)

            class Meta:
                is_table = True

            @model_validator(mode="after")
            def derive_key(self) -> Self:
                self._key = f"user:{self.id}"
                return self

            async def pre_save(
                self, *, is_create: bool, update_fields: set[str] | None = None
            ) -> None:
                self._context = "set by pre_save"

            async def post_save(
                self, *, is_create: bool, update_fields: set[str] | None = None
            ) -> None:
                states.append((self._key, self._context))

        obj = HookedModel(name="Alice")
        obj._local = "preserved"
        stub = StubExecuteClient([{"columns": ["id", "name"], "rows": [[42, "Alice"]]}])

        assert await HookedModel.objects.create(instance=obj, client=stub) is obj
        assert states == [("user:42", "set by pre_save")]
        assert obj._local == "preserved"

    @pytest.mark.asyncio
    async def test_post_save_called_after_create(self):
        """Test post_save is called after creating a new instance."""
        hook_calls: list[dict[str, Any]] = []

        class HookedModel(Model):
            id: int | None = Field(default=None, db_pk=True)
            name: str

            class Meta:
                is_table = True

            async def post_save(
                self, *, is_create: bool, update_fields: set[str] | None = None
            ) -> None:
                hook_calls.append(
                    {
                        "is_create": is_create,
                        "update_fields": update_fields,
                        "id": self.id,  # Should have ID assigned
                    }
                )

        stub = StubExecuteClient([{"affected": 1, "inserted_ids": [42]}])
        instance = HookedModel(name="Alice")
        await instance.save(client=stub)

        assert len(hook_calls) == 1
        assert hook_calls[0]["is_create"] is True
        assert hook_calls[0]["id"] == 42  # ID was assigned before post_save

    @pytest.mark.asyncio
    async def test_post_save_called_after_update(self):
        """Test post_save is called after updating an existing instance."""
        hook_calls: list[dict[str, Any]] = []

        class HookedModel(Model):
            id: int | None = Field(default=None, db_pk=True)
            name: str

            class Meta:
                is_table = True

            async def post_save(
                self, *, is_create: bool, update_fields: set[str] | None = None
            ) -> None:
                hook_calls.append(
                    {
                        "is_create": is_create,
                    }
                )

        stub = StubExecuteClient([{"columns": ["id", "name"], "rows": [[1, "Bob"]]}])
        instance = HookedModel(id=1, name="Bob")
        await instance.save(client=stub)

        assert len(hook_calls) == 1
        assert hook_calls[0]["is_create"] is False

    @pytest.mark.asyncio
    async def test_hooks_called_in_order(self):
        """Test pre_save is called before post_save."""
        call_order: list[str] = []

        class HookedModel(Model):
            id: int | None = Field(default=None, db_pk=True)
            name: str

            class Meta:
                is_table = True

            async def pre_save(
                self, *, is_create: bool, update_fields: set[str] | None = None
            ) -> None:
                call_order.append("pre_save")

            async def post_save(
                self, *, is_create: bool, update_fields: set[str] | None = None
            ) -> None:
                call_order.append("post_save")

        stub = StubExecuteClient([{"affected": 1, "inserted_ids": [1]}])
        instance = HookedModel(name="Test")
        await instance.save(client=stub)

        assert call_order == ["pre_save", "post_save"]


class TestPreDeleteHook:
    """Test pre_delete lifecycle hook."""

    @pytest.mark.asyncio
    async def test_pre_delete_called_before_delete(self):
        """Test pre_delete is called before deleting an instance."""
        hook_calls: list[int] = []

        class HookedModel(Model):
            id: int | None = Field(default=None, db_pk=True)
            name: str

            class Meta:
                is_table = True

            async def pre_delete(self) -> None:
                hook_calls.append(self.id)  # type: ignore

        stub = StubExecuteClient([{"affected": 1}])
        instance = HookedModel(id=42, name="ToDelete")
        await instance.delete(client=stub)

        assert len(hook_calls) == 1
        assert hook_calls[0] == 42


class TestPostDeleteHook:
    """Test post_delete lifecycle hook."""

    @pytest.mark.asyncio
    async def test_post_delete_called_after_delete(self):
        """Test post_delete is called after deleting an instance."""
        hook_calls: list[int] = []

        class HookedModel(Model):
            id: int | None = Field(default=None, db_pk=True)
            name: str

            class Meta:
                is_table = True

            async def post_delete(self) -> None:
                hook_calls.append(self.id)  # type: ignore

        stub = StubExecuteClient([{"affected": 1}])
        instance = HookedModel(id=42, name="ToDelete")
        await instance.delete(client=stub)

        assert len(hook_calls) == 1
        assert hook_calls[0] == 42

    @pytest.mark.asyncio
    async def test_delete_hooks_called_in_order(self):
        """Test pre_delete is called before post_delete."""
        call_order: list[str] = []

        class HookedModel(Model):
            id: int | None = Field(default=None, db_pk=True)
            name: str

            class Meta:
                is_table = True

            async def pre_delete(self) -> None:
                call_order.append("pre_delete")

            async def post_delete(self) -> None:
                call_order.append("post_delete")

        stub = StubExecuteClient([{"affected": 1}])
        instance = HookedModel(id=1, name="Test")
        await instance.delete(client=stub)

        assert call_order == ["pre_delete", "post_delete"]


class TestManagerCreateHooks:
    """Test hooks work with QueryManager.create()."""

    @pytest.mark.asyncio
    async def test_create_calls_hooks(self):
        """Test QueryManager.create() calls pre_save and post_save."""
        hook_calls: list[str] = []

        class HookedModel(Model):
            id: int | None = Field(default=None, db_pk=True)
            name: str

            class Meta:
                is_table = True

            async def pre_save(
                self, *, is_create: bool, update_fields: set[str] | None = None
            ) -> None:
                hook_calls.append(f"pre_save:is_create={is_create}")

            async def post_save(
                self, *, is_create: bool, update_fields: set[str] | None = None
            ) -> None:
                hook_calls.append(f"post_save:is_create={is_create}")

        stub = StubExecuteClient([{"affected": 1, "inserted_ids": [1]}])
        await HookedModel.objects.create(name="Test", client=stub)

        assert hook_calls == [
            "pre_save:is_create=True",
            "post_save:is_create=True",
        ]


class TestGetOrCreateHooks:
    """Test hooks work with QueryManager.get_or_create()."""

    @pytest.mark.asyncio
    async def test_get_or_create_calls_hooks_on_create(self):
        """Test get_or_create() calls hooks when creating."""
        hook_calls: list[str] = []

        class HookedModel(Model):
            id: int | None = Field(default=None, db_pk=True)
            name: str

            class Meta:
                is_table = True

            async def pre_save(
                self, *, is_create: bool, update_fields: set[str] | None = None
            ) -> None:
                hook_calls.append("pre_save")

            async def post_save(
                self, *, is_create: bool, update_fields: set[str] | None = None
            ) -> None:
                hook_calls.append("post_save")

        # First call returns empty (not found), second is the create
        stub = StubExecuteClient(
            [
                [],  # get() returns empty
                {"affected": 1, "inserted_ids": [1]},  # create()
            ]
        )
        obj, created = await HookedModel.objects.get_or_create(name="Test", client=stub)

        assert created is True
        assert hook_calls == ["pre_save", "post_save"]

    @pytest.mark.asyncio
    async def test_get_or_create_no_hooks_on_get(self):
        """Test get_or_create() does not call hooks when getting existing."""
        hook_calls: list[str] = []

        class HookedModel(Model):
            id: int | None = Field(default=None, db_pk=True)
            name: str

            class Meta:
                is_table = True

            async def pre_save(
                self, *, is_create: bool, update_fields: set[str] | None = None
            ) -> None:
                hook_calls.append("pre_save")

            async def post_save(
                self, *, is_create: bool, update_fields: set[str] | None = None
            ) -> None:
                hook_calls.append("post_save")

        stub = StubExecuteClient(
            [
                [{"id": 1, "name": "Existing"}],  # get() returns existing
            ]
        )
        obj, created = await HookedModel.objects.get_or_create(
            name="Existing", client=stub
        )

        assert created is False
        assert hook_calls == []  # No hooks called


class TestUpdateOrCreateHooks:
    """Test hooks work with QueryManager.update_or_create()."""

    @pytest.mark.asyncio
    async def test_update_or_create_calls_hooks_on_create(self):
        """update_or_create() calls hooks when creating through create()."""
        hook_calls: list[str] = []

        class HookedModel(Model):
            id: int | None = Field(default=None, db_pk=True)
            name: str

            class Meta:
                is_table = True

            async def pre_save(
                self, *, is_create: bool, update_fields: set[str] | None = None
            ) -> None:
                hook_calls.append("pre_save")

            async def post_save(
                self, *, is_create: bool, update_fields: set[str] | None = None
            ) -> None:
                hook_calls.append("post_save")

        stub = StubExecuteClient(
            [
                [],  # get() returns empty
                {"affected": 1, "inserted_ids": [1]},  # create()
            ]
        )
        obj, created = await HookedModel.objects.update_or_create(
            id=1,
            defaults={"name": "Test"},
            client=stub,
        )

        assert created is True
        assert isinstance(obj, HookedModel)
        assert [call["op"] for call in stub.calls] == ["select", "insert"]
        assert hook_calls == ["pre_save", "post_save"]

    @pytest.mark.asyncio
    async def test_update_or_create_calls_hooks_on_update(self):
        """update_or_create() fires hooks on the save/update path."""
        hook_calls: list[dict[str, Any]] = []

        class HookedModel(Model):
            id: int | None = Field(default=None, db_pk=True)
            name: str

            class Meta:
                is_table = True

            async def pre_save(
                self, *, is_create: bool, update_fields: set[str] | None = None
            ) -> None:
                hook_calls.append(
                    {
                        "hook": "pre_save",
                        "is_create": is_create,
                        "update_fields": update_fields,
                        "name": self.name,
                    }
                )

            async def post_save(
                self, *, is_create: bool, update_fields: set[str] | None = None
            ) -> None:
                hook_calls.append(
                    {
                        "hook": "post_save",
                        "is_create": is_create,
                        "update_fields": update_fields,
                        "name": self.name,
                    }
                )

        stub = StubExecuteClient(
            [
                [{"id": 1, "name": "Existing"}],  # get() returns existing
                {
                    "columns": ["id", "name"],
                    "rows": [[1, "Updated"]],
                },  # save() returning
            ]
        )
        obj, created = await HookedModel.objects.update_or_create(
            id=1,
            defaults={"name": "Updated"},
            client=stub,
        )

        assert created is False
        assert obj.name == "Updated"
        assert [call["op"] for call in stub.calls] == ["select", "update"]
        assert hook_calls == [
            {
                "hook": "pre_save",
                "is_create": False,
                "update_fields": {"name"},
                "name": "Updated",
            },
            {
                "hook": "post_save",
                "is_create": False,
                "update_fields": {"name"},
                "name": "Updated",
            },
        ]

    @pytest.mark.asyncio
    async def test_update_or_create_preserves_pre_save_lookup_changes(self):
        """update_or_create() keeps lookup fields modified by pre_save()."""

        class HookedModel(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str

            class Meta:
                is_table = True

            async def pre_save(
                self, *, is_create: bool, update_fields: set[str] | None = None
            ) -> None:
                self.email = self.email.lower()

        stub = StubExecuteClient(
            [
                [],  # get() returns empty
                {"affected": 1, "inserted_ids": [1]},  # create()
            ]
        )
        obj, created = await HookedModel.objects.update_or_create(
            email="Alice@Example.COM",
            client=stub,
        )

        assert created is True
        assert obj.email == "alice@example.com"
        assert stub.calls[1]["values"]["email"] == "alice@example.com"


class TestHookExceptionHandling:
    """Test exception handling in hooks."""

    @pytest.mark.asyncio
    async def test_pre_save_exception_prevents_save(self):
        """Test exception in pre_save prevents the save operation."""

        class HookedModel(Model):
            id: int | None = Field(default=None, db_pk=True)
            name: str

            class Meta:
                is_table = True

            async def pre_save(
                self, *, is_create: bool, update_fields: set[str] | None = None
            ) -> None:
                raise ValueError("Validation failed")

        stub = StubExecuteClient([{"affected": 1, "inserted_ids": [1]}])
        instance = HookedModel(name="Test")

        with pytest.raises(ValueError, match="Validation failed"):
            await instance.save(client=stub)

        # No database call should have been made
        assert len(stub.calls) == 0

    @pytest.mark.asyncio
    async def test_post_save_exception_propagates(self):
        """Test exception in post_save propagates after save completes."""

        class HookedModel(Model):
            id: int | None = Field(default=None, db_pk=True)
            name: str

            class Meta:
                is_table = True

            async def post_save(
                self, *, is_create: bool, update_fields: set[str] | None = None
            ) -> None:
                raise RuntimeError("Post-save failed")

        stub = StubExecuteClient([{"affected": 1, "inserted_ids": [1]}])
        instance = HookedModel(name="Test")

        with pytest.raises(RuntimeError, match="Post-save failed"):
            await instance.save(client=stub)

        # Database call was made before exception
        assert len(stub.calls) == 1
        assert instance.id == 1  # ID was assigned

    @pytest.mark.asyncio
    async def test_pre_delete_exception_prevents_delete(self):
        """Test exception in pre_delete prevents the delete operation."""

        class HookedModel(Model):
            id: int | None = Field(default=None, db_pk=True)
            name: str

            class Meta:
                is_table = True

            async def pre_delete(self) -> None:
                raise ValueError("Cannot delete")

        stub = StubExecuteClient([{"affected": 1}])
        instance = HookedModel(id=1, name="Test")

        with pytest.raises(ValueError, match="Cannot delete"):
            await instance.delete(client=stub)

        # No database call should have been made
        assert len(stub.calls) == 0


class TestHookInheritance:
    """Test hook inheritance behavior."""

    @pytest.mark.asyncio
    async def test_hooks_can_call_super(self):
        """Test child hooks can call super() to chain behavior."""
        call_order: list[str] = []

        class BaseModel(Model):
            id: int | None = Field(default=None, db_pk=True)
            name: str

            class Meta:
                is_table = True

            async def pre_save(
                self, *, is_create: bool, update_fields: set[str] | None = None
            ) -> None:
                call_order.append("base_pre_save")

        class ChildModel(BaseModel):
            class Meta:
                is_table = True

            async def pre_save(
                self, *, is_create: bool, update_fields: set[str] | None = None
            ) -> None:
                call_order.append("child_pre_save_before")
                await super().pre_save(is_create=is_create, update_fields=update_fields)
                call_order.append("child_pre_save_after")

        stub = StubExecuteClient([{"affected": 1, "inserted_ids": [1]}])
        instance = ChildModel(name="Test")
        await instance.save(client=stub)

        assert call_order == [
            "child_pre_save_before",
            "base_pre_save",
            "child_pre_save_after",
        ]
