"""Integration tests for scalar expression annotations (Coalesce/Greatest/Value)."""

from __future__ import annotations

from datetime import timedelta
from decimal import Decimal
from enum import Enum

import pytest
import pytest_asyncio

from oxyde import (
    AsyncDatabase,
    Coalesce,
    Concat,
    F,
    Field,
    Greatest,
    Model,
    RawSQL,
    Value,
    disconnect_all,
)
from oxyde.db.schema import create_tables
from oxyde.models.registry import clear_registry, register_table
from oxyde.queries.raw import execute_raw


class SaStatus(str, Enum):
    DRAFT = "draft"
    LIVE = "live"


class SaGroup(Model):
    id: int | None = Field(default=None, db_pk=True)
    label: str = ""

    class Meta:
        is_table = True
        table_name = "sa_groups"


class SaItem(Model):
    id: int | None = Field(default=None, db_pk=True)
    group: SaGroup | None = Field(default=None, db_nullable=True)
    minimum_quantity: int | None = Field(default=None, db_nullable=True)
    nickname: str | None = Field(default=None, db_nullable=True)
    status: SaStatus = Field(default=SaStatus.LIVE)
    stock: int = Field(default=0, db_column="stock_qty")

    class Meta:
        is_table = True
        table_name = "sa_items"


class SaPrice(Model):
    """`MONEY` is not a known db_type: it has no column spec, only a binding-only
    Decimal hint, so it types neither sibling literals nor update targets."""

    id: int | None = Field(default=None, db_pk=True)
    label: str = ""
    amount: Decimal | None = Field(default=None, db_type="MONEY", db_nullable=True)

    class Meta:
        is_table = True
        table_name = "sa_prices"


class SaBag(Model):
    """Array and interval columns, which only a known column spec can decode."""

    id: int | None = Field(default=None, db_pk=True)
    label: str = ""
    nums: list[int] | None = Field(default=None, db_type="BIGINT[]", db_nullable=True)
    tags: list[str] | None = Field(default=None, db_type="TEXT[]", db_nullable=True)
    wait: timedelta | None = Field(default=None, db_nullable=True)

    class Meta:
        is_table = True
        table_name = "sa_bags"


async def _drop(database: AsyncDatabase) -> None:
    await execute_raw("DROP TABLE IF EXISTS sa_bags CASCADE", client=database)
    await execute_raw("DROP TABLE IF EXISTS sa_prices CASCADE", client=database)
    await execute_raw("DROP TABLE IF EXISTS sa_items CASCADE", client=database)
    await execute_raw("DROP TABLE IF EXISTS sa_groups CASCADE", client=database)
    await execute_raw("DROP TYPE IF EXISTS sa_status_enum CASCADE", client=database)


@pytest_asyncio.fixture
async def sa_db(postgres_database_url):
    clear_registry()
    register_table(SaGroup, overwrite=True)
    register_table(SaItem, overwrite=True)
    register_table(SaPrice, overwrite=True)
    register_table(SaBag, overwrite=True)
    database = AsyncDatabase(postgres_database_url, name="scalar_ann", overwrite=True)
    await database.connect()
    try:
        await _drop(database)
        await create_tables(database)
        g = await SaGroup.objects.create(label="g", using=database.name)
        for minimum, nick, stock in [(None, None, 10), (0, "bo", 20), (5, "al", 30)]:
            await SaItem.objects.create(
                group_id=g.id,
                minimum_quantity=minimum,
                nickname=nick,
                stock=stock,
                using=database.name,
            )
        yield database
    finally:
        await _drop(database)
        await disconnect_all()
        clear_registry()


def _eff_min():
    return Greatest(Coalesce("minimum_quantity", 1), 1)


class TestScalarAnnotations:
    @pytest.mark.asyncio
    async def test_greatest_coalesce_values(self, sa_db):
        rows = await (
            SaItem.objects.annotate(eff_min=_eff_min())
            .values("id", "eff_min")
            .order_by("id")
            .all(using=sa_db.name)
        )
        assert [r["eff_min"] for r in rows] == [1, 1, 5]

    @pytest.mark.asyncio
    async def test_values_list_flat(self, sa_db):
        out = await (
            SaItem.objects.annotate(eff_min=_eff_min())
            .order_by("id")
            .values_list("eff_min", flat=True)
            .all(using=sa_db.name)
        )
        assert out == [1, 1, 5]

    @pytest.mark.asyncio
    async def test_alias_only_values(self, sa_db):
        rows = await (
            SaItem.objects.annotate(eff_min=_eff_min())
            .values("eff_min")
            .all(using=sa_db.name)
        )
        assert all(set(r) == {"eff_min"} for r in rows)

    @pytest.mark.asyncio
    async def test_values_without_args_includes_annotation(self, sa_db):
        rows = await (
            SaItem.objects.annotate(eff_min=_eff_min()).values().all(using=sa_db.name)
        )
        assert "eff_min" in rows[0] and "id" in rows[0]

    @pytest.mark.asyncio
    async def test_models_raise_typeerror(self, sa_db):
        q = SaItem.objects.annotate(eff_min=_eff_min())
        with pytest.raises(TypeError, match="values"):
            await q.all(using=sa_db.name)
        with pytest.raises(TypeError):
            await q.first(using=sa_db.name)

    @pytest.mark.asyncio
    async def test_count_and_terminal_aggregate_on_annotated_queryset(self, sa_db):
        q = SaItem.objects.annotate(eff_min=_eff_min())
        assert await q.count(using=sa_db.name) == 3
        assert await q.sum("minimum_quantity", using=sa_db.name) == 5
        assert await q.sum("stock", using=sa_db.name) == 60
        assert await SaItem.objects.max("stock", using=sa_db.name) == 30

    @pytest.mark.asyncio
    async def test_order_by_alias(self, sa_db):
        q = (
            SaItem.objects.annotate(eff_min=_eff_min())
            .values("id", "eff_min")
            .order_by("-eff_min", "id")
        )
        rows = await q.all(using=sa_db.name)
        assert [r["eff_min"] for r in rows] == [5, 1, 1]
        rows = await q.join("group").all(using=sa_db.name)
        assert [r["eff_min"] for r in rows] == [5, 1, 1]

    @pytest.mark.asyncio
    async def test_db_column_field_in_expression(self, sa_db):
        rows = await (
            SaItem.objects.annotate(s=F("stock") + 1)
            .values("s")
            .order_by("s")
            .all(using=sa_db.name)
        )
        assert [r["s"] for r in rows] == [11, 21, 31]

    @pytest.mark.asyncio
    async def test_coalesce_text_and_enum_columns(self, sa_db):
        rows = await (
            SaItem.objects.annotate(
                n=Coalesce("nickname", Value("anon")),
                st=Coalesce("status", Value("draft")),
            )
            .values("id", "n", "st")
            .order_by("id")
            .all(using=sa_db.name)
        )
        assert [r["n"] for r in rows] == ["anon", "bo", "al"]
        assert [str(getattr(r["st"], "value", r["st"])) for r in rows] == ["live"] * 3


    @pytest.mark.asyncio
    async def test_bare_enum_member_arg_is_a_literal(self, sa_db):
        rows = await (
            SaItem.objects.annotate(st=Coalesce("status", SaStatus.DRAFT))
            .values("id", "st")
            .order_by("id")
            .all(using=sa_db.name)
        )
        assert [str(getattr(r["st"], "value", r["st"])) for r in rows] == ["live"] * 3

    @pytest.mark.asyncio
    async def test_enum_alias_decodes_next_to_raw_sql_and_arithmetic(self, sa_db):
        # PostgreSQL resolves the result to the enum whatever else is an
        # argument, so the alias must still decode as an enum, not as None.
        rows = await (
            SaItem.objects.annotate(
                st=Coalesce("status", RawSQL("'draft'")),
                gt=Greatest("status", RawSQL("'draft'")),
            )
            .values("id", "st", "gt")
            .order_by("id")
            .all(using=sa_db.name)
        )
        assert [str(getattr(r["st"], "value", r["st"])) for r in rows] == ["live"] * 3
        assert [str(getattr(r["gt"], "value", r["gt"])) for r in rows] == ["live"] * 3

    @pytest.mark.asyncio
    async def test_nested_functions_cast_outer_enum_literal(self, sa_db):
        rows = await (
            SaItem.objects.annotate(
                st=Greatest(Coalesce("status", Value("draft")), Value("live"))
            )
            .values("id", "st")
            .order_by("id")
            .all(using=sa_db.name)
        )
        assert [str(getattr(r["st"], "value", r["st"])) for r in rows] == ["live"] * 3

    @pytest.mark.asyncio
    async def test_greatest_int_column_with_float_literal_decodes(self, sa_db):
        rows = await (
            SaItem.objects.annotate(m=Greatest("stock", 25.5))
            .values("id", "m")
            .order_by("id")
            .all(using=sa_db.name)
        )
        assert [float(r["m"]) for r in rows] == [25.5, 25.5, 30.0]

    @pytest.mark.asyncio
    async def test_values_without_fields_keeps_joined_columns(self, sa_db):
        rows = await SaItem.objects.join("group").values().order_by("id").all(
            using=sa_db.name
        )
        assert len(rows) == 3
        assert all(r["group__label"] == "g" for r in rows)

    @pytest.mark.asyncio
    async def test_alias_in_join_namespace_is_rejected(self, sa_db):
        q = (
            SaItem.objects.join("group")
            .annotate(group__x=Coalesce("id", 0))
            .values("id", "group__x")
        )
        with pytest.raises(Exception, match="group__x"):
            await q.all(using=sa_db.name)

class TestConcatAndRawSQL:
    @pytest.mark.asyncio
    async def test_concat_skips_nulls_and_mixes_types(self, sa_db):
        rows = await (
            SaItem.objects.annotate(
                c=Concat("nickname", "stock", "status", separator="-")
            )
            .values("c")
            .order_by("id")
            .all(using=sa_db.name)
        )
        assert [r["c"] for r in rows] == ["10-live", "bo-20-live", "al-30-live"]

    @pytest.mark.asyncio
    async def test_raw_sql_annotation(self, sa_db):
        rows = await (
            SaItem.objects.annotate(
                n=RawSQL("COALESCE(LENGTH(nickname), 0)"), s=RawSQL("stock_qty * 2")
            )
            .values("n", "s")
            .order_by("id")
            .all(using=sa_db.name)
        )
        assert [(r["n"], r["s"]) for r in rows] == [(0, 20), (2, 40), (2, 60)]

    @pytest.mark.asyncio
    async def test_concat_in_update(self, sa_db):
        await SaItem.objects.filter(id=1).update(
            nickname=Concat(Value("n"), "stock", separator=""), using=sa_db.name
        )
        assert (await SaItem.objects.get(id=1, using=sa_db.name)).nickname == "n10"


class TestExpressionMutations:
    @pytest.mark.asyncio
    async def test_update_with_coalesce(self, sa_db):
        affected = await SaItem.objects.filter(minimum_quantity=None).update(
            minimum_quantity=Coalesce("minimum_quantity", F("stock"), 0),
            using=sa_db.name,
        )
        assert affected == 1
        row = await SaItem.objects.get(id=1, using=sa_db.name)
        assert row.minimum_quantity == 10

    @pytest.mark.asyncio
    async def test_update_expression_with_db_column_name(self, sa_db):
        await SaItem.objects.filter(id=1).update(stock=F("stock") * 2, using=sa_db.name)
        assert (await SaItem.objects.get(id=1, using=sa_db.name)).stock == 20

    @pytest.mark.asyncio
    async def test_increment_on_custom_db_column(self, sa_db):
        affected = await SaItem.objects.filter(id=1).increment(
            "stock", by=5, using=sa_db.name
        )
        assert affected == 1
        assert (await SaItem.objects.get(id=1, using=sa_db.name)).stock == 15

    @pytest.mark.asyncio
    async def test_update_enum_column_with_value_member(self, sa_db):
        await SaItem.objects.filter(id=1).update(
            status=Value(SaStatus.DRAFT), using=sa_db.name
        )
        item = await SaItem.objects.get(id=1, using=sa_db.name)
        assert item.status == SaStatus.DRAFT

    @pytest.mark.asyncio
    async def test_update_enum_column_with_string_literal_coalesce(self, sa_db):
        await SaItem.objects.filter(id=1).update(
            status=Coalesce("status", Value("draft")), using=sa_db.name
        )
        assert (
            await SaItem.objects.get(id=1, using=sa_db.name)
        ).status is SaStatus.LIVE

    @pytest.mark.asyncio
    async def test_greatest_of_literal_only_coalesce_and_enum_column(self, sa_db):
        rows = await (
            SaItem.objects.annotate(
                st=Greatest(Coalesce(Value("draft"), Value("live")), "status")
            )
            .values("id", "st")
            .order_by("id")
            .all(using=sa_db.name)
        )
        assert [str(getattr(r["st"], "value", r["st"])) for r in rows] == ["live"] * 3

    @pytest.mark.asyncio
    async def test_coalesce_enum_column_with_null_literal(self, sa_db):
        rows = await (
            SaItem.objects.annotate(
                a=Coalesce("status", None), b=Coalesce("status", Value(None))
            )
            .values("id", "a", "b")
            .order_by("id")
            .all(using=sa_db.name)
        )
        for r in rows:
            assert str(getattr(r["a"], "value", r["a"])) == "live"
            assert str(getattr(r["b"], "value", r["b"])) == "live"

    @pytest.mark.asyncio
    async def test_coalesce_int_column_with_null_literal(self, sa_db):
        rows = await (
            SaItem.objects.annotate(q=Coalesce("minimum_quantity", None))
            .values("id", "q")
            .order_by("id")
            .all(using=sa_db.name)
        )
        assert [r["q"] for r in rows] == [None, 0, 5]

    @pytest.mark.asyncio
    async def test_update_enum_with_coalesce_of_literals(self, sa_db):
        await SaItem.objects.filter(id=1).update(
            status=Coalesce(Value(None), Value(SaStatus.DRAFT)), using=sa_db.name
        )
        item = await SaItem.objects.get(id=1, using=sa_db.name)
        assert item.status == SaStatus.DRAFT

    @pytest.mark.asyncio
    async def test_update_value_decimal_on_unknown_db_type_column(self, sa_db):
        # Read back through raw SQL: the ORM would try to decode MONEY.
        await execute_raw(
            "INSERT INTO sa_prices (label, amount) VALUES ('a', 1)", client=sa_db
        )
        await SaPrice.objects.filter(label="a").update(
            amount=Value(Decimal("2.50")), using=sa_db.name
        )
        rows = await execute_raw(
            "SELECT amount::numeric::text AS amount FROM sa_prices", client=sa_db
        )
        assert rows[0]["amount"] == "2.50"

    @pytest.mark.asyncio
    async def test_raw_decimal_update_on_unknown_db_type_column(self, sa_db):
        # A plain Decimal (not Value(...)) is a string on the wire; the model's
        # annotation tells Rust to bind it as a Decimal, not text.
        await execute_raw(
            "INSERT INTO sa_prices (label, amount) VALUES ('a', 1)", client=sa_db
        )
        await SaPrice.objects.filter(label="a").update(
            amount=Decimal("2.50"), using=sa_db.name
        )
        rows = await execute_raw(
            "SELECT amount::numeric::text AS amount FROM sa_prices WHERE label = 'a'",
            client=sa_db,
        )
        assert rows[0]["amount"] == "2.50"

    @pytest.mark.asyncio
    async def test_raw_decimal_insert_on_unknown_db_type_column(self, sa_db):
        await SaPrice.objects.bulk_create(
            [SaPrice(label="b", amount=Decimal("3.25"))], using=sa_db.name
        )
        rows = await execute_raw(
            "SELECT amount::numeric::text AS amount FROM sa_prices WHERE label = 'b'",
            client=sa_db,
        )
        assert rows[0]["amount"] == "3.25"


class TestAnnotationTypeInference:
    @pytest.mark.asyncio
    async def test_array_column_alias_decodes(self, sa_db):
        await SaBag.objects.create(nums=[1, 2], using=sa_db.name)
        rows = await (
            SaBag.objects.annotate(copy=F("nums"), same=Coalesce("nums", None))
            .values("copy", "same", "nums")
            .all(using=sa_db.name)
        )
        assert rows == [{"copy": [1, 2], "same": [1, 2], "nums": [1, 2]}]

    @pytest.mark.asyncio
    async def test_interval_alias_matches_the_field(self, sa_db):
        await SaBag.objects.create(wait=timedelta(seconds=90), using=sa_db.name)
        rows = await (
            SaBag.objects.annotate(w=F("wait"), g=Greatest("wait", "wait"))
            .values("wait", "w", "g")
            .all(using=sa_db.name)
        )
        assert rows[0]["w"] == rows[0]["wait"] == rows[0]["g"] == 90.0

    @pytest.mark.asyncio
    async def test_count_and_exists_ignore_annotations_with_special_names(self, sa_db):
        counted = SaItem.objects.annotate(_count=F("status"))
        assert await counted.count(using=sa_db.name) == 3
        existing = SaItem.objects.annotate(exists=F("status"))
        assert await existing.exists(using=sa_db.name) is True

    @pytest.mark.asyncio
    async def test_leading_null_literal_has_no_forced_text_type(self, sa_db):
        rows = await (
            SaItem.objects.annotate(
                a=Coalesce(None, 1),
                b=Coalesce(F("minimum_quantity") + 1, None),
            )
            .values("id", "a", "b")
            .order_by("id")
            .all(using=sa_db.name)
        )
        assert [r["a"] for r in rows] == [1, 1, 1]
        assert [r["b"] for r in rows] == [None, 1, 6]

    @pytest.mark.asyncio
    async def test_null_literal_next_to_unknown_db_type_column(self, sa_db):
        await execute_raw(
            "INSERT INTO sa_prices (label, amount) VALUES ('a', 1)", client=sa_db
        )
        rows = await (
            SaPrice.objects.annotate(a=Coalesce("amount", None))
            .values("label")
            .all(using=sa_db.name)
        )
        assert rows == [{"label": "a"}]

    @pytest.mark.asyncio
    async def test_null_literal_in_update_next_to_unknown_db_type_column(self, sa_db):
        await execute_raw(
            "INSERT INTO sa_prices (label, amount) VALUES ('a', 1)", client=sa_db
        )
        await SaPrice.objects.filter(label="a").update(
            amount=Coalesce("amount", None), using=sa_db.name
        )
        rows = await execute_raw(
            "SELECT amount::numeric::text AS amount FROM sa_prices WHERE label = 'a'",
            client=sa_db,
        )
        assert rows[0]["amount"] == "1.00"

    @pytest.mark.asyncio
    async def test_list_literal_with_wrong_element_types_is_rejected(self, sa_db):
        # "1" and 2.5 do not fit BIGINT[]; they used to be stored as NULLs.
        bag = await SaBag.objects.create(label="bad", using=sa_db.name)
        with pytest.raises(Exception, match="does not match"):
            await SaBag.objects.filter(id=bag.id).update(
                nums=Coalesce("nums", Value(["1", 2.5])), using=sa_db.name
            )
        assert (await SaBag.objects.get(id=bag.id, using=sa_db.name)).nums is None

    @pytest.mark.asyncio
    async def test_update_array_default_with_empty_list_literal(self, sa_db):
        bag = await SaBag.objects.create(label="empty", using=sa_db.name)
        full = await SaBag.objects.create(label="full", tags=["x"], using=sa_db.name)
        await SaBag.objects.filter(id=bag.id).update(
            tags=Coalesce("tags", Value([])), using=sa_db.name
        )
        await SaBag.objects.filter(id=full.id).update(
            tags=Coalesce("tags", Value([])), using=sa_db.name
        )
        assert (await SaBag.objects.get(id=bag.id, using=sa_db.name)).tags == []
        assert (await SaBag.objects.get(id=full.id, using=sa_db.name)).tags == ["x"]

    @pytest.mark.asyncio
    async def test_annotate_array_default_with_empty_list_literal(self, sa_db):
        await SaBag.objects.create(label="empty", using=sa_db.name)
        rows = await (
            SaBag.objects.annotate(t=Coalesce("tags", Value([])))
            .values("t")
            .all(using=sa_db.name)
        )
        assert rows == [{"t": []}]
