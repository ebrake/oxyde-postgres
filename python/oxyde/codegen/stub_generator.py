"""Generator for .pyi stub files with type hints for model Queries."""

from __future__ import annotations

import ast
import copy
import inspect
import re
import sys
import types
from datetime import date, datetime, time
from decimal import Decimal
from pathlib import Path
from typing import Any, ForwardRef, Literal, NamedTuple, Union, get_args, get_origin
from uuid import UUID

from oxyde.models.base import Model
from oxyde.models.lookups import (
    ARRAY_LOOKUPS,
    _allowed_lookups_for_meta,
    _field_spec,
    _lookup_category,
    _resolve_column_meta,
    _supports_array_lookups,
)
from oxyde.models.registry import registered_tables
from oxyde.models.utils import _unpack_annotated, _unwrap_optional


def _get_python_type_name(python_type: Any) -> str:
    """Get string representation of Python type for stub file."""
    if python_type is str:
        return "str"
    elif python_type is int:
        return "int"
    elif python_type is float:
        return "float"
    elif python_type is bool:
        return "bool"
    elif python_type is bytes:
        return "bytes"
    elif python_type is datetime:
        return "datetime"
    elif python_type is date:
        return "date"
    elif python_type is time:
        return "time"
    elif python_type is Decimal:
        return "Decimal"
    elif python_type is UUID:
        return "UUID"
    elif python_type is type(None):
        return "None"
    elif python_type is Ellipsis:
        return "..."
    elif isinstance(python_type, ForwardRef):
        # Unresolved forward reference (e.g. reverse FK to a class defined
        # later in the module). Its target name is already stub-safe.
        return python_type.__forward_arg__
    elif isinstance(python_type, str):
        # Unevaluated string annotation. Emit verbatim.
        return python_type
    elif isinstance(python_type, type) and issubclass(python_type, Model):
        # FK/relation field pointing to another model
        return python_type.__name__
    elif (origin := get_origin(python_type)) is not None:
        # Parameterized generics — rendered recursively so type arguments
        # referencing user models come out unqualified (list[Post], not
        # list[some_module.Post] as str() would produce).
        if origin is Literal:
            values = ", ".join(repr(arg) for arg in get_args(python_type))
            return f"Literal[{values}]"
        if origin is Union or origin is types.UnionType:
            return " | ".join(_get_python_type_name(a) for a in get_args(python_type))
        args = ", ".join(_get_python_type_name(a) for a in get_args(python_type))
        origin_name = getattr(origin, "__name__", None) or str(origin).replace(
            "typing.", ""
        )
        return f"{origin_name}[{args}]"
    elif isinstance(python_type, type):
        # Bare built-in types (dict, list, set, tuple, etc.)
        return python_type.__name__
    else:
        # Other complex types (e.g. typing constructs)
        return str(python_type).replace("typing.", "")


class _FilterFieldSpec(NamedTuple):
    type_name: str
    lookups: list[str]
    # Value type of contains/contained_by/overlap on array and JSON fields
    array_value_type: str | None = None
    # A str field stored as JSON: `contains` also takes a string (LIKE)
    string_contains: bool = False


def _array_value_type(python_type: Any) -> str:
    """Operand type of an array lookup; ``Any`` elements unless a list[T] field.

    ``None`` matches a null element, so elements may be ``None``. ``list`` is
    invariant, so ``list[T]`` stays for a ``list[T]`` variable, and mypy infers
    ``[None]`` as ``list[None]``; ``tuple`` is covariant and needs one form.
    """
    if get_origin(python_type) is list and (args := get_args(python_type)):
        element, _ = _unwrap_optional(_unpack_annotated(args[0])[0])
        name = _get_python_type_name(element)
        return (
            f"list[{name}] | list[{name} | None] | list[None]"
            f" | tuple[{name} | None, ...]"
        )
    return "list[Any] | tuple[Any, ...]"


def _filter_field_specs(model_class: type[Model]) -> dict[str, _FilterFieldSpec]:
    """Per-field filter value type and allowed lookups.

    Lookups come from the runtime lookup tables (``_allowed_lookups_for_meta``),
    so the stub can never drift from what ``filter()`` actually accepts. For FK
    fields the value type is the *primary-key* type of the synthetic column
    (``filter(author=5)``): the runtime compares against the pk value and
    cannot serialize a model instance.
    """
    from oxyde.models.field import OxydeFieldInfo

    specs: dict[str, _FilterFieldSpec] = {}
    for field_name, field_info in model_class.model_fields.items():
        # Virtual fields (reverse FK, m2m) are not filterable columns
        if isinstance(field_info, OxydeFieldInfo):
            if getattr(field_info, "db_reverse_fk", None) or getattr(
                field_info, "db_m2m", False
            ):
                continue

        meta = _resolve_column_meta(model_class, field_name)
        spec = _field_spec(model_class, meta)
        lookups = _allowed_lookups_for_meta(meta, spec)

        if meta.foreign_key is not None:
            synthetic = model_class.model_fields.get(meta.db_column)
            if synthetic is not None and synthetic is not field_info:
                base_hint, _ = _unpack_annotated(synthetic.annotation)
                python_type, _ = _unwrap_optional(base_hint)
            else:
                python_type = int
        else:
            base_hint, _ = _unpack_annotated(field_info.annotation)
            python_type, _ = _unwrap_optional(base_hint)

        array_value_type = None
        if _supports_array_lookups(meta, spec):
            array_value_type = _array_value_type(python_type)
        specs[field_name] = _FilterFieldSpec(
            _get_python_type_name(python_type),
            lookups,
            array_value_type,
            string_contains=_lookup_category(meta) == "string",
        )

    return specs


def _get_field_info(model_class: type[Model]) -> dict[str, tuple[Any, bool]]:
    """
    Get field info from model_fields, returns dict of field_name -> (python_type, is_pk).

    Uses model_fields (Pydantic) as primary source, with _db_meta as fallback for PK info.
    Excludes virtual fields (reverse FK, m2m) that don't map to DB columns.
    """
    from oxyde.models.field import OxydeFieldInfo

    result = {}

    for field_name, field_info in model_class.model_fields.items():
        # Skip virtual fields (reverse FK, m2m) - they don't map to DB columns
        if isinstance(field_info, OxydeFieldInfo):
            if getattr(field_info, "db_reverse_fk", None) or getattr(
                field_info, "db_m2m", False
            ):
                continue

        # Get python type from annotation
        annotation = field_info.annotation
        base_hint, _ = _unpack_annotated(annotation)
        python_type, _ = _unwrap_optional(base_hint)

        # Check if primary key
        is_pk = False
        if isinstance(field_info, OxydeFieldInfo):
            is_pk = field_info.db_pk or False
        else:
            json_extra = getattr(field_info, "json_schema_extra", None)
            if isinstance(json_extra, dict):
                is_pk = json_extra.get("db_pk", False)

        result[field_name] = (python_type, is_pk)

    return result


# Field names that would collide with service parameters in the generated
# method signatures (filter/exclude and the get-family share one parameter
# block). Colliding fields are not enumerated — they stay reachable through
# ``**kwargs``, which mirrors the runtime situation: terminal methods swallow
# e.g. ``client=`` as the service argument anyway.
_RESERVED_FILTER_PARAMS = frozenset(
    {"self", "args", "kwargs", "client", "using", "defaults"}
)
_RESERVED_CREATE_PARAMS = frozenset({"self", "instance", "client", "using"})


def _generate_filter_params(model_class: type[Model]) -> str:
    """Generate filter method parameters with all lookups.

    Ends with ``**kwargs: Any`` because the runtime also accepts FK traversal
    paths (``author__name="Alice"``) that cannot be enumerated statically.
    """
    lines = []

    for field_name, spec in sorted(_filter_field_specs(model_class).items()):
        type_name, lookups = spec.type_name, spec.lookups
        if field_name in _RESERVED_FILTER_PARAMS:
            continue

        # Bare field name (implicit exact match)
        lines.append(f"        {field_name}: {type_name} | None = None,")

        for lookup in lookups:
            lookup_field = f"{field_name}__{lookup}"

            # Determine type for lookup (matching what the runtime accepts:
            # `in` takes any iterable, between/range/month/day take a pair or
            # triple as tuple or list, year takes a bare int)
            if lookup == "in":
                lookup_type = f"Iterable[{type_name}] | None"
            elif lookup in ("between", "range"):
                lookup_type = (
                    f"tuple[{type_name}, {type_name}] | list[{type_name}] | None"
                )
            elif lookup == "isnull":
                lookup_type = "bool | None"
            elif lookup == "year":
                lookup_type = "int | None"
            elif lookup == "month":
                lookup_type = "tuple[int, int] | list[int] | None"
            elif lookup == "day":
                lookup_type = "tuple[int, int, int] | list[int] | None"
            elif lookup in ARRAY_LOOKUPS and spec.array_value_type is not None:
                if lookup == "contains" and spec.string_contains:
                    lookup_type = f"str | {spec.array_value_type} | None"
                else:
                    lookup_type = f"{spec.array_value_type} | None"
            else:
                lookup_type = f"{type_name} | None"

            lines.append(f"        {lookup_field}: {lookup_type} = None,")

    lines.append("        **kwargs: Any,")
    return "\n".join(lines)


def _generate_order_by_literal(model_class: type[Model]) -> str:
    """Generate the order_by fields type (includes - prefix for DESC).

    The union with ``str`` keeps the literals as autocomplete suggestions while
    still accepting values the runtime supports but the stub cannot enumerate:
    ``"?"`` (random ordering), ``annotate()`` aliases and FK traversal paths.
    """
    field_info = _get_field_info(model_class)
    literals = []

    for field_name in sorted(field_info.keys()):
        literals.append(f'"{field_name}"')
        literals.append(f'"-{field_name}"')

    if not literals:
        return "str"  # Fallback if no fields

    return f"Literal[{', '.join(literals)}] | str"


def _generate_field_literal(model_class: type[Model]) -> str:
    """Generate the field-names type (for select/values/group_by).

    Union with ``str`` for the same reason as ``_generate_order_by_literal``:
    ``annotate()`` aliases and traversal paths are legal at runtime.
    """
    field_info = _get_field_info(model_class)
    literals = [f'"{field_name}"' for field_name in sorted(field_info.keys())]

    if not literals:
        return "str"  # Fallback if no fields

    return f"Literal[{', '.join(literals)}] | str"


def _generate_create_params(model_class: type[Model]) -> str:
    """Generate create method parameters with proper optionality."""
    lines = []
    field_info = _get_field_info(model_class)

    for field_name, (python_type, _is_pk) in sorted(field_info.items()):
        if field_name in _RESERVED_CREATE_PARAMS:
            continue
        type_name = _get_python_type_name(python_type)
        # All create params are optional in stub (instance can be passed instead)
        lines.append(f"        {field_name}: {type_name} | None = None,")

    return "\n".join(lines)


def _has_overload_decorator(node: ast.FunctionDef | ast.AsyncFunctionDef) -> bool:
    """Return True if the function is decorated with ``@overload``."""
    for dec in node.decorator_list:
        if isinstance(dec, ast.Name) and dec.id == "overload":
            return True
        if isinstance(dec, ast.Attribute) and dec.attr == "overload":
            return True
    return False


def _is_generator(node: ast.FunctionDef | ast.AsyncFunctionDef) -> bool:
    """Find yields in the function body, excluding nested scopes."""
    stack: list[ast.AST] = list(node.body)
    while stack:
        child = stack.pop()
        if isinstance(child, (ast.Yield, ast.YieldFrom)):
            return True
        if isinstance(
            child, (ast.FunctionDef, ast.AsyncFunctionDef, ast.Lambda, ast.ClassDef)
        ):
            continue
        stack.extend(ast.iter_child_nodes(child))
    return False


def _stub_function(
    node: ast.FunctionDef | ast.AsyncFunctionDef,
) -> ast.FunctionDef | ast.AsyncFunctionDef:
    """Copy a function's declaration, replacing its body with ``...``."""
    body: list[ast.stmt] = [ast.Expr(value=ast.Constant(value=Ellipsis))]
    if isinstance(node, ast.AsyncFunctionDef) and _is_generator(node):
        # An ellipsis body would make checkers misclassify this as a coroutine.
        fields = {
            field: copy.deepcopy(getattr(node, field))
            for field in node._fields
            if field != "body"
        }
        return ast.copy_location(ast.FunctionDef(body=body, **fields), node)
    node_copy = copy.deepcopy(node)
    node_copy.body = body
    return node_copy


def _overloaded_names(body: list[ast.stmt]) -> set[str]:
    """Names whose implementations must be omitted from a stub body."""
    return {
        node.name
        for node in body
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef))
        and _has_overload_decorator(node)
    }


def _stub_function_defs(body: list[ast.stmt]) -> list[str]:
    """Turn the function definitions of an AST body into stub sources.

    Bodies are replaced by ``...`` (stub convention); decorators and type
    annotations are preserved. For ``@overload`` functions, only the
    ``@overload``-decorated variants are kept; the implementation (PEP 484
    requires one in a ``.py`` file but forbids it in a ``.pyi``) is dropped.
    """
    overloaded_names = _overloaded_names(body)

    functions: list[str] = []
    for node in body:
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            if node.name in overloaded_names and not _has_overload_decorator(node):
                continue
            functions.append(ast.unparse(_stub_function(node)))
    return functions


def _stubbed_class_body(body: list[ast.stmt]) -> list[ast.stmt]:
    """Class body with function bodies replaced by ``...``.

    Nested classes are processed recursively. Assignments are kept verbatim:
    for enums the member values ARE the type interface (checkers read them for
    Literal narrowing), and dataclass/plain-class attribute defaults are almost
    always literals. Overload implementations are dropped as at top level.
    """
    overloaded_names = _overloaded_names(body)

    out: list[ast.stmt] = []
    for node in body:
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            if node.name in overloaded_names and not _has_overload_decorator(node):
                continue
            out.append(_stub_function(node))
        elif isinstance(node, ast.ClassDef):
            class_copy = copy.deepcopy(node)
            class_copy.body = _stubbed_class_body(class_copy.body) or [ast.Pass()]
            out.append(class_copy)
        else:
            out.append(copy.deepcopy(node))
    return out or [ast.Pass()]


def _stub_class_def(class_def: ast.ClassDef) -> str:
    """Stub source for a supporting top-level class (enum, dataclass, helper)."""
    node_copy = copy.deepcopy(class_def)
    node_copy.body = _stubbed_class_body(node_copy.body)
    return ast.unparse(node_copy)


_TYPING_MODULES = {"typing", "typing_extensions"}
_MISSING = object()
_IDENT_RE = re.compile(r"[A-Za-z_]\w*")


def _import_origins(imports: list[ast.Import | ast.ImportFrom]) -> dict[str, str]:
    """Resolve imported bindings, preserving relative module prefixes."""
    origins: dict[str, str] = {}
    for node in imports:
        for alias in node.names:
            if alias.name == "*":
                continue
            if isinstance(node, ast.ImportFrom):
                module = "." * node.level + (node.module or "")
                origin = module + ("" if module.endswith(".") else ".") + alias.name
                bound = alias.asname or alias.name
            else:
                bound = alias.asname or alias.name.split(".")[0]
                origin = alias.name if alias.asname else bound
            origins[bound] = origin
    return origins


def _qualified_origin(value: ast.expr, origins: dict[str, str]) -> str | None:
    if isinstance(value, ast.Name):
        return origins.get(value.id)
    if isinstance(value, ast.Attribute):
        parent = _qualified_origin(value.value, origins)
        if parent is not None:
            return f"{parent}.{value.attr}"
    return None


def _is_typing_name(origin: str | None, name: str) -> bool:
    return origin in {f"{module}.{name}" for module in _TYPING_MODULES}


def _is_type_checking_guard(test: ast.expr, origins: dict[str, str]) -> bool:
    return _is_typing_name(_qualified_origin(test, origins), "TYPE_CHECKING")


def _extract_top_level_copyable(
    tree: ast.Module,
) -> tuple[list[ast.Import | ast.ImportFrom], list[str], list[ast.ClassDef]]:
    """Collect top-level imports, function stubs and class defs from source.

    - ``Import`` / ``ImportFrom``: returned as AST nodes so the caller can
      dedupe them against the stub skeleton imports and drop unused ones.
      ``from __future__ import ...`` is dropped entirely — a ``.pyi`` never
      needs it, and anywhere but the top of the file it is a syntax error.
    - ``FunctionDef`` / ``AsyncFunctionDef``: stubbed via ``_stub_function_defs``.
    - ``ClassDef``: returned as raw AST nodes. The caller stubs model classes
      via the generator and copies supporting classes (enums used as column
      types, helper classes referenced by custom methods) via
      ``_stub_class_def`` when public or referenced by the generated body.
    Imports in ``TYPE_CHECKING`` blocks are unconditional in the stub. Module
    assignments and exports are handled by ``_module_export_inventory``.
    """
    imports: list[ast.Import | ast.ImportFrom] = []
    origins = _import_origins(
        [node for node in tree.body if isinstance(node, (ast.Import, ast.ImportFrom))]
    )

    def collect_imports(body: list[ast.stmt]) -> None:
        for node in body:
            if isinstance(node, (ast.Import, ast.ImportFrom)):
                if not (
                    isinstance(node, ast.ImportFrom) and node.module == "__future__"
                ):
                    imports.append(node)
            elif isinstance(node, ast.If) and _is_type_checking_guard(
                node.test, origins
            ):
                collect_imports(node.body)

    collect_imports(tree.body)
    class_defs = [node for node in tree.body if isinstance(node, ast.ClassDef)]
    return imports, _stub_function_defs(tree.body), class_defs


def _union(types: set[str]) -> str:
    return " | ".join(sorted(types)) if types and "Any" not in types else "Any"


def _assignment_type(value: ast.expr | None) -> str:
    """Infer literal types without evaluating source; dynamic values use Any."""
    if isinstance(value, ast.Constant):
        if value.value is None:
            return "None"
        if type(value.value) in (bool, int, float, complex, str, bytes):
            return type(value.value).__name__
    if isinstance(value, ast.UnaryOp) and isinstance(value.op, (ast.UAdd, ast.USub)):
        if isinstance(value.operand, ast.Constant) and type(value.operand.value) in (
            int,
            float,
            complex,
        ):
            return type(value.operand.value).__name__
    if isinstance(value, (ast.List, ast.Set)):
        element = _union({_assignment_type(item) for item in value.elts})
        container = "list" if isinstance(value, ast.List) else "set"
        return f"{container}[{element}]"
    if isinstance(value, ast.Tuple):
        tuple_types = ", ".join(_assignment_type(item) for item in value.elts) or "()"
        return f"tuple[{tuple_types}]"
    if isinstance(value, ast.Dict):
        key_type = _union({_assignment_type(key) for key in value.keys})
        value_type = _union({_assignment_type(item) for item in value.values})
        return f"dict[{key_type}, {value_type}]"
    return "Any"


def _literal_export_names(value: ast.expr | None) -> list[str] | None:
    if isinstance(value, (ast.List, ast.Tuple)):
        names = [
            item.value
            for item in value.elts
            if isinstance(item, ast.Constant) and isinstance(item.value, str)
        ]
        return names if len(names) == len(value.elts) else None
    if isinstance(value, ast.BinOp) and isinstance(value.op, ast.Add):
        left = _literal_export_names(value.left)
        right = _literal_export_names(value.right)
        if left is not None and right is not None:
            return left + right
    return None


class _ModuleExports(NamedTuple):
    names: set[str]
    declarations: dict[str, str]
    all_declaration: str | None
    local: set[str]


_TYPE_FACTORIES = {
    *(
        f"{module}.{name}"
        for module in _TYPING_MODULES
        for name in (
            "TypeVar",
            "ParamSpec",
            "TypeVarTuple",
            "NewType",
            "TypeAliasType",
            "NamedTuple",
            "TypedDict",
        )
    ),
    *(f"enum.{name}" for name in ("Enum", "IntEnum", "StrEnum", "Flag", "IntFlag")),
}
_BUILTIN_TYPES = {
    "bool",
    "bytes",
    "complex",
    "dict",
    "float",
    "frozenset",
    "int",
    "list",
    "object",
    "set",
    "str",
    "tuple",
    "type",
}


def _is_type_expression(value: ast.expr, type_names: set[str]) -> bool:
    if isinstance(value, ast.Name):
        return value.id in type_names
    if isinstance(value, ast.Attribute):
        return _is_type_expression(value.value, type_names)
    if isinstance(value, ast.Constant):
        return value.value is None
    if isinstance(value, ast.Subscript):
        return _is_type_expression(value.value, type_names)
    if isinstance(value, ast.BinOp) and isinstance(value.op, ast.BitOr):
        return _is_type_expression(value.left, type_names) and _is_type_expression(
            value.right, type_names
        )
    return False


def _module_export_inventory(
    tree: ast.Module,
    imports: list[ast.Import | ast.ImportFrom],
    origins: dict[str, str],
    module: types.ModuleType | None,
) -> _ModuleExports:
    """Inventory declarations and intentional reexports before pruning names.

    Besides __all__ and redundant aliases, imported public CONSTANT_NAMES are
    treated as reexports, a common convention in model modules. Other imports
    remain dependencies rather than expanding the module's public interface.
    """
    names: set[str] = set()
    declarations: dict[str, str] = {}
    definitions = {
        node.name
        for node in tree.body
        if isinstance(node, (ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef))
    }
    type_names = (
        _BUILTIN_TYPES
        | set(origins)
        | {node.name for node in tree.body if isinstance(node, ast.ClassDef)}
    )
    all_names: list[str] | None = None
    has_all = False
    for node in tree.body:
        if isinstance(node, (ast.Assign, ast.AnnAssign)):
            targets = node.targets if isinstance(node, ast.Assign) else [node.target]
            if any(isinstance(t, ast.Name) and t.id == "__all__" for t in targets):
                has_all = True
                all_names = _literal_export_names(node.value)
                continue
            for target in targets:
                for binding in ast.walk(target):
                    if not isinstance(binding, ast.Name) or not isinstance(
                        binding.ctx, ast.Store
                    ):
                        continue
                    name = binding.id
                    if name in definitions or name in origins:
                        continue
                    annotation = (
                        ast.unparse(node.annotation)
                        if isinstance(node, ast.AnnAssign)
                        else _assignment_type(node.value)
                        if isinstance(target, ast.Name)
                        else "Any"
                    )
                    if annotation in ("Final", "typing.Final"):
                        annotation = f"{annotation}[{_assignment_type(node.value)}]"
                    declaration = f"{name}: {annotation}"
                    if isinstance(target, ast.Name) and node.value is not None:
                        if isinstance(node, ast.AnnAssign) and _is_typing_name(
                            _qualified_origin(node.annotation, origins), "TypeAlias"
                        ):
                            declaration += f" = {ast.unparse(node.value)}"
                        elif isinstance(node, ast.Assign) and (
                            isinstance(node.value, ast.Call)
                            and _qualified_origin(node.value.func, origins)
                            in _TYPE_FACTORIES
                            or isinstance(node.value, (ast.Subscript, ast.BinOp))
                            and _is_type_expression(node.value, type_names)
                        ):
                            declaration = f"{name} = {ast.unparse(node.value)}"
                    declarations[name] = declaration
                    if not name.startswith("_"):
                        names.add(name)
        elif isinstance(node, ast.TypeAlias):
            name = node.name.id
            if name not in definitions and name not in origins:
                declarations[name] = ast.unparse(node)
                if not name.startswith("_"):
                    names.add(name)
        elif isinstance(node, ast.AugAssign) and isinstance(node.target, ast.Name):
            if node.target.id == "__all__":
                extra = _literal_export_names(node.value)
                all_names = (
                    all_names + extra
                    if all_names is not None
                    and extra is not None
                    and isinstance(node.op, ast.Add)
                    else None
                )
        elif (
            isinstance(node, ast.Expr)
            and isinstance(node.value, ast.Call)
            and isinstance(node.value.func, ast.Attribute)
            and isinstance(node.value.func.value, ast.Name)
            and node.value.func.value.id == "__all__"
        ):
            call = node.value
            if (
                all_names is not None
                and isinstance(call.func, ast.Attribute)
                and len(call.args) == 1
            ):
                if call.func.attr == "extend":
                    extra = _literal_export_names(call.args[0])
                    all_names = all_names + extra if extra is not None else None
                elif (
                    call.func.attr == "append"
                    and isinstance(call.args[0], ast.Constant)
                    and isinstance(call.args[0].value, str)
                ):
                    all_names.append(call.args[0].value)
                else:
                    all_names = None
        elif isinstance(node, ast.ClassDef) and not node.name.startswith("_"):
            names.add(node.name)

    names.update(all_names or [])
    for node in imports:
        for alias in node.names:
            bound = alias.asname or alias.name.split(".")[0]
            value = getattr(module, bound, _MISSING)
            if alias.asname == alias.name or (
                not has_all
                and not bound.startswith("_")
                and not _is_typing_name(origins.get(bound), "TYPE_CHECKING")
                and bound.isupper()
                and not (
                    inspect.isclass(value)
                    or inspect.isfunction(value)
                    or inspect.isbuiltin(value)
                    or inspect.ismodule(value)
                )
            ):
                names.add(bound)
    all_declaration = None
    if has_all:
        all_declaration = (
            f"__all__ = {all_names!r}"
            if all_names is not None
            else "__all__: list[str]"
        )
    return _ModuleExports(
        names, declarations, all_declaration, definitions | declarations.keys()
    )


# Names the generated stub skeleton may reference, grouped by stdlib module.
# Each is emitted only when the generated body actually uses it.
_STUB_HEADER_IMPORTS: tuple[tuple[str, tuple[str, ...]], ...] = (
    ("collections.abc", ("Iterable",)),
    ("datetime", ("date", "datetime", "time")),
    ("decimal", ("Decimal",)),
    ("typing", ("Any", "ClassVar", "Literal", "overload")),
    ("uuid", ("UUID",)),
)


def _is_name_used(name: str, body: str) -> bool:
    return re.search(rf"\b{re.escape(name)}\b", body) is not None


def _assemble_imports(
    user_imports: list[ast.Import | ast.ImportFrom],
    body: str,
    exports: set[str] | None = None,
    local: set[str] | None = None,
) -> list[str]:
    """Build the stub import block: skeleton imports plus surviving user imports.

    Skeleton names (typing/datetime/decimal/uuid + oxyde) are emitted only when
    the generated body references them. User imports are kept for names the body
    still needs (cross-module FK types, custom-method signatures, decorators);
    unused names and duplicates of skeleton names are dropped — on a name
    collision the skeleton import wins, since the generated code references the
    skeleton's meaning of that name. All ``from`` imports are merged by module;
    star imports remain separate because they cannot share an import statement.
    """
    exports = exports or set()
    local = local or set()
    user_from_names = {
        (node.module, alias.name)
        for node in user_imports
        if isinstance(node, ast.ImportFrom) and node.level == 0
        for alias in node.names
        if alias.asname is None or alias.asname == alias.name
    }
    from_groups: dict[tuple[int, str | None], dict[str, ast.alias]] = {}
    for module, names in _STUB_HEADER_IMPORTS:
        used = {
            name: ast.alias(name=name, asname=name if name in exports else None)
            for name in sorted(names)
            if name not in local
            and (
                _is_name_used(name, body)
                or name in exports
                and (module, name) in user_from_names
            )
        }
        if used:
            from_groups[(0, module)] = used

    provided: set[str] = {
        "FlatValuesListQuery",
        "Model",
        "Query",
        "QueryManager",
        "ValuesListQuery",
        "ValuesQuery",
    }
    provided.update(*(group.keys() for group in from_groups.values()))

    plain: dict[str, ast.alias] = {}
    star_modules: set[tuple[int, str | None]] = set()
    for node in user_imports:
        for alias in node.names:
            if alias.name == "*" and isinstance(node, ast.ImportFrom):
                star_modules.add((node.level, node.module))
                continue
            bound = alias.asname or alias.name.split(".")[0]
            if bound in provided or not (
                _is_name_used(bound, body) or bound in exports
            ):
                continue
            alias = copy.deepcopy(alias)
            if bound in exports and alias.asname is None and "." not in alias.name:
                alias.asname = alias.name
            if isinstance(node, ast.ImportFrom):
                from_groups.setdefault((node.level, node.module), {})[bound] = alias
            else:
                plain[bound] = alias

    import_nodes: list[ast.Import | ast.ImportFrom] = [
        ast.ImportFrom(module=module, names=list(aliases.values()), level=level)
        for (level, module), aliases in from_groups.items()
    ]
    import_nodes.extend(
        ast.ImportFrom(module=module, names=[ast.alias(name="*")], level=level)
        for level, module in star_modules
    )
    import_nodes.extend(ast.Import(names=[alias]) for alias in plain.values())
    stdlib_lines: list[str] = []
    other_lines: list[str] = []
    for node in import_nodes:
        setattr(node, "is_lazy", False)
        line = ast.unparse(node)
        if (
            isinstance(node, ast.ImportFrom)
            and node.level == 0
            and node.module is not None
            and node.module.split(".")[0] in sys.stdlib_module_names
        ):
            stdlib_lines.append(line)
        else:
            other_lines.append(line)

    lines = sorted(stdlib_lines)
    if lines:
        lines.append("")
    # Parenthesized because the one-line form exceeds the 88-char line limit
    # and would trip isort's formatting check in user projects.
    oxyde_queries_import = (
        "from oxyde.queries import (\n"
        "    FlatValuesListQuery,\n"
        "    Query,\n"
        "    QueryManager,\n"
        "    ValuesListQuery,\n"
        "    ValuesQuery,\n"
        ")"
    )
    lines.extend(
        sorted(
            [
                "from oxyde import Model",
                oxyde_queries_import,
                *other_lines,
            ]
        )
    )
    return lines


def _extract_custom_methods(class_def: ast.ClassDef) -> list[str]:
    """Extract user-defined methods from a Model ClassDef with body replaced by `...`.

    Returns list of method definitions as strings, each indented relative to the
    class body (i.e. without the leading class indent — caller adds it).
    """
    return _stub_function_defs(class_def.body)


def _generate_model_class_stub(
    model_class: type[Model],
    custom_methods: list[str],
) -> str:
    """Generate stub class definition for model (to avoid circular imports in .pyi)."""
    model_name = model_class.__name__
    lines = [f"class {model_name}(Model):"]

    # Add Meta class if present
    if hasattr(model_class, "Meta"):
        lines.append("    class Meta:")
        meta = model_class.Meta
        if hasattr(meta, "is_table"):
            lines.append("        is_table: bool")
        if hasattr(meta, "table_name"):
            lines.append("        table_name: str")
        if hasattr(meta, "schema"):
            lines.append("        schema: str")
        if hasattr(meta, "database"):
            lines.append("        database: str")

    # Add field annotations. Virtual relation fields (reverse FK, m2m) are
    # included too: they exist on instances (filled by prefetch()) even though
    # they don't map to DB columns and are excluded from filter/select params.
    # Fields with a default (or default_factory) get an `= ...` marker —
    # pydantic's dataclass_transform derives the synthesized __init__ from
    # these assignments, so a bare annotation would make the field a required
    # constructor argument in every type checker.
    for field_name, field_info in model_class.model_fields.items():
        annotation = field_info.annotation
        base_hint, _ = _unpack_annotated(annotation)
        python_type, is_optional = _unwrap_optional(base_hint)
        type_name = _get_python_type_name(python_type)

        optional_suffix = " | None" if is_optional else ""
        default_marker = "" if field_info.is_required() else " = ..."
        lines.append(f"    {field_name}: {type_name}{optional_suffix}{default_marker}")

    # Copy user-defined methods (body replaced by `...`)
    for method_src in custom_methods:
        for raw_line in method_src.splitlines():
            lines.append(f"    {raw_line}" if raw_line else "")

    # Add objects manager with proper type. The base Model declares
    # `objects: ClassVar[QueryManager]`; narrowing it here is intentional,
    # so silence pyright's invariant-variable-override check.
    lines.append(
        f"    objects: ClassVar[{model_name}Manager]"
        "  # pyright: ignore[reportIncompatibleVariableOverride]"
    )

    return "\n".join(lines)


def generate_model_stub(model_class: type[Model]) -> str:
    """Generate .pyi stub content for a single model (without imports)."""
    model_name = model_class.__name__

    # Generate model-dependent parameters
    filter_params = _generate_filter_params(model_class)
    order_by_literal = _generate_order_by_literal(model_class)
    field_literal = _generate_field_literal(model_class)
    create_params = _generate_create_params(model_class)

    queryset_class = f"""
class {model_name}Query(Query[{model_name}]):
    \"\"\"Type-safe Query for {model_name} model.\"\"\"

    # Query building methods (sync, return Query)

    def filter(
        self,
        *args: Any,
{filter_params}
    ) -> {model_name}Query:
        \"\"\"Filter by Q-expressions or field lookups.\"\"\"
        ...

    def exclude(
        self,
        *args: Any,
{filter_params}
    ) -> {model_name}Query:
        \"\"\"Exclude objects matching field lookups.\"\"\"
        ...

    def order_by(self, *fields: {order_by_literal}) -> {model_name}Query:
        \"\"\"Order results by fields ("?" = random order).\"\"\"
        ...

    def limit(self, value: int) -> {model_name}Query:
        \"\"\"Limit number of results.\"\"\"
        ...

    def offset(self, value: int) -> {model_name}Query:
        \"\"\"Skip first value results.\"\"\"
        ...

    def distinct(self, distinct: bool = True) -> {model_name}Query:
        \"\"\"Return distinct results.\"\"\"
        ...

    def select(self, *fields: {field_literal}) -> {model_name}Query:
        \"\"\"Select specific fields.\"\"\"
        ...

    def join(self, *paths: str) -> {model_name}Query:
        \"\"\"Perform LEFT JOIN for relations.\"\"\"
        ...

    def prefetch(self, *paths: str) -> {model_name}Query:
        \"\"\"Prefetch related objects (separate queries).\"\"\"
        ...

    def for_update(self) -> {model_name}Query:
        \"\"\"Add FOR UPDATE lock to query.\"\"\"
        ...

    def for_share(self) -> {model_name}Query:
        \"\"\"Add FOR SHARE lock to query.\"\"\"
        ...

    def annotate(self, **annotations: Any) -> {model_name}Query:
        \"\"\"Add computed fields using aggregate functions.\"\"\"
        ...

    def group_by(self, *fields: {field_literal}) -> {model_name}Query:
        \"\"\"Add GROUP BY clause.\"\"\"
        ...

    def having(self, *q_exprs: Any, **kwargs: Any) -> {model_name}Query:
        \"\"\"Add HAVING clause for filtering grouped results.\"\"\"
        ...

    def values(self, *fields: {field_literal}) -> ValuesQuery[{model_name}]:
        \"\"\"Return dicts instead of models.\"\"\"
        ...

    @overload
    def values_list(self, field: {field_literal}, /, *, flat: Literal[True]) -> FlatValuesListQuery[{model_name}]:
        ...

    @overload
    def values_list(self, *fields: {field_literal}, flat: Literal[False] = ...) -> ValuesListQuery[{model_name}]:
        ...

    @overload
    def values_list(self, *fields: {field_literal}, flat: bool) -> ValuesListQuery[{model_name}] | FlatValuesListQuery[{model_name}]:
        ...

    # Terminal methods (async, execute query)

    async def all(
        self,
        *,
        client: Any | None = None,
        using: str | None = None,
    ) -> list[{model_name}]:
        \"\"\"Execute query and return all results.\"\"\"
        ...

    async def first(
        self,
        *,
        client: Any | None = None,
        using: str | None = None,
    ) -> {model_name} | None:
        \"\"\"Execute query and return first result.\"\"\"
        ...

    async def last(
        self,
        *,
        client: Any | None = None,
        using: str | None = None,
    ) -> {model_name} | None:
        \"\"\"Execute query and return last result.\"\"\"
        ...

    async def count(
        self,
        *,
        client: Any | None = None,
        using: str | None = None,
    ) -> int:
        \"\"\"Count matching objects.\"\"\"
        ...

    async def sum(
        self,
        field: str,
        *,
        client: Any | None = None,
        using: str | None = None,
    ) -> Any:
        \"\"\"Calculate sum of field values.\"\"\"
        ...

    async def avg(
        self,
        field: str,
        *,
        client: Any | None = None,
        using: str | None = None,
    ) -> Any:
        \"\"\"Calculate average of field values.\"\"\"
        ...

    async def max(
        self,
        field: str,
        *,
        client: Any | None = None,
        using: str | None = None,
    ) -> Any:
        \"\"\"Get maximum field value.\"\"\"
        ...

    async def min(
        self,
        field: str,
        *,
        client: Any | None = None,
        using: str | None = None,
    ) -> Any:
        \"\"\"Get minimum field value.\"\"\"
        ...

    async def exists(
        self,
        *,
        client: Any | None = None,
        using: str | None = None,
    ) -> bool:
        \"\"\"Check if any records match the query.\"\"\"
        ...

    async def get(
        self,
        *,
        client: Any | None = None,
        using: str | None = None,
    ) -> {model_name}:
        \"\"\"Return exactly one result (raises if none or many).\"\"\"
        ...

    async def get_or_none(
        self,
        *,
        client: Any | None = None,
        using: str | None = None,
    ) -> {model_name} | None:
        \"\"\"Return one result or None (raises if many).\"\"\"
        ...

    @overload
    async def update(
        self,
        *,
        returning: Literal[True],
        client: Any | None = ...,
        using: str | None = ...,
        **values: Any,
    ) -> list[{model_name}]:
        ...

    @overload
    async def update(
        self,
        *,
        returning: Literal[False] = ...,
        client: Any | None = ...,
        using: str | None = ...,
        **values: Any,
    ) -> int:
        ...

    async def increment(
        self,
        field: str,
        by: int | float = 1,
        *,
        client: Any | None = None,
        using: str | None = None,
    ) -> int:
        \"\"\"Atomically increment a field value.\"\"\"
        ...

    async def delete(
        self,
        *,
        client: Any | None = None,
        using: str | None = None,
    ) -> int:
        \"\"\"Delete matching objects.\"\"\"
        ...
"""

    # The Manager's create deliberately narrows **data to known model fields.
    # Its ty suppression uses the current rule name as of ty 0.0.100, renamed
    # from invalid-method-override to incompatible-method-override.
    manager_class = f"""
class {model_name}Manager(QueryManager[{model_name}]):
    \"\"\"Type-safe Manager for {model_name} model.\"\"\"

    # Query building methods (sync, return Query)

    def query(self) -> {model_name}Query:
        \"\"\"Return a Query builder for this model.\"\"\"
        ...

    def filter(
        self,
        *args: Any,
{filter_params}
    ) -> {model_name}Query:
        \"\"\"Filter by Q-expressions or field lookups.\"\"\"
        ...

    def exclude(
        self,
        *args: Any,
{filter_params}
    ) -> {model_name}Query:
        \"\"\"Exclude objects matching field lookups.\"\"\"
        ...

    def values(self, *fields: {field_literal}) -> ValuesQuery[{model_name}]:
        \"\"\"Return dicts instead of models.\"\"\"
        ...

    @overload
    def values_list(self, field: {field_literal}, /, *, flat: Literal[True]) -> FlatValuesListQuery[{model_name}]:
        ...

    @overload
    def values_list(self, *fields: {field_literal}, flat: Literal[False] = ...) -> ValuesListQuery[{model_name}]:
        ...

    @overload
    def values_list(self, *fields: {field_literal}, flat: bool) -> ValuesListQuery[{model_name}] | FlatValuesListQuery[{model_name}]:
        ...

    def distinct(self, distinct: bool = True) -> {model_name}Query:
        \"\"\"Return distinct results.\"\"\"
        ...

    def join(self, *paths: str) -> {model_name}Query:
        \"\"\"Perform LEFT JOIN for relations.\"\"\"
        ...

    def prefetch(self, *paths: str) -> {model_name}Query:
        \"\"\"Prefetch related objects (separate queries).\"\"\"
        ...

    def for_update(self) -> {model_name}Query:
        \"\"\"Add FOR UPDATE lock to query.\"\"\"
        ...

    def for_share(self) -> {model_name}Query:
        \"\"\"Add FOR SHARE lock to query.\"\"\"
        ...

    def order_by(self, *fields: {order_by_literal}) -> {model_name}Query:
        \"\"\"Order results by fields ("?" = random order).\"\"\"
        ...

    def limit(self, value: int) -> {model_name}Query:
        \"\"\"Limit number of results.\"\"\"
        ...

    def offset(self, value: int) -> {model_name}Query:
        \"\"\"Skip first value results.\"\"\"
        ...

    def annotate(self, **annotations: Any) -> {model_name}Query:
        \"\"\"Add computed fields using aggregate functions.\"\"\"
        ...

    # Terminal methods (async, execute query)

    async def get(
        self,
        *,
        client: Any | None = None,
        using: str | None = None,
{filter_params}
    ) -> {model_name}:
        \"\"\"Get single object matching lookups.\"\"\"
        ...

    async def get_or_none(
        self,
        *,
        client: Any | None = None,
        using: str | None = None,
{filter_params}
    ) -> {model_name} | None:
        \"\"\"Get object or None if not found.\"\"\"
        ...

    async def get_or_create(
        self,
        *,
        defaults: dict[str, Any] | None = None,
        client: Any | None = None,
        using: str | None = None,
{filter_params}
    ) -> tuple[{model_name}, bool]:
        \"\"\"Get object or create if not found. Returns (object, created).\"\"\"
        ...

    async def update_or_create(
        self,
        *,
        defaults: dict[str, Any] | None = None,
        client: Any | None = None,
        using: str | None = None,
{filter_params}
    ) -> tuple[{model_name}, bool]:
        \"\"\"Get object, create if missing, or update it when defaults are provided.\"\"\"
        ...

    async def exists(
        self,
        *,
        client: Any | None = None,
        using: str | None = None,
    ) -> bool:
        \"\"\"Check if any records exist.\"\"\"
        ...

    @overload
    async def all(
        self,
        *,
        client: Any | None = ...,
        using: str | None = ...,
        mode: Literal["models"] = ...,
    ) -> list[{model_name}]:
        ...

    @overload
    async def all(
        self,
        *,
        client: Any | None = ...,
        using: str | None = ...,
        mode: str,
    ) -> Any:
        ...

    async def first(
        self,
        *,
        client: Any | None = None,
        using: str | None = None,
    ) -> {model_name} | None:
        \"\"\"Get first object.\"\"\"
        ...

    async def last(
        self,
        *,
        client: Any | None = None,
        using: str | None = None,
    ) -> {model_name} | None:
        \"\"\"Get last object.\"\"\"
        ...

    async def count(
        self,
        *,
        client: Any | None = None,
        using: str | None = None,
    ) -> int:
        \"\"\"Count all objects.\"\"\"
        ...

    async def sum(
        self,
        field: str,
        *,
        client: Any | None = None,
        using: str | None = None,
    ) -> Any:
        \"\"\"Calculate sum of field values.\"\"\"
        ...

    async def avg(
        self,
        field: str,
        *,
        client: Any | None = None,
        using: str | None = None,
    ) -> Any:
        \"\"\"Calculate average of field values.\"\"\"
        ...

    async def max(
        self,
        field: str,
        *,
        client: Any | None = None,
        using: str | None = None,
    ) -> Any:
        \"\"\"Get maximum field value.\"\"\"
        ...

    async def min(
        self,
        field: str,
        *,
        client: Any | None = None,
        using: str | None = None,
    ) -> Any:
        \"\"\"Get minimum field value.\"\"\"
        ...

    async def create(  # type: ignore[override]  # ty: ignore[incompatible-method-override]
        self,
        *,
        instance: {model_name} | None = None,
        client: Any | None = None,
        using: str | None = None,
        _skip_hooks: bool = False,
{create_params}
    ) -> {model_name}:
        \"\"\"Create new object.\"\"\"
        ...

    async def bulk_create(
        self,
        objects: Iterable[{model_name} | dict[str, Any]],
        *,
        batch_size: int | None = None,
        client: Any | None = None,
        using: str | None = None,
    ) -> list[{model_name}]:
        \"\"\"Bulk create objects.\"\"\"
        ...

    async def bulk_update(
        self,
        objects: Iterable[{model_name}],
        fields: Iterable[str],
        *,
        client: Any | None = None,
        using: str | None = None,
    ) -> int:
        \"\"\"Bulk update objects.\"\"\"
        ...
"""

    return queryset_class + manager_class


def _build_stub(file_path: Path, models: list[type[Model]]) -> str:
    """Build .pyi stub content for a source file that contains Model classes.

    The stub shadows the whole module: preserve its public declarations and
    reexports, along with private dependencies required by their signatures.
    Runtime assignment expressions become typed declarations.
    """
    source = file_path.read_text()
    tree = ast.parse(source)

    user_imports, function_stubs, class_defs = _extract_top_level_copyable(tree)
    origins = _import_origins(user_imports)
    module = sys.modules.get(models[0].__module__) if models else None
    exports = _module_export_inventory(tree, user_imports, origins, module)

    model_names = {m.__name__ for m in models}
    custom_methods_by_model: dict[str, list[str]] = {
        node.name: _extract_custom_methods(node)
        for node in class_defs
        if node.name in model_names
    }

    body_parts: list[str] = []
    if exports.all_declaration is not None:
        body_parts.extend([exports.all_declaration, ""])
    for function_stub in function_stubs:
        body_parts.append(function_stub)
        body_parts.append("")
    for model in models:
        body_parts.append(
            _generate_model_class_stub(
                model,
                custom_methods_by_model.get(model.__name__, []),
            )
        )
        body_parts.append("")
        body_parts.append(generate_model_stub(model))
        body_parts.append("")
    core_body = "\n".join(body_parts)

    # Include public classes/constants and follow private dependencies to a
    # fixpoint (e.g. a public helper whose annotation uses a private enum).
    support_defs = [node for node in class_defs if node.name not in model_names]
    support_stubs: dict[str, str] = {}
    declarations: dict[str, str] = {}
    used = set(_IDENT_RE.findall(core_body))
    changed = True
    while changed:
        changed = False
        for node in support_defs:
            if node.name in support_stubs:
                continue
            if node.name in exports.names or node.name in used:
                stub_src = _stub_class_def(node)
                support_stubs[node.name] = stub_src
                used.update(_IDENT_RE.findall(stub_src))
                changed = True
        for name, declaration in exports.declarations.items():
            if name not in declarations and (name in exports.names or name in used):
                declarations[name] = declaration
                used.update(_IDENT_RE.findall(declaration))
                changed = True

    support_parts: list[str] = []
    for node in support_defs:
        if node.name in support_stubs:
            support_parts.append(support_stubs[node.name])
            support_parts.append("")
    body = "\n".join([*declarations.values(), "", *support_parts, core_body])

    parts: list[str] = [
        "# Auto-generated by oxyde generate-stubs",
        "# DO NOT EDIT - This file will be overwritten",
        "",
        *_assemble_imports(user_imports, body, exports.names, exports.local),
        "",
        body,
    ]
    return "\n".join(parts)


def generate_stub_for_file(file_path: Path) -> None:
    """Generate .pyi stub file for models in a Python file."""
    # Import the module to get model classes
    import importlib.util

    spec = importlib.util.spec_from_file_location("temp_module", file_path)
    if spec is None or spec.loader is None:
        print(f"Could not load module from {file_path}")
        return

    module = importlib.util.module_from_spec(spec)
    sys.modules["temp_module"] = module
    spec.loader.exec_module(module)

    # Find all Model subclasses in the module
    models = []
    for name in dir(module):
        obj = getattr(module, name)
        if (
            inspect.isclass(obj)
            and issubclass(obj, Model)
            and obj is not Model
            and getattr(obj, "_is_table", False)
        ):
            models.append(obj)

    if not models:
        print(f"No table models found in {file_path}")
        return

    stub_path = file_path.with_suffix(".pyi")
    stub_path.write_text(_build_stub(file_path, models))
    print(f"Generated stub: {stub_path}")


def generate_stubs_for_models(
    models: list[type[Model]] | None = None,
) -> dict[Path, str]:
    """
    Generate stubs for models and return mapping of file paths to stub content.

    Args:
        models: List of model classes. If None, uses all registered tables.

    Returns:
        Dict mapping source file paths to stub content
    """
    if models is None:
        models = list(registered_tables().values())

    # Group models by source file
    file_models: dict[Path, list[type[Model]]] = {}
    for model in models:
        if not getattr(model, "_is_table", False):
            continue

        source_file = inspect.getfile(model)
        file_path = Path(source_file)

        if file_path not in file_models:
            file_models[file_path] = []
        file_models[file_path].append(model)

    return {
        file_path.with_suffix(".pyi"): _build_stub(file_path, file_model_list)
        for file_path, file_model_list in file_models.items()
    }


def write_stubs(stub_mapping: dict[Path, str]) -> None:
    """Write stub files to disk."""
    for stub_path, content in stub_mapping.items():
        stub_path.write_text(content)
        print(f"Generated stub: {stub_path}")


__all__ = [
    "generate_model_stub",
    "generate_stub_for_file",
    "generate_stubs_for_models",
    "write_stubs",
]
