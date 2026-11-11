"""Shared PostgreSQL 18+ fixtures. OXYDE_TEST_POSTGRES_URL must be disposable."""
from __future__ import annotations

import os
import uuid
from urllib.parse import quote, urlencode

import psycopg2
import pytest
from psycopg2 import sql


@pytest.fixture(scope="session")
def postgres_url():
    configured = os.environ.get("OXYDE_TEST_POSTGRES_URL")
    if configured:
        yield configured
        return
    from testcontainers.postgres import PostgresContainer
    with PostgresContainer("postgres:18-alpine") as container:
        host = container.get_container_host_ip()
        port = container.get_exposed_port(5432)
        yield f"postgresql://test:test@{host}:{port}/test"

@pytest.fixture
def postgres_database_url(postgres_url):
    schema = "test_" + uuid.uuid4().hex
    with psycopg2.connect(postgres_url) as conn:
        with conn.cursor() as cursor:
            cursor.execute(sql.SQL("CREATE SCHEMA {}").format(sql.Identifier(schema)))
    separator = "&" if "?" in postgres_url else "?"
    yield postgres_url + separator + urlencode({"options": f"-c search_path={schema}"}, quote_via=quote)
    with psycopg2.connect(postgres_url) as conn:
        with conn.cursor() as cursor:
            cursor.execute(sql.SQL("DROP SCHEMA {} CASCADE").format(sql.Identifier(schema)))
