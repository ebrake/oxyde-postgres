"""End-to-end typecheck tests: generate stubs, run mypy/pyright/ty on them."""

from __future__ import annotations

import json
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

import pytest

import oxyde

FIXTURES_DIR = Path(__file__).parent / "fixtures"
OXYDE_PARENT = str(Path(oxyde.__file__).parent.parent)

PYRIGHT_EXE = shutil.which("pyright")
TY_EXE = shutil.which("ty")


FIXTURES: list[tuple[str, str, str, str]] = [
    # (test_id, fixture_rel_dir, model_module_basename, usage_file_basename)
    ("smoke", "smoke", "tiny_model", "tiny_usage.py"),
    ("kitchen_sink", "kitchen_sink", "models", "usage.py"),
    ("edges/helpers_only", "edges/helpers_only", "module", "usage.py"),
    ("edges/mixed_module", "edges/mixed_module", "module", "usage.py"),
    ("edges/toplevel_overload", "edges/toplevel_overload", "module", "usage.py"),
    ("edges/async_generators", "edges/async_generators", "module", "usage.py"),
    ("edges/forward_ref", "edges/forward_ref", "module", "usage.py"),
    ("edges/forward_ref_cross", "edges/forward_ref_cross", "module", "usage.py"),
    ("edges/inheritance_chain", "edges/inheritance_chain", "module", "usage.py"),
    ("edges/reserved_names", "edges/reserved_names", "module", "usage.py"),
    ("edges/generics_in_field", "edges/generics_in_field", "module", "usage.py"),
    ("edges/enums", "edges/enums", "module", "usage.py"),
    ("edges/module_exports", "edges/module_exports", "module", "usage.py"),
    ("edges/explicit_all", "edges/explicit_all", "module", "usage.py"),
    ("edges/type_checking", "edges/type_checking", "module", "usage.py"),
    ("edges/type_level", "edges/type_level", "module", "usage.py"),
]


def _run_mypy(target: Path | list[Path], cwd: Path) -> subprocess.CompletedProcess[str]:
    env = {**os.environ, "MYPYPATH": OXYDE_PARENT}
    return subprocess.run(
        [
            sys.executable,
            "-m",
            "mypy",
            "--no-incremental",
            "--explicit-package-bases",
            "--show-error-codes",
            *map(str, [target] if isinstance(target, Path) else target),
        ],
        capture_output=True,
        text=True,
        cwd=cwd,
        env=env,
    )


def _stub_targets(work_dir: Path, usage_file: str) -> list[Path]:
    """Usage sample plus the generated .pyi files themselves."""
    usage_path = work_dir / usage_file
    assert usage_path.exists(), f"usage fixture missing: {usage_path}"
    return [usage_path, *sorted(work_dir.glob("*.pyi"))]


def _run_pyright(targets: list[Path], cwd: Path) -> subprocess.CompletedProcess[str]:
    assert PYRIGHT_EXE is not None
    (cwd / "pyrightconfig.json").write_text(
        json.dumps({"typeCheckingMode": "standard", "extraPaths": [OXYDE_PARENT]})
    )
    return subprocess.run(
        [PYRIGHT_EXE, "--pythonpath", sys.executable, *map(str, targets)],
        capture_output=True,
        text=True,
        cwd=cwd,
    )


def _run_ty(
    targets: list[Path],
    cwd: Path,
    *,
    concise: bool = False,
    extra_args: list[str] | None = None,
) -> subprocess.CompletedProcess[str]:
    assert TY_EXE is not None
    return subprocess.run(
        [
            TY_EXE,
            "check",
            *(["--output-format", "concise"] if concise else []),
            "--python",
            sys.prefix,
            "--extra-search-path",
            OXYDE_PARENT,
            "--extra-search-path",
            ".",
            *(extra_args or []),
            *map(str, targets),
        ],
        capture_output=True,
        text=True,
        cwd=cwd,
    )


@pytest.mark.skipif(TY_EXE is None, reason="ty is not installed")
@pytest.mark.parametrize(
    ("fixture_dir", "model_module"),
    [(f[1], f[2]) for f in FIXTURES],
    ids=[f[0] for f in FIXTURES],
)
def test_ty_stub_suppressions_are_current(
    fixture_dir: str, model_module: str, generate_stubs
) -> None:
    """The ty suppressions must work without mypy ignores and use current rules."""
    work_dir = generate_stubs(FIXTURES_DIR / fixture_dir, model_module)
    targets = sorted(work_dir.glob("*.pyi"))
    if fixture_dir == "edges/helpers_only" and not targets:
        pytest.skip("helpers-only fixture has no models or generated stubs")
    assert targets, f"no stubs generated for fixture '{fixture_dir}'"
    result = _run_ty(
        targets,
        work_dir,
        extra_args=[
            "-c",
            "analysis.respect-type-ignore-comments=false",
            "--error",
            "ignore-comment-unknown-rule",
            "--error",
            "renamed-rule-in-ignore-comment",
            "--error",
            "unused-ignore-comment",
        ],
    )
    assert result.returncode == 0, (
        f"ty suppression check failed for fixture '{fixture_dir}':\n"
        f"--- STDOUT ---\n{result.stdout}\n"
        f"--- STDERR ---\n{result.stderr}"
    )


@pytest.mark.parametrize(
    ("fixture_dir", "model_module", "usage_file"),
    [(f[1], f[2], f[3]) for f in FIXTURES],
    ids=[f[0] for f in FIXTURES],
)
def test_mypy_accepts_generated_stubs(
    fixture_dir: str,
    model_module: str,
    usage_file: str,
    generate_stubs,
) -> None:
    source_dir = FIXTURES_DIR / fixture_dir
    work_dir = generate_stubs(source_dir, model_module)
    result = _run_mypy(_stub_targets(work_dir, usage_file), work_dir)
    assert result.returncode == 0, (
        f"mypy failed for fixture '{fixture_dir}':\n"
        f"--- STDOUT ---\n{result.stdout}\n"
        f"--- STDERR ---\n{result.stderr}"
    )


@pytest.mark.skipif(PYRIGHT_EXE is None, reason="pyright is not installed")
@pytest.mark.parametrize(
    ("fixture_dir", "model_module", "usage_file"),
    [(f[1], f[2], f[3]) for f in FIXTURES],
    ids=[f[0] for f in FIXTURES],
)
def test_pyright_accepts_generated_stubs(
    fixture_dir: str,
    model_module: str,
    usage_file: str,
    generate_stubs,
) -> None:
    """pyright (standard mode) must accept the usage file AND the stubs."""
    source_dir = FIXTURES_DIR / fixture_dir
    work_dir = generate_stubs(source_dir, model_module)

    result = _run_pyright(_stub_targets(work_dir, usage_file), work_dir)
    assert result.returncode == 0, (
        f"pyright failed for fixture '{fixture_dir}':\n"
        f"--- STDOUT ---\n{result.stdout}\n"
        f"--- STDERR ---\n{result.stderr}"
    )


@pytest.mark.skipif(TY_EXE is None, reason="ty is not installed")
@pytest.mark.parametrize(
    ("fixture_dir", "model_module", "usage_file"),
    [(f[1], f[2], f[3]) for f in FIXTURES],
    ids=[f[0] for f in FIXTURES],
)
def test_ty_accepts_generated_stubs(
    fixture_dir: str,
    model_module: str,
    usage_file: str,
    generate_stubs,
) -> None:
    """ty must accept the usage file AND the stubs."""
    source_dir = FIXTURES_DIR / fixture_dir
    work_dir = generate_stubs(source_dir, model_module)

    result = _run_ty(_stub_targets(work_dir, usage_file), work_dir)
    assert result.returncode == 0, (
        f"ty failed for fixture '{fixture_dir}':\n"
        f"--- STDOUT ---\n{result.stdout}\n"
        f"--- STDERR ---\n{result.stderr}"
    )


@pytest.mark.parametrize(
    ("fixture_dir", "model_module", "usage_file"),
    [(f[1], f[2], f[3]) for f in FIXTURES],
    ids=[f[0] for f in FIXTURES],
)
def test_mypy_accepts_model_source(
    fixture_dir: str,
    model_module: str,
    usage_file: str,
    generate_stubs,
) -> None:
    """Mypy must accept the model source file itself, not just the usage stub.

    Reproduces issue #13: pyright/ty (and mypy without the pydantic plugin)
    flag every ``field: T = Field(...)`` assignment because ``Field()`` returns
    ``OxydeFieldInfo`` rather than ``Any``. The existing typecheck e2e only
    runs mypy on the usage file, which resolves through the generated ``.pyi``
    and hides the problem.
    """
    source_dir = FIXTURES_DIR / fixture_dir
    work_dir = generate_stubs(source_dir, model_module)
    model_path = work_dir / f"{model_module}.py"
    assert model_path.exists(), f"model fixture missing: {model_path}"

    result = _run_mypy(model_path, work_dir)
    assert result.returncode == 0, (
        f"mypy failed for fixture '{fixture_dir}':\n"
        f"--- STDOUT ---\n{result.stdout}\n"
        f"--- STDERR ---\n{result.stderr}"
    )


@pytest.mark.skipif(sys.version_info < (3, 15), reason="requires native lazy imports")
@pytest.mark.parametrize("checker", ["mypy", "pyright", "ty"])
def test_native_lazy_imports_generate_portable_stubs(
    checker: str, tmp_path: Path, generate_stubs
) -> None:
    source_dir = tmp_path / "lazy_source"
    source_dir.mkdir()
    (source_dir / "module.py").write_text(
        "from __future__ import annotations\n"
        "from oxyde import Field, Model\n"
        "lazy import sibling\n"
        "lazy from sibling import Related, DEFAULT_INTERVAL\n"
        "class Entry(Model):\n"
        "    id: int | None = Field(default=None, db_pk=True)\n"
        "    def related(self) -> Related | None:\n"
        "        return None\n"
        "    class Meta:\n"
        "        is_table = True\n"
        "def helper(value: sibling.Related) -> Related:\n"
        "    return value\n"
    )
    (source_dir / "sibling.py").write_text(
        "DEFAULT_INTERVAL: int = 30\n"
        "class Related:\n"
        "    def label(self) -> str:\n"
        "        return 'related'\n"
    )
    (source_dir / "usage.py").write_text(
        "from typing import assert_type\n"
        "from module import DEFAULT_INTERVAL, Entry, helper\n"
        "from sibling import Related\n"
        "interval: int = DEFAULT_INTERVAL\n"
        "def check(entry: Entry, related: Related) -> None:\n"
        "    assert_type(entry.related(), Related | None)\n"
        "    assert_type(helper(related), Related)\n"
    )
    work_dir = generate_stubs(source_dir, "module")
    stub = (work_dir / "module.pyi").read_text()
    assert "lazy " not in stub
    assert "from sibling import Related, DEFAULT_INTERVAL as DEFAULT_INTERVAL" in stub
    assert "import sibling" in stub
    targets = _stub_targets(work_dir, "usage.py")
    if checker == "mypy":
        result = _run_mypy(targets, work_dir)
    elif checker == "pyright":
        if PYRIGHT_EXE is None:
            pytest.skip("pyright is not installed")
        result = _run_pyright(targets, work_dir)
    else:
        if TY_EXE is None:
            pytest.skip("ty is not installed")
        result = _run_ty(targets, work_dir)
    assert result.returncode == 0, result.stdout + result.stderr


# --- Negative tests: wrong usage MUST be rejected. -------------------------
#
# Every positive fixture above would stay green if the stubs degraded to
# permissive Any-signatures. The ``negative`` fixture is the guard: each line
# of bad_usage.py marked ``# type-error`` must be flagged by the checker, and
# unmarked lines must stay clean.

_NEGATIVE_FIXTURE = ("negative", "module", "bad_usage.py")


def _expected_error_lines(path: Path) -> set[int]:
    return {
        lineno
        for lineno, line in enumerate(path.read_text().splitlines(), start=1)
        if line.rstrip().endswith("# type-error")
    }


def _reported_error_lines(output: str, usage_file: str) -> set[int]:
    """Line numbers of error diagnostics for usage_file in checker output.

    All three checkers emit one-line diagnostics matching
    ``<path>:<line>[:<col>]`` with the word ``error`` on the same line
    (mypy: ``file:16: error: ...``; pyright: ``/abs/file:16:36 - error: ...``;
    ty concise: ``file:16:31: error[rule] ...``). mypy ``note:`` lines carry
    no ``error`` token and are ignored.
    """
    pattern = re.compile(rf"{re.escape(usage_file)}:(\d+)")
    return {
        int(match.group(1))
        for line in output.splitlines()
        if "error" in line and (match := pattern.search(line))
    }


def _assert_flagged_exactly(
    checker: str, output: str, work_dir: Path, usage_file: str
) -> None:
    expected = _expected_error_lines(work_dir / usage_file)
    assert expected, "negative fixture has no '# type-error' markers"
    reported = _reported_error_lines(output, usage_file)
    assert reported == expected, (
        f"{checker} flagged lines {sorted(reported)}, "
        f"expected exactly {sorted(expected)}:\n{output}"
    )


def test_mypy_flags_bad_usage(generate_stubs) -> None:
    fixture_dir, model_module, usage_file = _NEGATIVE_FIXTURE
    work_dir = generate_stubs(FIXTURES_DIR / fixture_dir, model_module)

    result = _run_mypy(work_dir / usage_file, work_dir)
    assert result.returncode != 0, "mypy accepted intentionally wrong usage"
    _assert_flagged_exactly("mypy", result.stdout, work_dir, usage_file)


@pytest.mark.skipif(PYRIGHT_EXE is None, reason="pyright is not installed")
def test_pyright_flags_bad_usage(generate_stubs) -> None:
    fixture_dir, model_module, usage_file = _NEGATIVE_FIXTURE
    work_dir = generate_stubs(FIXTURES_DIR / fixture_dir, model_module)

    result = _run_pyright([work_dir / usage_file], work_dir)
    assert result.returncode != 0, "pyright accepted intentionally wrong usage"
    _assert_flagged_exactly("pyright", result.stdout, work_dir, usage_file)


@pytest.mark.skipif(TY_EXE is None, reason="ty is not installed")
def test_ty_flags_bad_usage(generate_stubs) -> None:
    fixture_dir, model_module, usage_file = _NEGATIVE_FIXTURE
    work_dir = generate_stubs(FIXTURES_DIR / fixture_dir, model_module)

    result = _run_ty([work_dir / usage_file], work_dir, concise=True)
    assert result.returncode != 0, "ty accepted intentionally wrong usage"
    _assert_flagged_exactly("ty", result.stdout, work_dir, usage_file)
