"""Capture released replay behavior using an isolated checkout and native core.

Run with the baseline's interpreter and PYTHONPATH pointing at its python/:
    python capture_replay_goldens.py --source-root <new-worktree> --output <goldens>

The baseline must be release 0.2.2. This script never rewrites fixture migrations.
"""

from __future__ import annotations

import argparse
import ast
import json
import random
import re
import tempfile
import warnings
from pathlib import Path
from types import SimpleNamespace
from typing import Any


def stable_text(text):
    return re.sub(r"Created: .*", "Created: <timestamp>", text)


def materialize(source_root, root):
    """Reuse fixture writers and generated sequences without importing new replay."""
    import pytest
    from oxyde.tests.unit.test_migrations_squash import (
        LEGACY_0001,
        LEGACY_0002,
        LEGACY_IDENTITY,
    )

    test_path = source_root / "python/oxyde/tests/unit/test_schema_replay.py"
    tree = ast.parse(test_path.read_text())
    functions: list[ast.stmt] = [
        n
        for n in tree.body
        if isinstance(n, ast.FunctionDef)
        and n.name in ("_generated_migration", "_integration_fixture_cases")
    ]
    namespace: dict[str, Any] = {
        "random": random,
        "SimpleNamespace": SimpleNamespace,
        "pytest": pytest,
    }
    exec(
        compile(ast.Module(body=functions, type_ignores=[]), str(test_path), "exec"),
        namespace,
    )
    cases = []
    for number, case in enumerate(namespace["_integration_fixture_cases"]()):
        fixtures, source, arguments, expected = case.values
        directory = root / f"fixture_{number:02}"
        directory.mkdir()
        fixture_namespace = {
            **vars(fixtures),
            **arguments,
            "empty_db": (None, "postgres"),
            "tmp_path": directory,
        }
        exec(
            compile(ast.fix_missing_locations(source), fixtures.__file__, "exec"),
            fixture_namespace,
        )
        assert len(list(directory.glob("[0-9]*.py"))) == expected
        cases.append((case.id, directory))
    legacy = root / "legacy"
    legacy.mkdir()
    for name, source in [
        ("0001_users.py", LEGACY_0001),
        ("0002_age.py", LEGACY_0002),
        ("0003_identity.py", LEGACY_IDENTITY),
    ]:
        (legacy / name).write_text(source)
    cases.append(("legacy_history", legacy))
    # The generator is a complete deterministic migration entry point. Preserve
    # its raw SQL and operation order rather than round-tripping through codegen.
    generated_source = ast.get_source_segment(test_path.read_text(), functions[0])
    assert generated_source is not None
    for seed in range(40):
        directory = root / f"seed_{seed:02}"
        directory.mkdir()
        (directory / "0001_generated.py").write_text(
            "import random\nfrom types import SimpleNamespace\n"
            + generated_source
            + f"\ndef upgrade(ctx):\n    _generated_migration({seed}).upgrade(ctx)\n"
        )
        cases.append((f"generated_seed_{seed:02}", directory))
    return cases


def sql_exclusion(operations):
    """Annotate only the intentional intra-migration rename→alter change."""
    renamed_tables = set()
    renamed_columns = set()
    for op in operations:
        if op["type"] == "rename_table":
            renamed_tables.add(op["new_name"])
        elif op["type"] == "rename_column":
            renamed_columns.add((op["table"], op["new_name"]))
        elif op["type"] == "alter_column" and (
            op["table"] in renamed_tables
            or (op["table"], op["column"]) in renamed_columns
        ):
            return (
                "0.3 tracks renames before later alterations within the same migration"
            )
    return None


def capture(directory):
    from oxyde.core import migration_compute_diff
    from oxyde.migrations.context import MigrationContext
    from oxyde.migrations.executor import replay_migrations_up_to
    from oxyde.migrations.generator import generate_migration_file
    from oxyde.migrations.replay import _topological_sort_migrations, replay_migrations
    from oxyde.migrations.squash import squash_migrations
    from oxyde.migrations.utils import load_migration_module

    snapshot = replay_migrations(str(directory))
    sql = {}
    for file in _topological_sort_migrations(list(directory.glob("[0-9]*.py"))):
        module = load_migration_module(file)
        assert module is not None
        collect = MigrationContext(mode="collect")
        module.upgrade(collect)
        exclusion = sql_exclusion(collect.get_collected_operations())
        if exclusion:
            sql[file.name] = {"excluded": exclusion}
            continue
        # Match the released executor, including its pre-migration schema state.
        ctx = MigrationContext(
            mode="execute", schema_state=replay_migrations_up_to(str(directory), file)
        )
        module.upgrade(ctx)
        sql[file.name] = {"statements": ctx._sql_statements}
    empty = {"version": 1, "tables": {}}
    ops = json.loads(migration_compute_diff(json.dumps(empty), json.dumps(snapshot)))
    with tempfile.TemporaryDirectory() as generated:
        makemigrations = stable_text(
            generate_migration_file(
                ops, migrations_dir=generated, name="golden"
            ).read_text()
        )
    squashed = squash_migrations(directory, name="golden")
    assert squashed.new_file is not None
    squash = stable_text(squashed.new_file.read_text())
    return {
        "snapshot": snapshot,
        "sql": sql,
        "makemigrations": makemigrations,
        "squash": squash,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-root", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    import oxyde

    if oxyde.__version__ != "0.2.2":
        raise SystemExit(
            "Capture requires the isolated oxyde 0.2.2 checkout and its core"
        )
    args.output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory() as tmp, warnings.catch_warnings():
        warnings.simplefilter("ignore", FutureWarning)
        for number, (label, directory) in enumerate(
            materialize(args.source_root, Path(tmp))
        ):
            # Store exact source files before squash replaces them.
            files = {
                file.name: file.read_text() for file in directory.glob("[0-9]*.py")
            }
            result = {
                "baseline": "0.2.2 (5f9f406)",
                "case": label,
                "files": files,
                **capture(directory),
            }
            (args.output / f"{number:03}.json").write_text(
                json.dumps(result, indent=2) + "\n"
            )
            print(label, flush=True)


if __name__ == "__main__":
    main()
