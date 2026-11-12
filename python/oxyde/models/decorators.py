"""Table-level constraint definitions: Index and Check.

This module defines dataclasses for table-level constraints that cannot be
expressed at the field level. Use these in the model's Meta class.

When to use:
    Field-level (use Field()):
        - Single-column indexes: Field(db_index=True)
        - Single-column unique: Field(db_unique=True)
        - Primary key: Field(db_pk=True)

    Table-level (use this module):
        - Composite indexes: Index(("col1", "col2"))
        - Partial indexes: Index(("email",), where="deleted_at IS NULL")
        - Custom index methods: Index(("data",), method="gin")
        - Expression indexes: Index((Lower("email"),), name="user_email_lower_idx")
        - Operator classes: Index(("name",), opclasses=("text_pattern_ops",))
        - CHECK constraints: Check("price > 0")

Classes:
    Index: Composite/partial/expression index definition.
        fields: Column names, F()/expression objects (Lower, Upper, Coalesce,
            Greatest, RawSQL, ...) as tuple
        method: btree | hash | gin | gist | spgist | brin (default: btree)
        name: Custom index name (auto-generated if None; required when any
            field is an expression). PostgreSQL truncates names to 63 bytes;
            names that collide after truncation are rejected.
        unique: Create UNIQUE index
        nulls_distinct: False treats NULLs as equal (requires unique=True)
        where: Partial index condition (SQL fragment)
        opclasses: Operator class per field (None entries use the default)

    Check: CHECK constraint definition.
        expression: SQL boolean expression
        name: Constraint name (auto-generated if None)

Example:
    class Order(Model):
        user_id: int
        created_at: datetime
        total: Decimal
        status: str

        class Meta:
            is_table = True
            indexes = [
                Index(("user_id", "created_at")),
                Index(("status",), where="status != 'archived'"),
            ]
            constraints = [
                Check("total >= 0", name="positive_total"),
            ]
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import TYPE_CHECKING, Literal

if TYPE_CHECKING:
    # Imported lazily at runtime: oxyde.queries.expressions imports
    # oxyde.models.metadata, so a module-level import here would be circular.
    from oxyde.queries.expressions import F, _Expression

IndexMethod = Literal["btree", "hash", "gin", "gist", "spgist", "brin"]


@dataclass
class Index:
    """Index definition for Meta.indexes (table-level).

    Used for composite, partial and expression indexes.
    For single-field indexes, use Field(db_index=True) instead.

    A field is a column name, or an expression object such as ``Lower("email")``
    or ``Coalesce("qty", 1)``. A bare ``F("col")`` is the same as ``"col"``.
    Names inside expressions are field names and are mapped to their
    ``db_column``. An index with an expression field needs an explicit ``name``.

    ``opclasses`` gives an operator class per field (``None`` keeps the
    default), e.g. ``text_pattern_ops`` or ``gin_trgm_ops``. Extension-provided
    classes need the extension created first, e.g. with
    ``ctx.execute("CREATE EXTENSION IF NOT EXISTS pg_trgm")`` in an earlier
    migration.

    Examples:
        >>> class User(Model):
        ...     class Meta:
        ...         indexes = [
        ...             Index(("city", "created_at"), method="btree"),
        ...             Index(("email",), unique=True, where="deleted_at IS NULL"),
        ...             Index(("email",), unique=True, nulls_distinct=False),
        ...             Index((Lower("email"),), unique=True, name="user_email_lower"),
        ...             Index(("name",), opclasses=("text_pattern_ops",)),
        ...         ]
    """

    fields: tuple[str | F | _Expression, ...]
    method: IndexMethod | None = None
    name: str | None = None
    unique: bool = False
    where: str | None = None  # Partial index condition
    nulls_distinct: bool | None = None
    opclasses: tuple[str | None, ...] | None = None

    def __init__(
        self,
        fields: tuple[str | F | _Expression, ...] | list[str | F | _Expression],
        method: IndexMethod | None = None,
        name: str | None = None,
        unique: bool = False,
        where: str | None = None,
        nulls_distinct: bool | None = None,
        opclasses: tuple[str | None, ...] | list[str | None] | None = None,
    ):
        from oxyde.queries.expressions import F, _Expression

        if not fields:
            raise ValueError("Index requires at least one field")
        if nulls_distinct is False and not unique:
            raise ValueError("nulls_distinct requires unique=True")

        keys: list[str | _Expression] = []
        for key in fields:
            if isinstance(key, F):
                key = key.name
            if not isinstance(key, (str, _Expression)):
                raise TypeError(
                    "Index fields must be column names or expressions "
                    f"(e.g. Lower('email')), got {type(key).__name__}"
                )
            keys.append(key)

        if any(isinstance(key, _Expression) for key in keys):
            if not name:
                raise ValueError(
                    "An index with an expression field needs an explicit name="
                )

        if opclasses is not None:
            opclasses = tuple(opclasses)
            if len(opclasses) != len(keys):
                raise ValueError(
                    f"Index opclasses has {len(opclasses)} entries for "
                    f"{len(keys)} fields; give one per field (None for the default)"
                )
            for opclass in opclasses:
                if opclass is not None and not isinstance(opclass, str):
                    raise TypeError(
                        "Index opclasses entries must be str or None, "
                        f"got {type(opclass).__name__}"
                    )

        self.fields = tuple(keys)
        self.method = method
        self.name = name
        self.unique = unique
        self.nulls_distinct = nulls_distinct
        self.opclasses = opclasses
        if where is not None:
            where = where.strip() or None
        self.where = where


@dataclass
class Check:
    """Check constraint definition for Meta.constraints (table-level).

    Examples:
        >>> class Event(Model):
        ...     start_date: datetime
        ...     end_date: datetime
        ...     class Meta:
        ...         constraints = [
        ...             Check("start_date < end_date", name="valid_dates"),
        ...         ]
    """

    expression: str
    name: str | None = None


__all__ = ["Index", "Check"]
