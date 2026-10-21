"""Integration tests for array lookups: contains, contained_by, overlap."""

from __future__ import annotations

import json
import sqlite3
from contextlib import closing
from datetime import datetime, timedelta, timezone
from decimal import Decimal
from uuid import UUID

import pytest

from oxyde import Field, Model, Q
from oxyde.migrations.utils import detect_dialect
from oxyde.queries.raw import execute_raw

from .conftest import ArrayDoc, ArrayNote, ArrayShadow, ArrayShadowJson

U1 = UUID("10000000-0000-4000-8000-000000000001")
U2 = UUID("20000000-0000-4000-8000-000000000002")
CEST = timezone(timedelta(hours=2))
# SQLite compares scalar elements only.
SQLITE_SCALARS_ONLY = "only supported on PostgreSQL and MySQL"


class PgArrayOverrides(Model):
    """PostgreSQL-only db_type overrides; created by the test, not create_tables."""

    id: int | None = Field(default=None, db_pk=True)
    small: list[int] | None = Field(default=None, db_type="INTEGER[]")
    codes: list[str] | None = Field(default=None, db_type="CHAR(3)[]")
    names: list[str] | None = Field(default=None, db_type="VARCHAR(5)[]")
    stamps: list[datetime] | None = Field(default=None, db_type="TIMESTAMPTZ[]")
    ratios: list[float] | None = Field(default=None, db_type="FLOAT(24)[]")
    blobs: list[bytes] | None = None
    # Elements are JSON text: '"a"' is stored as the JSON string "a"
    docs: list[str | None] | None = Field(default=None, db_type="JSONB[]")

    class Meta:
        is_table = True
        table_name = "pg_array_overrides"


async def _seed(db) -> dict[str, int]:
    docs = {
        "full": dict(
            tags=["rust", "python"],
            scores=[1, 2, 3],
            amounts=[Decimal("1.50"), Decimal("2.00")],
            ids=[U1],
            items=[{"a": 1, "b": {"x": [1, 2]}}, {"c": 3}],
            payload=[{"a": 1, "b": 2}, [1, 2], "x", 1.5, None],
            raw=[1, "a"],
        ),
        "empty": dict(
            tags=["go", None],
            scores=[],
            amounts=[],
            ids=[],
            items=[],
            payload=[],
            raw=[],
        ),
        # SQL NULL arrays; JSON columns hold objects, not arrays
        "nulls": dict(payload={"tags": ["rust"]}, raw={"k": 1}),
        "dupes": dict(
            tags=["rust"],
            scores=[3, 3, 1],
            payload=[1.0, True],
            raw=["A"],
        ),
    }
    ids = {}
    for name, values in docs.items():
        doc = await ArrayDoc.objects.create(name=name, **values, using=db.name)
        ids[name] = doc.id
    return ids


async def _names(db, *args, **filters) -> set[str]:
    rows = await ArrayDoc.objects.filter(*args, **filters).all(using=db.name)
    return {row.name for row in rows}


async def _check_structured(db, cases) -> None:
    """Cases with object or nested-array elements in the lookup value: compared
    as complete values on PostgreSQL and MySQL, rejected on SQLite."""
    sqlite = detect_dialect(db.url) == "sqlite"
    for filters, expected in cases:
        if sqlite:
            with pytest.raises(RuntimeError, match=SQLITE_SCALARS_ONLY):
                await _names(db, **filters)
        else:
            assert await _names(db, **filters) == expected, filters


CASES = [
    # Typed text arrays
    ({"tags__contains": ["rust"]}, {"full", "dupes"}),
    ({"tags__contains": ("rust", "python")}, {"full"}),
    ({"tags__contains": []}, {"full", "empty", "dupes"}),
    ({"tags__contains": [None]}, {"empty"}),
    ({"tags__contains": ["Rust"]}, set()),
    ({"tags__contained_by": ["rust", "python", "java"]}, {"full", "dupes"}),
    ({"tags__contained_by": ["go", None]}, {"empty"}),
    ({"tags__overlap": ["go", "java"]}, {"empty"}),
    ({"tags__overlap": []}, set()),
    # Typed integer arrays: order and duplicates ignored
    ({"scores__contains": [3, 1]}, {"full", "dupes"}),
    ({"scores__contained_by": [1, 3]}, {"empty", "dupes"}),
    ({"scores__contained_by": []}, {"empty"}),
    ({"scores__overlap": [2, 9]}, {"full"}),
    # Decimals compare by value, not by stored scale
    ({"amounts__contains": [Decimal("1.5")]}, {"full"}),
    ({"amounts__overlap": [Decimal("2")]}, {"full"}),
    ({"amounts__contains": [Decimal("1.51")]}, set()),
    ({"ids__contains": [U1]}, {"full"}),
    ({"ids__overlap": [U2]}, set()),
    # Arrays of objects, with scalar values: stored objects match no scalar
    ({"items__contains": []}, {"full", "empty"}),
    ({"items__contained_by": ["x"]}, {"empty"}),
    # JSON column holding arrays; non-array documents never match
    ({"payload__contains": ["x", 1.5]}, {"full"}),
    ({"payload__contains": [None]}, {"full"}),
    ({"payload__contains": [1]}, {"dupes"}),
    ({"payload__contains": [True]}, {"dupes"}),
    ({"payload__overlap": [0, False, "y"]}, set()),
    ({"payload__contains": []}, {"full", "empty", "dupes"}),
    ({"payload__contained_by": [1, True, 2]}, {"empty", "dupes"}),
    # db_type="JSON" column (plain json on PostgreSQL)
    ({"raw__contains": ["a"]}, {"full"}),
    ({"raw__contains": ["A"]}, {"dupes"}),
    ({"raw__overlap": [1, "A"]}, {"full", "dupes"}),
]

# Objects and nested arrays: whole values, key order ignored, nested order kept
STRUCTURED_CASES = [
    ({"items__contains": [{"b": {"x": [1, 2]}, "a": 1}]}, {"full"}),
    ({"items__contains": [{"a": 1}]}, set()),
    ({"items__contains": [{"a": 1, "b": {"x": [2, 1]}}]}, set()),
    (
        {"items__contained_by": [{"c": 3}, {"a": 1, "b": {"x": [1, 2]}}]},
        {"full", "empty"},
    ),
    ({"payload__contains": [[1, 2]]}, {"full"}),
    ({"payload__contains": [{"b": 2, "a": 1}, "x"]}, {"full"}),
    ({"payload__overlap": [{"a": 1}, [2, 1]]}, set()),
]


class TestArrayLookups:
    @pytest.mark.asyncio
    async def test_lookups(self, db):
        await _seed(db)
        for filters, expected in CASES:
            assert await _names(db, **filters) == expected, filters

    @pytest.mark.asyncio
    async def test_object_and_nested_array_elements(self, db):
        await _seed(db)
        await _check_structured(db, STRUCTURED_CASES)

    @pytest.mark.asyncio
    async def test_decimal_operands_that_would_round_are_rejected(self, db):
        await _seed(db)
        # 29 fractional digits: rounding it would match the stored 1.50.
        near = Decimal("1.50000000000000000000000000001")
        for lookup in ("contains", "contained_by", "overlap"):
            with pytest.raises(RuntimeError, match="not a valid decimal value"):
                await _names(db, **{f"amounts__{lookup}": [near]})
        # Trailing zeros past 28 places are still exact.
        exact = Decimal("1.500000000000000000000000000000")
        assert await _names(db, amounts__contains=[exact]) == {"full"}

    @pytest.mark.asyncio
    async def test_json_elements_from_strings(self, db):
        # Strings are JSON strings, never parsed as JSON text, for operands
        # as on insert: '"hello"' and "hello" are different elements.
        await ArrayDoc.objects.create(
            name="strings", items=['"hello"', "world", '{"a": 1}'], using=db.name
        )
        await ArrayDoc.objects.create(name="other", items=["x"], using=db.name)
        assert await _names(db, items__contains=['"hello"']) == {"strings"}
        assert await _names(db, items__contains=["hello"]) == set()
        assert await _names(db, items__contains=['{"a": 1}', "world"]) == {"strings"}
        assert await _names(db, items__overlap=["x", "world"]) == {"strings", "other"}
        assert await _names(db, items__contained_by=["x", '"x"']) == {"other"}
        # An object is not its JSON text.
        await _check_structured(db, [({"items__contains": [{"a": 1}]}, set())])

    @pytest.mark.asyncio
    async def test_q_combinations_and_negation(self, db):
        await _seed(db)

        assert await _names(
            db, Q(tags__contains=["python"]) | Q(tags__overlap=["go"])
        ) == {"full", "empty"}
        assert await _names(
            db, Q(tags__contains=["rust"]) & Q(scores__contains=[2])
        ) == {"full"}

        # SQL NULL arrays stay unknown under negation; non-array JSON is false.
        rows = await ArrayDoc.objects.exclude(tags__contains=["rust"]).all(
            using=db.name
        )
        assert {row.name for row in rows} == {"empty"}
        rows = await ArrayDoc.objects.exclude(payload__contains=["x"]).all(
            using=db.name
        )
        assert {row.name for row in rows} == {"empty", "nulls", "dupes"}
        assert await _names(db, ~Q(scores__overlap=[1])) == {"empty"}

    @pytest.mark.asyncio
    async def test_count_and_exists(self, db):
        await _seed(db)
        query = ArrayDoc.objects.filter(tags__contains=["rust"])
        assert await query.count(using=db.name) == 2
        assert await query.exists(using=db.name) is True
        missing = ArrayDoc.objects.filter(tags__overlap=["cobol"])
        assert await missing.exists(using=db.name) is False

    @pytest.mark.asyncio
    async def test_filtered_update_and_delete(self, db):
        await _seed(db)
        # The same rows by an object and a nested array, or by scalars on
        # SQLite, which rejects the others before anything runs.
        update_filter = {"items__contains": [{"c": 3}]}
        delete_filter = {"payload__overlap": [True, [1, 2]]}
        if detect_dialect(db.url) == "sqlite":
            for filters in (update_filter, delete_filter):
                query = ArrayDoc.objects.filter(**filters)
                with pytest.raises(RuntimeError, match=SQLITE_SCALARS_ONLY):
                    await query.update(name="touched", using=db.name)
                with pytest.raises(RuntimeError, match=SQLITE_SCALARS_ONLY):
                    await query.delete(using=db.name)
            update_filter = {"payload__contains": ["x"]}
            delete_filter = {"payload__overlap": [True, 1.5]}

        updated = await ArrayDoc.objects.filter(**update_filter).update(
            name="touched", using=db.name
        )
        assert updated == 1
        assert await _names(db, name="touched") == {"touched"}

        deleted = await ArrayDoc.objects.filter(**delete_filter).delete(using=db.name)
        assert deleted == 2
        assert await _names(db) == {"empty", "nulls"}

    @pytest.mark.asyncio
    async def test_joined_array_column(self, db):
        ids = await _seed(db)
        await ArrayNote.objects.create(doc_id=ids["full"], tags="go", using=db.name)
        await ArrayNote.objects.create(doc_id=ids["empty"], tags="rust", using=db.name)

        # doc__tags is the joined array column, not the note's own text `tags`.
        rows = await ArrayNote.objects.filter(doc__tags__contains=["rust"]).all(
            using=db.name
        )
        assert [row.tags for row in rows] == ["go"]

        rows = await ArrayNote.objects.filter(
            doc__tags__overlap=["go", "python"], tags="rust"
        ).all(using=db.name)
        assert [row.tags for row in rows] == ["rust"]

    @pytest.mark.asyncio
    async def test_renamed_column(self, db):
        await ArrayNote.objects.create(tags="a", labels=["x", "y"], using=db.name)
        await ArrayNote.objects.create(tags="b", labels=["z"], using=db.name)
        rows = await ArrayNote.objects.filter(labels__overlap=["x", "q"]).all(
            using=db.name
        )
        assert [row.tags for row in rows] == ["a"]


class TestColumnsNamedLikeInternalAliases:
    """A stored column named like an alias or column inside the lookup's
    subqueries (`__oxyde_v` on PostgreSQL, `oxyde_v` / `oxyde_j` on MySQL,
    `oxyde_c` and json_each()'s `value` / `type` on SQLite) is still the
    column being filtered."""

    @pytest.mark.asyncio
    async def test_typed_and_json_columns(self, db):
        rows = {
            "full": dict(
                v=[1, 2], i=[1, 2], j=["a", "b"], c=[1, "a"], val=[1, 2], kind=[1, "a"]
            ),
            "nulls": dict(
                v=[3, None], i=[3, None], j=["c", None], c=[None], val=[3, None], kind=[None]
            ),
        }
        for name, values in rows.items():
            await ArrayShadow.objects.create(name=name, **values, using=db.name)
            await ArrayShadowJson.objects.create(
                name=name, v=values["c"], using=db.name
            )

        async def names(model, **filters) -> set[str]:
            rows = await model.objects.filter(**filters).all(using=db.name)
            return {row.name for row in rows}

        # None elements take PostgreSQL's element-by-element path.
        for field in ("v", "i", "val"):
            assert await names(ArrayShadow, **{f"{field}__contains": [1]}) == {"full"}
            assert await names(ArrayShadow, **{f"{field}__contains": [None]}) == {"nulls"}
            assert await names(
                ArrayShadow, **{f"{field}__contained_by": [1, 2, None]}
            ) == {"full"}
            assert await names(
                ArrayShadow, **{f"{field}__overlap": [None, 2]}
            ) == {"full", "nulls"}
        assert await names(ArrayShadow, j__contains=["c", None]) == {"nulls"}
        assert await names(ArrayShadow, j__overlap=["b", None]) == {"full", "nulls"}
        for model, field in ((ArrayShadow, "c"), (ArrayShadow, "kind"), (ArrayShadowJson, "v")):
            assert await names(model, **{f"{field}__contains": ["a"]}) == {"full"}
            assert await names(model, **{f"{field}__contained_by": [None, 2]}) == {
                "nulls"
            }
            assert await names(model, **{f"{field}__overlap": [1, None]}) == {
                "full",
                "nulls",
            }
        assert await names(ArrayShadowJson, v__contains=[1, "a"]) == {"full"}
        assert await names(ArrayShadowJson, v__contained_by=[None]) == {"nulls"}


class TestTemporalArrays:
    @pytest.mark.asyncio
    async def test_aware_and_naive_elements_match_as_utc(self, db):
        # An aware element of a naive list is stored as naive UTC, and lookup
        # operands are converted the same way.
        await ArrayDoc.objects.create(
            name="t",
            times=[datetime(2024, 1, 1, 10, 0, tzinfo=CEST), datetime(2024, 1, 2)],
            using=db.name,
        )
        assert await _names(db, times__contains=[datetime(2024, 1, 1, 8, 0)]) == {"t"}
        assert await _names(
            db, times__contains=[datetime(2024, 1, 1, 10, 0, tzinfo=CEST)]
        ) == {"t"}
        assert await _names(
            db, times__overlap=[datetime(2024, 1, 2, tzinfo=timezone.utc)]
        ) == {"t"}
        assert await _names(db, times__contains=[datetime(2024, 1, 1, 10, 0)]) == set()


class TestDecimalArraysWithForeignValues:
    """Elements written outside Oxyde never fail or fake a decimal match."""

    ROWS = {
        "text": '["abc"]',
        "bool": "[true]",
        "object": '[{"a": 1}]',
        "exponent": '["1E+2"]',
        "huge": '["1e100"]',
        "tiny": '["1E-40"]',
        "scaled": '["1.50"]',
        "number": "[1.5]",
        # 31 fractional digits: DECIMAL(65,30) would round it to 1e-28.
        "rounds": '["1.001e-28"]',
        # Exactly 1e-28, however it is written.
        "tiny_exact": '["1000e-31"]',
    }

    @pytest.mark.asyncio
    async def test_only_numeric_elements_compare(self, db):
        if detect_dialect(db.url) == "postgres":
            pytest.skip("NUMERIC[] columns cannot hold non-numeric elements")
        for name, amounts in self.ROWS.items():
            await execute_raw(
                "INSERT INTO array_docs (name, amounts) VALUES (?, ?)",
                [name, amounts],
                using=db.name,
            )

        async def names(*args, **filters) -> set[str]:
            # Names only: these rows do not load as ArrayDoc models.
            query = ArrayDoc.objects.filter(*args, **filters)
            return set(await query.values_list("name", flat=True).all(using=db.name))

        # "abc", true and {"a": 1} are not 0, and 1E-40 does not round to it.
        zero = [Decimal("0")]
        assert await names(amounts__contains=zero) == set()
        assert await names(amounts__overlap=zero) == set()
        assert await names(~Q(amounts__contains=zero)) == set(self.ROWS)
        assert await names(amounts__contains=[Decimal("100")]) == {"exponent"}
        assert await names(amounts__contains=[Decimal("1.5")]) == {"scaled", "number"}
        assert await names(amounts__contained_by=[Decimal("1.5")]) == {
            "scaled",
            "number",
        }

        # Values that would round into the operand never match; exact ones do.
        smallest = [Decimal("1e-28")]
        assert await names(amounts__contains=smallest) == {"tiny_exact"}
        assert await names(amounts__overlap=smallest) == {"tiny_exact"}
        assert await names(amounts__contained_by=smallest) == {"tiny_exact"}

        # Strict mode turns a failed cast into an error in UPDATE/DELETE.
        query = ArrayDoc.objects.filter(amounts__contains=zero)
        assert await query.update(name="touched", using=db.name) == 0
        assert await query.delete(using=db.name) == 0
        query = ArrayDoc.objects.filter(amounts__overlap=smallest)
        assert await query.update(name="touched", using=db.name) == 1
        assert await query.delete(using=db.name) == 1


class TestPostgresArrayCasts:
    @pytest.mark.asyncio
    async def test_operands_cast_to_overridden_column_types(self, db):
        if detect_dialect(db.url) != "postgres":
            pytest.skip("db_type array overrides are PostgreSQL DDL")
        await execute_raw("DROP TABLE IF EXISTS pg_array_overrides", using=db.name)
        await execute_raw(
            "CREATE TABLE pg_array_overrides ("
            "id BIGSERIAL PRIMARY KEY, small INTEGER[], codes CHAR(3)[], names VARCHAR(5)[], "
            "stamps TIMESTAMPTZ[], ratios FLOAT(24)[], blobs BYTEA[], docs JSONB[])",
            using=db.name,
        )
        await PgArrayOverrides.objects.create(
            small=[1, 2],
            codes=["abc", "de"],
            names=["annab"],
            # Naive elements of a TIMESTAMPTZ[] column are taken as UTC
            stamps=[datetime(2024, 1, 1, 8, 0)],
            ratios=[1.5, 0.25],
            blobs=[b"\x00ab", b"cd"],
            # JSON text and a plain string
            docs=['"hello"', "world"],
            using=db.name,
        )

        async def count(**filters) -> int:
            return await PgArrayOverrides.objects.filter(**filters).count(using=db.name)

        # integer[] vs a bigint[] operand has no @> operator without the cast.
        assert await count(small__contains=[1]) == 1
        assert await count(small__contained_by=[1, 2, 3]) == 1
        assert await count(small__contains=[1, None]) == 0
        # A bare CHAR cast would truncate "abc" to "a"; bpchar keeps it.
        assert await count(codes__contains=["abc"]) == 1
        assert await count(codes__contains=["a"]) == 0
        assert await count(codes__overlap=["de"]) == 1
        # VARCHAR(5) is not applied to the operand: "annabel" must not be
        # truncated into a match for the stored "annab".
        assert await count(names__overlap=["annabel"]) == 0
        assert await count(names__contains=["annab"]) == 1
        assert await count(stamps__contains=[datetime(2024, 1, 1, 8, 0)]) == 1
        assert await count(stamps__contains=[datetime(2024, 1, 1, 10, 0, tzinfo=CEST)]) == 1
        # FLOAT(24) is real: the operand is cast to real[], not FLOAT[]
        # (double precision[]), which has no operator with real[].
        assert await count(ratios__contains=[1.5]) == 1
        assert await count(ratios__overlap=[0.25, 9.0]) == 1
        # Elements the column type cannot hold equal no stored element.
        assert await count(small__overlap=[1, 2**31]) == 1
        assert await count(small__contained_by=[1, 2, 2**31]) == 1
        assert await count(small__contains=[1, 2**31]) == 0
        assert await count(ratios__contains=[1e300]) == 0
        # bytea[] operands bind as native arrays.
        assert await count(blobs__contains=[b"\x00ab"]) == 1
        assert await count(blobs__contained_by=[b"\x00ab", b"cd", b"ef"]) == 1
        assert await count(blobs__overlap=[b"ef"]) == 0
        # jsonb[] elements: strings are JSON strings, never parsed, for
        # operands as on insert ('"hello"' and "hello" differ).
        assert await count(docs__contains=['"hello"']) == 1
        assert await count(docs__contains=["hello"]) == 0
        assert await count(docs__overlap=["world", "x"]) == 1
        assert await count(docs__contained_by=['"hello"', "world", "z"]) == 1
        assert await count(docs__contains=["x"]) == 0

        # None stores an SQL NULL element, and matches a jsonb null one too
        # (written from "null" before strings were stored as strings);
        # "null" is a string.
        await PgArrayOverrides.objects.create(docs=[None], using=db.name)
        await execute_raw(
            "INSERT INTO pg_array_overrides (docs) VALUES (ARRAY['null'::jsonb])",
            using=db.name,
        )
        await PgArrayOverrides.objects.create(docs=["null"], using=db.name)
        for lookup in ("contains", "contained_by", "overlap"):
            assert await count(**{f"docs__{lookup}": [None]}) == 2, lookup
            assert await count(**{f"docs__{lookup}": ["null"]}) == 1, lookup
        # Integers past 2^53 are fine when a double holds them exactly.
        await PgArrayOverrides.objects.create(ratios=[float(2**54)], using=db.name)
        assert await count(ratios__contains=[2**54]) == 1
        with pytest.raises(RuntimeError):
            await count(ratios__contains=[2**54 + 1])


class TestSqliteInterop:
    """Array lookups are plain SQL: rows written by other clients match like
    Oxyde's own, and the generated SQL runs on any SQLite 3.45+ connection."""

    # Rows another client writes: text and JSONB arrays, malformed text and
    # a JSON document that is not an array.
    EXTERNAL = {
        "text": """'["zig", "rust"]'""",
        "jsonb": """jsonb('["zig"]')""",
        "malformed": """'["zig"'""",
        "scalar": """'"zig"'""",
    }

    @pytest.mark.asyncio
    async def test_other_clients_share_the_file(self, db):
        if detect_dialect(db.url) != "sqlite":
            pytest.skip("SQLite file interop")
        if sqlite3.sqlite_version_info < (3, 45):
            pytest.skip("jsonb() needs SQLite 3.45+")
        await _seed(db)
        path = db.url.removeprefix("sqlite://")

        # A stdlib connection opened while Oxyde's pool is still live.
        with closing(sqlite3.connect(path, timeout=5)) as conn:
            for name, tags in self.EXTERNAL.items():
                conn.execute(
                    f"INSERT INTO array_docs (name, tags) VALUES (?, {tags})", [name]
                )
            conn.commit()

        def query(*args, **filters):
            # Names only: the malformed row does not load as an ArrayDoc.
            return ArrayDoc.objects.filter(*args, **filters).values_list(
                "name", flat=True
            )

        async def names(*args, **filters) -> set[str]:
            return set(await query(*args, **filters).all(using=db.name))

        # Malformed text and non-arrays are false, not errors, so negation
        # includes them; SQL NULL stays unknown.
        assert await names(tags__contains=["zig"]) == {"text", "jsonb"}
        assert await names(tags__contains=["rust"]) == {"full", "dupes", "text"}
        assert await names(~Q(tags__overlap=["zig"])) == {
            "full",
            "empty",
            "dupes",
            "malformed",
            "scalar",
        }

        # The SQL Oxyde runs gives the same rows on a connection without
        # Oxyde, with the lookup value bound as the JSON text Oxyde binds
        # (decimals as strings).
        lookups = [
            ("tags__contains", ["zig"]),
            ("tags__contained_by", ["zig", "rust", "python", None]),
            ("tags__overlap", ["go", "zig"]),
            ("scores__contains", [3, 1]),
            ("amounts__overlap", [Decimal("2")]),
            ("payload__contained_by", [1, True, 2]),
        ]
        with closing(sqlite3.connect(path, timeout=5)) as conn:
            for key, value in lookups:
                sql, params = query(**{key: value}).sql(dialect="sqlite")
                assert len(params) == 1
                bound = [json.dumps(value, default=str)]
                rows = {name for (name,) in conn.execute(sql, bound)}
                assert rows == await names(**{key: value}), key
                assert rows, key
