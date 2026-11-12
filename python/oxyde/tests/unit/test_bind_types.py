"""Binding-only type hints for columns whose db_type has no column spec.

A column with an unrecognised ``db_type`` (MONEY, CITEXT, a domain) is omitted
from ``column_types`` (Rust decodes it natively). Its Python annotation still
tells Rust how to bind a value that msgpack carries as a string (Decimal,
UUID), through the separate ``bind_types`` map.
"""

from __future__ import annotations

from decimal import Decimal
from enum import Enum
from uuid import UUID

from oxyde import Field, Model

U = UUID("550e8400-e29b-41d4-a716-446655440000")


class HintStatus(Enum):
    DRAFT = "draft"


class HintModel(Model):
    id: int | None = Field(default=None, db_pk=True)
    amount: Decimal | None = Field(default=None, db_nullable=True, db_type="MONEY")
    ref: UUID | None = Field(default=None, db_nullable=True, db_type="MY_DOMAIN")
    code: str = Field(default="", db_type="CITEXT")
    # db_type on an enum field = plain verbatim column, enum machinery off
    status: HintStatus = Field(default=HintStatus.DRAFT, db_type="CITEXT")
    # recognised db_type: has a column spec, needs no hint
    price: Decimal | None = Field(default=None, db_nullable=True, db_type="NUMERIC(10,2)")

    class Meta:
        is_table = True
        table_name = "hint_model"


class TestBindTypesMetadata:
    def test_unrecognised_db_type_columns_get_annotation_hints(self):
        meta = HintModel._db_meta
        assert meta.bind_types["amount"] == {"kind": "decimal"}
        assert meta.bind_types["ref"] == {"kind": "uuid"}
        assert meta.bind_types["code"]["kind"] in {"text", "string"}

    def test_hinted_columns_stay_out_of_column_types(self):
        column_types = HintModel._db_meta.column_types or {}
        for column in ("amount", "ref", "code"):
            assert column not in column_types

    def test_enum_with_db_type_gets_no_enum_hint(self):
        assert "status" not in (HintModel._db_meta.bind_types or {})

    def test_recognised_db_type_needs_no_hint(self):
        assert "price" not in (HintModel._db_meta.bind_types or {})
        assert "price" in HintModel._db_meta.column_types


class TestBindTypesBinding:
    def test_decimal_filter_on_money_column_binds_as_decimal(self):
        _, params = HintModel.objects.filter(amount=Decimal("2.50")).sql(
            dialect="postgres", with_types=True
        )
        assert params == [("Decimal", "2.50")]

    def test_uuid_filter_on_domain_column_binds_as_uuid(self):
        _, params = HintModel.objects.filter(ref=U).sql(
            dialect="postgres", with_types=True
        )
        assert params == [("Uuid", str(U))]

    def test_text_column_string_is_not_reinterpreted(self):
        _, params = HintModel.objects.filter(code="2024-01-15T12:30:00Z").sql(
            dialect="postgres", with_types=True
        )
        assert params == [("String", "2024-01-15T12:30:00Z")]

    def test_enum_with_db_type_binds_plain_string_without_cast(self):
        sql, params = HintModel.objects.filter(status=HintStatus.DRAFT).sql(
            dialect="postgres", with_types=True
        )
        assert "::" not in sql
        assert params == [("String", "draft")]
