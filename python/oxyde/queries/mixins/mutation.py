"""Mutation mixin for query building."""

from __future__ import annotations

import warnings
from collections import Counter
from collections.abc import Iterable, Iterator
from contextlib import contextmanager
from enum import Enum
from functools import cache
from itertools import groupby
from typing import (
    TYPE_CHECKING,
    Any,
    Generic,
    Literal,
    TypeVar,
    get_args,
    get_origin,
    overload,
)

from pydantic import TypeAdapter

from oxyde._msgpack import msgpack
from oxyde.core import ir
from oxyde.core.types import serialize_value
from oxyde.exceptions import ManagerError
from oxyde.models.db_schema import (
    _PRIVATE_DEFAULTS,
    _changed_private_containers,
    _dump_db_fields,
    _validate_db_row,
    _validate_db_rows,
)
from oxyde.models.serializers import (
    _dump_insert_data,
    _dump_update_data,
    _normalize_instance,
)
from oxyde.models.utils import _unpack_annotated, _unwrap_optional
from oxyde.queries.base import (
    SupportsExecute,
    _build_bind_types,
    _build_column_db_types,
    _build_column_types,
    _map_values_to_columns,
    _model_key,
    _resolve_execution_client,
    _translate_core_error,
)
from oxyde.queries.expressions import (
    F,
    Value,
    _Expression,
    _serialize_value_for_ir,
)
from oxyde.queries.insert import InsertQuery

if TYPE_CHECKING:
    from oxyde.models.base import Model


TModel = TypeVar("TModel", bound="Model")


def _hydrate_models(
    model_class: type[TModel], columns: list[str], rows: list[list[Any]]
) -> list[TModel]:
    """Convert raw columns + rows into validated model instances."""
    rmap = model_class._db_meta.reverse_column_map
    field_columns = [rmap.get(c, c) for c in columns] if rmap else columns
    return _validate_db_rows(
        model_class, [dict(zip(field_columns, row)) for row in rows]
    )


@contextmanager
def _track_private_assignments() -> Iterator[
    tuple[list[tuple[Model, str]], dict[int, dict[str, Any]]]
]:
    """Record (model, private name) pairs assigned while validating."""
    from oxyde.models.base import _PRIVATE_ASSIGNMENTS

    assigned: list[tuple[Model, str]] = []
    token = _PRIVATE_ASSIGNMENTS.set(assigned)
    snapshots: dict[int, dict[str, Any]] = {}
    snapshot_token = _PRIVATE_DEFAULTS.set(snapshots)
    try:
        yield assigned, snapshots
    finally:
        _PRIVATE_DEFAULTS.reset(snapshot_token)
        _PRIVATE_ASSIGNMENTS.reset(token)


def _refresh_in_place(
    instance: Model, hydrated: Model, assigned_private: set[str]
) -> None:
    """Apply a returned row while retaining the instance's local state.

    Private attributes are taken from ``hydrated`` only when its validation
    assigned them (validator-derived state) or when ``instance`` has none yet.
    Defaults and factory values (e.g. ``PrivateAttr(default_factory=uuid4)``)
    are never assigned through ``__setattr__``, so local state survives.
    """
    instance.__dict__.update(hydrated.__dict__)
    object.__setattr__(
        instance,
        "__pydantic_fields_set__",
        instance.model_fields_set | hydrated.model_fields_set,
    )

    if hydrated.__pydantic_private__ is not None:
        private = dict(instance.__pydantic_private__ or {})
        for name, value in hydrated.__pydantic_private__.items():
            if name in assigned_private or name not in private:
                private[name] = value
        object.__setattr__(instance, "__pydantic_private__", private)

    if hydrated.__pydantic_extra__ is not None:
        extras = {**(instance.__pydantic_extra__ or {}), **hydrated.__pydantic_extra__}
        object.__setattr__(instance, "__pydantic_extra__", extras)


def _decode_returning_models(
    model_class: type[TModel], result: dict[str, Any]
) -> list[TModel]:
    """Decode mutation-returning result {columns, rows} into model instances."""
    rows = result.get("rows", [])
    if not rows:
        return []
    return _hydrate_models(model_class, result.get("columns", []), rows)


def _decode_columnar_models(
    model_class: type[TModel], result: list[Any]
) -> list[TModel]:
    """Decode columnar SELECT result [columns, rows] into model instances."""
    if len(result) < 2 or not result[1]:
        return []
    return _hydrate_models(model_class, result[0], result[1])


def _is_enum_annotation(annotation: Any) -> bool:
    annotation, _ = _unpack_annotated(annotation)
    annotation, _ = _unwrap_optional(annotation)
    origin = get_origin(annotation)
    if origin is list:
        args = get_args(annotation)
        return bool(args) and _is_enum_annotation(args[0])
    if origin is not None:
        return any(
            arg is not type(None) and _is_enum_annotation(arg)
            for arg in get_args(annotation)
        )
    return isinstance(annotation, type) and issubclass(annotation, Enum)


@cache
def _enum_type_adapter(python_type: Any) -> TypeAdapter:
    return TypeAdapter(python_type)


def _validate_enum_update_values(
    model_class: type[Model],
    values: dict[str, Any],
) -> dict[str, Any]:
    validated = dict(values)
    metadata = model_class._db_meta.field_metadata
    for field, value in values.items():
        # A bare Value() is a plain literal for the target column: validate it
        # like a raw value. Any other expression is computed by the database.
        is_literal = isinstance(value, Value)
        if not is_literal and isinstance(value, (F, _Expression)):
            continue
        meta = metadata.get(field)
        if meta is not None and _is_enum_annotation(meta.python_type):
            raw = value._expr["value"] if is_literal else value
            if raw is None and meta.nullable:
                checked = None
            else:
                checked = _enum_type_adapter(meta.python_type).validate_python(raw)
            validated[field] = Value(checked) if is_literal else checked
    return validated


class MutationMixin(Generic[TModel]):
    """Mixin providing data mutation capabilities."""

    # These attributes are defined in the base Query class
    model_class: type[TModel]

    def _build_filter_tree(self) -> ir.FilterNode | None:
        """Must be implemented by FilteringMixin."""
        raise NotImplementedError

    async def increment(
        self,
        field: str,
        by: int | float = 1,
        *,
        using: str | None = None,
        client: SupportsExecute | None = None,
    ) -> int:
        """
        Atomically increment a field value.

        Args:
            field: Field name to increment
            by: Amount to increment (default 1)
            using: Database alias
            client: Optional database client

        Returns:
            Number of affected rows

        Examples:
            await Post.objects.filter(id=42).increment("views", by=1)
            await User.objects.filter(is_active=True).increment("login_count")
        """
        exec_client = await _resolve_execution_client(using, client)
        column_types = _build_column_types(self.model_class)
        update_ir = ir.build_update_ir(
            table=self.model_class.get_table_name(),
            values=_map_values_to_columns(
                self.model_class,
                {field: _serialize_value_for_ir(F(field) + by, self.model_class)},
            ),
            filter_tree=self._build_filter_tree(),
            column_types=column_types,
            bind_types=_build_bind_types(self.model_class),
            column_db_types=_build_column_db_types(self.model_class),
            model=_model_key(self.model_class),
        )
        result_bytes = await exec_client.execute(update_ir)
        result = msgpack.unpackb(result_bytes, raw=False)
        affected: int = result.get("affected", 0)
        return affected

    @overload
    async def update(
        self,
        *,
        returning: Literal[True],
        using: str | None = ...,
        client: SupportsExecute | None = ...,
        **values: Any,
    ) -> list[TModel]: ...

    @overload
    async def update(
        self,
        *,
        returning: Literal[False] = ...,
        using: str | None = ...,
        client: SupportsExecute | None = ...,
        **values: Any,
    ) -> int: ...

    async def update(
        self,
        *,
        returning: bool = False,
        using: str | None = None,
        client: SupportsExecute | None = None,
        **values: Any,
    ) -> int | list[TModel]:
        """
        Update records matching the query.

        Args:
            returning: If True, return updated model instances.
                Default False — returns number of affected rows (Django-compatible).
            using: Database alias
            client: Optional database client
            **values: Field values to update

        Returns:
            Number of affected rows (default), or list of updated model instances
            if returning=True.

        Examples:
            count = await Post.objects.filter(id=42).update(status="published")
            posts = await Post.objects.filter(id=42).update(
                status="published", returning=True
            )
        """
        exec_client = await _resolve_execution_client(using, client)
        column_types = _build_column_types(self.model_class)
        values = _validate_enum_update_values(self.model_class, values)
        mapped_values = _map_values_to_columns(self.model_class, values)
        serialized_values = {
            key: _serialize_value_for_ir(value, self.model_class)
            for key, value in mapped_values.items()
        }

        update_ir = ir.build_update_ir(
            table=self.model_class.get_table_name(),
            values=serialized_values,
            filter_tree=self._build_filter_tree(),
            column_types=column_types,
            bind_types=_build_bind_types(self.model_class),
            column_db_types=_build_column_db_types(self.model_class),
            model=_model_key(self.model_class),
            returning=returning,
        )
        result_bytes = await exec_client.execute(update_ir)
        result = msgpack.unpackb(result_bytes, raw=False)
        if returning:
            return _decode_returning_models(self.model_class, result)
        affected: int = result.get("affected", 0)
        return affected

    async def delete(
        self,
        *,
        using: str | None = None,
        client: SupportsExecute | None = None,
    ) -> int:
        """
        Delete records matching the query.

        Args:
            using: Database alias
            client: Optional database client

        Returns:
            Number of affected rows

        Examples:
            await Post.objects.filter(id=42).delete()
            await User.objects.filter(is_active=False).delete()
        """
        exec_client = await _resolve_execution_client(using, client)
        delete_ir = ir.build_delete_ir(
            table=self.model_class.get_table_name(),
            filter_tree=self._build_filter_tree(),
            column_types=_build_column_types(self.model_class),
            bind_types=_build_bind_types(self.model_class),
            column_db_types=_build_column_db_types(self.model_class),
            model=_model_key(self.model_class),
        )
        result_bytes = await exec_client.execute(delete_ir)
        result = msgpack.unpackb(result_bytes, raw=False)
        affected: int = result.get("affected", 0)
        return affected

    def _primary_key_field(self) -> str | None:
        """Get primary key field name."""
        return self.model_class._db_meta.pk_field

    async def _run_mutation(
        self, query: Any, client: SupportsExecute
    ) -> dict[str, Any]:
        """Execute mutation query with error handling."""
        try:
            result = await query.execute(client)
        except ManagerError:
            raise
        except Exception as exc:
            translated = _translate_core_error(exc)
            if translated is not None:
                raise translated from exc
            raise ManagerError(str(exc)) from exc
        if not isinstance(result, dict):
            raise ManagerError("Mutation response must be a dict")
        return result

    async def create(
        self,
        *,
        instance: TModel | None = None,
        using: str | None = None,
        client: SupportsExecute | None = None,
        _skip_hooks: bool = False,
        **data: Any,
    ) -> TModel:
        """
        Create a new record in the database.

        Args:
            instance: Model instance to insert and populate in place (alternative to **data)
            using: Database alias
            client: Optional database client
            _skip_hooks: Skip pre_save/post_save hooks
            **data: Field values for the new record

        Returns:
            Created model instance with populated PK

        Examples:
            user = await User.objects.create(name="Alice", email="alice@example.com")
            # Or with instance:
            user = User(name="Alice")
            user = await User.objects.create(instance=user)
        """
        if instance is not None and data:
            raise ManagerError(
                "create() accepts either 'instance' or field values, not both"
            )
        in_place = instance is not None
        if instance is None:
            if not data:
                raise ManagerError("create() requires an instance or field values")
            instance = self.model_class(**data)

        # Call pre_save hook
        if not _skip_hooks:
            await instance.pre_save(is_create=True, update_fields=None)

        instance, _ = await self._insert_instance(
            instance, using=using, client=client, in_place=in_place
        )

        if not _skip_hooks:
            await instance.post_save(is_create=True, update_fields=None)

        return instance

    async def _insert_instance(
        self,
        instance: TModel,
        *,
        using: str | None,
        client: SupportsExecute | None,
        in_place: bool,
        upsert: bool = False,
        update_fields: set[str] | None = None,
    ) -> tuple[TModel, bool]:
        """Insert an instance and report whether a new row was created."""
        exec_client = await _resolve_execution_client(using, client)
        payload = _dump_insert_data(instance)
        pk_field = self._primary_key_field()
        if upsert:
            assert pk_field is not None
            if payload.get(pk_field) is None:
                raise ManagerError(
                    "pre_save cleared the primary key for a default-key upsert"
                )
        if not payload:
            raise ManagerError("create() requires at least one value")
        query = InsertQuery(self.model_class).values(**payload)
        inserted_alias: str | None = None
        if upsert:
            assert pk_field is not None
            if update_fields is None:
                excluded = [field for field in payload if field != pk_field]
                explicit = {}
            else:
                requested = update_fields - {pk_field}
                excluded = sorted(requested & payload.keys())
                explicit = _dump_update_data(instance, requested - payload.keys())
            if excluded or explicit:
                query = query.on_conflict_update(excluded, values=explicit)
                assert query._on_conflict is not None
                inserted_alias = query._on_conflict["inserted_alias"]
            else:
                query = query.on_conflict_do_nothing()
        result = await self._run_mutation(query, exec_client)

        inserted = inserted_alias is None
        hydrated = None
        rows = result.get("rows")
        inserted_ids = result.get("inserted_ids", [])
        with _track_private_assignments() as (assigned, snapshots):
            if rows:
                columns = list(result.get("columns", []))
                row = list(rows[0])
                if columns and columns[-1] == inserted_alias:
                    # The SQL builder appends the outcome flag after RETURNING *,
                    # so read the last column even if the table has an unmapped
                    # column of the same name earlier in the row.
                    inserted = bool(row.pop())
                    columns.pop()
                rmap = self.model_class._db_meta.reverse_column_map
                row_dict = {
                    rmap.get(col, col): value for col, value in zip(columns, row)
                }
                hydrated = _validate_db_row(
                    self.model_class, row_dict, track_private=in_place
                )
            elif (
                inserted_alias is None
                and pk_field
                and inserted_ids
                and inserted_ids[0] is not None
            ):
                row_dict = _dump_db_fields(instance)
                row_dict[pk_field] = inserted_ids[0]
                hydrated = _validate_db_row(
                    self.model_class, row_dict, track_private=in_place
                )
        assigned_private = {name for obj, name in assigned if obj is hydrated}
        if upsert and hydrated is None:
            # No row came back: either the key already exists or a trigger
            # suppressed the insert. Persistence is unconfirmed, so the default
            # key stays retryable; the next full save() upserts either way.
            inserted = False
            if in_place:
                assert pk_field is not None
                instance.__pydantic_fields_set__.discard(pk_field)

        if hydrated is not None:
            if in_place:
                baseline = snapshots.get(id(hydrated), {})
                changed_private = _changed_private_containers(hydrated, baseline)
                _refresh_in_place(
                    instance, hydrated, assigned_private | changed_private
                )
            else:
                instance = hydrated

        return instance, inserted

    async def bulk_create(
        self,
        objects: Iterable[TModel | dict[str, Any]],
        *,
        using: str | None = None,
        client: SupportsExecute | None = None,
        batch_size: int | None = None,
    ) -> list[TModel]:
        """
        Insert multiple objects efficiently.

        Args:
            objects: Iterable of model instances or dicts
            using: Database alias
            client: Optional database client
            batch_size: Optional batch size. If None, inserts all in one query.
                Use if hitting PostgreSQL parameter limits (65535).

        Returns:
            List of created model instances
        """
        instances = [_normalize_instance(self.model_class, obj) for obj in objects]
        if not instances:
            return []

        exec_client = await _resolve_execution_client(using, client)

        # Determine batches: all at once or user-specified batch_size
        if batch_size is not None and batch_size <= 0:
            raise ValueError("batch_size must be a positive integer")
        if batch_size is None:
            batches = [instances]
        else:
            batches = [
                instances[i : i + batch_size]
                for i in range(0, len(instances), batch_size)
            ]

        pk_field = self._primary_key_field()
        for batch in batches:
            entries = []
            for instance in batch:
                payload = _dump_insert_data(instance)
                if not payload:
                    raise ManagerError("bulk_create() encountered an empty payload")
                entries.append((instance, payload))

            # Each SQL statement has one column list. Keep generated keys in
            # separate runs so they cannot match another input's submitted key.
            for (_, has_keys), group in groupby(
                entries,
                key=lambda entry: (
                    frozenset(entry[1]),
                    pk_field is not None and entry[1].get(pk_field) is not None,
                ),
            ):
                instances_in_run, payloads = zip(*group)
                query = InsertQuery(self.model_class).bulk_values(list(payloads))
                result = await self._run_mutation(query, exec_client)
                if pk_field is None:
                    continue

                ids = result.get("inserted_ids", [])
                count = len(payloads)
                affected = result.get("affected", len(ids))
                if has_keys:
                    if len(ids) == 0 and affected == count:
                        # Some clients only report a row count.
                        for instance in instances_in_run:
                            instance._mark_pk_persisted()
                    else:
                        submitted = [
                            msgpack.packb(serialize_value(payload[pk_field]))
                            for payload in payloads
                        ]
                        sent = Counter(submitted)
                        returned = Counter(
                            msgpack.packb(serialize_value(key))
                            for key in ids
                            if key is not None
                        )
                        for instance, key in zip(instances_in_run, submitted):
                            if sent[key] == returned[key] == 1:
                                instance._mark_pk_persisted()
                elif affected == len(ids) == count and all(
                    key is not None for key in ids
                ):
                    pk_adapter = TypeAdapter(
                        self.model_class.model_fields[pk_field].annotation or Any
                    )
                    keys = [pk_adapter.validate_python(key) for key in ids]
                    for instance, key in zip(instances_in_run, keys):
                        setattr(instance, pk_field, key)
                elif affected or ids:
                    warnings.warn(
                        f"bulk_create could not match generated keys to {count} inputs",
                        RuntimeWarning,
                        stacklevel=2,
                    )

        return instances

    async def bulk_update(
        self,
        objects: Iterable[TModel],
        fields: Iterable[str],
        *,
        using: str | None = None,
        client: SupportsExecute | None = None,
    ) -> int:
        """
        Bulk update multiple objects with CASE WHEN.

        Args:
            objects: Instances to update
            fields: Field names to update
            using: Database alias
            client: Optional database client

        Returns:
            Number of affected rows

        Examples:
            users = await User.objects.filter(is_active=True).all()
            for user in users:
                user.login_count += 1
            await User.objects.bulk_update(users, ["login_count"])
        """
        objects_list = list(objects)
        if not objects_list:
            return 0

        fields_list = list(fields)
        if not fields_list:
            raise ManagerError("bulk_update() requires at least one field")

        pk_field = self._primary_key_field()
        if not pk_field:
            raise ManagerError("bulk_update() requires a primary key")

        pk_column = self.model_class._db_meta.field_metadata[pk_field].db_column

        exec_client = await _resolve_execution_client(using, client)

        # Build bulk_update payload
        bulk_entries: list[dict[str, Any]] = []
        for obj in objects_list:
            pk_value = getattr(obj, pk_field)
            if pk_value is None:
                continue

            all_values = _dump_db_fields(obj)

            # Extract requested fields and map to db_column names
            values = {
                field_name: _serialize_value_for_ir(all_values[field_name])
                for field_name in fields_list
                if field_name in all_values
            }
            mapped_values = _map_values_to_columns(self.model_class, values)

            if mapped_values:
                bulk_entries.append(
                    {
                        "filters": {pk_column: pk_value},
                        "values": mapped_values,
                    }
                )

        if not bulk_entries:
            return 0

        column_types = _build_column_types(self.model_class)
        update_ir = ir.build_update_ir(
            table=self.model_class.get_table_name(),
            bulk_update=bulk_entries,
            column_types=column_types,
            bind_types=_build_bind_types(self.model_class),
            model=_model_key(self.model_class),
        )

        result_bytes = await exec_client.execute(update_ir)
        result = msgpack.unpackb(result_bytes, raw=False)
        return int(result.get("affected", 0))
