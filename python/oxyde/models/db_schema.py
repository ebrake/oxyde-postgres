"""Compiled Pydantic schemas for the field-name boundary at the database."""

from __future__ import annotations

from collections.abc import Callable
from contextvars import ContextVar
from dataclasses import dataclass
from threading import Lock
from typing import TYPE_CHECKING, Any, Literal, TypeVar, cast, overload

from pydantic import AliasPath
from pydantic_core import (
    PydanticUndefined,
    SchemaSerializer,
    SchemaValidator,
    core_schema,
)

if TYPE_CHECKING:
    from oxyde.models.base import Model


TModel = TypeVar("TModel", bound="Model")
Schema = dict[str, Any]

_PRIVATE_DEFAULTS: ContextVar[dict[int, dict[str, Any]] | None] = ContextVar(
    "_PRIVATE_DEFAULTS", default=None
)


@dataclass(slots=True)
class _InitResult:
    instance: Model
    result: Model
    row: dict[str, Any]


_INIT_RESULT: ContextVar[_InitResult | None] = ContextVar("_INIT_RESULT", default=None)
_BUILT: ContextVar[list[Model] | None] = ContextVar("_BUILT", default=None)


def _capture_init_result(
    data: Any, handler: core_schema.ValidatorFunctionWrapHandler
) -> Any:
    result = handler(data)
    slot = _INIT_RESULT.get()
    if slot is None or result is slot.instance:
        return result

    slot.result = result
    # Let __init__ finish on its initialized self. The constructor returns the
    # replacement after BaseModel.__init__ has checked the returned identity.
    return slot.instance


def _record_built_model(
    data: Any, handler: core_schema.ValidatorFunctionWrapHandler
) -> Any:
    instance = handler(data)
    built = _BUILT.get()
    if built is not None and instance is not data:
        built.append(instance)
    return instance


def _name_first_alias(
    name: str, alias: str | list[str | int] | list[list[str | int]]
) -> list[list[str | int]]:
    if isinstance(alias, str):
        return [[name], [alias]]
    if isinstance(alias[0], list):
        return [[name], *cast(list[list[str | int]], alias)]
    return [[name], cast(list[str | int], alias)]


def _container_state(value: Any, seen: dict[int, int]) -> Any:
    """Snapshot built-in containers without invoking private objects' copy hooks."""
    kind = type(value)
    identity = id(value)
    if kind not in (dict, list, tuple, set, frozenset):
        return identity

    if identity in seen:
        return ("ref", seen[identity])
    seen[identity] = len(seen)
    contents: tuple | frozenset
    if kind is dict:
        contents = tuple(
            (_container_state(key, seen), _container_state(item, seen))
            for key, item in value.items()
        )
    elif kind in (set, frozenset):
        # Set members can share immutable objects in different iteration order.
        contents = frozenset(_container_state(item, {}) for item in value)
    else:
        contents = tuple(_container_state(item, seen) for item in value)
    return kind, contents


def _capture_private_defaults(instance: Model) -> None:
    snapshots = _PRIVATE_DEFAULTS.get()
    if snapshots is not None:
        snapshots[id(instance)] = {
            name: _container_state(value, {})
            for name, value in (instance.__pydantic_private__ or {}).items()
            if type(value) in (dict, list, set, tuple, frozenset)
        }


def _changed_private_containers(instance: Model, baseline: dict[str, Any]) -> set[str]:
    return {
        name
        for name, value in (instance.__pydantic_private__ or {}).items()
        if name in baseline and baseline[name] != _container_state(value, {})
    }


def _child_key(node: Schema) -> str:
    return "python_schema" if node["type"] == "json-or-python" else "schema"


def _outer_nodes(model: type[Model], schema: Schema) -> list[Schema]:
    """Walk the outer model and its validation wrappers, without entering fields."""
    definitions: dict[str, Schema] = {}
    nodes = []
    while True:
        tag = schema["type"]
        if tag == "definition-ref":
            schema = definitions[schema["schema_ref"]]
            continue
        if tag in ("union", "tagged-union", "chain", "lax-or-strict", "nullable"):
            raise TypeError(
                f"{model.__name__} uses unsupported database schema {tag!r}"
            )
        nodes.append(schema)
        child_key = _child_key(schema)
        if tag == "model-fields" or child_key not in schema:
            return nodes
        if tag == "definitions":
            definitions.update((node["ref"], node) for node in schema["definitions"])
        schema = schema[child_key]


def _is_provenance_wrap(node: Schema) -> bool:
    from oxyde.models.base import _pk_provenance_wrap

    return (
        node["type"] == "function-wrap"
        and node["function"].get("function") is _pk_provenance_wrap
    )


class _DatabaseSchemas:
    def __init__(self, model: type[Model], canonical: Schema) -> None:
        self.canonical = canonical
        self.nodes = _outer_nodes(model, canonical)
        self.model = model
        self.has_provenance = any(_is_provenance_wrap(node) for node in self.nodes)
        self.can_skip_provenance = (
            self.has_provenance
            and not model.__pydantic_custom_init__
            and not any(
                node["type"] in ("function-before", "function-wrap")
                and not _is_provenance_wrap(node)
                for node in self.nodes
            )
        )
        self.pk_field = model._db_meta.pk_field if self.can_skip_provenance else None
        self.can_replace = False
        for node in self.nodes:
            tag = node["type"]
            if tag == "model":
                break
            if tag in ("function-after", "function-wrap", "function-plain") and (
                not _is_provenance_wrap(node)
            ):
                self.can_replace = True
                break
        self.initializer_aliases: dict[str, list[list[str | int]]] = {}
        self.colliding_aliases: list[list[tuple[AliasPath, str | None]]] = []
        if (
            model.__pydantic_custom_init__
            and model.model_config.get("validate_by_alias", True)
            and self.nodes[-1]["type"] == "model-fields"
        ):
            self._prepare_initializer_aliases(self.nodes[-1]["fields"])
        self.custom_init_schemas: dict[
            tuple[bool, bool, bool], core_schema.CoreSchema
        ] = {}
        self.validators: dict[tuple[bool, bool, bool, bool], SchemaValidator] = {}
        self.serializer: SchemaSerializer | None = None

    def _prepare_initializer_aliases(self, fields: dict[str, Schema]) -> None:
        for name, field in fields.items():
            if "validation_alias" not in field:
                continue
            field_name, *aliases = _name_first_alias(name, field["validation_alias"])
            initializer_choices = [field_name]
            collision_paths: list[tuple[AliasPath, str | None]] = [
                (AliasPath(name), None)
            ]
            has_collision = False
            link = self.model._db_meta.fk_links.get(name)
            for path in aliases:
                if link is not None and name == link[1] and path == [link[0], link[2]]:
                    # A row cannot supply the virtual relation as an alias source.
                    continue
                head = cast(str, path[0])
                sentinel = None
                if head != name and head in fields:
                    sentinel = "\x00alias:" + head
                    has_collision = True
                collision_paths.append((AliasPath(head, *path[1:]), sentinel))
                initializer_choices.append([sentinel, *path[1:]] if sentinel else path)
            self.initializer_aliases[name] = initializer_choices
            if has_collision:
                self.colliding_aliases.append(collision_paths)

    def _expose_constructor_aliases(self, data: Any) -> Any:
        slot = _INIT_RESULT.get()
        if slot is None or not isinstance(data, dict):
            return data

        # A row key belongs to its own field. Expose constructor-added aliases
        # only when Pydantic will use that choice, including the full alias path.
        exposed = {}
        for choices in self.colliding_aliases:
            for path, sentinel in choices:
                head = path.path[0]
                if sentinel is not None and head in slot.row:
                    continue
                if path.search_dict_for_path(data) is PydanticUndefined:
                    continue
                if sentinel is not None:
                    exposed[sentinel] = data[head]
                break
        return {**data, **exposed} if exposed else data

    def _specialize(
        self,
        nodes: list[Schema],
        *,
        track_pk: bool,
        track_private: bool,
        use_custom_init: bool = True,
        name_first: bool = False,
        record_built: bool = False,
        inner_schema: core_schema.CoreSchema | None = None,
    ) -> core_schema.CoreSchema:
        # Reuse original definitions for nested references, including references
        # back to this model. The copied outer branch is a separate occurrence.
        schema = cast(Schema | None, inner_schema)
        for original in reversed(nodes):
            tag = original["type"]
            if not track_pk and _is_provenance_wrap(original):
                continue
            node = {key: value for key, value in original.items() if key != "ref"}
            if schema is not None:
                node[_child_key(node)] = schema
                if tag == "json-or-python":
                    node["json_schema"] = schema
            if tag == "model-fields":
                fields = {}
                for name, field in node["fields"].items():
                    specialized = {
                        key: value
                        for key, value in field.items()
                        if key not in ("validation_alias", "serialization_alias")
                    }
                    if name_first and name in self.initializer_aliases:
                        specialized["validation_alias"] = self.initializer_aliases[name]
                    fields[name] = specialized
                node["fields"] = fields
                if "computed_fields" in node:
                    node["computed_fields"] = [
                        {key: value for key, value in field.items() if key != "alias"}
                        for field in node["computed_fields"]
                    ]
                if name_first and self.colliding_aliases:
                    node = cast(
                        Schema,
                        core_schema.no_info_before_validator_function(
                            self._expose_constructor_aliases,
                            cast(core_schema.CoreSchema, node),
                        ),
                    )
            elif tag == "model":
                if not use_custom_init:
                    node.pop("custom_init", None)
                if track_private and self.model.__private_attributes__:
                    node["post_init"] = "_capture_private_before_post_init"
                elif (
                    not track_pk
                    and node.get("post_init") == "_capture_pk_before_post_init"
                ):
                    post_init = self.model.__pydantic_post_init__
                    if post_init is None:
                        node.pop("post_init")
                    else:
                        node["post_init"] = post_init
                if record_built:
                    node = cast(
                        Schema,
                        core_schema.no_info_wrap_validator_function(
                            _record_built_model, cast(core_schema.CoreSchema, node)
                        ),
                    )
            schema = node
        return cast(core_schema.CoreSchema, schema)

    def _split_custom_init(
        self,
    ) -> tuple[list[Schema], list[Schema], list[Schema]] | None:
        # Keep the after validators nearest the model in super().__init__().
        # The outer wraps must see the row before the constructor runs.
        model_index = next(
            (i for i, node in enumerate(self.nodes) if node["type"] == "model"),
            None,
        )
        if model_index is None:
            return None
        construct_index = model_index
        while (
            construct_index > 0
            and self.nodes[construct_index - 1]["type"] == "function-after"
        ):
            construct_index -= 1
        return (
            self.nodes[:construct_index],
            self.nodes[construct_index:],
            self.nodes[model_index:],
        )

    def _custom_init_schema(
        self, track_pk: bool, track_private: bool, record_built: bool
    ) -> core_schema.CoreSchema:
        from oxyde.models.base import _PK_VALIDATION, _pk_provenance_wrap

        key = (track_pk, track_private, record_built)
        schema = self.custom_init_schemas.get(key)
        if schema is not None:
            return schema

        split = self._split_custom_init()
        if split is None:
            return self._specialize(
                self.nodes, track_pk=track_pk, track_private=track_private
            )
        outer, inner, model_nodes = split
        definitions = [node for node in outer if node["type"] == "definitions"]

        def compile_initializer(
            nodes: list[Schema], *, capture_result: bool, name_first: bool
        ) -> SchemaValidator:
            initializer_schema = self._specialize(
                nodes,
                track_pk=track_pk,
                track_private=track_private,
                use_custom_init=False,
                name_first=name_first,
                record_built=record_built and not name_first,
            )
            if capture_result:
                initializer_schema = core_schema.no_info_wrap_validator_function(
                    _capture_init_result, initializer_schema
                )
            if track_pk:
                initializer_schema = core_schema.no_info_wrap_validator_function(
                    _pk_provenance_wrap, initializer_schema
                )
            if definitions:
                initializer_schema = self._specialize(
                    definitions,
                    track_pk=track_pk,
                    track_private=track_private,
                    inner_schema=initializer_schema,
                )
            return SchemaValidator(initializer_schema, _use_prebuilt=False)

        initializer = compile_initializer(
            inner, capture_result=len(inner) > len(model_nodes), name_first=True
        )
        fallback = compile_initializer(
            model_nodes, capture_result=False, name_first=False
        )
        model = self.model
        allocate = cast("Callable[..., Model]", model.__new__)
        pk_field = model._db_meta.pk_field if track_pk else None

        def _construct(row: Any) -> Model:
            if not isinstance(row, dict):
                # Pydantic's custom-init path also skips __init__ when the
                # input cannot be unpacked as keyword arguments.
                return cast("Model", fallback.validate_python(row))
            instance = allocate(model, **row)
            before = (
                getattr(instance, "__pydantic_fields_set__", None)
                if record_built
                else None
            )
            # BaseModel.__init__ reads the validator from self, so nested
            # constructors keep the class validator and their application aliases.
            instance.__dict__["__pydantic_validator__"] = initializer
            slot = _InitResult(instance, instance, row)
            token = _INIT_RESULT.set(slot)
            try:
                type(instance).__init__(instance, **row)
            finally:
                instance.__dict__.pop("__pydantic_validator__", None)
                _INIT_RESULT.reset(token)
            if not record_built and pk_field is None:
                return slot.result
            fields_set = getattr(instance, "__pydantic_fields_set__", None)
            if record_built and (before is None or fields_set is not before):
                built = _BUILT.get()
                if built is not None:
                    built.append(instance)

            # The initializer has already restored key membership. Track it
            # again for outer wraps, unless __init__ explicitly set the key.
            if (
                pk_field is not None
                and fields_set is not None
                and pk_field not in fields_set
            ):
                tracked = _PK_VALIDATION.get()
                if tracked is not None:
                    tracked.append((instance, pk_field))
            return slot.result

        schema = self._specialize(
            outer,
            track_pk=track_pk,
            track_private=track_private,
            inner_schema=core_schema.no_info_plain_validator_function(_construct),
        )
        self.custom_init_schemas[key] = schema
        return schema

    def _validator(
        self,
        *,
        many: bool,
        track_pk: bool,
        track_private: bool,
        record_built: bool = False,
    ) -> SchemaValidator:
        key = (many, track_pk, track_private, record_built)
        compiled = self.validators.get(key)
        if compiled is not None:
            return compiled
        with _CACHE_LOCK:
            compiled = self.validators.get(key)
            if compiled is None:
                schema: core_schema.CoreSchema
                config = None
                if self.model.__pydantic_custom_init__:
                    schema = self._custom_init_schema(
                        track_pk, track_private, record_built
                    )
                    name = self.model.__name__
                    config = core_schema.CoreConfig(
                        title=f"list[{name}]" if many else name
                    )
                else:
                    schema = self._specialize(
                        self.nodes,
                        track_pk=track_pk,
                        track_private=track_private,
                        record_built=record_built,
                    )
                if many:
                    schema = core_schema.list_schema(schema)
                compiled = SchemaValidator(schema, config, _use_prebuilt=False)
                self.validators[key] = compiled
        return compiled

    def validate(
        self,
        data: dict[str, Any] | list[dict[str, Any]],
        *,
        many: bool,
        track_pk: bool,
        track_private: bool = False,
        record_built: bool = False,
    ) -> Any:
        validator = self._validator(
            many=many,
            track_pk=track_pk,
            track_private=track_private,
            record_built=record_built,
        )
        return validator.validate_python(data)

    def dump(self, instance: Model, exclude: set[str] | None) -> dict[str, Any]:
        serializer = self.serializer
        if serializer is None:
            with _CACHE_LOCK:
                serializer = self.serializer
                if serializer is None:
                    schema = self._specialize(
                        self.nodes, track_pk=False, track_private=False
                    )
                    serializer = SchemaSerializer(schema, _use_prebuilt=False)
                    self.serializer = serializer
        return cast(
            dict[str, Any],
            serializer.to_python(instance, mode="python", exclude=exclude),
        )


_CACHE: dict[type[Model], _DatabaseSchemas] = {}
_CACHE_LOCK = Lock()


def _database_schemas(model: type[Model]) -> _DatabaseSchemas:
    canonical = model.__pydantic_core_schema__
    cached = _CACHE.get(model)
    if cached is not None and cached.canonical is canonical:
        return cached
    with _CACHE_LOCK:
        canonical = model.__pydantic_core_schema__
        if not isinstance(canonical, dict):
            model.model_rebuild()
            canonical = model.__pydantic_core_schema__
        cached = _CACHE.get(model)
        if cached is None or cached.canonical is not canonical:
            cached = _DatabaseSchemas(model, cast(Schema, canonical))
            _CACHE[model] = cached
        return cached


@overload
def _validate_db_rows(
    model: type[TModel], rows: list[dict[str, Any]], *, owned: Literal[False] = False
) -> list[TModel]: ...


@overload
def _validate_db_rows(
    model: type[TModel], rows: list[dict[str, Any]], *, owned: Literal[True]
) -> tuple[list[TModel], dict[int, Model] | None]: ...


def _validate_db_rows(
    model: type[TModel], rows: list[dict[str, Any]], *, owned: bool = False
) -> list[TModel] | tuple[list[TModel], dict[int, Model] | None]:
    if not rows:
        return ([], None) if owned else []
    schemas = _database_schemas(model)
    pk = schemas.pk_field
    track_pk = schemas.has_provenance and (
        pk is None or not all(pk in row for row in rows)
    )
    if owned and (schemas.can_replace or model.__new__ is not object.__new__):
        built: list[Model] = []
        token = _BUILT.set(built)
        try:
            models = cast(
                list[TModel],
                schemas.validate(rows, many=True, track_pk=track_pk, record_built=True),
            )
        finally:
            _BUILT.reset(token)
        # Keep strong references so these IDs remain valid during stamping.
        return models, {id(instance): instance for instance in built}
    models = cast(list[TModel], schemas.validate(rows, many=True, track_pk=track_pk))
    return (models, None) if owned else models


def _validate_db_row(
    model: type[TModel], row: dict[str, Any], *, track_private: bool = False
) -> TModel:
    schemas = _database_schemas(model)
    pk = schemas.pk_field
    track_pk = schemas.has_provenance and (pk is None or pk not in row)
    return cast(
        TModel,
        schemas.validate(
            row, many=False, track_pk=track_pk, track_private=track_private
        ),
    )


def _dump_db_fields(
    instance: Model, *, exclude: set[str] | None = None
) -> dict[str, Any]:
    return _database_schemas(type(instance)).dump(instance, exclude)
