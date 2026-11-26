import sys
from types import ModuleType
from unittest.mock import AsyncMock, Mock

import pytest

from oxyde.core.wrapper import _load_functions


@pytest.mark.asyncio
async def test_stale_core_uses_raising_stubs(monkeypatch, capsys):
    stale_core = ModuleType("_oxyde_core")
    stale_execute = AsyncMock()
    stale_render = Mock()
    stale_core.__dict__.update(
        __abi_version__=1, execute=stale_execute, render_sql_debug=stale_render
    )
    monkeypatch.setitem(sys.modules, "_oxyde_core", stale_core)

    functions = _load_functions()

    warning = capsys.readouterr().err
    assert "ABI version mismatch: expected 2, got 1" in warning
    assert "oxyde-postgres-core>=0.4.0" in warning
    with pytest.raises(RuntimeError, match="missing 'execute'"):
        await functions["execute"]("default", b"")
    with pytest.raises(RuntimeError, match="missing 'render_sql_debug'"):
        functions["render_sql_debug"](b"")
    stale_execute.assert_not_called()
    stale_render.assert_not_called()
