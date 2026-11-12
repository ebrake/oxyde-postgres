"""SQL aggregate and scalar functions for SELECT queries.

This module provides Python wrappers for SQL aggregate functions
used with annotate() and direct aggregate queries.

Aggregate Functions:
    Count(field="*", distinct=False) → COUNT(*) or COUNT(DISTINCT field)
    Sum(field)                       → SUM(field)
    Avg(field)                       → AVG(field)
    Max(field)                       → MAX(field)
    Min(field)                       → MIN(field)

Scalar Functions:
    Concat(*fields, separator="")    → Concatenate strings (an expression)
    Coalesce(*args)                  → First non-NULL value (an expression;
                                       see oxyde.queries.expressions)

Raw SQL:
    RawSQL(sql)                      → Inject raw SQL expression (an expression)

Usage with annotate():
    # Add computed columns to query results
    query = User.objects.annotate(
        post_count=Count("id"),
        total_score=Sum("score"),
    )

Usage with group_by():
    query = Order.objects.annotate(
        total=Sum("amount"),
    ).group_by("customer_id")

Direct aggregates (execute immediately):
    total = await User.objects.filter(active=True).count()
    avg_age = await User.objects.avg("age")
    max_score = await User.objects.max("score")

IR Format:
    Count("id").to_ir() returns:
    {"func": "count", "field": "id"}

    Count("id", distinct=True).to_ir() returns:
    {"func": "count", "field": "id", "distinct": True}
"""

from __future__ import annotations

from typing import Any

from oxyde.queries.expressions import Coalesce, Concat, RawSQL


class Aggregate:
    """Base class for aggregate functions."""

    def __init__(self, field: str, *, distinct: bool = False):
        self.field = field
        self.distinct = distinct
        self.func_name = self.__class__.__name__.lower()

    def to_ir(self) -> dict[str, Any]:
        """Convert to IR representation."""
        ir: dict[str, Any] = {
            "func": self.func_name,
            "field": self.field,
        }
        if self.distinct:
            ir["distinct"] = True
        return ir


class Count(Aggregate):
    """COUNT aggregate function."""

    def __init__(self, field: str = "*", *, distinct: bool = False):
        super().__init__(field, distinct=distinct)


class Sum(Aggregate):
    """SUM aggregate function."""


class Avg(Aggregate):
    """AVG aggregate function."""


class Max(Aggregate):
    """MAX aggregate function."""


class Min(Aggregate):
    """MIN aggregate function."""


__all__ = [
    "Aggregate",
    "Count",
    "Sum",
    "Avg",
    "Max",
    "Min",
    "Concat",
    "Coalesce",
    "RawSQL",
]  # Coalesce, Concat, RawSQL live in expressions.py; re-exported for compatibility
