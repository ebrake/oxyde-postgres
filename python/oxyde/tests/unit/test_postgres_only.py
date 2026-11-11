"""Public boundaries reject unsupported backend configuration."""
import pytest

from oxyde import AsyncDatabase, PoolSettings
from oxyde._msgpack import msgpack
from oxyde.core import migration_to_sql, render_sql_debug
from oxyde.migrations.config import OxydeConfig, generate_config_file
from oxyde.migrations.context import MigrationContext
from oxyde.migrations.extract import extract_current_schema
from oxyde.migrations.utils import detect_dialect


@pytest.mark.parametrize("url", ["sqlite::memory:", "sqlite:///test.db", "mysql://localhost/db", "mariadb://localhost/db", "postgresjunk://localhost/db", "postgresqlx://localhost/db", "postgres"])
def test_unsupported_urls_are_rejected(url):
    with pytest.raises(ValueError, match="PostgreSQL 18"):
        AsyncDatabase(url, auto_register=False)
    with pytest.raises(ValueError):
        detect_dialect(url)

@pytest.mark.parametrize("dialect", ["sqlite", "mysql", "mariadb", "unknown"])
def test_unsupported_dialects_are_rejected(dialect):
    for call in [lambda: MigrationContext(dialect=dialect), lambda: OxydeConfig(dialect=dialect), lambda: generate_config_file([], dialect=dialect), lambda: extract_current_schema(dialect=dialect), lambda: migration_to_sql("[]", dialect), lambda: render_sql_debug(msgpack.packb({"proto": 1, "op": "select", "table": "users"}), dialect, False)]:
        with pytest.raises(ValueError):
            call()

def test_removed_pool_settings_are_not_accepted():
    for name in ["sqlite_journal_mode", "sqlite_busy_timeout", "mysql_charset", "mysql_collation"]:
        with pytest.raises(TypeError):
            PoolSettings(**{name: "unsupported"})
