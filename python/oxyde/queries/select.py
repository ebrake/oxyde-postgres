"""SELECT query builder with Django-style fluent interface.

This module provides the Query class - the primary way to build SELECT queries
in Oxyde. Query objects are immutable: each method returns a new Query instance.

Architecture:
    Query inherits from multiple mixins that provide different capabilities:
        FilteringMixin: filter(), exclude()
        PaginationMixin: limit(), offset(), order_by(), distinct(), values()
        JoiningMixin: join(), prefetch()
        AggregationMixin: annotate(), group_by(), having(), count(), sum()
        ExecutionMixin: fetch_all(), fetch_models(), exists(), all()
        MutationMixin: update(), delete(), increment()
        DebugMixin: sql(), explain(), union()

Immutability:
    All methods return new Query instances via _clone(). The original is unchanged.

    base = User.objects.filter(active=True)
    admins = base.filter(role="admin")  # base is unchanged
    users = base.filter(role="user")    # base is unchanged

State Attributes:
    _filter_tree: FilterNode tree for WHERE clause
    _limit_value: LIMIT value
    _offset_value: OFFSET value
    _order_by_fields: List of (field, direction) tuples
    _selected_fields: Explicit field list or None for all
    _distinct: DISTINCT flag
    _join_specs: JOIN descriptors
    _annotations: Annotation aliases {alias: Aggregate | scalar expression}
    _group_by_fields: GROUP BY columns
    _lock_type: "update" | "share" | None for row locking

Execution:
    to_ir() converts Query state to dict (Intermediate Representation).
    IR is serialized to MessagePack and sent to Rust for SQL generation.

    The IR includes column_types (ColumnTypeSpec dicts per column)
    which enables type-aware decoding in Rust without expensive type_info() calls.

Example:
    query = (
        User.objects
        .filter(status="active")
        .exclude(role="bot")
        .order_by("-created_at")
        .limit(10)
    )
    ir = query.to_ir()
    # {"table": "user", "columns": [...], "col_types": {"id": "int", ...}, ...}
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any, Generic, Self, TypeVar

from oxyde.core import ir
from oxyde.models.lookups import _resolve_related_field
from oxyde.models.utils import _unwrap_optional
from oxyde.queries.base import _model_key
from oxyde.queries.expressions import (
    F,
    _is_scalar_annotation,
    _map_expression_columns,
    _resolve_column_name,
)
from oxyde.queries.joins import _JoinDescriptor
from oxyde.queries.mixins import (
    AggregationMixin,
    DebugMixin,
    ExecutionMixin,
    FilteringMixin,
    JoiningMixin,
    MutationMixin,
    PaginationMixin,
)
from oxyde.queries.mixins.joining import _POSTGRES_IDENTIFIER_BYTES

if TYPE_CHECKING:
    from oxyde.models.base import Model


TModel = TypeVar("TModel", bound="Model")


class Query(
    FilteringMixin[TModel],
    PaginationMixin[TModel],
    JoiningMixin[TModel],
    AggregationMixin[TModel],
    ExecutionMixin[TModel],
    MutationMixin[TModel],
    DebugMixin[TModel],
    Generic[TModel],
):
    """
    Query builder for SELECT operations.

    Combines all query capabilities through mixins:
    - FilteringMixin: filter, exclude
    - PaginationMixin: limit, offset, order_by, distinct, values, values_list
    - JoiningMixin: join, prefetch
    - AggregationMixin: annotate, group_by, having, count, sum, avg, max, min
    - ExecutionMixin: fetch_all, fetch_one, fetch_models, exists, all
    - MutationMixin: update, delete, increment
    - DebugMixin: sql, query, explain, union, union_all
    """

    def __init__(self, model_class: type[TModel]):
        self.model_class = model_class
        # Filtering state
        self._filter_tree: ir.FilterNode | None = None
        # Pagination state
        self._limit_value: int | None = None
        self._offset_value: int | None = None
        self._order_by_fields: list[tuple[str, str]] = []
        self._selected_fields: list[str] | None = None
        self._distinct: bool = False
        self._result_mode: str | None = None
        self._values_flat: bool = False
        # Joining state
        self._join_specs: list[_JoinDescriptor] = []
        self._prefetch_paths: list[str] = []
        # Aggregation state
        self._annotations: dict[str, Any] = {}
        self._group_by_fields: list[str] = []
        self._having: ir.FilterNode | None = None
        # Union state
        self._union_query: Query | None = None
        self._union_all: bool = False
        # Locking state (FOR UPDATE / FOR SHARE)
        self._lock_type: str | None = None
        # Shortcut query flags (count / exists)
        self._count: bool = False
        self._exists: bool = False

    def _clone(self) -> Self:
        """Create a copy of this query."""
        clone = self.__class__(self.model_class)
        # Filtering
        clone._filter_tree = self._filter_tree
        # Pagination
        clone._limit_value = self._limit_value
        clone._offset_value = self._offset_value
        clone._order_by_fields = list(self._order_by_fields)
        clone._selected_fields = (
            None if self._selected_fields is None else list(self._selected_fields)
        )
        clone._distinct = self._distinct
        clone._result_mode = self._result_mode
        clone._values_flat = self._values_flat
        # Joining
        clone._join_specs = list(self._join_specs)
        clone._prefetch_paths = list(self._prefetch_paths)
        # Aggregation
        clone._annotations = dict(self._annotations)
        clone._group_by_fields = list(self._group_by_fields)
        clone._having = self._having
        # Union
        clone._union_query = self._union_query
        clone._union_all = self._union_all
        # Locking
        clone._lock_type = self._lock_type
        # Shortcut flags
        clone._count = self._count
        clone._exists = self._exists
        return clone

    def select(self, *fields: str) -> Self:
        """Specify fields to select."""
        if not fields:
            raise ValueError("select() requires at least one column name")
        clone = self._clone()
        clone._selected_fields = list(fields)
        return clone

    def for_update(self) -> Self:
        """Add FOR UPDATE lock to query.

        Locks selected rows for update, preventing other transactions
        from modifying them until the current transaction commits.


        Returns:
            Query with FOR UPDATE lock

        Examples:
            async with atomic():
                user = await User.objects.filter(id=1).for_update().first()
                user.balance -= 100
                await user.save()
        """
        clone = self._clone()
        clone._lock_type = "update"
        return clone

    def for_share(self) -> Self:
        """Add FOR SHARE lock to query.

        Locks selected rows for reading, preventing other transactions
        from modifying them (but allowing reads) until the current
        transaction commits.


        Returns:
            Query with FOR SHARE lock

        Examples:
            async with atomic():
                users = await User.objects.filter(status="active").for_share().all()
                # Other transactions can read but not modify these rows
        """
        clone = self._clone()
        clone._lock_type = "share"
        return clone

    def _base_ir_ref(self, column: str) -> str:
        table = self.model_class.get_table_name()
        if column.startswith(f"{table}."):
            return f"{table}.{column}"
        return column

    def _ir_ref(self, field: str, join_specs: dict[str, _JoinDescriptor]) -> str:
        related = _resolve_related_field(self.model_class, field)
        if related is None:
            return self._base_ir_ref(self._column_for_field(field))
        path = "__".join(name for name, _ in related.joins)
        return f"{join_specs[path].alias}.{related.column_meta.db_column}"

    def _grouped_ir_ref(
        self, field: str, join_specs: dict[str, _JoinDescriptor]
    ) -> str:
        ref = self._ir_ref(field, join_specs)
        if (
            ref in self._annotations
            and _resolve_related_field(self.model_class, field) is None
            and ref == self._column_for_field(field)
        ):
            return f"{self.model_class.get_table_name()}.{ref}"
        return ref

    def _output_label(self, field: str) -> str:
        """Result label of a grouped query's projected field: the relation
        path for a related field, the field name for a base column."""
        if _resolve_related_field(self.model_class, field) is not None:
            return field
        if field in self.model_class._db_meta.field_metadata:
            return field
        rmap = self.model_class._db_meta.reverse_column_map or {}
        return rmap.get(field, field)

    def _is_grouped(self) -> bool:
        """Whether the query aggregates or groups, so SQL returns grouped rows."""
        return bool(self._group_by_fields or self._aggregate_annotations())

    def _implicit_join_fields(self) -> list[str]:
        """Fields whose relation paths the query joins without an explicit
        join(): group keys, aggregate inputs and ordering fields. Annotation
        aliases and random ordering are not fields."""
        fields = [
            field
            for field in self._group_by_fields
            if not _is_scalar_annotation(self._annotations.get(field))
        ]
        for agg_obj in self._aggregate_annotations().values():
            if hasattr(agg_obj, "to_ir"):
                input_field = agg_obj.to_ir().get("field")
                if input_field is not None:
                    fields.append(input_field)
        fields.extend(
            field
            for field, _direction in self._order_by_fields
            if field != "?" and field not in self._annotations
        )
        return fields

    def _own_join_specs(self) -> dict[str, _JoinDescriptor]:
        """Explicit and filter joins, plus the joins that group keys,
        aggregate inputs and ordering fields need."""
        specs = {descriptor.path: descriptor for descriptor in self._join_specs}
        for field in self._implicit_join_fields():
            related = _resolve_related_field(self.model_class, field)
            if related is None:
                continue
            path = "__".join(name for name, _ in related.joins)
            if path not in specs:
                self._compute_join_descriptors(path, specs)
        return specs

    def _union_join_paths(self) -> tuple[str, ...]:
        """Collect shared relation paths in projection order for a UNION."""
        paths: dict[str, None] = {}
        branch: Query[Any] | None = self
        while branch is not None:
            if branch.model_class is not self.model_class or branch._is_grouped():
                return ()
            paths.update(dict.fromkeys(branch._own_join_specs()))
            branch = branch._union_query
        return tuple(paths)

    def _planned_join_specs(
        self, paths: tuple[str, ...] | None = None
    ) -> dict[str, _JoinDescriptor]:
        specs = self._own_join_specs()
        if paths is None and self._union_query is not None:
            paths = self._union_join_paths()
        if not paths:
            return specs
        # Existing descriptors may already be referenced by a filter.
        for path in paths:
            if path not in specs:
                self._compute_join_descriptors(path, specs)
        return {path: specs[path] for path in paths}

    def _projection_fields(self) -> list[str]:
        if self._selected_fields is not None:
            if not self._selected_fields:
                raise ValueError("SELECT query must include at least one column")
            return [
                name
                for name in self._selected_fields
                if not _is_scalar_annotation(self._annotations.get(name))
            ]

        from oxyde.models.base import Model

        fields = []
        for field_name, field_info in self.model_class.model_fields.items():
            if getattr(field_info, "db_reverse_fk", None) or getattr(
                field_info, "db_m2m", False
            ):
                continue
            # Select the physical FK column instead of the relation field.
            if field_info.annotation is not None:
                inner_type, _ = _unwrap_optional(field_info.annotation)
                if isinstance(inner_type, type) and issubclass(inner_type, Model):
                    continue
            fields.append(field_name)
        return fields

    def _grouped_labels(
        self, join_specs: dict[str, _JoinDescriptor]
    ) -> tuple[dict[str, str], dict[str, str]]:
        """Return SQL aliases by column reference and original long labels by alias."""
        labels: dict[str, str] = {}
        for field in self._group_by_fields:
            if _is_scalar_annotation(self._annotations.get(field)):
                if (
                    self._selected_fields is not None
                    and field not in self._selected_fields
                ):
                    raise ValueError(
                        f"group_by({field!r}) refers to an annotation that is not "
                        "selected; include it in values()/values_list()"
                    )
                continue
            label = self._output_label(field)
            if label in self._annotations:
                raise ValueError(
                    f"Annotation alias {label!r} duplicates a group_by() column; "
                    "choose another name"
                )
            labels.setdefault(self._grouped_ir_ref(field, join_specs), label)
        if not self._aggregate_annotations():
            for field in self._projection_fields():
                labels.setdefault(
                    self._grouped_ir_ref(field, join_specs), self._output_label(field)
                )

        taken = set(labels.values()) | self._annotations.keys()
        restored: dict[str, str] = {}
        alias_number = 0
        for ref, label in labels.items():
            if len(label.encode("utf-8")) <= _POSTGRES_IDENTIFIER_BYTES:
                continue
            alias = f"_oxyde_k{alias_number}"
            while alias in taken:
                alias_number += 1
                alias = f"_oxyde_k{alias_number}"
            taken.add(alias)
            labels[ref] = alias
            restored[alias] = label
        return labels, restored

    def _column_types_for_query(
        self, join_specs: list[_JoinDescriptor]
    ) -> dict[str, dict] | None:
        """Return base and joined column type specs keyed by SQL/result column names."""
        base_types = self.model_class._db_meta.column_types or {}
        column_types: dict[str, dict] = dict(base_types)

        for descriptor in join_specs:
            target_types = descriptor.target_model._db_meta.column_types or {}
            for _field, column in descriptor.columns:
                spec = target_types.get(column)
                if spec is None:
                    continue
                column_types[f"{descriptor.alias}.{column}"] = spec

        return column_types or None

    def _bind_types_for_query(
        self, join_specs: list[_JoinDescriptor]
    ) -> dict[str, dict] | None:
        """Return base and joined binding-only type hints keyed by SQL column
        reference ("alias.column" for joined columns, as filters name them)."""
        bind_types = dict(self.model_class._db_meta.bind_types or {})

        for descriptor in join_specs:
            target_hints = descriptor.target_model._db_meta.bind_types or {}
            for _field, column in descriptor.columns:
                hint = target_hints.get(column)
                if hint is not None:
                    bind_types[f"{descriptor.alias}.{column}"] = hint

        return bind_types or None

    def _column_db_types_for_query(
        self, join_specs: list[_JoinDescriptor]
    ) -> dict[str, str] | None:
        """Return base and joined db_type overrides keyed by SQL column
        reference ("alias.column" for joined columns, as filters name them)."""
        column_db_types = dict(self.model_class._db_meta.column_db_types or {})

        for descriptor in join_specs:
            target_db_types = descriptor.target_model._db_meta.column_db_types or {}
            for _field, column in descriptor.columns:
                db_type = target_db_types.get(column)
                if db_type is not None:
                    column_db_types[f"{descriptor.alias}.{column}"] = db_type

        return column_db_types or None

    def to_ir(self) -> dict[str, Any]:
        """Convert query to IR format for Rust execution."""
        return self._build_ir(for_models=self._result_mode is None)

    def _can_append_pk(self, pk_column: str) -> bool:
        """Check whether every UNION branch can add the same key column.

        Annotated branches are ineligible: aggregates replace the selected
        columns in SQL, so only some branches would gain the key.
        """
        branch: Query[Any] | None = self
        while branch is not None:
            if (
                branch.model_class is not self.model_class
                or branch._selected_fields is None
                or branch._group_by_fields
                or branch._annotations
                or any(
                    branch._column_for_field(field) == pk_column
                    for field in branch._selected_fields
                )
            ):
                return False
            branch = branch._union_query
        return True

    def _union_projects_joins(
        self, for_models: bool, *, append_pk: bool = False
    ) -> bool:
        """Check whether every UNION branch can project its joined columns."""
        branch: Query[Any] | None = self
        while branch is not None:
            if branch._selected_fields is not None:
                if not for_models:
                    return False
                # Forward FK joins add only one set of values per base key.
                if (
                    branch._distinct
                    and not append_pk
                    and not any(
                        branch._column_for_field(field)
                        == branch.model_class._db_meta.pk_column
                        for field in branch._selected_fields
                    )
                ):
                    return False
            branch = branch._union_query
        return True

    def _aggregate_annotations(self) -> dict[str, Any]:
        return {
            alias: obj
            for alias, obj in self._annotations.items()
            if not _is_scalar_annotation(obj)
        }

    def _has_aggregate_annotations(self) -> bool:
        branch: Query[Any] | None = self
        while branch is not None:
            if branch._aggregate_annotations():
                return True
            branch = branch._union_query
        return False

    def _build_ir(
        self,
        *,
        for_models: bool,
        append_pk: bool | None = None,
        project_joins: bool | None = None,
        union_join_paths: tuple[str, ...] | None = None,
    ) -> dict[str, Any]:
        table_name = self.model_class.get_table_name()

        # Scalar annotations (Coalesce/Greatest/F-arithmetic) are emitted through
        # `annotations`, not `cols`; aggregates go through `aggregates`.
        scalar_annotations = {
            alias: obj
            for alias, obj in self._annotations.items()
            if _is_scalar_annotation(obj)
        }
        aggregate_annotations = self._aggregate_annotations()
        grouped = self._is_grouped()
        if scalar_annotations and not grouped:
            # Ordinary projections still use physical column labels.
            for meta in self.model_class._db_meta.field_metadata.values():
                if meta.db_column in scalar_annotations:
                    raise ValueError(
                        f"annotate() alias {meta.db_column!r} conflicts with a model "
                        "column; choose another name"
                    )

        fields = self._projection_fields()
        pk_column = self.model_class._db_meta.pk_column
        if append_pk is None:
            append_pk = (
                for_models and pk_column is not None and self._can_append_pk(pk_column)
            )
        if project_joins is None:
            project_joins = self._union_projects_joins(for_models, append_pk=append_pk)
        if union_join_paths is None and self._union_query is not None:
            union_join_paths = self._union_join_paths() if project_joins else ()
        join_specs = self._planned_join_specs(union_join_paths)
        emitted_annotations = (
            list(scalar_annotations)
            if self._selected_fields is None
            else list(
                dict.fromkeys(
                    field
                    for field in self._selected_fields
                    if field in scalar_annotations
                )
            )
        )

        order_by = []
        for field, direction in self._order_by_fields:
            if field in scalar_annotations and field not in emitted_annotations:
                raise ValueError(
                    f"order_by({field!r}) refers to an annotation that is not "
                    "selected; include it in values()/values_list()"
                )
            if field == "?" or field in self._annotations:
                order_by.append((field, direction))
                continue

            related = _resolve_related_field(self.model_class, field)
            if related is None:
                column = self._column_for_field(field)
                ref = (
                    f"{table_name}.{column}"
                    if column in self._annotations
                    else self._base_ir_ref(column)
                )
            else:
                projected_group_key = field in self._group_by_fields and (
                    bool(aggregate_annotations) or field in fields
                )
                if (
                    not project_joins
                    and self._union_query is not None
                    and not projected_group_key
                ):
                    raise ValueError(
                        "ordering a union by a related field needs the default projection"
                    )
                if (
                    self._distinct
                    and not project_joins
                    and not grouped
                    and not self._count
                    and not self._exists
                ):
                    raise ValueError(
                        f"Cannot order by related field {field!r} with distinct() "
                        "and an explicit projection"
                    )
                path = "__".join(name for name, _ in related.joins)
                ref = f"{join_specs[path].alias}.{related.column_meta.db_column}"
            order_by.append((ref, direction))

        if grouped:
            for field in fields:
                if (
                    field not in self._group_by_fields
                    and field not in self._annotations
                    and _resolve_related_field(self.model_class, field) is not None
                ):
                    raise ValueError(
                        f"Related field {field!r} must be in group_by() to be "
                        "returned by an aggregate query"
                    )
            db_columns = [
                field
                if field in self._annotations
                else self._grouped_ir_ref(field, join_specs)
                for field in fields
            ]
        else:
            db_columns = [
                self._base_ir_ref(self._column_for_field(field)) for field in fields
            ]
        if append_pk and pk_column is not None:
            db_columns.append(self._base_ir_ref(pk_column))

        # Build filter_tree
        final_filter_tree = self._build_filter_tree()

        # Convert aggregate annotations to aggregates IR format
        aggregates_ir = None
        if aggregate_annotations:
            aggregates_ir = []
            for alias, agg_obj in aggregate_annotations.items():
                if not hasattr(agg_obj, "to_ir"):
                    raise TypeError(f"annotate() received non-expression: {agg_obj!r}")
                agg_spec = agg_obj.to_ir()
                func_name = agg_spec.get("func", "").lower()
                input_field = agg_spec.get("field")
                agg_ir: dict[str, Any] = {
                    "op": func_name,
                    "field": self._ir_ref(input_field, join_specs)
                    if input_field is not None
                    else None,
                    "alias": alias,
                }
                if agg_spec.get("distinct"):
                    agg_ir["distinct"] = True
                aggregates_ir.append(agg_ir)

        annotations_ir = None
        if emitted_annotations:
            annotations_ir = []
            for alias in emitted_annotations:
                obj = scalar_annotations[alias]
                node = obj._expression._expr if isinstance(obj, F) else obj._expr
                annotations_ir.append(
                    {
                        "alias": alias,
                        "expr": _map_expression_columns(
                            node,
                            lambda name: _resolve_column_name(name, self.model_class),
                        ),
                    }
                )

        group_by_columns = [
            field
            if _is_scalar_annotation(self._annotations.get(field))
            else self._grouped_ir_ref(field, join_specs)
            for field in self._group_by_fields
        ]
        output_labels = self._grouped_labels(join_specs)[0] if grouped else None
        descriptors = list(join_specs.values())

        # Build union IR recursively
        union_ir = None
        if self._union_query is not None:
            union_ir = self._union_query._build_ir(
                for_models=for_models,
                append_pk=append_pk,
                project_joins=project_joins,
                union_join_paths=union_join_paths,
            )

        return ir.build_select_ir(
            table=table_name,
            columns=db_columns,
            column_types=self._column_types_for_query(descriptors),
            bind_types=self._bind_types_for_query(descriptors),
            column_db_types=self._column_db_types_for_query(descriptors),
            model=_model_key(self.model_class),
            column_mappings=None,
            output_labels=output_labels or None,
            filter_tree=final_filter_tree,
            distinct=self._distinct or None,
            limit=self._limit_value,
            offset=self._offset_value,
            order_by=order_by or None,
            joins=self._join_specs_to_ir(descriptors) or None,
            project_joins=False if join_specs and not project_joins else None,
            group_by=group_by_columns or None,
            having=self._having,
            aggregates=aggregates_ir,
            annotations=annotations_ir,
            lock=self._lock_type,
            pk_column=pk_column if join_specs else None,
            union_query=union_ir,
            union_all=self._union_all or None,
            count=self._count or None,
            exists=self._exists or None,
        )


__all__ = ["Query"]
