"""Regression coverage for database schemas and insert confirmations."""

from __future__ import annotations

import inspect
import warnings
from types import SimpleNamespace
from uuid import UUID

import pytest
from pydantic import (
    AliasChoices,
    AliasPath,
    BaseModel,
    ConfigDict,
    PrivateAttr,
    ValidationError,
    model_validator,
)
from pydantic_core import core_schema

from oxyde import Field, Model
from oxyde.exceptions import ManagerError
from oxyde.models.db_schema import (
    _database_schemas,
    _dump_db_fields,
    _outer_nodes,
    _validate_db_row,
    _validate_db_rows,
)
from oxyde.models.registry import register_table
from oxyde.models.utils import _AggregateFieldsSet, _LoadedFieldsSet
from oxyde.queries import Count
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


@pytest.mark.asyncio
@pytest.mark.parametrize(
    ("alias", "in_union"), [("total", False), ("id", False), ("id", True)]
)
async def test_aggregate_projection_cannot_be_saved(alias, in_union):
    class DefaultKey(Model):
        id: int = Field(default=1, db_pk=True)

        class Meta:
            is_table = True

    aggregate = DefaultKey.objects.annotate(**{alias: Count("id")})
    query = (
        DefaultKey.objects.filter().select("id").union_all(aggregate)
        if in_union
        else aggregate
    )
    stub = StubExecuteClient([([alias], [[42]])])

    (instance,) = await query.all(client=stub)
    assert instance.id == (42 if alias == "id" else 1)
    assert instance.model_fields_set == ({"id"} if alias == "id" else set())
    with pytest.raises(
        ManagerError,
        match="DefaultKey instances built from aggregate query results cannot be saved",
    ):
        await instance.save(client=stub)
    assert len(stub.calls) == 1


@pytest.mark.asyncio
@pytest.mark.parametrize("payload", [(["total"], [[42]]), [{"total": 42}]])
async def test_aggregate_values_survive_exclude_unset(payload):
    class AggregateResult(Model):
        id: int = Field(default=1, db_pk=True)
        total: int | None = None

        class Meta:
            is_table = True

    stub = StubExecuteClient([payload])
    (instance,) = await AggregateResult.objects.annotate(total=Count("id")).all(
        client=stub
    )

    assert instance.model_dump(exclude_unset=True) == {"total": 42}
    with pytest.raises(
        ManagerError,
        match=(
            "AggregateResult instances built from aggregate query results cannot be saved"
        ),
    ):
        await instance.save(client=stub)
    assert len(stub.calls) == 1


@pytest.mark.asyncio
@pytest.mark.parametrize("hook", ["after_validator", "model_post_init"])
async def test_aggregate_keeps_validator_assigned_fields(hook):
    class AggregateResult(Model):
        id: int = Field(db_pk=True)
        total: int | None = None
        summary: str | None = None

        class Meta:
            is_table = True

        if hook == "after_validator":

            @model_validator(mode="after")
            def summarize(self):
                self.summary = f"{self.total} rows"
                return self

        else:

            def model_post_init(self, _context, /):
                self.summary = f"{self.total} rows"

    stub = StubExecuteClient([(["id", "total"], [[1, 42]])])
    (instance,) = await AggregateResult.objects.annotate(total=Count("id")).all(
        client=stub
    )

    assert instance.model_dump(exclude_unset=True) == {
        "id": 1,
        "total": 42,
        "summary": "42 rows",
    }
    assert isinstance(instance.model_fields_set, _AggregateFieldsSet)
    with pytest.raises(
        ManagerError,
        match=(
            "AggregateResult instances built from aggregate query results cannot be saved"
        ),
    ):
        await instance.save(client=stub)
    assert len(stub.calls) == 1


@pytest.mark.asyncio
@pytest.mark.parametrize("custom_init", [False, True])
@pytest.mark.parametrize("projection", [False, True])
@pytest.mark.parametrize("mixed_batch", [False, True])
async def test_partial_query_preserves_cached_model(
    custom_init, projection, mixed_batch
):
    cache = {}

    class CachedRow(Model):
        id: int = Field(db_pk=True)
        name: str = ""
        total: int | None = None

        class Meta:
            is_table = True

        if custom_init:

            def __init__(self, **data):
                super().__init__(**data)

        @model_validator(mode="wrap")
        @classmethod
        def from_cache(cls, data, handler):
            if data["id"] in cache:
                return cache[data["id"]]
            return handler(data)

    full_row = (["id", "name"], [[1, "saved"]])
    (original,) = await CachedRow.objects.all(client=StubExecuteClient([full_row]))
    cache[1] = original
    original_fields = original.model_fields_set
    ids = [1, 2] if mixed_batch else [1]
    if projection:
        query = CachedRow.objects.filter().select("id")
        payload = (["id"], [[key] for key in ids])
    else:
        query = CachedRow.objects.annotate(total=Count("id"))
        payload = (["id", "total"], [[key, 42] for key in ids])

    results = await query.all(client=StubExecuteClient([payload]))

    assert len(results) == len(ids)
    assert results[0] is original
    assert original.model_fields_set is original_fields
    assert original.model_fields_set == {"id", "name"}
    save_client = StubExecuteClient(
        [{"columns": ["id", "name"], "rows": [[1, "saved"]]}]
    )
    await original.save(client=save_client)
    assert save_client.calls[0]["op"] == "update"

    if mixed_batch:
        fresh = results[1]
        if projection:
            assert isinstance(fresh.model_fields_set, _LoadedFieldsSet)
            assert fresh.model_fields_set == {"id"}
        else:
            assert isinstance(fresh.model_fields_set, _AggregateFieldsSet)
            assert fresh.model_fields_set == {"id", "total"}


@pytest.mark.parametrize("many", [False, True])
def test_custom_init_hydrates_by_field_name(many):
    init_calls = []
    validated_ids = []

    class CustomInit(Model):
        id: int = Field(default=1, db_pk=True, alias="name")
        name: str

        class Meta:
            is_table = True

        def __init__(self, **data):
            init_calls.append(data)
            super().__init__(**data)

        @model_validator(mode="after")
        def record_validation(self):
            validated_ids.append(self.id)
            return self

    rows = [{"id": 42, "name": "first"}, {"id": 43, "name": "second"}]
    if many:
        loaded = _validate_db_rows(CustomInit, rows)
    else:
        rows = rows[:1]
        loaded = [_validate_db_row(CustomInit, rows[0])]

    assert [instance.id for instance in loaded] == [row["id"] for row in rows]
    assert [instance.name for instance in loaded] == [row["name"] for row in rows]
    assert init_calls == rows
    assert validated_ids == [row["id"] for row in rows]
    assert all("__pydantic_validator__" not in instance.__dict__ for instance in loaded)
    assert CustomInit(id=12, name="9").id == 9

    (defaulted,) = (
        _validate_db_rows(CustomInit, [{"name": "first"}])
        if many
        else [_validate_db_row(CustomInit, {"name": "first"})]
    )
    assert defaulted.id == 1
    assert defaulted.model_fields_set == {"name"}


@pytest.mark.parametrize("many", [False, True])
def test_custom_init_can_pass_alias_to_super(many):
    before_inputs = []

    class AliasedKey(Model):
        id: int = Field(alias="key", db_pk=True)

        class Meta:
            is_table = True

        def __init__(self, id=None, **data):
            super().__init__(key=id, **data)

        @model_validator(mode="before")
        @classmethod
        def record_input(cls, data):
            before_inputs.append(data)
            return data

    assert AliasedKey(id=5).id == 5
    before_inputs.clear()
    rows = [{"id": 7}, {"id": 8}] if many else [{"id": 7}]
    if many:
        loaded = _validate_db_rows(AliasedKey, rows)
    else:
        loaded = [_validate_db_row(AliasedKey, rows[0])]

    assert [instance.id for instance in loaded] == [row["id"] for row in rows]
    assert before_inputs == [{"key": row["id"]} for row in rows]


@pytest.mark.parametrize(
    "validation_alias", [AliasPath("key", 0), AliasChoices("pk", AliasPath("key", 0))]
)
def test_custom_init_can_pass_alias_path_to_super(validation_alias):
    class AliasedKey(Model):
        id: int = Field(validation_alias=validation_alias)

        def __init__(self, id, **data):
            super().__init__(key=[id], **data)

    assert _validate_db_row(AliasedKey, {"id": 7}).id == 7


@pytest.mark.parametrize("extra", ["allow", "forbid"])
@pytest.mark.parametrize("explicit_nickname", [False, True])
def test_custom_init_resolves_colliding_alias_only_if_constructor_supplies_it(
    extra, explicit_nickname
):
    class Nick(Model):
        model_config = ConfigDict(extra=extra)
        name: str = "name default"
        nickname: str = Field(default="dflt", alias="name")

        def __init__(self, *, supply_name=False, **data):
            if supply_name:
                data["name"] = "constructor"
                if explicit_nickname:
                    data["nickname"] = "preferred"
            super().__init__(**data)

    row = {"name": "row"}
    loaded = _validate_db_row(Nick, row)
    assert row == {"name": "row"}
    assert loaded.name == "row"
    assert loaded.nickname == "dflt"
    assert loaded.model_fields_set == {"name"}

    constructed = _validate_db_row(Nick, {"supply_name": True})
    assert constructed.name == "constructor"
    assert constructed.nickname == ("preferred" if explicit_nickname else "constructor")
    assert constructed.model_fields_set == {"name", "nickname"}
    assert constructed.model_extra == ({} if extra == "allow" else None)


def test_custom_init_alias_exposure_respects_validate_by_alias():
    class Nick(Model):
        model_config = ConfigDict(extra="forbid", validate_by_alias=False)
        name: str
        nickname: str = Field(default="dflt", alias="name")

        def __init__(self, **_data):
            super().__init__(name="constructor")

    loaded = _validate_db_row(Nick, {})
    assert loaded.name == "constructor"
    assert loaded.nickname == "dflt"
    assert loaded.model_fields_set == {"name"}


@pytest.mark.parametrize("payload_has_name", [False, True])
def test_custom_init_checks_higher_priority_alias_path_before_exposing_collision(
    payload_has_name,
):
    class Nick(Model):
        model_config = ConfigDict(extra="forbid")
        payload: dict
        name: str
        nickname: str = Field(
            validation_alias=AliasChoices(AliasPath("payload", "nick"), "name")
        )

        def __init__(self, payload_source):
            super().__init__(payload=payload_source, name="constructor")

    payload = {"nick": "preferred"} if payload_has_name else {}
    loaded = _validate_db_row(Nick, {"payload_source": payload})
    assert loaded.nickname == ("preferred" if payload_has_name else "constructor")


def test_custom_init_hydrates_through_json_or_python():
    class Branch(Model):
        id: int = Field(default=1, db_pk=True, alias="key")

        class Meta:
            is_table = True

        @classmethod
        def __get_pydantic_core_schema__(cls, source, handler):
            schema = super().__get_pydantic_core_schema__(source, handler)
            return core_schema.json_or_python_schema(schema, schema)

        def __init__(self, **data):
            super().__init__(**data)

    instance = _validate_db_row(Branch, {"id": 2})
    assert instance.id == 2
    assert instance.model_fields_set == {"id"}


def test_custom_init_hydrates_with_plain_schema_replacement():
    class Leaf(Model):
        name: str

        @classmethod
        def __get_pydantic_core_schema__(cls, source, handler):
            return core_schema.no_info_plain_validator_function(
                lambda data: cls.model_construct(name=data["name"])
            )

        def __init__(self, **data):
            raise AssertionError("the replacement schema should build the instance")

    assert _validate_db_row(Leaf, {"name": "row"}).name == "row"


@pytest.mark.asyncio
@pytest.mark.parametrize("assign_in_init", [False, True])
async def test_custom_init_restores_pk_provenance_before_super_returns(assign_in_init):
    fields_after_super = []

    class Prov(Model):
        id: int = Field(default=1, db_pk=True)
        name: str

        class Meta:
            is_table = True

        def model_post_init(self, _context, /):
            self.id += 1

        def __init__(self, **data):
            super().__init__(**data)
            fields_after_super.append(set(self.model_fields_set))
            if assign_in_init:
                self.id = 7

    canonical = Prov(name="row")
    loaded = _validate_db_row(Prov, {"name": "row"})
    assert fields_after_super == [{"name"}, {"name"}]
    assert loaded.id == canonical.id == (7 if assign_in_init else 2)
    assert loaded.model_fields_set == canonical.model_fields_set
    assert ("id" in loaded.model_fields_set) is assign_in_init

    if assign_in_init:
        client = StubExecuteClient(
            [{"columns": ["id", "name"], "rows": [[7, "row"]]}]
        )
        await loaded.save(client=client)
        assert client.calls[0]["op"] == "update"


def test_custom_init_outer_wrap_keeps_assigned_default_pk_unset():
    class Prov(Model):
        id: int = Field(default=1, db_pk=True)
        name: str

        class Meta:
            is_table = True

        def __init__(self, **data):
            super().__init__(**data)

        @model_validator(mode="wrap")
        @classmethod
        def set_default(cls, data, handler):
            instance = handler(data)
            instance.id = 2
            return instance

    loaded = _validate_db_row(Prov, {"name": "row"})
    assert loaded.id == 2
    assert loaded.model_fields_set == {"name"}


@pytest.mark.asyncio
@pytest.mark.parametrize("aggregate", [False, True])
async def test_custom_init_tracks_instances_with_field_set_seeded_by_new(aggregate):
    class Seeded(Model):
        id: int = Field(db_pk=True)
        count: int = 0

        class Meta:
            is_table = True

        def __new__(cls, **data):
            instance = super().__new__(cls)
            object.__setattr__(instance, "__pydantic_fields_set__", set())
            return instance

        def __init__(self, **data):
            super().__init__(**data)

    if aggregate:
        query = Seeded.objects.annotate(count=Count("id"))
        rows = (["id", "count"], [[1, 2]])
    else:
        query = Seeded.objects.filter().select("id")
        rows = (["id"], [[1]])
    client = StubExecuteClient([rows])
    (loaded,) = await query.all(client=client)

    if aggregate:
        assert isinstance(loaded.model_fields_set, _AggregateFieldsSet)
        with pytest.raises(ManagerError, match="cannot be saved"):
            await loaded.save(client=client)
    else:
        assert type(loaded.model_fields_set) is _LoadedFieldsSet
    assert len(client.calls) == 1


@pytest.mark.asyncio
@pytest.mark.parametrize("reinitialize", [False, True])
async def test_custom_init_tracks_cached_instance_only_if_reinitialized(reinitialize):
    cached = None

    class Cached(Model):
        id: int = Field(db_pk=True)
        name: str = ""

        class Meta:
            is_table = True

        def __new__(cls, **data):
            if cached is not None:
                return cached
            return super().__new__(cls)

        def __init__(self, **data):
            if cached is None or reinitialize:
                super().__init__(**data)

    cached = Cached(id=1, name="saved")
    fields_before = cached.model_fields_set
    client = StubExecuteClient([(["id"], [[2]])])
    (loaded,) = await Cached.objects.filter().select("id").all(client=client)
    assert loaded is cached
    if reinitialize:
        assert type(loaded.model_fields_set) is _LoadedFieldsSet
        assert loaded.model_fields_set == {"id"}
        assert loaded.id == 2
    else:
        assert loaded.model_fields_set is fields_before
        assert loaded.model_fields_set == {"id", "name"}
        assert loaded.id == 1


@pytest.mark.parametrize("many", [False, True])
def test_custom_init_wrap_validator_can_supply_constructor_argument(many):
    class TenantRow(Model):
        id: int
        tenant: str

        def __init__(self, *, tenant, **data):
            super().__init__(tenant=tenant.upper(), **data)

        @model_validator(mode="wrap")
        @classmethod
        def supply_tenant(cls, data, handler):
            return handler({**data, "tenant": "local"})

    rows = [{"id": 1}, {"id": 2}] if many else [{"id": 1}]
    if many:
        loaded = _validate_db_rows(TenantRow, rows)
    else:
        loaded = [_validate_db_row(TenantRow, rows[0])]

    assert [instance.tenant for instance in loaded] == ["LOCAL"] * len(rows)


@pytest.mark.parametrize("many", [False, True])
@pytest.mark.parametrize("after_inside_wrap", [False, True])
def test_custom_init_validator_order(many, after_inside_wrap):
    calls = []

    class Ordered(Model):
        id: int

        def __init__(self, **data):
            calls.append("init-pre")
            super().__init__(**data)
            calls.append("init-post")

        @model_validator(mode="before")
        @classmethod
        def before(cls, data):
            calls.append("before")
            return data

        if after_inside_wrap:

            @model_validator(mode="after")
            def after(self):
                calls.append("after")
                return self

        @model_validator(mode="wrap")
        @classmethod
        def wrap(cls, data, handler):
            calls.append("wrap-pre")
            instance = handler(data)
            calls.append("wrap-post")
            return instance

        if not after_inside_wrap:

            @model_validator(mode="after")
            def after(self):
                calls.append("after")
                return self

    if many:
        _validate_db_rows(Ordered, [{"id": 1}, {"id": 2}])
    else:
        _validate_db_row(Ordered, {"id": 1})

    expected = ["wrap-pre", "init-pre", "before"]
    if after_inside_wrap:
        expected += ["after", "init-post", "wrap-post"]
    else:
        expected += ["init-post", "wrap-post", "after"]
    assert calls == expected * (2 if many else 1)


@pytest.mark.parametrize("many", [False, True])
def test_custom_init_sees_after_validator_changes(many):
    calls = []

    class Derived(Model):
        id: int = Field(db_pk=True)
        name: str
        derived: str = ""

        class Meta:
            is_table = True

        def __init__(self, **data):
            super().__init__(**data)
            calls.append(("init", self.derived))

        @model_validator(mode="after")
        def derive_name(self):
            calls.append(("validator", self.name))
            self.derived = self.name.upper()
            return self

    rows = [{"id": 42, "name": "first"}, {"id": 43, "name": "second"}]
    if many:
        _validate_db_rows(Derived, rows)
        assert calls == [
            ("validator", "first"),
            ("init", "FIRST"),
            ("validator", "second"),
            ("init", "SECOND"),
        ]
    else:
        _validate_db_row(Derived, rows[0])
        assert calls == [("validator", "first"), ("init", "FIRST")]


@pytest.mark.parametrize("many", [False, True])
def test_custom_init_honors_replacing_after_validator(many):
    init_ids = []
    replacements = []

    class Replacing(Model):
        id: int = Field(db_pk=True)
        child: Replacing | None = None

        class Meta:
            is_table = True

        def __init__(self, **data):
            super().__init__(**data)
            init_ids.append(self.id)

        @model_validator(mode="after")
        def replace(self):
            replacement = self.model_construct(id=self.id + 100)
            replacements.append(replacement)
            return replacement

    row = {"id": 1}
    with warnings.catch_warnings():
        warnings.simplefilter("error")
        if many:
            (instance,) = _validate_db_rows(Replacing, [row])
        else:
            instance = _validate_db_row(Replacing, row)

    (replacement,) = replacements
    assert instance is replacement
    assert instance.id == 101
    assert init_ids == [1]


@pytest.mark.parametrize("many", [False, True])
def test_custom_init_wrap_validator_can_skip_handler(many):
    init_calls = []

    class Cached(Model):
        id: int = Field(db_pk=True)

        class Meta:
            is_table = True

        def __init__(self, **data):
            init_calls.append("entered")
            try:
                super().__init__(**data)
            except Exception:
                pass
            init_calls.append(self)

        @model_validator(mode="wrap")
        @classmethod
        def from_cache(cls, _data, _handler):
            return cached

    cached = Cached.model_construct(id=42)
    row = {"id": 1}
    with warnings.catch_warnings():
        warnings.simplefilter("error")
        if many:
            (instance,) = _validate_db_rows(Cached, [row])
        else:
            instance = _validate_db_row(Cached, row)

    assert instance is cached
    assert init_calls == []


@pytest.mark.parametrize("custom_init", [False, True])
def test_provenance_recorder_handles_non_dict_wrap_input(custom_init):
    class FromAttributes(Model):
        model_config = ConfigDict(from_attributes=True)
        id: int

        if custom_init:

            def __init__(self, **_data):
                raise AssertionError("non-dict data must use Pydantic's fallback")

        @model_validator(mode="wrap")
        @classmethod
        def transform_input(cls, data, handler):
            return handler(cached if data["id"] == 1 else SimpleNamespace(**data))

    cached = FromAttributes.model_construct(id=1)
    models, built = _validate_db_rows(
        FromAttributes, [{"id": 1}, {"id": 2}], owned=True
    )
    assert models[0] is cached
    assert models[1].id == 2
    assert built == {id(models[1]): models[1]}


def test_custom_init_batch_errors_keep_row_index():
    def make_model(custom_init):
        class Bad(Model):
            id: int
            name: str

            if custom_init:

                def __init__(self, **data):
                    super().__init__(**data)

        return Bad

    rows = [
        {"id": 1, "name": "ok"},
        {"id": "invalid", "name": "bad id"},
        {"id": 3, "name": None},
    ]
    with pytest.raises(ValidationError) as plain:
        _validate_db_rows(make_model(False), rows)
    with pytest.raises(ValidationError) as custom:
        _validate_db_rows(make_model(True), rows)

    expected_locations = [error["loc"] for error in plain.value.errors()]
    assert [error["loc"] for error in custom.value.errors()] == expected_locations
    assert custom.value.title == plain.value.title


@pytest.mark.parametrize("many", [False, True])
def test_custom_init_body_value_error_becomes_validation_error(many):
    class RejectsNegative(Model):
        id: int

        def __init__(self, **data):
            super().__init__(**data)
            if self.id < 0:
                raise ValueError("negative id")

    row = {"id": -1}
    with pytest.raises(ValidationError, match="negative id") as exc_info:
        if many:
            _validate_db_rows(RejectsNegative, [{"id": 1}, row])
        else:
            _validate_db_row(RejectsNegative, row)

    (error,) = exc_info.value.errors()
    assert error["loc"] == ((1,) if many else ())
    assert error["type"] == "value_error"


def test_custom_init_nested_constructor_uses_application_aliases():
    nested = []

    class Nested(Model):
        id: int = Field(default=1, db_pk=True, alias="key")

        class Meta:
            is_table = True

        def __init__(self, **data):
            if "id" in data:
                nested.append(Nested(key=99))
            super().__init__(**data)

    outer = _validate_db_row(Nested, {"id": 42})

    assert outer.id == 42
    assert [instance.id for instance in nested] == [99]


def test_custom_init_passes_row_to_new():
    class CustomNew(Model):
        id: int

        def __new__(cls, *, id):
            return super().__new__(cls)

        def __init__(self, **data):
            super().__init__(**data)

    assert _validate_db_row(CustomNew, {"id": 42}).id == 42


def test_pydantic_detects_custom_init_from_mixin():
    class Setup(BaseModel):
        _setup_done: bool = PrivateAttr(default=False)

        def __init__(self, **data):
            super().__init__(**data)
            self._setup_done = True

    class Table(Model, Setup):
        id: int = Field(db_pk=True, alias="key")

        class Meta:
            is_table = True

    assert Table.__pydantic_custom_init__ is True
    assert Table.model_validate({"key": 1})._setup_done
    assert _validate_db_row(Table, {"id": 2})._setup_done
    assert all(
        instance._setup_done
        for instance in _validate_db_rows(Table, [{"id": 3}, {"id": 4}])
    )


@pytest.mark.parametrize("raise_error", [False, True])
def test_custom_init_removes_temporary_validator_without_super(raise_error):
    instances = []
    error = ValueError("custom init failed")

    class SkipsValidation(Model):
        def __init__(self, **_data):
            instances.append(self)
            if raise_error:
                raise error

    if raise_error:
        with pytest.raises(ValidationError) as exc_info:
            _validate_db_row(SkipsValidation, {})
        assert exc_info.value.errors()[0]["ctx"]["error"] is error
    else:
        assert _validate_db_row(SkipsValidation, {}) is instances[0]

    (instance,) = instances
    assert "__pydantic_validator__" not in instance.__dict__


def test_plain_models_keep_pydantic_base_init():
    assert HiddenKey.__pydantic_custom_init__ is False


class HydrationAuthor(Model):
    id: int = Field(db_pk=True)

    class Meta:
        is_table = True


class HydrationPost(Model):
    id: int = Field(db_pk=True)
    author: HydrationAuthor | None = None

    class Meta:
        is_table = True


def test_fk_model_keeps_constructor_signature_and_plain_db_schema():
    assert HydrationPost.__pydantic_custom_init__ is False
    assert str(inspect.signature(HydrationPost)) == (
        f"(*, id: int, author: {HydrationAuthor.__module__}.HydrationAuthor | None = None, "
        "author_id: int | None = None) -> None"
    )
    schemas = _database_schemas(HydrationPost)
    schema = schemas._specialize(schemas.nodes, track_pk=False, track_private=False)
    nodes = _outer_nodes(HydrationPost, dict(schema))
    model = next(node for node in nodes if node["type"] == "model")
    assert "post_init" not in model
    assert all("validation_alias" not in field for field in nodes[-1]["fields"].values())


def test_fk_alias_adds_no_db_initializer_wrapper():
    class CustomInitFk(Model):
        author: HydrationAuthor | None = None

        def __init__(self, **data):
            super().__init__(**data)

        class Meta:
            is_table = True

    assert CustomInitFk.__pydantic_custom_init__ is True
    schemas = _database_schemas(CustomInitFk)
    schema = schemas._specialize(
        schemas.nodes, track_pk=False, track_private=False, name_first=True
    )
    assert not schemas.colliding_aliases
    assert all(
        node["type"] != "function-before"
        for node in _outer_nodes(CustomInitFk, dict(schema))
    )


@pytest.mark.asyncio
async def test_join_hydration_does_not_assign_fk_fields(monkeypatch):
    register_table(HydrationAuthor)
    calls = []
    original = Model.__setattr__

    def record_assignment(self, name, value):
        calls.append(name)
        original(self, name, value)

    monkeypatch.setattr(Model, "__setattr__", record_assignment)
    stub = StubExecuteClient(
        [
            [
                ["id"],
                [[1], [2], [3]],
                {"author": {"columns": ["id"], "data": {9: [9]}, "refs": [9, None, 9]}},
            ]
        ]
    )
    posts = await HydrationPost.objects.join("author").select("id").all(client=stub)
    assert calls == []
    assert [post.author.id if post.author else None for post in posts] == [9, None, 9]
    assert all(post.model_fields_set == {"id", "author"} for post in posts)


def test_plain_owned_validation_reuses_validator_without_recorder():
    rows = [{"id": 1, "name": "saved"}]
    _validate_db_rows(HiddenKey, rows)
    schemas = _database_schemas(HiddenKey)
    validators = schemas.validators.copy()

    (instance,), built = _validate_db_rows(HiddenKey, rows, owned=True)

    assert instance.name == "saved"
    assert built is None
    assert schemas.validators == validators
    assert all(not key[-1] for key in validators)


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
@pytest.mark.parametrize("custom_init", [False, True])
async def test_refresh_captures_mutated_private_container_and_preserves_untouched_state(
    custom_init,
):
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

        if custom_init:

            def __init__(self, **data):
                super().__init__(**data)

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
