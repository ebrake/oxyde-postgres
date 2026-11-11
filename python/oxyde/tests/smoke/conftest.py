"""Smoke test fixtures — real PostgreSQL 18 database."""
from __future__ import annotations

import uuid

import psycopg2
import pytest_asyncio

from oxyde import AsyncDatabase, disconnect_all


def _prepare_db(url: str) -> None:
    """Create schema and seed data via psycopg2 on PostgreSQL 18."""
    conn = psycopg2.connect(url)
    cur = conn.cursor()

    # Create all tables first
    cur.execute(
        "CREATE TABLE IF NOT EXISTS articles (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL, views BIGINT NOT NULL)"
    )
    cur.execute(
        "CREATE TABLE IF NOT EXISTS authors (id BIGSERIAL PRIMARY KEY, email TEXT NOT NULL, name TEXT NOT NULL)"
    )
    cur.execute(
        """
        CREATE TABLE IF NOT EXISTS posts (
            id BIGSERIAL PRIMARY KEY,
            title TEXT NOT NULL,
            author_id BIGINT NOT NULL,
            views BIGINT NOT NULL,
            FOREIGN KEY(author_id) REFERENCES authors(id)
        )
        """
    )
    cur.execute(
        """
        CREATE TABLE IF NOT EXISTS comments (
            id BIGSERIAL PRIMARY KEY,
            post_id BIGINT NOT NULL,
            body TEXT NOT NULL,
            FOREIGN KEY(post_id) REFERENCES posts(id)
        )
        """
    )

    # Delete in reverse FK order (child tables first)
    cur.execute("DELETE FROM comments")
    cur.execute("DELETE FROM posts")
    cur.execute("DELETE FROM authors")
    cur.execute("DELETE FROM articles")

    # Insert in FK order (parent tables first)
    cur.executemany(
        "INSERT INTO articles (id, title, views) VALUES (%s, %s, %s)",
        [
            (1, "First", 10),
            (2, "Second", 5),
            (3, "Third", 20),
        ],
    )
    cur.executemany(
        "INSERT INTO authors (id, email, name) VALUES (%s, %s, %s)",
        [
            (1, "ada@example.com", "Ada Lovelace"),
            (2, "linus@example.com", "Linus Torvalds"),
        ],
    )
    cur.executemany(
        "INSERT INTO posts (id, title, author_id, views) VALUES (%s, %s, %s, %s)",
        [
            (1, "Rust Patterns", 1, 120),
            (2, "Async ORM", 1, 35),
            (3, "Kernel Notes", 2, 80),
        ],
    )
    cur.executemany(
        "INSERT INTO comments (id, post_id, body) VALUES (%s, %s, %s)",
        [
            (1, 1, "Great read!"),
            (2, 1, "Thanks for sharing"),
            (3, 3, "Subscribed!"),
        ],
    )
    cur.execute("SELECT setval(pg_get_serial_sequence('articles', 'id'), 3)")
    conn.commit()
    conn.close()


@pytest_asyncio.fixture
async def postgres_db(postgres_database_url):
    """Fresh PostgreSQL DB with seed data for each test."""
    _prepare_db(postgres_database_url)

    db = AsyncDatabase(
        postgres_database_url,
        name=f"test_{uuid.uuid4().hex}",
        overwrite=True,
    )
    await db.connect()

    try:
        yield db
    finally:
        await disconnect_all()
