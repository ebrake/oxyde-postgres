"""Joining mixin for query building."""

from __future__ import annotations

from collections.abc import Iterable
from typing import TYPE_CHECKING, Any, Generic, Self, TypeVar

from oxyde.exceptions import FieldLookupError
from oxyde.models.metadata import db_column_for
from oxyde.queries.base import (
    _collect_model_columns,
    _resolve_registered_model,
)
from oxyde.queries.joins import _JoinDescriptor

if TYPE_CHECKING:
    from oxyde.models.base import Model


TModel = TypeVar("TModel", bound="Model")
_POSTGRES_IDENTIFIER_BYTES = 63


def _truncate_identifier(name: str, max_bytes: int) -> str:
    return name.encode("utf-8")[:max_bytes].decode("utf-8", errors="ignore")


class JoiningMixin(Generic[TModel]):
    """Mixin providing join and prefetch capabilities."""

    # These attributes are defined in the base Query class
    model_class: type[TModel]
    _join_specs: list[_JoinDescriptor]
    _prefetch_paths: list[str]

    def _clone(self) -> Self:
        """Must be implemented by the main Query class."""
        raise NotImplementedError

    def join(self, *paths: str) -> Self:
        """
        Add LEFT JOIN for related models.

        Args:
            *paths: Relation paths (e.g., "author", "author__profile")

        Examples:
            Post.objects.join("author")
            Post.objects.join("author", "category")
        """
        if not paths:
            raise ValueError("join() requires at least one relation path")
        clone = self._clone()
        for path in paths:
            clone._add_join_path(path)
        return clone

    def prefetch(self, *paths: str) -> Self:
        """
        Prefetch related objects (separate queries).

        Args:
            *paths: Relation paths for one-to-many relations

        Examples:
            Author.objects.prefetch("posts")
        """
        if not paths:
            raise ValueError("prefetch() requires at least one relation path")
        clone = self._clone()
        for path in paths:
            if path not in clone._prefetch_paths:
                clone._prefetch_paths.append(path)
        return clone

    def _column_for_field(self, field: str) -> str:
        """Get database column name for a field."""
        return db_column_for(self.model_class, field)

    def _column_mappings_for_fields(self, fields: Iterable[str]) -> dict[str, str]:
        """Get column mappings for fields where column differs from field name."""
        mappings: dict[str, str] = {}
        for field in fields:
            column = self._column_for_field(field)
            if column != field:
                mappings[field] = column
        return mappings

    def _add_join_path(self, path: str) -> _JoinDescriptor:
        """Add join descriptors and return the descriptor for the full path."""
        existing = {spec.path: spec for spec in self._join_specs}
        self._join_specs.extend(self._compute_join_descriptors(path, existing))
        return existing[path]

    def _compute_join_descriptors(
        self, path: str, existing: dict[str, _JoinDescriptor]
    ) -> list[_JoinDescriptor]:
        """Plan missing joins, preserving aliases assigned to existing paths."""
        if not path:
            raise ValueError("join() path must be non-empty")
        taken = {spec.alias for spec in existing.values()}
        taken.add(
            _truncate_identifier(
                self.model_class.get_table_name(), _POSTGRES_IDENTIFIER_BYTES
            )
        )
        # Reserve dotted base-column prefixes so alias.column can only name
        # a joined column when the prefix is actually a join alias.
        taken.update(
            column.split(".", 1)[0]
            for _, column in _collect_model_columns(self.model_class)
            if "." in column
        )

        current_model: type[Model] = self.model_class
        parent: _JoinDescriptor | None = None
        descriptors: list[_JoinDescriptor] = []
        prefix: list[str] = []

        for segment in path.split("__"):
            prefix.append(segment)
            path_key = "__".join(prefix)
            descriptor = existing.get(path_key)
            if descriptor is None:
                column_meta = current_model._db_meta.field_metadata.get(segment)
                if column_meta is None or column_meta.foreign_key is None:
                    raise FieldLookupError(
                        f"{current_model.__name__}.{segment} is not a joinable relation"
                    )
                foreign_key = column_meta.foreign_key
                target_model = _resolve_registered_model(foreign_key.target)
                alias = _truncate_identifier(path_key, _POSTGRES_IDENTIFIER_BYTES)
                suffix_number = 2
                while alias in taken:
                    suffix = f"_{suffix_number}"
                    alias = (
                        _truncate_identifier(
                            path_key, _POSTGRES_IDENTIFIER_BYTES - len(suffix)
                        )
                        + suffix
                    )
                    suffix_number += 1
                taken.add(alias)
                descriptor = _JoinDescriptor(
                    path=path_key,
                    alias=alias,
                    result_prefix=path_key,
                    parent_path=parent.path if parent is not None else None,
                    parent_alias=parent.alias if parent is not None else None,
                    attr_name=segment,
                    source_column=foreign_key.column_name,
                    target_column=db_column_for(target_model, foreign_key.target_field),
                    columns=_collect_model_columns(target_model),
                    target_model=target_model,
                    parent_model=current_model,
                    nullable=column_meta.nullable,
                )
                existing[path_key] = descriptor
                descriptors.append(descriptor)
            current_model = descriptor.target_model
            parent = descriptor

        return descriptors

    def _join_specs_to_ir(
        self, join_specs: list[_JoinDescriptor]
    ) -> list[dict[str, Any]]:
        """Convert join specs to IR format."""
        specs: list[dict[str, Any]] = []
        for descriptor in join_specs:
            spec = {
                "path": descriptor.path,
                "alias": descriptor.alias,
                "parent": descriptor.parent_alias,
                "table": descriptor.target_model.get_table_name(),
                "source_column": descriptor.source_column,
                "target_column": descriptor.target_column,
                "result_prefix": descriptor.result_prefix,
                "columns": [
                    {"field": field, "column": column}
                    for field, column in descriptor.columns
                ],
            }
            if (pk_column := descriptor.target_model._db_meta.pk_column) is not None:
                spec["pk_column"] = pk_column
            specs.append(spec)
        return specs
