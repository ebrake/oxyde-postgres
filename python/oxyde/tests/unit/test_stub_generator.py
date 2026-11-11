"""Tests for .pyi stub generation utilities."""

from __future__ import annotations

import ast
import sys
import types
from datetime import date, datetime, time
from decimal import Decimal
from uuid import UUID

import pytest

from oxyde import Field, Model
from oxyde.codegen.stub_generator import (
    _assemble_imports,
    _build_stub,
    _extract_custom_methods,
    _extract_top_level_copyable,
    _generate_create_params,
    _generate_filter_params,
    _get_python_type_name,
    _stub_class_def,
    generate_model_stub,
)


class TestGetPythonTypeName:
    """Test _get_python_type_name returns valid type annotation strings."""

    @pytest.mark.parametrize(
        ("python_type", "expected"),
        [
            (str, "str"),
            (int, "int"),
            (float, "float"),
            (bool, "bool"),
            (bytes, "bytes"),
            (datetime, "datetime"),
            (date, "date"),
            (time, "time"),
            (Decimal, "Decimal"),
            (UUID, "UUID"),
        ],
    )
    def test_builtin_and_stdlib_types(self, python_type: type, expected: str):
        """Explicitly handled types return their name."""
        assert _get_python_type_name(python_type) == expected

    @pytest.mark.parametrize(
        ("python_type", "expected"),
        [
            (dict, "dict"),
            (list, "list"),
            (set, "set"),
            (tuple, "tuple"),
            (frozenset, "frozenset"),
        ],
    )
    def test_bare_container_types(self, python_type: type, expected: str):
        """Bare container types return their name, not repr (e.g. not <class 'dict'>)."""
        result = _get_python_type_name(python_type)
        assert result == expected
        assert "<class" not in result

    @pytest.mark.parametrize(
        ("python_type", "expected"),
        [
            (list[str], "list[str]"),
            (dict[str, int], "dict[str, int]"),
            (tuple[int, ...], "tuple[int, ...]"),
        ],
    )
    def test_parameterized_generic_types(self, python_type: type, expected: str):
        """Parameterized generics use their string representation."""
        assert _get_python_type_name(python_type) == expected

    def test_model_subclass(self):
        """Model subclasses return their class name."""

        class MyModel(Model):
            id: int = Field(db_pk=True)

            class Meta:
                is_table = True

        assert _get_python_type_name(MyModel) == "MyModel"


class TestExtractTopLevelCopyable:
    """Test extraction of imports and function stubs from model source."""

    def test_future_import_dropped(self):
        """`from __future__ import ...` must never reach the stub (issue #43)."""
        tree = ast.parse(
            "from __future__ import annotations\nfrom pydantic import field_validator\n"
        )
        imports, functions, classes = _extract_top_level_copyable(tree)
        assert [ast.unparse(node) for node in imports] == [
            "from pydantic import field_validator"
        ]
        assert functions == []
        assert classes == []

    def test_overload_implementation_dropped(self):
        """Only @overload variants survive; the implementation body is dropped."""
        tree = ast.parse(
            "from typing import overload\n"
            "@overload\n"
            "def f(x: int) -> str: ...\n"
            "@overload\n"
            "def f(x: str) -> int: ...\n"
            "def f(x): return x\n"
        )
        _, functions, _ = _extract_top_level_copyable(tree)
        assert len(functions) == 2
        assert all("@overload" in fn for fn in functions)

    def test_class_defs_collected(self):
        """Top-level classes are returned as raw AST nodes for later copying."""
        tree = ast.parse(
            "from enum import Enum\n"
            "class Color(str, Enum):\n"
            "    RED = 'red'\n"
            "def helper() -> int: return 1\n"
        )
        _, functions, classes = _extract_top_level_copyable(tree)
        assert len(functions) == 1
        assert [node.name for node in classes] == ["Color"]

    @pytest.mark.parametrize(
        ("source", "guard"),
        [
            ("from typing import TYPE_CHECKING", "TYPE_CHECKING"),
            ("import typing", "typing.TYPE_CHECKING"),
            ("import typing as t", "t.TYPE_CHECKING"),
            ("import typing_extensions", "typing_extensions.TYPE_CHECKING"),
            ("from typing import TYPE_CHECKING as TC", "TC"),
            ("from typing_extensions import TYPE_CHECKING as TC", "TC"),
        ],
    )
    def test_type_checking_imports_collected(self, source, guard):
        tree = ast.parse(
            f"{source}\nif {guard}:\n"
            "    from .users import User as Owner\n"
            "    import decimal as numbers\n"
            "else:\n"
            "    from runtime_only import Fallback\n"
            "if runtime_condition:\n"
            "    from unrelated import Other\n"
            "def helper():\n"
            "    from local import Local\n"
        )
        imports, _, _ = _extract_top_level_copyable(tree)
        assert [ast.unparse(node) for node in imports] == [
            source,
            "from .users import User as Owner",
            "import decimal as numbers",
        ]

    @pytest.mark.parametrize(
        "source",
        ["from .typing import TYPE_CHECKING", "from other import TYPE_CHECKING"],
    )
    def test_unrelated_type_checking_names_are_not_guards(self, source):
        tree = ast.parse(
            f"{source}\nif TYPE_CHECKING:\n    from hidden import Hidden\n"
        )
        imports, _, _ = _extract_top_level_copyable(tree)
        assert [ast.unparse(node) for node in imports] == [source]


class TestStubClassDef:
    """Test stubbing of supporting top-level classes (enums, helpers)."""

    def test_enum_members_kept_verbatim(self):
        """Enum member values are the type interface — they must survive."""
        tree = ast.parse(
            "class Color(str, Enum):\n"
            '    """Doc."""\n'
            "    RED = 'red'\n"
            "    GREEN = 'green'\n"
            "    def label(self) -> str:\n"
            "        return self.value.upper()\n"
        )
        class_def = tree.body[0]
        assert isinstance(class_def, ast.ClassDef)
        stub = _stub_class_def(class_def)
        assert "class Color(str, Enum):" in stub
        assert "RED = 'red'" in stub
        assert "GREEN = 'green'" in stub
        assert "def label(self) -> str:" in stub
        assert "self.value.upper()" not in stub

    def test_nested_class_and_overloads_stubbed(self):
        """Nested classes recurse; overload implementations are dropped."""
        tree = ast.parse(
            "class Helper:\n"
            "    class Inner:\n"
            "        x: int = 0\n"
            "    @overload\n"
            "    def f(self, x: int) -> str: ...\n"
            "    @overload\n"
            "    def f(self, x: str) -> int: ...\n"
            "    def f(self, x): return x\n"
        )
        class_def = tree.body[0]
        assert isinstance(class_def, ast.ClassDef)
        stub = _stub_class_def(class_def)
        assert "class Inner:" in stub
        assert "x: int = 0" in stub
        assert stub.count("def f") == 2
        assert "return x" not in stub


class TestAssembleImports:
    """Test the stub import block assembly (dedupe, unused-drop, merging)."""

    @staticmethod
    def _imports(source: str) -> list[ast.Import | ast.ImportFrom]:
        imports, _, _ = _extract_top_level_copyable(ast.parse(source))
        return imports

    def test_skeleton_names_deduped_and_unused_dropped(self):
        """User re-imports of skeleton names and unused names are dropped."""
        imports = self._imports(
            "from datetime import datetime\n"
            "from typing import Any\n"
            "from uuid import UUID, uuid4\n"
            "from oxyde import Field, Model\n"
        )
        body = "created_at: datetime\nmeta: dict[str, Any]\nslug: UUID\nx: Any\n"
        lines = _assemble_imports(imports, body)
        joined = "\n".join(lines)
        assert joined.count("from datetime import") == 1
        assert joined.count("from uuid import") == 1
        assert "uuid4" not in joined
        assert "Field" not in joined
        assert "from oxyde import Model" in lines

    def test_used_third_party_import_survives(self):
        """Imports the stub body still references are kept (cross-module FK)."""
        imports = self._imports(
            "from pydantic import model_validator\nfrom sibling import Owner\n"
        )
        body = "@model_validator(mode='after')\nowner: Owner | None\n"
        lines = _assemble_imports(imports, body)
        assert "from pydantic import model_validator" in lines
        assert "from sibling import Owner" in lines

    def test_stdlib_user_import_merged_into_skeleton_block(self):
        """A user stdlib from-import merges with the skeleton's module line."""
        imports = self._imports("from typing import overload\n")
        body = "@overload\ndef f(x: int) -> str: ...\nx: Any\ny: ClassVar[int]\n"
        lines = _assemble_imports(imports, body)
        typing_lines = [ln for ln in lines if ln.startswith("from typing import")]
        assert typing_lines == ["from typing import Any, ClassVar, overload"]

    def test_explicit_reexports_and_star_imports_survive(self):
        imports = self._imports(
            "from .constants import EXPORTED\n"
            "from datetime import datetime\n"
            "from .helpers import Helper as Helper\n"
            "from .other import *\n"
        )
        lines = _assemble_imports(imports, "", {"EXPORTED", "Helper", "datetime"})
        assert "from .constants import EXPORTED as EXPORTED" in lines
        assert "from datetime import datetime as datetime" in lines
        assert "from .helpers import Helper as Helper" in lines
        assert "from .other import *" in lines

    @pytest.mark.parametrize(
        ("source", "body", "expected"),
        [
            (
                "from datetime import *\n",
                "value: datetime",
                ["from datetime import *", "from datetime import datetime"],
            ),
            (
                "from .a import Foo\nfrom typing import TYPE_CHECKING\n"
                "if TYPE_CHECKING:\n    from .a import Foo, Bar\n",
                "value: Foo | Bar",
                ["from .a import Foo, Bar"],
            ),
            (
                "from third_party import Foo\nfrom third_party import Foo, Bar\n",
                "value: Foo | Bar",
                ["from third_party import Foo, Bar"],
            ),
            (
                "import package as pkg\nimport package as pkg\n",
                "value: pkg.Foo",
                ["import package as pkg"],
            ),
            (
                "from . import Foo\nfrom . import Foo, Bar\n",
                "value: Foo | Bar",
                ["from . import Foo, Bar"],
            ),
        ],
    )
    def test_imports_merged_without_combining_stars(self, source, body, expected):
        lines = _assemble_imports(self._imports(source), body)
        for line in expected:
            assert lines.count(line) == 1
        ast.parse("\n".join([*lines, body]))
        if any(" import Foo" in line for line in expected):
            assert sum(" import Foo" in line for line in lines) == 1

    def test_reexport_does_not_force_an_unrelated_skeleton_import(self):
        imports = self._imports("from .constants import UUID\n")
        lines = _assemble_imports(imports, "", {"UUID"})
        assert "from .constants import UUID as UUID" in lines
        assert not any(line.startswith("from uuid import") for line in lines)
        ast.parse("\n".join(lines))

    @pytest.mark.parametrize(
        "source", ["import sibling", "from sibling import Related"]
    )
    def test_lazy_flag_cleared_on_copy(self, source, monkeypatch):
        imports = self._imports(source)
        setattr(imports[0], "is_lazy", True)
        unparse = ast.unparse

        def check_eager(node):
            assert getattr(node, "is_lazy") is False
            return unparse(node)

        monkeypatch.setattr(ast, "unparse", check_eager)
        _assemble_imports(imports, "value: sibling.Related | Related")
        assert getattr(imports[0], "is_lazy") is True

    @pytest.mark.skipif(
        sys.version_info < (3, 15), reason="requires native lazy imports"
    )
    def test_native_lazy_imports_render_as_ordinary_imports(self):
        imports = self._imports(
            "lazy import sibling\nlazy from sibling import Related\n"
        )
        lines = _assemble_imports(imports, "value: sibling.Related | Related")
        assert "import sibling" in lines
        assert "from sibling import Related" in lines
        assert all(not line.startswith("lazy ") for line in lines)


class TestModuleExports:
    @staticmethod
    def _stub(tmp_path, source):
        path = tmp_path / "module.py"
        path.write_text(source)
        stub = _build_stub(path, [])
        ast.parse(stub)
        return stub

    @pytest.mark.parametrize(
        ("source", "declaration"),
        [
            ("from typing import TypeVar\nT = TypeVar('T')", "T = TypeVar('T')"),
            ("import typing as t\nT = t.TypeVar('T')", "T = t.TypeVar('T')"),
            ("from typing_extensions import TypeVar as TV\nT = TV('T')", "T = TV('T')"),
            ("from typing import ParamSpec\nP = ParamSpec('P')", "P = ParamSpec('P')"),
            (
                "from typing import TypeVarTuple\nTs = TypeVarTuple('Ts')",
                "Ts = TypeVarTuple('Ts')",
            ),
            (
                "from typing import NewType\nId = NewType('Id', int)",
                "Id = NewType('Id', int)",
            ),
            (
                "from typing_extensions import TypeAliasType\nIds = TypeAliasType('Ids', list[int])",
                "Ids = TypeAliasType('Ids', list[int])",
            ),
            (
                "from typing import NamedTuple\nPair = NamedTuple('Pair', [('x', int)])",
                "Pair = NamedTuple('Pair', [('x', int)])",
            ),
            (
                "from typing import TypedDict\nRow = TypedDict('Row', {'id': int})",
                "Row = TypedDict('Row', {'id': int})",
            ),
            (
                "from enum import Enum\nStatus = Enum('Status', 'A B')",
                "Status = Enum('Status', 'A B')",
            ),
            (
                "from enum import Enum as E\nStatus = E('Status', 'A B')",
                "Status = E('Status', 'A B')",
            ),
            (
                "import enum as e\nStatus = e.Enum('Status', 'A B')",
                "Status = e.Enum('Status', 'A B')",
            ),
            ("Ids = list[int]", "Ids = list[int]"),
            ("Number = int | str | None", "Number = int | str | None"),
            ("class Helper: pass\nHelpers = list[Helper]", "Helpers = list[Helper]"),
            (
                "from typing import TypeAlias\nJsonDict: TypeAlias = dict[str, int]",
                "JsonDict: TypeAlias = dict[str, int]",
            ),
            (
                "import typing_extensions as t\nJsonDict: t.TypeAlias = dict[str, int]",
                "JsonDict: t.TypeAlias = dict[str, int]",
            ),
            ("type Ids = list[int]", "type Ids = list[int]"),
            ("type Items[T] = list[T]", "type Items[T] = list[T]"),
        ],
    )
    def test_type_level_declarations_preserved(self, tmp_path, source, declaration):
        stub = self._stub(tmp_path, source + "\n")
        assert declaration in stub
        name = declaration.removeprefix("type ").split()[0].split(":")[0].split("[")[0]
        assert f"{name}: Any" not in stub

    @pytest.mark.parametrize("factory", ["IntEnum", "StrEnum", "Flag", "IntFlag"])
    def test_enum_factories_preserved(self, tmp_path, factory):
        stub = self._stub(
            tmp_path,
            f"from enum import {factory}\nStatus = {factory}('Status', 'A B')\n",
        )
        assert f"Status = {factory}('Status', 'A B')" in stub
        assert f"from enum import {factory}" in stub

    @pytest.mark.parametrize(
        ("source", "expected", "absent"),
        [
            (
                "import functools\ndef cached(x: int) -> int: return x\ncached = functools.cache(cached)\n",
                "def cached(x: int) -> int:",
                "cached: Any",
            ),
            (
                "from functools import total_ordering\nclass Helper: pass\nHelper = total_ordering(Helper)\n",
                "class Helper:",
                "Helper: Any",
            ),
            (
                "from helpers import Helper\nHelper = wrap(Helper)\ndef use(x: Helper) -> Helper: return x\n",
                "from helpers import Helper",
                "Helper: Any",
            ),
            (
                "from .typing import TypeVar\nT = TypeVar('T')\n",
                "T: Any",
                "T = TypeVar",
            ),
            ("DATA = {'key': 1}\nVALUE = DATA['key']\n", "VALUE: Any", "VALUE = DATA"),
            (
                "LEFT = 1\nRIGHT = 2\nVALUE = LEFT | RIGHT\n",
                "VALUE: Any",
                "VALUE = LEFT",
            ),
        ],
    )
    def test_runtime_assignments_do_not_create_type_bindings(
        self, tmp_path, source, expected, absent
    ):
        stub = self._stub(tmp_path, source)
        assert expected in stub
        assert absent not in stub

    @pytest.mark.parametrize("name", ["time", "date", "datetime", "Decimal", "UUID"])
    def test_local_declarations_do_not_force_skeleton_imports(self, tmp_path, name):
        stub = self._stub(tmp_path, f"{name} = 5\n")
        assert f"{name}: int" in stub
        assert not any(
            isinstance(node, ast.ImportFrom)
            and any(alias.name == name for alias in node.names)
            for node in ast.parse(stub).body
        )

    @pytest.mark.parametrize(
        "source",
        [
            "import typing as t\nif t.TYPE_CHECKING:\n    from .helpers import Helper\n",
            "import typing_extensions\nif typing_extensions.TYPE_CHECKING:\n    from .helpers import Helper\n",
            "from typing import TYPE_CHECKING as TC\nif TC:\n    from .helpers import Helper\n",
        ],
    )
    def test_guarded_dependencies_survive_without_guard_reexports(
        self, tmp_path, source
    ):
        stub = self._stub(tmp_path, source + "def use(x: Helper) -> Helper: return x\n")
        assert "from .helpers import Helper" in stub
        assert "TYPE_CHECKING" not in stub
        assert "TC as TC" not in stub

    def test_all_caps_reexports_exclude_runtime_types_and_callables(
        self, tmp_path, monkeypatch
    ):
        class Entry(Model):
            id: int = Field(db_pk=True)

        module = types.ModuleType("stub_runtime")
        module.__dict__.update(
            UUID=UUID, CONST=1, FUNC=lambda: None, BUILTIN=len, MODULE=sys
        )
        monkeypatch.setitem(sys.modules, module.__name__, module)
        monkeypatch.setattr(Entry, "__module__", module.__name__)
        path = tmp_path / "module.py"
        path.write_text(
            "from uuid import UUID\n"
            "from helpers import CONST, FUNC, BUILTIN, MODULE\n"
            "def use(value: UUID) -> UUID: return value\n"
            "class Entry(Model):\n    id: int\n"
        )
        stub = _build_stub(path, [Entry])
        ast.parse(stub)
        assert "from uuid import UUID\n" in stub
        assert "UUID as UUID" not in stub
        assert "from helpers import CONST as CONST" in stub
        assert all(name not in stub for name in ("FUNC", "BUILTIN", "MODULE"))

    def test_all_caps_reexports_fall_back_without_runtime_module(self, tmp_path):
        stub = self._stub(tmp_path, "from constants import DEFAULT_INTERVAL\n")
        assert "from constants import DEFAULT_INTERVAL as DEFAULT_INTERVAL" in stub

    def test_constants_become_declarations_without_executable_values(self, tmp_path):
        stub = self._stub(
            tmp_path,
            "from decimal import Decimal\n"
            "from typing import Final\n"
            "UNSPECIFIED_ENUM_VALUE = -1\n"
            "ANNOTATED: Decimal = Decimal('0')\n"
            "LIMIT: Final = 10\n"
            "DYNAMIC = create_value()\n"
            "_private = create_private_value()\n",
        )
        assert "UNSPECIFIED_ENUM_VALUE: int" in stub
        assert "ANNOTATED: Decimal" in stub
        assert "LIMIT: Final[int]" in stub
        assert "DYNAMIC: Any" in stub
        assert "create_value" not in stub
        assert "create_private_value" not in stub
        assert "from decimal import Decimal" in stub

    @pytest.mark.parametrize(
        ("value", "annotation"),
        [
            ("True", "bool"),
            ("1.5", "float"),
            ("1j", "complex"),
            ("'text'", "str"),
            ("b'bytes'", "bytes"),
            ("None", "None"),
            ("[1, 2]", "list[int]"),
            ("[]", "list[Any]"),
            ("{'a': 1}", "dict[str, int]"),
            ("{1, 2}", "set[int]"),
            ("(1, 'a')", "tuple[int, str]"),
            ("()", "tuple[()]"),
            ("factory()", "Any"),
        ],
    )
    def test_literal_types(self, tmp_path, value, annotation):
        stub = self._stub(tmp_path, f"VALUE = {value}\n")
        assert f"VALUE: {annotation}" in stub

    def test_all_preserved_and_aliased_imports_exported(self, tmp_path):
        stub = self._stub(
            tmp_path,
            "from .constants import Original as ExportedName, UNUSED\n"
            "__all__ = ['ExportedName']\n"
            "__all__ += ['_hidden']\n"
            "__all__.extend(['Helper'])\n"
            "__all__.append('VALUE')\n"
            "_hidden: int = compute()\n"
            "VALUE = 1\n"
            "class Helper: pass\n",
        )
        assert "__all__ = ['ExportedName', '_hidden', 'Helper', 'VALUE']" in stub
        assert "from .constants import Original as ExportedName" in stub
        assert "_hidden: int" in stub
        assert "UNUSED" not in stub

    def test_public_classes_and_private_dependencies_preserved(self, tmp_path):
        stub = self._stub(
            tmp_path,
            "from .constants import DEFAULT_INTERVAL, helper\n"
            "class _PrivateBase: pass\n"
            "class PublicHelper(_PrivateBase):\n"
            "    def value(self) -> int: return 1\n"
            "class _Unused: pass\n",
        )
        assert "class PublicHelper(_PrivateBase):" in stub
        assert "class _PrivateBase:" in stub
        assert "class _Unused:" not in stub
        assert "from .constants import DEFAULT_INTERVAL as DEFAULT_INTERVAL" in stub
        assert "import helper" not in stub

    def test_private_alias_dependencies_in_forward_references_preserved(self, tmp_path):
        stub = self._stub(
            tmp_path,
            "class _Helper: pass\n"
            "class _Unused: pass\n"
            "type _Items = list[_Helper]\n"
            "def use(value: '_Items') -> '_Items': return value\n",
        )
        assert "type _Items = list[_Helper]" in stub
        assert "class _Helper:" in stub
        assert "class _Unused:" not in stub


class TestGenerateFilterParams:
    """Test lookup parameter generation."""

    def test_range_lookup_takes_a_pair(self):
        """`__range`/`__between` accept a (lo, hi) pair as tuple or list."""

        class Ranged(Model):
            id: int = Field(db_pk=True)

        params = _generate_filter_params(Ranged)
        assert "id__range: tuple[int, int] | list[int] | None = None," in params
        assert "id__between: tuple[int, int] | list[int] | None = None," in params

    def test_in_lookup_takes_any_iterable(self):
        """`__in` accepts any iterable at runtime (tuple, set, generator)."""

        class Inny(Model):
            id: int = Field(db_pk=True)

        params = _generate_filter_params(Inny)
        assert "id__in: Iterable[int] | None = None," in params

    def test_date_part_lookups_match_runtime_shapes(self):
        """Runtime wants year=int, month=(y, m), day=(y, m, d)."""

        class Dated(Model):
            id: int = Field(db_pk=True)
            created: datetime = Field(default_factory=datetime.now)

        params = _generate_filter_params(Dated)
        assert "created__year: int | None = None," in params
        assert "created__month: tuple[int, int] | list[int] | None = None," in params
        assert "created__day: tuple[int, int, int] | list[int] | None = None," in params

    def test_time_fields_get_comparisons_but_no_date_parts(self):
        """A time-of-day has no calendar part: the runtime rejects
        year/month/day for it, so the stub must not offer them."""

        class Timed(Model):
            id: int = Field(db_pk=True)
            starts_at: time = Field(default_factory=lambda: time(9, 0))

        params = _generate_filter_params(Timed)
        assert "starts_at__gt: time | None = None," in params
        assert (
            "starts_at__between: tuple[time, time] | list[time] | None = None,"
            in params
        )
        assert "starts_at__year" not in params
        assert "starts_at__month" not in params
        assert "starts_at__day" not in params

    def test_array_and_json_fields_get_sequence_typed_array_lookups(self):
        class Tagged(Model):
            id: int = Field(db_pk=True)
            tags: list[str] | None = None
            ids: list[UUID] = Field(default_factory=list)
            maybe: list[int | None] = Field(default_factory=list)
            payload: dict | list | None = None
            note: str = Field(default="", db_type="JSON")

            class Meta:
                is_table = True  # db_type is only read from table metadata

        params = _generate_filter_params(Tagged)
        for lookup in ("contains", "contained_by", "overlap"):
            assert (
                f"tags__{lookup}: list[str] | list[str | None] | list[None]"
                " | tuple[str | None, ...] | None = None," in params
            )
            assert (
                f"ids__{lookup}: list[UUID] | list[UUID | None] | list[None]"
                " | tuple[UUID | None, ...] | None = None," in params
            )
            assert (
                f"maybe__{lookup}: list[int] | list[int | None] | list[None]"
                " | tuple[int | None, ...] | None = None," in params
            )
            assert (
                f"payload__{lookup}: list[Any] | tuple[Any, ...] | None = None,"
                in params
            )
        # A str field stored as JSON: `contains` also keeps its LIKE string form.
        assert (
            "note__contains: str | list[Any] | tuple[Any, ...] | None = None," in params
        )
        assert "note__overlap: list[Any] | tuple[Any, ...] | None = None," in params
        assert "note__icontains: str | None = None," in params
        assert "tags__icontains" not in params
        assert "id__overlap" not in params

    def test_uuid_fields_get_typed_ranges_without_string_lookups(self):
        class Keyed(Model):
            id: UUID = Field(db_pk=True)

        params = _generate_filter_params(Keyed)
        assert "id__gt: UUID | None = None," in params
        assert "id__range: tuple[UUID, UUID] | list[UUID] | None = None," in params
        assert "id__between: tuple[UUID, UUID] | list[UUID] | None = None," in params
        assert "id__contains" not in params
        assert "id__year" not in params

    def test_time_lookup_table_matches_builders(self):
        """The runtime permission table itself must not advertise date parts
        for time fields (the year/month/day builders raise for them)."""
        from oxyde.models.lookups import (
            _allowed_lookups_for_meta,
            _field_spec,
            _resolve_column_meta,
        )

        class Clocked(Model):
            id: int = Field(db_pk=True)
            starts_at: time = Field(default_factory=lambda: time(9, 0))
            created: datetime = Field(default_factory=datetime.now)

        starts_at = _resolve_column_meta(Clocked, "starts_at")
        time_lookups = _allowed_lookups_for_meta(
            starts_at, _field_spec(Clocked, starts_at)
        )
        assert "gt" in time_lookups and "between" in time_lookups
        assert not {"year", "month", "day"} & set(time_lookups)

        created = _resolve_column_meta(Clocked, "created")
        dt_lookups = _allowed_lookups_for_meta(created, _field_spec(Clocked, created))
        assert {"year", "month", "day"} <= set(dt_lookups)

    def test_reserved_field_names_not_enumerated(self):
        """Fields named like service params must not produce duplicate args."""

        class Clashy(Model):
            id: int = Field(db_pk=True)
            client: str = Field(default="")
            kwargs: str = Field(default="")
            defaults: str = Field(default="")

        params = _generate_filter_params(Clashy)
        for reserved in ("client", "kwargs", "defaults"):
            assert f"\n        {reserved}:" not in f"\n{params}"
        assert params.rstrip().endswith("**kwargs: Any,")
        # The resulting signature must stay syntactically valid
        ast.parse(f"def f(\n        self,\n        *args,\n{params}\n): ...\n")

        create_params = _generate_create_params(Clashy)
        assert "\n        client:" not in f"\n{create_params}"
        ast.parse(
            "def f(\n        self,\n        *,\n        instance=None,\n"
            "        client=None,\n        using=None,\n"
            f"{create_params}\n): ...\n"
        )


class TestExtractCustomMethods:
    """Test model-method stubbing."""

    def test_class_overload_implementation_dropped(self):
        """@overload variants survive; the in-class implementation is dropped."""
        tree = ast.parse(
            "class M:\n"
            "    @overload\n"
            "    def render(self, short: bool) -> str: ...\n"
            "    @overload\n"
            "    def render(self) -> bytes: ...\n"
            "    def render(self, short=None):\n"
            "        return b''\n"
            "    def plain(self) -> int:\n"
            "        return 1\n"
        )
        class_def = tree.body[0]
        assert isinstance(class_def, ast.ClassDef)
        methods = _extract_custom_methods(class_def)
        renders = [m for m in methods if "def render" in m]
        assert len(renders) == 2
        assert all("@overload" in m for m in renders)
        assert any("def plain" in m for m in methods)


class TestAsyncGeneratorStubs:
    """Async generators must keep their synchronous calling convention in stubs."""

    def test_async_context_manager(self):
        tree = ast.parse(
            "@asynccontextmanager\n"
            "async def audited_mutation(actor: str) -> AsyncGenerator[None]:\n"
            "    yield\n"
        )
        _, functions, _ = _extract_top_level_copyable(tree)
        assert functions == [
            "@asynccontextmanager\n"
            "def audited_mutation(actor: str) -> AsyncGenerator[None]:\n"
            "    ..."
        ]
        # Stubbing must not mutate the source AST.
        assert isinstance(tree.body[0], ast.AsyncFunctionDef)
        assert isinstance(tree.body[0].body[0], ast.Expr)
        assert isinstance(tree.body[0].body[0].value, ast.Yield)

    @pytest.mark.parametrize(
        "body",
        [
            "return 1",
            "async def nested():\n        yield 1\n    return 1",
            "def nested():\n        yield 1\n    return 1",
            "nested = lambda: (yield 1)\n    return 1",
            "class Nested:\n        async def stream(self):\n            yield 1\n    return 1",
        ],
        ids=["plain", "nested_async", "nested_sync", "lambda", "class"],
    )
    def test_coroutine_stays_async(self, body):
        tree = ast.parse(f"async def count() -> int:\n    {body}\n")
        _, functions, _ = _extract_top_level_copyable(tree)
        assert functions == ["async def count() -> int:\n    ..."]

    @pytest.mark.parametrize("model_method", [False, True], ids=["support", "model"])
    def test_async_generator_method(self, model_method):
        tree = ast.parse(
            "class EventFeed:\n"
            "    async def __aiter__(self) -> AsyncIterator[Event]:\n"
            "        if ready:\n"
            "            yield event\n"
        )
        class_def = tree.body[0]
        assert isinstance(class_def, ast.ClassDef)
        if model_method:
            stub = "\n".join(_extract_custom_methods(class_def))
        else:
            stub = _stub_class_def(class_def)
        assert "def __aiter__(self) -> AsyncIterator[Event]:" in stub
        assert "async def" not in stub
        assert "yield" not in stub

    def test_type_parameters_preserved(self):
        tree = ast.parse(
            "async def stream[T](x: T) -> AsyncIterator[T]:\n"
            "    yield x\n"
        )
        _, functions, _ = _extract_top_level_copyable(tree)
        assert functions == ["def stream[T](x: T) -> AsyncIterator[T]:\n    ..."]

    def test_sync_generator_stays_sync(self):
        tree = ast.parse("def stream() -> Iterator[int]:\n    yield from values\n")
        _, functions, _ = _extract_top_level_copyable(tree)
        assert functions == ["def stream() -> Iterator[int]:\n    ..."]

    def test_async_overload_stays_async(self):
        tree = ast.parse(
            "@overload\n"
            "async def stream(x: int) -> AsyncIterator[int]: ...\n"
            "@overload\n"
            "async def stream(x: str) -> AsyncIterator[str]: ...\n"
            "async def stream(x):\n"
            "    yield x\n"
        )
        _, functions, _ = _extract_top_level_copyable(tree)
        assert len(functions) == 2
        assert all(fn.startswith("@overload\nasync def stream") for fn in functions)


class TestGenerateModelStub:
    def test_create_skip_hooks_and_override_suppression(self):
        class Event(Model):
            id: int | None = Field(default=None, db_pk=True)

            class Meta:
                is_table = True

        stub = generate_model_stub(Event)
        tree = ast.parse(stub)
        manager = next(
            node
            for node in tree.body
            if isinstance(node, ast.ClassDef) and node.name == "EventManager"
        )
        create = next(
            node
            for node in manager.body
            if isinstance(node, ast.AsyncFunctionDef) and node.name == "create"
        )
        params = dict(
            zip(
                (param.arg for param in create.args.kwonlyargs), create.args.kw_defaults
            )
        )
        default = params["_skip_hooks"]
        assert isinstance(default, ast.Constant)
        assert default.value is False
        skip_hooks = next(p for p in create.args.kwonlyargs if p.arg == "_skip_hooks")
        assert skip_hooks.annotation is not None
        assert ast.unparse(skip_hooks.annotation) == "bool"
        assert create.args.kwarg is None
        assert (
            "async def create(  # type: ignore[override]"
            "  # ty: ignore[incompatible-method-override]"
        ) in stub
