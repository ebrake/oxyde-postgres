import inspect
import subprocess
import sys
from types import ModuleType
from typing import Any
from unittest.mock import AsyncMock, Mock

import pytest

from oxyde._msgpack import msgpack
from oxyde.core import wrapper
from oxyde.core.wrapper import _load_functions
from oxyde.db import pool
from oxyde.db.pool import AsyncDatabase
from oxyde.queries._result_set import MsgpackResultSet, fetch_result


@pytest.mark.asyncio
async def test_stale_core_uses_raising_stubs(monkeypatch, capsys):
    stale_core = ModuleType("oxyde._oxyde_core")
    stale_execute = AsyncMock()
    stale_render = Mock()
    stale_core.__dict__.update(
        __abi_version__=2, execute=stale_execute, render_sql_debug=stale_render
    )
    monkeypatch.setitem(sys.modules, "oxyde._oxyde_core", stale_core)

    functions = _load_functions()

    warning = capsys.readouterr().err
    assert "ABI version mismatch: expected 3, got 2" in warning
    assert "Reinstall oxyde-postgres, or run make build-core-dev" in warning
    with pytest.raises(RuntimeError, match="missing 'execute'"):
        await functions["execute"]("default", b"")
    with pytest.raises(RuntimeError, match="missing 'render_sql_debug'"):
        functions["render_sql_debug"](b"")
    stale_execute.assert_not_called()
    stale_render.assert_not_called()
    for name in ("execute_rows", "execute_rows_in_transaction"):
        with pytest.raises(RuntimeError, match=f"missing '{name}'"):
            await functions[name]()


def test_native_rows_exports():
    core = pytest.importorskip("oxyde._oxyde_core")

    assert core.__abi_version__ == 3
    assert wrapper.execute_rows is core.execute_rows
    assert wrapper.execute_rows_in_transaction is core.execute_rows_in_transaction
    assert core.ResultRows.__module__ == "oxyde._oxyde_core"


class ExecuteOverride(AsyncDatabase):
    async def execute(self, ir):
        return await super().execute({**ir, "traced": True})


class ExecuteAndRowsOverride(ExecuteOverride):
    async def execute(self, ir):
        return await super().execute(ir)

    async def execute_rows(self, ir):
        return await super().execute_rows(ir)


class RowsOverride(AsyncDatabase):
    async def execute_rows(self, ir):
        return await super().execute_rows(ir)


class ForwardingClient:
    def __init__(self, url, *, auto_register):
        self.db = AsyncDatabase(url, auto_register=auto_register)

    def __getattr__(self, name):
        return getattr(self.db, name)

    async def execute(self, ir):
        return await self.db.execute({**ir, "traced": True})


@pytest.mark.asyncio
@pytest.mark.parametrize(
    ("client_type", "use_native_rows"),
    [
        (ExecuteOverride, False),
        (ExecuteAndRowsOverride, True),
        (ForwardingClient, False),
        (AsyncDatabase, True),
        (RowsOverride, True),
    ],
)
async def test_fetch_result_respects_execute_owner(
    monkeypatch, client_type, use_native_rows
):
    url = "postgresql://localhost/test"
    ir = {"op": "select"}
    native_rows = MsgpackResultSet([["id"], [[1]]])
    execute_rows = AsyncMock(return_value=native_rows)
    execute = AsyncMock(return_value=msgpack.packb([["id"], [[2]]]))
    monkeypatch.setattr(pool, "_execute_rows", execute_rows)
    monkeypatch.setattr(pool, "_execute", execute)
    monkeypatch.setattr(AsyncDatabase, "_pack_query", lambda _self, query: query)
    client = client_type(url, auto_register=False)

    rows = await fetch_result(client, ir)

    if use_native_rows:
        assert rows is native_rows
        execute_rows.assert_awaited_once_with("default", ir)
        execute.assert_not_called()
    else:
        assert rows.column(0) == [2]
        execute.assert_awaited_once_with("default", {**ir, "traced": True})
        execute_rows.assert_not_called()


@pytest.mark.asyncio
async def test_fetch_result_respects_instance_execute_override(monkeypatch):
    client = AsyncDatabase("postgresql://localhost/test", auto_register=False)
    execute = AsyncMock(return_value=msgpack.packb([["id"], [[2]]]))
    execute_rows = AsyncMock()
    monkeypatch.setattr(client, "execute", execute)
    monkeypatch.setattr(pool, "_execute_rows", execute_rows)
    ir = {"op": "select"}

    rows = await fetch_result(client, ir)

    assert rows.column(0) == [2]
    execute.assert_awaited_once_with(ir)
    execute_rows.assert_not_called()


@pytest.mark.asyncio
async def test_fetch_result_uses_rows_mixin_before_database():
    expected_ir = {"op": "select"}
    native_rows = MsgpackResultSet([["id"], [[1]]])

    class NativeRowsMixin:
        async def execute_rows(self, ir):
            assert ir == expected_ir
            return native_rows

    class Client(NativeRowsMixin, AsyncDatabase):
        pass

    client = Client("postgresql://localhost/test", auto_register=False)

    assert await fetch_result(client, expected_ir) is native_rows


@pytest.mark.asyncio
async def test_fetch_result_respects_slotted_execute_override():
    class Client(AsyncDatabase):
        __slots__ = ("execute",)

        async def execute_rows(self, ir):
            pytest.fail(f"execute_rows bypassed the slotted execute override for {ir}")

    client = Client("postgresql://localhost/test", auto_register=False)
    execute = AsyncMock(return_value=msgpack.packb([["id"], [[2]]]))
    client.execute = execute
    ir = {"op": "select"}

    rows = await fetch_result(client, ir)

    assert rows.column(0) == [2]
    execute.assert_awaited_once_with(ir)


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
        [
            sys.executable,
            "-c",
            "import sys, oxyde; assert '_oxyde_core' not in sys.modules",
        ],
        check=True,
    )
