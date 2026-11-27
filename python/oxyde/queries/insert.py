"""INSERT query builder for single and bulk inserts (internal).

InsertQuery is used internally by QueryManager for create() and bulk_create().
Use Model.objects.create() and Model.objects.bulk_create() instead.

Example (via Manager):
    user = await User.objects.create(name="Alice", age=30)
    users = await User.objects.bulk_create([
        User(name="Alice", age=30),
        User(name="Bob", age=25),
    ])
"""

from __future__ import annotations

from collections.abc import Iterable
from typing import TYPE_CHECKING, Any

from oxyde._msgpack import msgpack
from oxyde.core import ir
from oxyde.models.metadata import is_column_field
from oxyde.queries.base import (
    SupportsExecute,
    _build_bind_types,
    _build_column_types,
    _map_values_to_columns,
    _model_key,
    _primary_key_meta,
)
from oxyde.queries.expressions import _serialize_value_for_ir

if TYPE_CHECKING:
    from oxyde.models.base import Model


_INSERTED_ALIAS = "__oxyde_inserted"


class InsertQuery:
    """INSERT query builder."""

    def __init__(self, model_class: type[Model]):
        self.model_class = model_class
        self._values: dict[str, Any] = {}
        self._bulk_values: list[dict[str, Any]] | None = None
        self._on_conflict: dict[str, Any] | None = None

    def _clone(self) -> InsertQuery:
        clone = InsertQuery(self.model_class)
        clone._values = dict(self._values)
        clone._bulk_values = list(self._bulk_values) if self._bulk_values else None
        clone._on_conflict = self._on_conflict
        return clone

    def values(self, **kwargs: Any) -> InsertQuery:
        """Set values to insert."""
        clone = self._clone()
        clone._values = dict(kwargs)
        clone._bulk_values = None
        return clone

    def bulk_values(self, values: list[dict[str, Any]]) -> InsertQuery:
        """Set bulk values for batch insert."""
        clone = self._clone()
        clone._bulk_values = list(values)
        clone._values = {}
        return clone

    def on_conflict_update(
        self, update_fields: Iterable[str], values: dict[str, Any] | None = None
    ) -> InsertQuery:
        """On primary-key conflict, assign proposed columns and explicit values."""
        clone = self._clone()
        pk_column = _primary_key_meta(self.model_class).db_column
        update_columns = _map_values_to_columns(
            self.model_class, dict.fromkeys(update_fields)
        )
        table_columns = {
            meta.db_column
            for meta in self.model_class._db_meta.field_metadata.values()
            if is_column_field(meta)
        }
        alias = _INSERTED_ALIAS
        suffix = 0
        while alias in table_columns:
            suffix += 1
            alias = f"{_INSERTED_ALIAS}_{suffix}"
        clone._on_conflict = {
            "columns": [pk_column],
            "action": "update",
            "inserted_alias": alias,
        }
        if update_columns:
            clone._on_conflict["update_columns"] = list(update_columns)
        if values:
            clone._on_conflict["update_values"] = dict(values)
        return clone

    def on_conflict_do_nothing(self) -> InsertQuery:
        """Skip the insert when the primary key already exists."""
        clone = self._clone()
        pk_column = _primary_key_meta(self.model_class).db_column
        clone._on_conflict = {"columns": [pk_column], "action": "nothing"}
        return clone

    def _get_pk_column(self) -> str | None:
        """Get the database column name for the primary key field."""
        try:
            pk_meta = _primary_key_meta(self.model_class)
            return pk_meta.db_column
        except Exception:
            return None

    def _serialize_values(self, values: dict[str, Any]) -> dict[str, Any]:
        mapped = _map_values_to_columns(self.model_class, values)
        return {
            key: _serialize_value_for_ir(value, self.model_class)
            for key, value in mapped.items()
        }

    def to_ir(self) -> dict[str, Any]:
        """Convert to IR format."""
        table_name = self.model_class.get_table_name()
        pk_column = self._get_pk_column()
        column_types = _build_column_types(self.model_class)
        bind_types = _build_bind_types(self.model_class)

        on_conflict = self._on_conflict
        if on_conflict is not None and "update_values" in on_conflict:
            explicit = self._serialize_values(on_conflict["update_values"])
            overlap = explicit.keys() & on_conflict.get("update_columns", [])
            if overlap:
                columns = ", ".join(sorted(overlap))
                raise ValueError(
                    f"ON CONFLICT UPDATE has multiple assignments for: {columns}"
                )
            on_conflict = {
                **on_conflict,
                "update_values": explicit,
            }

        if self._bulk_values is not None:
            if not self._bulk_values:
                raise ValueError("Bulk INSERT requires at least one row")

            serialized_bulk = [self._serialize_values(row) for row in self._bulk_values]
            if on_conflict is not None:
                # The outcome flag needs full-row RETURNING, which bulk skips.
                on_conflict = {
                    key: value
                    for key, value in on_conflict.items()
                    if key != "inserted_alias"
                }

            # Bulk insert: only return PKs for efficiency
            return ir.build_insert_ir(
                table=table_name,
                bulk_values=serialized_bulk,
                column_types=column_types,
                bind_types=bind_types,
                model=_model_key(self.model_class),
                pk_column=pk_column,
                on_conflict=on_conflict,
            )
        else:
            if not self._values:
                raise ValueError("INSERT query requires at least one column/value pair")

            # Fetch the complete inserted row for models with a primary key.
            return ir.build_insert_ir(
                table=table_name,
                values=self._serialize_values(self._values),
                column_types=column_types,
                bind_types=bind_types,
                model=_model_key(self.model_class),
                pk_column=pk_column,
                returning=pk_column is not None,
                on_conflict=on_conflict,
            )

    async def execute(self, client: SupportsExecute) -> dict[str, Any]:
        """Execute insert query."""
        ir = self.to_ir()
        result_bytes = await client.execute(ir)
        result: dict[str, Any] = msgpack.unpackb(result_bytes, raw=False)
        return result


__all__ = ["InsertQuery"]
