import inspect
import subprocess
import sys
from types import ModuleType
from typing import Any
from unittest.mock import AsyncMock, Mock

import pytest

from oxyde.core import wrapper
from oxyde.core.wrapper import _load_functions


@pytest.mark.asyncio
async def test_stale_core_uses_raising_stubs(monkeypatch, capsys):
    stale_core = ModuleType("oxyde._oxyde_core")
    stale_execute = AsyncMock()
    stale_render = Mock()
    stale_core.__dict__.update(
        __abi_version__=1, execute=stale_execute, render_sql_debug=stale_render
    )
    monkeypatch.setitem(sys.modules, "oxyde._oxyde_core", stale_core)

    functions = _load_functions()

    warning = capsys.readouterr().err
    assert "ABI version mismatch: expected 2, got 1" in warning
    assert "Reinstall oxyde-postgres, or run make build-core-dev" in warning
    with pytest.raises(RuntimeError, match="missing 'execute'"):
        await functions["execute"]("default", b"")
    with pytest.raises(RuntimeError, match="missing 'render_sql_debug'"):
        functions["render_sql_debug"](b"")
    stale_execute.assert_not_called()
    stale_render.assert_not_called()


def _old_core_explain(calls: list[dict[str, Any]]):
    async def explain(pool_name, ir_bytes, analyze=None, format=None):
        calls.append({"analyze": analyze, "format": format})
        return "plan"

    return explain


@pytest.mark.asyncio
async def test_explain_io_on_old_core_asks_for_upgrade(monkeypatch):
    calls: list[dict[str, Any]] = []
    monkeypatch.setitem(wrapper._exports, "explain_query", _old_core_explain(calls))

    with pytest.raises(RuntimeError, match="Reinstall oxyde-postgres"):
        await wrapper.explain_query("default", b"", analyze=True, io=True)

    assert calls == []


@pytest.mark.asyncio
async def test_explain_without_io_on_old_core_omits_keyword(monkeypatch):
    calls: list[dict[str, Any]] = []
    monkeypatch.setitem(wrapper._exports, "explain_query", _old_core_explain(calls))

    assert await wrapper.explain_query("default", b"", analyze=True) == "plan"

    assert calls == [{"analyze": True, "format": "text"}]


@pytest.mark.asyncio
async def test_explain_io_fails_closed_without_signature(monkeypatch):
    class Unintrospectable:
        called = False

        @property
        def __signature__(self):
            raise ValueError("no signature")

        async def __call__(self, *args: Any, **kwargs: Any) -> Any:
            self.called = True

    native = Unintrospectable()
    monkeypatch.setitem(wrapper._exports, "explain_query", native)

    with pytest.raises(RuntimeError, match="Reinstall oxyde-postgres"):
        await wrapper.explain_query("default", b"", analyze=True, io=True)

    assert not native.called


def test_native_explain_signature_accepts_io():
    core = pytest.importorskip("oxyde._oxyde_core")

    assert "io" in inspect.signature(core.explain).parameters
    assert wrapper._accepts_keyword(core.explain, "io")


def test_native_types_report_bundled_module():
    assert wrapper.IntegrityError.__module__ == "oxyde._oxyde_core"
    assert wrapper.SchemaReplay.__module__ == "oxyde._oxyde_core"


def test_import_oxyde_leaves_top_level_extension_unloaded():
    subprocess.run(
        [sys.executable, "-c", "import sys, oxyde; assert '_oxyde_core' not in sys.modules"],
        check=True,
    )
