"""F-expressions for database-side field references and arithmetic.

F() allows referencing database columns in UPDATE statements for atomic
operations that execute entirely in the database, avoiding race conditions.

Basic Usage:
    F("price")           → Reference column "price"
    F("price") * 1.1     → price * 1.1
    F("count") + 1       → count + 1
    F("a") + F("b")      → a + b

Supported Operations:
    F("x") + y   → Addition
    F("x") - y   → Subtraction
    F("x") * y   → Multiplication
    F("x") / y   → Division
    -F("x")      → Negation

    Operands can be: F(), numbers, or other _Expression objects.

Scalar Functions and Literals:
    Coalesce("a", F("b"), 0)   → COALESCE(a, b, $1)   (str args are columns)
    Greatest("a", 1)           → GREATEST(a, $1)
    Concat("a", "b", separator=" ") → CONCAT_WS($1, a, b)   (skips NULLs)
    RawSQL("LENGTH(name)")     → verbatim SQL fragment
    Value("draft")             → literal (a str is never read as a column here)

    These are _Expression subclasses, so they compose with F() arithmetic and
    work in update() values and in annotate() (read back with .values()).

Column names:
    Names inside expressions are field names. update(), increment() and
    annotate() map them to their ``db_column``; names that are not model
    fields pass through unchanged, so F("db_col") keeps working. Related-field
    names ("author__id") are rejected.

Use Cases:
    # Atomic increment (no race condition)
    await User.objects.filter(id=1).update(login_count=F("login_count") + 1)

    # Percentage increase
    await Product.objects.filter(category="sale").update(price=F("price") * 0.9)

    # Combine columns
    await Order.objects.update(total=F("price") * F("quantity"))

Internal Structure:
    F("name") creates:
        _Expression({"type": "column", "name": "name"})

    F("x") + 5 creates:
        _Expression({
            "type": "op",
            "op": "add",
            "lhs": {"type": "column", "name": "x"},
            "rhs": {"type": "value", "value": 5}
        })

    _serialize_value_for_ir() wraps expressions in {"__expr__": ...}
    for the Rust IR parser to recognize them. Literal nodes carry an
    optional value_type ColumnTypeSpec dict ({"kind": "decimal"}).
"""

from __future__ import annotations

from collections.abc import Callable
from enum import Enum
from typing import TYPE_CHECKING, Any

from oxyde.core.column_types import spec_for_literal
from oxyde.core.types import serialize_value as _serialize_type_value
from oxyde.models.metadata import db_column_for

if TYPE_CHECKING:
    from oxyde.models.base import Model


class _Expression:
    """Represents an arithmetic expression usable in mutation statements."""

    __slots__ = ("_expr",)

    def __init__(self, expr: dict[str, Any]):
        self._expr = expr

    def _binary(self, op: str, other: Any) -> _Expression:
        rhs = _coerce_expression(other)
        return _Expression(
            {
                "type": "op",
                "op": op,
                "lhs": self._expr,
                "rhs": rhs._expr,
            }
        )

    def __add__(self, other: Any) -> _Expression:
        return self._binary("add", other)

    def __radd__(self, other: Any) -> _Expression:
        return _coerce_expression(other)._binary("add", self)

    def __sub__(self, other: Any) -> _Expression:
        return self._binary("sub", other)

    def __rsub__(self, other: Any) -> _Expression:
        return _coerce_expression(other)._binary("sub", self)

    def __mul__(self, other: Any) -> _Expression:
        return self._binary("mul", other)

    def __rmul__(self, other: Any) -> _Expression:
        return _coerce_expression(other)._binary("mul", self)

    def __truediv__(self, other: Any) -> _Expression:
        return self._binary("div", other)

    def __rtruediv__(self, other: Any) -> _Expression:
        return _coerce_expression(other)._binary("div", self)

    def __neg__(self) -> _Expression:
        return _Expression({"type": "neg", "expr": self._expr})


def _coerce_expression(value: Any) -> _Expression:
    """Coerce a value to an _Expression.

    For known Python types, attach a `value_type` ColumnTypeSpec dict.
    The Rust side uses it to recover typed bindings (Decimal, UUID,
    datetime, ...) that msgpack-encode as strings via `serialize_value`.
    """
    if isinstance(value, _Expression):
        return value
    if isinstance(value, F):
        return value._expression
    node: dict[str, Any] = {"type": "value", "value": value}
    spec = spec_for_literal(type(value))
    if spec is not None:
        node["value_type"] = spec
    return _Expression(node)


class Value(_Expression):
    """A literal in an expression. A ``str`` here is data, never a column."""

    __slots__ = ()

    def __init__(self, value: Any):
        if isinstance(value, (_Expression, F)):
            raise TypeError("Value() wraps a plain literal, not an expression")
        super().__init__(_coerce_expression(value)._expr)


class _Function(_Expression):
    """Base for scalar SQL functions: ``NAME(arg, ...)``.

    ``str`` arguments are column names; ``F``/expressions pass through; any
    other object (including ``Enum`` members, even ``str``-based ones) is a
    literal. Wrap in ``Value`` to force a string literal.
    """

    __slots__ = ()
    _func_name: str = ""

    def __init__(self, *args: Any):
        if not args:
            raise ValueError(f"{type(self).__name__}() requires at least one argument")
        nodes = [
            F(arg)._expression._expr
            if isinstance(arg, str) and not isinstance(arg, Enum)
            else _coerce_expression(arg)._expr
            for arg in args
        ]
        super().__init__({"type": "func", "name": self._func_name, "args": nodes})


class Coalesce(_Function):
    """COALESCE — the first non-NULL argument."""

    __slots__ = ()
    _func_name = "coalesce"


class Concat(_Function):
    """String concatenation: ``CONCAT_WS(separator, arg, ...)``.

    NULL arguments are skipped. ``str`` arguments are columns.
    """

    __slots__ = ()
    _func_name = "concat_ws"

    def __init__(self, *fields: Any, separator: str = ""):
        if not fields:
            raise ValueError("Concat() requires at least one field")
        super().__init__(Value(separator), *fields)


class RawSQL(_Expression):
    """A raw SQL fragment, sent verbatim (no parameters).

    Column names are database column names, not mapped from field names.
    The fragment is never qualified or rewritten: in a query with join(), qualify
    columns yourself (``"posts"."name"``), or a name present in several tables
    is ambiguous. Never build the fragment from untrusted input.
    """

    __slots__ = ()

    def __init__(self, sql: str):
        if not isinstance(sql, str) or not sql.strip():
            raise ValueError("RawSQL() requires a non-empty SQL string")
        super().__init__({"type": "raw", "sql": sql})


class Greatest(_Function):
    """GREATEST — the largest argument (NULLs are ignored by PostgreSQL)."""

    __slots__ = ()
    _func_name = "greatest"


class F:
    """Field expression helper for arithmetic updates."""

    __slots__ = ("name", "_expression")

    def __init__(self, name: str):
        self.name = name
        self._expression = _Expression({"type": "column", "name": name})

    def __add__(self, other: Any) -> _Expression:
        return self._expression.__add__(other)

    def __radd__(self, other: Any) -> _Expression:
        return self._expression.__radd__(other)

    def __sub__(self, other: Any) -> _Expression:
        return self._expression.__sub__(other)

    def __rsub__(self, other: Any) -> _Expression:
        return self._expression.__rsub__(other)

    def __mul__(self, other: Any) -> _Expression:
        return self._expression.__mul__(other)

    def __rmul__(self, other: Any) -> _Expression:
        return self._expression.__rmul__(other)

    def __truediv__(self, other: Any) -> _Expression:
        return self._expression.__truediv__(other)

    def __rtruediv__(self, other: Any) -> _Expression:
        return self._expression.__rtruediv__(other)

    def __neg__(self) -> _Expression:
        return -self._expression


def _is_scalar_annotation(obj: Any) -> bool:
    """True for objects annotate() treats as scalar (non-aggregate) expressions."""
    return isinstance(obj, (_Expression, F))


def _resolve_column_name(name: str, model_class: type[Model]) -> str:
    """Map a field name in an expression to its db column.

    Unknown names pass through (so ``F("db_col")`` keeps working);
    related-field names are rejected.
    """
    if "__" in name:
        raise ValueError(
            f"Related-field reference {name!r} is not supported in expressions; "
            "use a column of the model itself"
        )
    return db_column_for(model_class, name)


def _map_expression_columns(node: Any, resolve: Callable[[str], str]) -> Any:
    """Return a copy of an expression dict with column names resolved."""
    if isinstance(node, dict):
        node_type = node.get("type")
        if node_type == "value":
            # Literal payload is user data, never column references.
            return node
        if node_type == "column":
            return {**node, "name": resolve(node["name"])}
        return {key: _map_expression_columns(val, resolve) for key, val in node.items()}
    if isinstance(node, list):
        return [_map_expression_columns(item, resolve) for item in node]
    return node


def _serialize_value_for_ir(value: Any, model_class: type[Model] | None = None) -> Any:
    """Serialize values for IR, handling expressions and non-msgpack types.

    With ``model_class``, column names inside expressions are mapped to their
    ``db_column``.
    """
    if isinstance(value, F):
        value = value._expression
    if isinstance(value, _Expression):
        expr = value._expr
        if model_class is not None:
            expr = _map_expression_columns(
                expr, lambda name: _resolve_column_name(name, model_class)
            )
        return {"__expr__": expr}
    if isinstance(value, list):
        return [_serialize_value_for_ir(item, model_class) for item in value]
    if isinstance(value, dict):
        return {
            key: _serialize_value_for_ir(val, model_class) for key, val in value.items()
        }
    return _serialize_type_value(value)


__all__ = [
    "F",
    "Value",
    "Coalesce",
    "Concat",
    "Greatest",
    "RawSQL",
    "_Expression",
    "_coerce_expression",
    "_serialize_value_for_ir",
]
