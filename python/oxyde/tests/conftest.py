"""Shared PostgreSQL 18+ fixtures. OXYDE_TEST_POSTGRES_URL must be disposable."""
from __future__ import annotations

import os
import uuid
from contextlib import closing
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
    image = os.environ.get("OXYDE_TEST_POSTGRES_IMAGE", "postgres:18-alpine")
    with PostgresContainer(image) as container:
        host = container.get_container_host_ip()
        port = container.get_exposed_port(5432)
        yield f"postgresql://test:test@{host}:{port}/test"


@pytest.fixture(scope="session")
def server_version_num(postgres_url):
    with closing(psycopg2.connect(postgres_url)) as conn, conn.cursor() as cursor:
        cursor.execute("SHOW server_version_num")
        return int(cursor.fetchone()[0])


@pytest.fixture(autouse=True)
def _skip_for_postgres_version(request):
    if request.node.get_closest_marker("requires_pg19"):
        if request.getfixturevalue("server_version_num") < 190000:
            pytest.skip("requires PostgreSQL 19 or newer")
    if request.node.get_closest_marker("requires_pre_pg19"):
        if request.getfixturevalue("server_version_num") >= 190000:
            pytest.skip("requires PostgreSQL earlier than 19")


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
