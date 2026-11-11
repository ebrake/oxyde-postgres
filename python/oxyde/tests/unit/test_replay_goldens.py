"""Released 0.2.2 snapshots, executor SQL, and byte-exact generated files."""

import json
import re
from pathlib import Path

import pytest

from oxyde.core import migration_compute_diff
from oxyde.migrations.context import MigrationContext
from oxyde.migrations.executor import replay_migrations_up_to
from oxyde.migrations.generator import generate_migration_file
from oxyde.migrations.replay import replay_migrations
from oxyde.migrations.squash import squash_migrations
from oxyde.migrations.utils import load_migration_module

from .replay_parity import canonical_snapshot

GOLDENS = sorted((Path(__file__).parent / "goldens/replay").glob("*.json"))
assert len(GOLDENS) == 51, "Released replay corpus must not silently disappear"


def stable_text(text):
    return re.sub(r"Created: .*", "Created: <timestamp>", text)


@pytest.mark.parametrize(
    "golden", GOLDENS, ids=lambda path: json.loads(path.read_text())["case"]
)
def test_released_replay_golden(golden, tmp_path):
    expected = json.loads(golden.read_text())
    migrations = tmp_path / "migrations"
    migrations.mkdir()
    for name, source in expected["files"].items():
        (migrations / name).write_text(source)
    snapshot = replay_migrations(str(migrations))
    # Canonical view deliberately drops legacy metadata/omitted-key provenance.
    assert snapshot == canonical_snapshot(expected["snapshot"])
    for name, sql in expected["sql"].items():
        if "excluded" in sql:
            assert "renames" in sql["excluded"]
            continue
        file = migrations / name
        ctx = MigrationContext(
            mode="execute", schema_state=replay_migrations_up_to(str(migrations), file)
        )
        module = load_migration_module(file)
        assert module is not None
        module.upgrade(ctx)
        assert ctx._sql_statements == sql["statements"]
    ops = json.loads(
        migration_compute_diff(
            json.dumps({"version": 1, "tables": {}}), json.dumps(snapshot)
        )
    )
    generated = generate_migration_file(
        ops, migrations_dir=tmp_path / "generated", name="golden"
    )
    assert stable_text(generated.read_text()) == expected["makemigrations"]
    squashed = squash_migrations(migrations, name="golden")
    assert squashed.new_file is not None
    assert stable_text(squashed.new_file.read_text()) == expected["squash"]
