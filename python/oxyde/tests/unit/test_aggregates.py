"""Tests for aggregate functions: Count, Sum, Avg, Max, Min + group_by."""

from __future__ import annotations

from uuid import UUID

import pytest

from oxyde import F, Field, Model
from oxyde.models.registry import register_table
from oxyde.queries.aggregates import (
    Aggregate,
    Avg,
    Coalesce,
    Concat,
    Count,
    Max,
    Min,
    RawSQL,
    Sum,
)
from oxyde.queries.expressions import Lower
from oxyde.tests.helpers import StubExecuteClient


class Product(Model):
    """Test model for aggregate tests."""

    id: int | None = Field(default=None, db_pk=True)
    name: str
    category: str = "default"
    price: float = 0.0
    quantity: int = 0
    rating: float | None = None

    class Meta:
        is_table = True


class AggregateSupplier(Model):
    id: int | None = Field(default=None, db_pk=True)
    name: str
    rating: int = Field(default=0, db_column="supplier_rating")

    class Meta:
        is_table = True


class BucketPost(Model):
    id: int | None = Field(default=None, db_pk=True)
    title: str = Field(db_column="bucket")

    class Meta:
        is_table = True


class SuppliedProduct(Model):
    id: int | None = Field(default=None, db_pk=True)
    supplier: AggregateSupplier | None = None
    cached_supplier: str = Field(default="", db_column="supplier__name")

    class Meta:
        is_table = True


class Tenant(Model):
    id: int | None = Field(default=None, db_pk=True)
    uuid: UUID = Field(db_unique=True, db_column="tenant_uuid")
    name: str

    class Meta:
        is_table = True


class TenantItem(Model):
    id: int | None = Field(default=None, db_pk=True)
    tenant: Tenant = Field(db_fk="uuid")

    class Meta:
        is_table = True


class ReviewPost(Model):
    id: int | None = Field(default=None, db_pk=True)
    title: str
    author: AggregateSupplier
    regional_affiliate_with_a_long_name_east: AggregateSupplier
    regional_affiliate_with_a_long_name_west: AggregateSupplier

    class Meta:
        is_table = True


class ReviewComment(Model):
    id: int | None = Field(default=None, db_pk=True)
    post: ReviewPost
    post_author: AggregateSupplier
    department_with_a_long_name_for_reporting: ReviewPost

    class Meta:
        is_table = True
        table_name = "post"


class QuotedProduct(Model):
    id: int | None = Field(default=None, db_pk=True)
    supplier: AggregateSupplier
    amount: int = Field(db_column="supplier.rating")

    class Meta:
        is_table = True


def test_explicit_filter_and_group_joins_use_the_fk_target_column():
    register_table(Tenant)
    queries = (
        TenantItem.objects.join("tenant"),
        TenantItem.objects.filter(tenant__name="Acme"),
        TenantItem.objects.annotate(n=Count("*")).group_by("tenant__name"),
    )
    for query in queries:
        assert query.to_ir()["joins"][0]["target_column"] == "tenant_uuid"
        sql, _ = query.sql()
        assert '"tenant"."tenant_uuid"' in sql


def test_nested_join_keeps_distinct_path_and_reuses_parent_alias():
    register_table(AggregateSupplier)
    register_table(ReviewPost)
    query = (
        ReviewComment.objects.filter(post__author__name="Alice")
        .group_by("post_author__name", "post__author__name")
        .values("post_author__name", "post__author__name")
    )
    ir = query.to_ir()
    joins = {join["path"]: join for join in ir["joins"]}

    assert joins["post"]["alias"] == "post_2"
    assert joins["post__author"]["parent"] == "post_2"
    assert joins["post__author"]["alias"] == "post__author"
    assert joins["post_author"]["alias"] == "post_author"
    assert ir["group_by"] == ["post_author.name", "post__author.name"]
    sql, _ = query.sql()
    assert '"post_2"."author_id" = "post__author"."id"' in sql
    assert 'WHERE "post__author"."name" = $1' in sql


def test_dotted_base_column_reserves_the_join_alias():
    register_table(AggregateSupplier)
    query = (
        QuotedProduct.objects.join("supplier")
        .filter(amount=7, supplier__rating=11)
        .order_by("amount", "supplier__rating")
    )
    ir = query.to_ir()
    assert ir["joins"][0]["alias"] == "supplier_2"
    assert ir["order_by"] == [
        ("supplier.rating", "ASC"),
        ("supplier_2.supplier_rating", "ASC"),
    ]
    sql, _ = query.sql()
    assert f'"{QuotedProduct.get_table_name()}"."supplier.rating" = $1' in sql
    assert '"supplier_2"."supplier_rating" = $2' in sql


@pytest.mark.parametrize("related_filter", [False, True])
def test_group_by_selected_scalar_annotation(related_filter):
    register_table(AggregateSupplier)
    query = ReviewPost.objects.annotate(bucket=Lower("title")).group_by("bucket")
    if related_filter:
        query = query.filter(author__name="Alice")

    sql, _ = query.values("bucket").sql()
    assert 'AS "bucket"' in sql
    assert "GROUP BY 1" in sql
    with pytest.raises(ValueError, match=r"group_by\('bucket'\).*not selected"):
        query.values("title").to_ir()


def test_order_by_base_column_or_aggregate_alias_with_same_name():
    query = SuppliedProduct.objects.filter().group_by("cached_supplier").annotate(
        supplier__name=Count("*")
    )
    sql, _ = query.order_by("cached_supplier").sql()
    table = SuppliedProduct.get_table_name()
    assert sql.endswith(f'ORDER BY "{table}"."supplier__name" ASC')

    sql, _ = query.order_by("supplier__name").sql()
    assert sql.endswith('ORDER BY "supplier__name" ASC')

    sql, _ = query.union_all(query).order_by("cached_supplier").sql()
    assert sql.endswith("ORDER BY 2 ASC")


def test_group_by_base_column_and_scalar_annotation_with_same_database_name():
    query = (
        BucketPost.objects.annotate(bucket=Lower("title"))
        .group_by("title", "bucket")
        .values("title", "bucket")
    )
    column = f"{BucketPost.get_table_name()}.bucket"
    ir = query.to_ir()
    assert ir["cols"] == [column]
    assert ir["group_by"] == [column, "bucket"]
    assert ir["output_labels"] == {column: "title"}


def test_union_related_order_projects_the_same_joins_in_every_operand():
    register_table(AggregateSupplier)
    first = ReviewPost.objects.filter(id=1)
    second = ReviewPost.objects.filter(
        regional_affiliate_with_a_long_name_east__name="Alice"
    )
    query = first.union_all(second).order_by("author__name")
    ir = query.to_ir()
    other = ir["union_query"]
    paths = ["author", "regional_affiliate_with_a_long_name_east"]
    assert [join["path"] for join in ir["joins"]] == paths
    assert [join["path"] for join in other["joins"]] == paths
    assert ir["cols"] == other["cols"]
    sql, _ = query.sql()
    assert sql.count('AS "author__name"') == 2
    assert sql.endswith(f'ORDER BY {len(ir["cols"]) + 2} ASC')

    with pytest.raises(ValueError, match="ordering a union by a related field"):
        query.values("title").to_ir()


def test_long_join_aliases_fit_and_long_group_labels_are_restored():
    register_table(AggregateSupplier)
    register_table(ReviewPost)
    east = "department_with_a_long_name_for_reporting__regional_affiliate_with_a_long_name_east__name"
    west = "department_with_a_long_name_for_reporting__regional_affiliate_with_a_long_name_west__name"
    query = (
        ReviewComment.objects.annotate(_oxyde_k0=Count("*"))
        .group_by(east, west)
        .values(east, west, "_oxyde_k0")
    )
    ir = query.to_ir()

    east_join, west_join = ir["joins"][1:]
    assert len(east_join["alias"].encode()) == 63
    assert len(west_join["alias"].encode()) == 63
    assert west_join["alias"].endswith("_2")
    assert east_join["alias"] != west_join["alias"]
    aliases = [ir["output_labels"][ref] for ref in ir["group_by"]]
    assert aliases == ["_oxyde_k1", "_oxyde_k2"]
    assert query._result_columns(["_oxyde_k0", *aliases]) == ["_oxyde_k0", east, west]


def test_ordering_by_a_related_field_locks_only_the_base_table():
    register_table(AggregateSupplier)
    sql, _ = ReviewPost.objects.order_by("author__name").for_update().sql()
    assert sql.endswith(f'FOR UPDATE OF "{ReviewPost.get_table_name()}"')


def test_distinct_with_explicit_columns_requires_ordering_by_selected_columns():
    register_table(AggregateSupplier)
    query = ReviewPost.objects.order_by("author__name").values("title").distinct()
    with pytest.raises(ValueError, match="author__name"):
        query.to_ir()

    sql, _ = ReviewPost.objects.order_by("author__name").distinct().sql()
    assert '"author"."name" AS "author__name"' in sql


def test_distinct_values_omit_joined_columns():
    register_table(AggregateSupplier)
    query = ReviewPost.objects.join("author").values("title").distinct()
    sql, _ = query.sql()
    assert query.to_ir()["project_joins"] is False
    assert "LEFT JOIN" in sql
    assert 'AS "author__name"' not in sql


def test_docs_group_posts_by_author():
    register_table(AggregateSupplier)
    query = (
        ReviewPost.objects.annotate(count=Count("*"))
        .group_by("author__name")
        .order_by("author__name")
        .values("author__name", "count")
    )
    sql, _ = query.sql()
    assert '"author"."name" AS "author__name"' in sql
    assert 'GROUP BY "author"."name" ORDER BY "author"."name" ASC' in sql


def test_related_grouping_adds_join():
    register_table(AggregateSupplier)
    query = SuppliedProduct.objects.filter()
    ir = (
        query.group_by("supplier__name")
        .annotate(n=Count("id"))
        .values("supplier__name", "n")
        .to_ir()
    )

    assert ir["group_by"] == ["supplier.name"]
    assert ir["cols"] == ["supplier.name", "n"]
    assert ir["output_labels"] == {"supplier.name": "supplier__name"}
    assert [join["path"] for join in ir["joins"]] == ["supplier"]
    assert not query.to_ir().get("joins")


def test_related_aggregate_input_adds_join():
    register_table(AggregateSupplier)
    ir = SuppliedProduct.objects.annotate(total=Sum("supplier__rating")).to_ir()

    assert ir["aggregates"][0]["field"] == "supplier.supplier_rating"
    assert [join["path"] for join in ir["joins"]] == ["supplier"]
    assert "supplier.supplier_rating" in ir["column_types"]
    assert {"field": "rating", "column": "supplier_rating"} in ir["joins"][0]["columns"]


def test_order_by_preserves_aggregate_alias_matching_mapped_field():
    ir = (
        AggregateSupplier.objects.annotate(rating=Max("rating"))
        .order_by("-rating")
        .to_ir()
    )

    assert ir["aggregates"][0]["field"] == "supplier_rating"
    assert ir["order_by"] == [("rating", "DESC")]


@pytest.mark.parametrize("replacement", [Sum("id"), F("id")])
def test_replacing_related_aggregate_drops_its_implicit_join(replacement):
    register_table(AggregateSupplier)
    original = SuppliedProduct.objects.annotate(total=Sum("supplier__rating"))
    replaced = original.annotate(total=replacement)

    assert not replaced.to_ir().get("joins")
    assert [join["path"] for join in original.to_ir()["joins"]] == ["supplier"]
    assert [
        join["path"]
        for join in replaced.filter(supplier__name="Alice").to_ir()["joins"]
    ] == ["supplier"]


def test_mapped_base_column_with_double_underscore_stays_on_base_table():
    register_table(AggregateSupplier)
    query = (
        SuppliedProduct.objects.join("supplier")
        .group_by("cached_supplier")
        .annotate(n=Count("id"))
        .order_by("cached_supplier")
        .values("cached_supplier", "n")
    )

    ir = query.to_ir()
    assert ir["group_by"] == ["supplier__name"]
    assert ir["order_by"] == [("supplier__name", "ASC")]
    table = SuppliedProduct.get_table_name()
    sql, _ = query.sql()
    assert sql.endswith(
        f'GROUP BY "{table}"."supplier__name" ORDER BY "{table}"."supplier__name" ASC'
    )


def test_ungrouped_related_selection_requires_grouping_without_an_explicit_join():
    register_table(AggregateSupplier)
    query = SuppliedProduct.objects.annotate(n=Count("id")).values("supplier__name", "n")

    with pytest.raises(ValueError, match=r"supplier__name.*group_by\(\)"):
        query.to_ir()


def test_related_and_same_named_base_group_keys_get_distinct_labels():
    register_table(AggregateSupplier)
    query = (
        SuppliedProduct.objects.filter()
        .group_by("cached_supplier", "supplier__name")
        .annotate(n=Count("id"))
        .values("cached_supplier", "supplier__name", "n")
    )

    ir = query.to_ir()
    assert ir["group_by"] == ["supplier__name", "supplier.name"]
    assert ir["output_labels"] == {
        "supplier__name": "cached_supplier",
        "supplier.name": "supplier__name",
    }
    table = SuppliedProduct.get_table_name()
    sql, _ = query.sql()
    assert f'"{table}"."supplier__name" AS "cached_supplier"' in sql
    assert '"supplier"."name" AS "supplier__name"' in sql


@pytest.mark.parametrize("explicit_join", [False, True])
def test_order_by_related_field_plans_its_join(explicit_join):
    register_table(AggregateSupplier)
    query = SuppliedProduct.objects.filter()
    if explicit_join:
        query = query.join("supplier")
    query = query.order_by("-supplier__name")

    ir = query.to_ir()
    assert ir["order_by"] == [("supplier.name", "DESC")]
    assert [join["path"] for join in ir["joins"]] == ["supplier"]
    sql, _ = query.sql()
    assert sql.endswith('ORDER BY "supplier"."name" DESC')
    # The base column keeps its field name as its spelling.
    by_column = SuppliedProduct.objects.order_by("cached_supplier").to_ir()
    assert by_column["order_by"] == [("supplier__name", "ASC")]
    assert not by_column.get("joins")


def test_order_by_annotation_alias_is_not_a_relation_path():
    register_table(AggregateSupplier)
    ir = (
        SuppliedProduct.objects.annotate(supplier__name=Count("id"))
        .order_by("-supplier__name")
        .to_ir()
    )

    assert ir["order_by"] == [("supplier__name", "DESC")]
    assert not ir.get("joins")


def test_order_by_random_plans_no_join():
    assert not SuppliedProduct.objects.order_by("?").to_ir().get("joins")


class Ledger(Model):
    id: int | None = Field(default=None, db_pk=True)
    amount: int = Field(default=0, db_column="legacy.amount")

    class Meta:
        is_table = True


def test_dotted_db_column_stays_one_identifier():
    total = Ledger.objects.annotate(total=Sum("amount"))
    assert total.to_ir()["aggregates"][0]["field"] == "legacy.amount"
    sql, _ = total.sql()
    assert 'SUM("legacy.amount") AS "total"' in sql

    ordered = Ledger.objects.order_by("amount")
    assert ordered.to_ir()["order_by"] == [("legacy.amount", "ASC")]
    sql, _ = ordered.sql()
    assert sql.endswith('ORDER BY "legacy.amount" ASC')

    grouped = (
        Ledger.objects.filter()
        .group_by("amount")
        .annotate(n=Count("id"))
        .values("amount", "n")
    )
    assert grouped.to_ir()["output_labels"] == {"legacy.amount": "amount"}
    table = Ledger.get_table_name()
    sql, _ = grouped.sql()
    assert f'"{table}"."legacy.amount" AS "amount"' in sql
    assert sql.endswith('GROUP BY "legacy.amount"')


class TestAggregateBase:
    """Test Aggregate base class."""

    def test_aggregate_init(self):
        """Test Aggregate initialization."""
        agg = Aggregate("price")
        assert agg.field == "price"
        assert agg.distinct is False

    def test_aggregate_with_distinct(self):
        """Test Aggregate with distinct=True."""
        agg = Aggregate("category", distinct=True)
        assert agg.distinct is True

    def test_aggregate_to_ir(self):
        """Test Aggregate.to_ir()."""
        agg = Aggregate("price")
        ir = agg.to_ir()

        assert ir["func"] == "aggregate"
        assert ir["field"] == "price"

    def test_aggregate_to_ir_with_distinct(self):
        """Test Aggregate.to_ir() with distinct."""
        agg = Aggregate("price", distinct=True)
        ir = agg.to_ir()

        assert ir["distinct"] is True


class TestCountAggregate:
    """Test Count aggregate function."""

    def test_count_default_star(self):
        """Test Count() defaults to '*'."""
        count = Count()
        assert count.field == "*"

    def test_count_with_field(self):
        """Test Count(field)."""
        count = Count("id")
        assert count.field == "id"

    def test_count_to_ir(self):
        """Test Count.to_ir()."""
        count = Count("id")
        ir = count.to_ir()

        assert ir["func"] == "count"
        assert ir["field"] == "id"

    def test_count_distinct(self):
        """Test Count with distinct."""
        count = Count("category", distinct=True)
        ir = count.to_ir()

        assert ir["func"] == "count"
        assert ir["distinct"] is True


class TestSumAggregate:
    """Test Sum aggregate function."""

    def test_sum_to_ir(self):
        """Test Sum.to_ir()."""
        s = Sum("price")
        ir = s.to_ir()

        assert ir["func"] == "sum"
        assert ir["field"] == "price"


class TestAvgAggregate:
    """Test Avg aggregate function."""

    def test_avg_to_ir(self):
        """Test Avg.to_ir()."""
        avg = Avg("rating")
        ir = avg.to_ir()

        assert ir["func"] == "avg"
        assert ir["field"] == "rating"


class TestMaxAggregate:
    """Test Max aggregate function."""

    def test_max_to_ir(self):
        """Test Max.to_ir()."""
        m = Max("price")
        ir = m.to_ir()

        assert ir["func"] == "max"
        assert ir["field"] == "price"


class TestMinAggregate:
    """Test Min aggregate function."""

    def test_min_to_ir(self):
        """Test Min.to_ir()."""
        m = Min("price")
        ir = m.to_ir()

        assert ir["func"] == "min"
        assert ir["field"] == "price"


class TestConcatExpression:
    """Concat is a scalar expression: CONCAT_WS(separator, ...)."""

    def test_concat_expression_tree(self):
        c = Concat("first_name", "last_name", separator=" ")

        assert c._expr["name"] == "concat_ws"
        assert c._expr["args"][0]["value"] == " "
        assert [a["name"] for a in c._expr["args"][1:]] == ["first_name", "last_name"]

    def test_default_separator_is_empty(self):
        assert Concat("a", "b")._expr["args"][0]["value"] == ""

    def test_requires_fields(self):
        with pytest.raises(ValueError):
            Concat()


class TestCoalesceExpression:
    """Coalesce is a scalar expression (not an aggregate)."""

    def test_coalesce_expression_tree(self):
        """Str args are column references."""
        c = Coalesce("nickname", "name", "email")

        assert c._expr == {
            "type": "func",
            "name": "coalesce",
            "args": [
                {"type": "column", "name": "nickname"},
                {"type": "column", "name": "name"},
                {"type": "column", "name": "email"},
            ],
        }


class TestRawSQL:
    """RawSQL is a scalar expression carrying a verbatim fragment."""

    def test_raw_sql_expression_node(self):
        raw = RawSQL("CURRENT_TIMESTAMP")

        assert raw._expr == {"type": "raw", "sql": "CURRENT_TIMESTAMP"}

    def test_raw_sql_rejects_empty(self):
        with pytest.raises(ValueError):
            RawSQL("  ")


class TestAnnotateMethod:
    """Test Query.annotate() method."""

    def test_annotate_with_count(self):
        """Test annotate() with Count."""
        query = Product.objects.filter().annotate(total=Count("*"))
        ir = query.to_ir()

        assert ir["aggregates"] is not None
        assert len(ir["aggregates"]) == 1
        assert ir["aggregates"][0]["op"] == "count"
        assert ir["aggregates"][0]["alias"] == "total"

    def test_annotate_with_sum(self):
        """Test annotate() with Sum."""
        query = Product.objects.filter().annotate(total_price=Sum("price"))
        ir = query.to_ir()

        assert ir["aggregates"][0]["op"] == "sum"
        assert ir["aggregates"][0]["field"] == "price"

    def test_annotate_with_avg(self):
        """Test annotate() with Avg."""
        query = Product.objects.filter().annotate(avg_rating=Avg("rating"))
        ir = query.to_ir()

        assert ir["aggregates"][0]["op"] == "avg"

    def test_annotate_immutability(self):
        """Test that annotate() returns new instance."""
        base = Product.objects.filter()
        annotated = base.annotate(total=Count("*"))

        assert base is not annotated


class TestGroupByMethod:
    """Test Query.group_by() method."""

    def test_group_by_single_field(self):
        """Test group_by() with single field."""
        query = Product.objects.filter().annotate(count=Count("*")).group_by("category")
        ir = query.to_ir()

        assert ir["group_by"] is not None
        assert "category" in ir["group_by"]

    def test_group_by_multiple_fields(self):
        """Test group_by() with multiple fields."""
        query = Product.objects.filter().annotate(count=Count("*")).group_by("category", "name")
        ir = query.to_ir()

        assert len(ir["group_by"]) == 2

    def test_group_by_immutability(self):
        """Test that group_by() returns new instance."""
        base = Product.objects.filter().annotate(count=Count("*"))
        grouped = base.group_by("category")

        assert base is not grouped


class TestHavingMethod:
    """Test Query.having() method."""

    def test_having_with_condition(self):
        """Test having() with condition via kwargs."""
        query = (
            Product.objects.filter()
            .annotate(count=Count("*"))
            .group_by("category")
            .having(id__gt=0)  # Use kwargs for having clause
        )
        ir = query.to_ir()

        assert ir["having"] is not None

    def test_having_alias_ending_in_a_lookup_name(self):
        """An alias like "tags__contains" is the alias, not alias + lookup."""
        query = (
            Product.objects.filter()
            .annotate(tags__contains=Count("*"), total=Count("*"))
            .group_by("category")
            .having(tags__contains=3, total__gt=1)
        )
        having = query.to_ir()["having"]
        conditions = having["conditions"]

        assert {c["field"]: c["operator"] for c in conditions} == {
            "tags__contains": "=",
            "total": ">",
        }

    def test_having_unsupported_lookup_on_an_alias(self):
        """A lookup having() cannot apply to an alias names the alias."""
        query = Product.objects.annotate(total=Count("*")).group_by("category")
        with pytest.raises(ValueError, match="Unsupported lookup 'bogus'"):
            query.having(total__bogus=1)
        with pytest.raises(ValueError, match="Unsupported lookup 'contains'"):
            query.having(total__contains=1)

    def test_having_model_keys_take_the_model_path(self):
        """Keys that do not start with an alias are model field filters."""
        query = (
            Product.objects.annotate(total=Count("*"))
            .group_by("category")
            .having(name__startswith="a", total__gte=2)
        )
        conditions = query.to_ir()["having"]["conditions"]

        assert {c["field"]: c["operator"] for c in conditions} == {
            "total": ">=",
            "name": "LIKE",
        }


class TestManagerAggregates:
    """Test manager aggregate methods."""

    @pytest.mark.asyncio
    async def test_manager_count(self):
        """Test manager.count() method."""
        # count() expects aggregate result with _count field
        stub = StubExecuteClient([(["_count"], [[3]])])

        count = await Product.objects.count(client=stub)

        assert count == 3

    @pytest.mark.asyncio
    async def test_manager_sum(self):
        """Test manager.sum() method."""
        stub = StubExecuteClient([(["__sum__"], [[150.0]])])

        await Product.objects.sum("price", client=stub)

        # Result depends on implementation
        assert stub.calls[0]["aggregates"][0]["op"] == "sum"

    @pytest.mark.asyncio
    async def test_manager_avg(self):
        """Test manager.avg() method."""
        stub = StubExecuteClient([(["__avg__"], [[4.5]])])

        await Product.objects.avg("rating", client=stub)

        assert stub.calls[0]["aggregates"][0]["op"] == "avg"

    @pytest.mark.asyncio
    async def test_manager_max(self):
        """Test manager.max() method."""
        stub = StubExecuteClient([(["__max__"], [[99.99]])])

        await Product.objects.max("price", client=stub)

        assert stub.calls[0]["aggregates"][0]["op"] == "max"

    @pytest.mark.asyncio
    async def test_manager_min(self):
        """Test manager.min() method."""
        stub = StubExecuteClient([(["__min__"], [[0.99]])])

        await Product.objects.min("price", client=stub)

        assert stub.calls[0]["aggregates"][0]["op"] == "min"


class TestAggregatesWithFilters:
    """Test aggregates combined with filters."""

    def test_filter_then_annotate(self):
        """Test filter().annotate()."""
        query = Product.objects.filter(category="electronics").annotate(
            total=Count("*")
        )
        ir = query.to_ir()

        assert ir.get("filter_tree") is not None
        assert ir["aggregates"] is not None

    def test_annotate_then_filter(self):
        """Test annotate().filter() - filter applied to base query."""
        query = (
            Product.objects.filter()
            .annotate(total=Count("*"))
            .filter(category="electronics")
        )
        ir = query.to_ir()

        assert ir["aggregates"] is not None


class TestGroupByWithAggregates:
    """Test group_by combined with various aggregates."""

    def test_group_by_with_count(self):
        """Test group_by with Count."""
        query = Product.objects.filter().annotate(count=Count("*")).group_by("category")
        ir = query.to_ir()

        assert ir["aggregates"][0]["op"] == "count"
        assert ir["group_by"] is not None

    def test_group_by_with_sum(self):
        """Test group_by with Sum."""
        query = Product.objects.filter().annotate(total=Sum("price")).group_by("category")
        ir = query.to_ir()

        assert ir["aggregates"][0]["op"] == "sum"
        assert ir["group_by"] is not None

    def test_group_by_with_avg(self):
        """Test group_by with Avg."""
        query = Product.objects.filter().annotate(avg_price=Avg("price")).group_by("category")
        ir = query.to_ir()

        assert ir["aggregates"][0]["op"] == "avg"


class TestMultipleAggregates:
    """Test multiple aggregates in a single query (bug fix verification)."""

    def test_multiple_aggregates_in_annotate(self):
        """Test that multiple aggregates are all included in IR."""
        query = Product.objects.filter().annotate(
            total=Count("*"),
            avg_price=Avg("price"),
            max_price=Max("price"),
        )
        ir = query.to_ir()

        assert ir["aggregates"] is not None
        assert len(ir["aggregates"]) == 3

        # Check all aggregates are present
        ops = {agg["alias"]: agg["op"] for agg in ir["aggregates"]}
        assert ops["total"] == "count"
        assert ops["avg_price"] == "avg"
        assert ops["max_price"] == "max"

    def test_multiple_aggregates_with_group_by(self):
        """Test multiple aggregates with GROUP BY."""
        query = (
            Product.objects.filter()
            .annotate(
                count=Count("*"),
                total_price=Sum("price"),
                min_price=Min("price"),
            )
            .group_by("category")
        )
        ir = query.to_ir()

        assert len(ir["aggregates"]) == 3
        assert ir["group_by"] is not None

    def test_chained_annotate_calls(self):
        """Test chaining multiple annotate() calls."""
        query = (
            Product.objects.filter().annotate(total=Count("*")).annotate(avg_price=Avg("price"))
        )
        ir = query.to_ir()

        assert len(ir["aggregates"]) == 2
