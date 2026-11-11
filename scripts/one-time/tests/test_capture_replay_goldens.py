"""Check deterministic capture helpers without building a historical core."""

import importlib.util
from pathlib import Path

script = Path(__file__).parents[1] / "capture_replay_goldens.py"
spec = importlib.util.spec_from_file_location("capture_replay_goldens", script)
assert spec is not None and spec.loader is not None
capture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(capture)


def test_timestamp_only_is_scrubbed():
    assert (
        capture.stable_text('Created: 2026-11-11 12:00:00\nname = "t"\n')
        == 'Created: <timestamp>\nname = "t"\n'
    )


def test_sql_exclusion_is_limited_to_renamed_target():
    rename = {"type": "rename_column", "table": "t", "old_name": "x", "new_name": "y"}
    alter = {"type": "alter_column", "table": "t", "column": "y"}
    assert capture.sql_exclusion([rename, alter])
    assert capture.sql_exclusion([alter, rename]) is None
    assert capture.sql_exclusion([rename, {**alter, "column": "other"}]) is None
