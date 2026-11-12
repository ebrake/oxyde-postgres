"""Expression and operator-class indexes: Index() API and schema extraction.

The wire shape asserted here is the Rust ``IndexKey`` contract: a plain column
without an operator class stays a bare string; everything else is an object
with ``column`` or ``expr`` and an optional ``opclass``.
"""

from __future__ import annotations

import json
import warnings
from datetime import date
from decimal import Decimal
from enum import Enum
from uuid import UUID

import pytest

from oxyde import (
    Coalesce,
    F,
    Field,
    Greatest,
    Index,
    Lower,
    Model,
    RawSQL,
    Upper,
    Value,
)
from oxyde.core import migration_compute_diff
from oxyde.migrations.extract import extract_current_schema
from oxyde.models.registry import clear_registry, register_table


def _extract(*models: type[Model]) -> dict:
    clear_registry()
    for model in models:
        register_table(model, overwrite=True)
    return extract_current_schema(dialect="postgres")


def _indexes(snapshot: dict, table: str) -> dict[str, dict]:
    return {index["name"]: index for index in snapshot["tables"][table]["indexes"]}


class Status(Enum):
    ACTIVE = "active"
    CLOSED = "closed"


class TestIndexValidation:
    def test_plain_fields_are_unchanged(self):
        index = Index(["a", "b"], unique=True)
        assert index.fields == ("a", "b")
        assert index.opclasses is None

    def test_bare_f_is_its_column_name(self):
        assert Index((F("email"),)).fields == ("email",)

    def test_expression_fields_are_kept(self):
        expr = Lower("email")
        index = Index((expr, "tenant_id"), name="ix_users_lower_email")
        assert index.fields == (expr, "tenant_id")

    @pytest.mark.parametrize("bad", [42, None, 1.5, ("a",), b"a"])
    def test_other_key_types_are_rejected(self, bad):
        with pytest.raises(TypeError, match="column names or expressions"):
            Index((bad,), name="ix")

    def test_expression_index_requires_a_name(self):
        with pytest.raises(ValueError, match="explicit name"):
            Index((Lower("email"),))
        with pytest.raises(ValueError, match="explicit name"):
            Index((Lower("email"),), name="")

    def test_index_names_are_not_length_checked_here(self):
        # Truncation collisions are rejected by the Rust schema check at
        # extract time, for every kind of index; the model itself must load.
        assert Index(("a",), name="x" * 100).name == "x" * 100
        assert Index((Lower("a"),), name="x" * 100).name == "x" * 100

    def test_opclasses_must_match_fields(self):
        assert Index(("a", "b"), opclasses=["text_pattern_ops", None]).opclasses == (
            "text_pattern_ops",
            None,
        )
        with pytest.raises(ValueError, match="one per field"):
            Index(("a", "b"), opclasses=("text_pattern_ops",))
        with pytest.raises(TypeError, match="str or None"):
            Index(("a",), opclasses=(1,))

    def test_existing_validation_still_applies(self):
        with pytest.raises(ValueError, match="at least one field"):
            Index(())
        with pytest.raises(ValueError, match="nulls_distinct"):
            Index(("a",), nulls_distinct=False)

    @pytest.mark.parametrize("method", ["spgist", "brin"])
    def test_new_methods_pass_through(self, method):
        assert Index(("a",), method=method).method == method


class TestLowerUpper:
    def test_lower_wraps_a_column(self):
        assert Lower("email")._expr == {
            "type": "func",
            "name": "lower",
            "args": [{"type": "column", "name": "email"}],
        }

    def test_upper_accepts_expressions_and_literals(self):
        assert Upper(F("name"))._expr["args"] == [{"type": "column", "name": "name"}]
        assert Upper(Value("x"))._expr["args"][0]["type"] == "value"

    @pytest.mark.parametrize("cls", [Lower, Upper])
    def test_exactly_one_argument(self, cls):
        with pytest.raises(TypeError):
            cls()
        with pytest.raises(TypeError):
            cls("a", "b")


class TestExtractShape:
    def test_plain_index_bytes_are_unchanged(self):
        class Plain(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str
            tenant_id: int

            class Meta:
                is_table = True
                table_name = "plain"
                indexes = [Index(("tenant_id", "email"))]

        index = _indexes(_extract(Plain), "plain")["plain_tenant_id_email_idx"]
        assert index["fields"] == ["tenant_id", "email"]

    def test_expression_key_shape_and_db_column_mapping(self):
        class Account(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str = Field(db_column="email_address")
            tenant_id: int = Field(db_column="tenant")

            class Meta:
                is_table = True
                table_name = "accounts"
                indexes = [
                    Index(
                        ("tenant_id", Lower("email")),
                        unique=True,
                        name="ux_accounts_tenant_email_lower",
                    )
                ]

        index = _indexes(_extract(Account), "accounts")[
            "ux_accounts_tenant_email_lower"
        ]
        assert index["fields"] == [
            "tenant",
            {
                "expr": {
                    "type": "func",
                    "name": "lower",
                    "args": [{"type": "column", "name": "email_address"}],
                }
            },
        ]

    def test_unknown_names_in_expressions_pass_through(self):
        class Raw(Model):
            id: int | None = Field(default=None, db_pk=True)
            # "legacy_col" is a db column, not a field name: it passes through.
            old: str = Field(db_column="legacy_col")

            class Meta:
                is_table = True
                table_name = "raw_names"
                indexes = [Index((Lower("legacy_col"),), name="ix_raw_names_lower")]

        key = _indexes(_extract(Raw), "raw_names")["ix_raw_names_lower"]["fields"][0]
        assert key["expr"]["args"] == [{"type": "column", "name": "legacy_col"}]

    def test_related_field_names_are_rejected(self):
        class Rel(Model):
            id: int | None = Field(default=None, db_pk=True)

            class Meta:
                is_table = True
                table_name = "rel"
                indexes = [Index((Lower("author__name"),), name="ix_rel_lower")]

        with pytest.raises(ValueError, match="Related-field"):
            _extract(Rel)

    def test_opclasses_become_column_objects(self):
        class Doc(Model):
            id: int | None = Field(default=None, db_pk=True)
            title: str = Field(db_column="doc_title")
            slug: str

            class Meta:
                is_table = True
                table_name = "docs"
                indexes = [
                    Index(
                        ("title", "slug"),
                        opclasses=("text_pattern_ops", None),
                        name="ix_docs_title_pattern",
                    ),
                    Index(
                        (Lower("title"),),
                        method="gin",
                        opclasses=("gin_trgm_ops",),
                        name="ix_docs_title_trgm",
                    ),
                ]

        indexes = _indexes(_extract(Doc), "docs")
        assert indexes["ix_docs_title_pattern"]["fields"] == [
            {"column": "doc_title", "opclass": "text_pattern_ops"},
            "slug",
        ]
        trgm = indexes["ix_docs_title_trgm"]
        assert trgm["method"] == "gin"
        assert trgm["fields"][0]["opclass"] == "gin_trgm_ops"
        assert trgm["fields"][0]["expr"]["args"][0]["name"] == "doc_title"

    def test_literals_are_serialized_and_snapshot_safe(self):
        uid = UUID("12345678-1234-5678-1234-567812345678")

        class Lit(Model):
            id: int | None = Field(default=None, db_pk=True)
            qty: int = 0
            price: Decimal = Decimal("0")
            day: date | None = None
            ref: str | None = None
            status: Status = Status.ACTIVE

            class Meta:
                is_table = True
                table_name = "lits"
                indexes = [
                    Index((Coalesce("qty", 1),), name="ix_qty"),
                    Index((Coalesce("price", Decimal("1.50")),), name="ix_price"),
                    Index((Coalesce("day", date(2024, 1, 31)),), name="ix_day"),
                    Index((Coalesce("ref", uid),), name="ix_ref"),
                    Index((Coalesce("status", Status.ACTIVE),), name="ix_status"),
                    Index((Greatest("qty", 1, 2),), name="ix_greatest"),
                    Index((F("qty") * 2 + 1,), name="ix_arith"),
                    Index((RawSQL("(qty % 2)"),), name="ix_raw"),
                ]

        snapshot = _extract(Lit)
        indexes = _indexes(snapshot, "lits")

        def literal(name: str) -> dict:
            return indexes[name]["fields"][0]["expr"]["args"][1]

        assert literal("ix_qty")["value"] == 1
        assert literal("ix_price")["value"] == "1.50"
        assert literal("ix_price")["value_type"]["kind"] == "decimal"
        assert literal("ix_day")["value"] == "2024-01-31"
        assert literal("ix_ref")["value"] == str(uid)
        # An enum literal is serialized to its value. Its value_type embeds the
        # enum's member list; the core drops it when it reads the key (see
        # test_core_drops_enum_value_lists_from_index_keys).
        assert literal("ix_status")["value"] == "active"
        assert indexes["ix_raw"]["fields"][0] == {
            "expr": {"type": "raw", "sql": "(qty % 2)"}
        }
        # The whole snapshot must be plain JSON (it is written to disk and
        # replayed): no Decimal/date/UUID/Enum objects may remain.
        assert json.loads(json.dumps(snapshot)) == snapshot

    def test_core_drops_enum_value_lists_from_index_keys(self):
        class Tagged(Model):
            id: int | None = Field(default=None, db_pk=True)
            status: Status = Status.ACTIVE

            class Meta:
                is_table = True
                table_name = "tagged"
                indexes = [Index((Coalesce("status", Status.ACTIVE),), name="ix_status")]

        snapshot = _extract(Tagged)
        # What makemigrations writes into a migration file comes out of the
        # core's diff, not out of this dict.
        empty = {"version": 1, "tables": {}}
        ops = json.loads(migration_compute_diff(json.dumps(empty), json.dumps(snapshot)))
        create = next(op for op in ops if op["type"] == "create_table")
        index = next(i for i in create["table"]["indexes"] if i["name"] == "ix_status")
        assert index["fields"][0]["expr"]["args"][1] == {
            "type": "value",
            "value": "active",
        }

    def test_dedupe_ignores_expression_and_opclass_indexes(self):
        class Dedupe(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str = Field(db_index=True)
            name: str = Field(db_index=True)
            code: str = Field(db_index=True)

            class Meta:
                is_table = True
                table_name = "dedupe"
                indexes = [
                    Index((Lower("email"),), name="ix_dedupe_lower_email"),
                    Index(
                        ("name",),
                        opclasses=("text_pattern_ops",),
                        name="ix_dedupe_name_p",
                    ),
                    Index(("code",), name="ix_dedupe_code"),
                ]

        with warnings.catch_warnings(record=True) as caught:
            warnings.simplefilter("always")
            snapshot = _extract(Dedupe)

        indexes = _indexes(snapshot, "dedupe")
        # db_index=True on email / name still produces its own plain index:
        # the declared ones are different indexes.
        assert indexes["dedupe_email_idx"]["fields"] == ["email"]
        assert indexes["dedupe_name_idx"]["fields"] == ["name"]
        # A plain duplicate is still skipped, with the existing warning.
        assert "dedupe_code_idx" not in indexes
        messages = [str(w.message) for w in caught]
        assert any("'code'" in m and "db_index=True" in m for m in messages)
        assert not any("'email'" in m or "'name'" in m for m in messages)

    @pytest.mark.parametrize("fallback", [[1, 2], []])
    def test_array_literals_retain_the_remapped_column_type(self, fallback):
        class Tags(Model):
            id: int | None = Field(default=None, db_pk=True)
            tags: list[int] | None = Field(default=None, db_column="tag_ids")

            class Meta:
                is_table = True
                table_name = "tags"
                indexes = [
                    Index(
                        (Coalesce(Coalesce("tags", fallback), fallback),),
                        name="ix_tags",
                    )
                ]

        snapshot = _extract(Tags)
        key = _indexes(snapshot, "tags")["ix_tags"]["fields"][0]
        assert key["column_types"] == {
            "tag_ids": {"kind": "array", "item": {"kind": "big_integer"}}
        }
        assert json.loads(json.dumps(snapshot)) == snapshot

    @pytest.mark.parametrize(
        "fallback", [b"fallback", b"", b"\x00\xff'\\", bytearray(b"abc")]
    )
    def test_binary_literals_are_json_safe_and_keep_the_blob_type(self, fallback):
        class Binary(Model):
            id: int | None = Field(default=None, db_pk=True)
            data: bytes | None = None

            class Meta:
                is_table = True
                table_name = "binary_literals"
                indexes = [Index((Coalesce("data", fallback),), name="ix_data")]

        snapshot = _extract(Binary)
        literal = _indexes(snapshot, "binary_literals")["ix_data"]["fields"][0]["expr"][
            "args"
        ][1]
        assert literal == {
            "type": "value",
            "value": list(fallback),
            "value_type": {"kind": "blob"},
        }
        assert json.loads(json.dumps(snapshot)) == snapshot

    def test_byte_list_literals_say_they_are_bytes(self):
        class Blobs(Model):
            id: int | None = Field(default=None, db_pk=True)
            blobs: list[bytes] | None = None
            numbers: list[int] | None = None

            class Meta:
                is_table = True
                table_name = "byte_lists"
                indexes = [
                    Index((Coalesce("blobs", [b"\x01\x02", b""]),), name="ix_blobs"),
                    Index((Coalesce("numbers", [1, 2]),), name="ix_numbers"),
                ]

        indexes = _indexes(_extract(Blobs), "byte_lists")

        def literal(name: str) -> dict:
            return indexes[name]["fields"][0]["expr"]["args"][1]

        # Octet lists are indistinguishable from integer lists in JSON, so a
        # list of bytes carries its type for the core to restore them.
        assert literal("ix_blobs") == {
            "type": "value",
            "value": [[1, 2], []],
            "value_type": {"kind": "array", "item": {"kind": "blob"}},
        }
        assert "value_type" not in literal("ix_numbers")

    def test_byte_list_literals_render_as_bytea_arrays(self):
        from oxyde.core import migration_compute_diff, migration_to_sql

        class Blobs(Model):
            id: int | None = Field(default=None, db_pk=True)
            blobs: list[bytes] | None = None

            class Meta:
                is_table = True
                table_name = "byte_arrays"
                indexes = [Index((Coalesce("blobs", [b"\x01\x02"]),), name="ix_blobs")]

        snapshot = _extract(Blobs)
        empty = {"version": 1, "tables": {}}
        ops = migration_compute_diff(json.dumps(empty), json.dumps(snapshot))
        statements = migration_to_sql(ops, "postgres")
        index_sql = next(sql for sql in statements if "ix_blobs" in sql)
        assert "0102" in index_sql, index_sql


class TestColumnMappingAndNaming:
    """Index keys are db columns; unnamed opclass indexes get distinct names."""

    def test_plain_and_opclass_keys_map_to_db_columns(self):
        class Remap(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str = Field(db_column="email_addr")

            class Meta:
                is_table = True
                table_name = "remap"
                indexes = [
                    Index(("email",), name="ix_remap_plain"),
                    Index(
                        ("email",),
                        opclasses=("text_pattern_ops",),
                        name="ix_remap_pattern",
                    ),
                ]

        indexes = _indexes(_extract(Remap), "remap")
        assert indexes["ix_remap_plain"]["fields"] == ["email_addr"]
        assert indexes["ix_remap_pattern"]["fields"] == [
            {"column": "email_addr", "opclass": "text_pattern_ops"}
        ]

    def test_fk_virtual_field_maps_to_its_column(self):
        class Owner(Model):
            id: int | None = Field(default=None, db_pk=True)

            class Meta:
                is_table = True
                table_name = "owners"

        class Pet(Model):
            id: int | None = Field(default=None, db_pk=True)
            owner: Owner | None = Field(default=None)
            name: str

            class Meta:
                is_table = True
                table_name = "pets"
                indexes = [Index(("owner", "name"), name="ix_pets_owner_name")]

        indexes = _indexes(_extract(Owner, Pet), "pets")
        assert indexes["ix_pets_owner_name"]["fields"] == ["owner_id", "name"]

    def test_unknown_index_column_is_rejected_by_the_schema_check(self):
        class Typo(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str

            class Meta:
                is_table = True
                table_name = "typo"
                indexes = [Index(("emial",), name="ix_typo_emial")]

        with pytest.raises(ValueError, match="ix_typo_emial.*'emial'"):
            _extract(Typo)

    def test_unnamed_opclass_index_does_not_collide_with_db_index(self):
        class Both(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str = Field(db_index=True)

            class Meta:
                is_table = True
                table_name = "both"
                indexes = [Index(("email",), opclasses=("text_pattern_ops",))]

        indexes = _indexes(_extract(Both), "both")
        assert set(indexes) == {"both_email_idx", "both_email_text_pattern_ops_idx"}
        assert indexes["both_email_idx"]["fields"] == ["email"]

    def test_schema_qualified_opclass_name_is_flattened(self):
        class Qualified(Model):
            id: int | None = Field(default=None, db_pk=True)
            name: str

            class Meta:
                is_table = True
                table_name = "qualified"
                indexes = [
                    Index(("name",), method="gin", opclasses=("public.gin_trgm_ops",))
                ]

        assert "qualified_name_public_gin_trgm_ops_idx" in _indexes(
            _extract(Qualified), "qualified"
        )

    def test_unnamed_plain_index_name_is_unchanged(self):
        class Plain2(Model):
            id: int | None = Field(default=None, db_pk=True)
            a: str
            b: str

            class Meta:
                is_table = True
                table_name = "plain2"
                indexes = [Index(("a", "b"))]

        assert "plain2_a_b_idx" in _indexes(_extract(Plain2), "plain2")

    def test_duplicate_warning_fires_for_a_remapped_column(self):
        class Dup(Model):
            id: int | None = Field(default=None, db_pk=True)
            email: str = Field(db_column="email_addr", db_index=True)

            class Meta:
                is_table = True
                table_name = "dup"
                indexes = [Index(("email",), name="ix_dup_email")]

        with warnings.catch_warnings(record=True) as caught:
            warnings.simplefilter("always")
            indexes = _indexes(_extract(Dup), "dup")

        assert set(indexes) == {"ix_dup_email"}
        assert any("db_index=True" in str(w.message) for w in caught)

    def test_colliding_explicit_names_are_rejected(self):
        class Clash(Model):
            id: int | None = Field(default=None, db_pk=True)
            a: str
            b: str

            class Meta:
                is_table = True
                table_name = "clash"
                indexes = [
                    Index(("a",), name="ix_clash"),
                    Index(("b",), name="ix_clash"),
                ]

        with pytest.raises(ValueError, match="ix_clash"):
            _extract(Clash)

    def test_empty_opclass_is_rejected_with_a_readable_error(self):
        class Empty(Model):
            id: int | None = Field(default=None, db_pk=True)
            name: str

            class Meta:
                is_table = True
                table_name = "empty_opclass"
                indexes = [Index(("name",), opclasses=("",), name="ix_empty")]

        with pytest.raises(ValueError, match="invalid operator class"):
            _extract(Empty)
