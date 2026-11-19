"""Compiled Pydantic schemas for the field-name boundary at the database."""

from __future__ import annotations

from contextvars import ContextVar
from threading import Lock
from typing import TYPE_CHECKING, Any, TypeVar, cast

from pydantic_core import SchemaSerializer, SchemaValidator, core_schema

if TYPE_CHECKING:
    from oxyde.models.base import Model


TModel = TypeVar("TModel", bound="Model")
Schema = dict[str, Any]

_PRIVATE_DEFAULTS: ContextVar[dict[int, dict[str, Any]] | None] = ContextVar(
    "_PRIVATE_DEFAULTS", default=None
)


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
        self.can_skip_provenance = self.has_provenance and not any(
            node["type"] in ("function-before", "function-wrap")
            and not _is_provenance_wrap(node)
            for node in self.nodes
        )
        self.pk_field = model._db_meta.pk_field if self.can_skip_provenance else None
        self.validators: dict[tuple[bool, bool, bool], SchemaValidator] = {}
        self.serializer: SchemaSerializer | None = None

    def _specialize(
        self, *, track_pk: bool, track_private: bool
    ) -> core_schema.CoreSchema:
        # Reuse original definitions for nested references, including references
        # back to this model. The copied outer branch is a separate occurrence.
        schema = None
        for original in reversed(self.nodes):
            tag = original["type"]
            if not track_pk and _is_provenance_wrap(original):
                continue
            node = {key: value for key, value in original.items() if key != "ref"}
            if schema is not None:
                node[_child_key(node)] = schema
                if tag == "json-or-python":
                    node["json_schema"] = schema
            if tag == "model-fields":
                node["fields"] = {
                    name: {
                        key: value
                        for key, value in field.items()
                        if key not in ("validation_alias", "serialization_alias")
                    }
                    for name, field in node["fields"].items()
                }
                if "computed_fields" in node:
                    node["computed_fields"] = [
                        {key: value for key, value in field.items() if key != "alias"}
                        for field in node["computed_fields"]
                    ]
            elif tag == "model":
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
            schema = node
        return cast(core_schema.CoreSchema, schema)

    def validator(
        self, *, many: bool, track_pk: bool, track_private: bool = False
    ) -> SchemaValidator:
        key = (many, track_pk, track_private)
        compiled = self.validators.get(key)
        if compiled is not None:
            return compiled
        with _CACHE_LOCK:
            compiled = self.validators.get(key)
            if compiled is None:
                schema = self._specialize(
                    track_pk=track_pk, track_private=track_private
                )
                if many:
                    schema = core_schema.list_schema(schema)
                compiled = SchemaValidator(schema, _use_prebuilt=False)
                self.validators[key] = compiled
        return compiled

    def dump(self, instance: Model, exclude: set[str] | None) -> dict[str, Any]:
        serializer = self.serializer
        if serializer is None:
            with _CACHE_LOCK:
                serializer = self.serializer
                if serializer is None:
                    schema = self._specialize(track_pk=False, track_private=False)
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


def _validate_db_rows(model: type[TModel], rows: list[dict[str, Any]]) -> list[TModel]:
    if not rows:
        return []
    schemas = _database_schemas(model)
    pk = schemas.pk_field
    track_pk = schemas.has_provenance and (
        pk is None or not all(pk in row for row in rows)
    )
    validator = schemas.validator(many=True, track_pk=track_pk)
    return cast(list[TModel], validator.validate_python(rows))


def _validate_db_row(
    model: type[TModel], row: dict[str, Any], *, track_private: bool = False
) -> TModel:
    schemas = _database_schemas(model)
    pk = schemas.pk_field
    track_pk = schemas.has_provenance and (pk is None or pk not in row)
    validator = schemas.validator(
        many=False, track_pk=track_pk, track_private=track_private
    )
    return cast(TModel, validator.validate_python(row))


def _dump_db_fields(
    instance: Model, *, exclude: set[str] | None = None
) -> dict[str, Any]:
    return _database_schemas(type(instance)).dump(instance, exclude)
