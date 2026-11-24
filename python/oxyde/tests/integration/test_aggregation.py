"""Integration tests for aggregation functions and group_by."""
from __future__ import annotations

from datetime import date, timedelta
from decimal import Decimal

import pytest

from oxyde import F, Count, Max, Min, Q, Sum, atomic
from oxyde.db.registry import get_connection
from oxyde.queries.expressions import Lower

from .conftest import (
    AliasedEvent,
    AllTypes,
    Comment,
    Post,
    ReviewGroup,
    ReviewLink,
    ReviewReport,
    SuppliedItem,
    Supplier,
    TdModel,
    Tenant,
    TenantItem,
)


class TestAggregateShortcuts:
    @pytest.mark.asyncio
    async def test_count(self, db):
        count = await Post.objects.filter(published=True).count(using=db.name)
        assert count == 4

    @pytest.mark.asyncio
    async def test_count_with_join(self, db):
        """count() must work when join() is applied (no dedup panic)."""
        count = await Post.objects.join("author").count(using=db.name)
        assert count == 6

    @pytest.mark.asyncio
    async def test_count_with_join_and_filter(self, db):
        """count() with join + filter on joined field."""
        count = await Post.objects.join("author").filter(author__name="Alice").count(using=db.name)
        assert count == 2

    @pytest.mark.asyncio
    async def test_exists_with_join(self, db):
        """exists() must work when join() is applied (no dedup panic)."""
        exists = await Post.objects.join("author").exists(using=db.name)
        assert exists is True

    @pytest.mark.asyncio
    async def test_sum(self, db):
        total = await Post.objects.filter(published=True).sum("views", using=db.name)
        assert total == 435  # 120 + 35 + 80 + 200

    @pytest.mark.asyncio
    async def test_avg(self, db):
        average = await Post.objects.filter(published=True).avg("views", using=db.name)
        assert average == pytest.approx(108.75)

    @pytest.mark.asyncio
    async def test_avg_related_integer_returns_float(self, db):
        average = await Post.objects.avg("author__id", using=db.name)

        assert isinstance(average, float)
        assert average == pytest.approx(2.0)

    @pytest.mark.asyncio
    async def test_max(self, db):
        maximum = await Post.objects.max("views", using=db.name)
        assert maximum == 200

    @pytest.mark.asyncio
    async def test_min(self, db):
        minimum = await Post.objects.filter(published=True).min("views", using=db.name)
        assert minimum == 35

    @pytest.mark.asyncio
    @pytest.mark.parametrize(
        ("operation", "expected"), [("sum", 155), ("avg", 77.5), ("min", 35), ("max", 120)]
    )
    async def test_shortcut_with_related_filter(self, db, operation, expected):
        related = Post.objects.filter(author__name="Alice")
        for query in (
            related,
            Post.objects.filter(Q(author__name="Alice")),
            Post.objects.join("author").filter(author__name="Alice"),
        ):
            assert await getattr(query, operation)("views", using=db.name) == expected

        async with atomic(using=db.name):
            assert await getattr(related, operation)("views", using=db.name) == expected

    @pytest.mark.asyncio
    async def test_shortcut_with_no_matching_related_rows(self, db):
        total = await Post.objects.filter(author__name="Nobody").sum("views", using=db.name)
        assert total is None


class TestPr8ReviewRegressions:
    @pytest.mark.asyncio
    async def test_min_max_alias_matching_timedelta_column_keep_units(self, db):
        for seconds in (90, 30):
            await TdModel.objects.create(duration=timedelta(seconds=seconds), using=db.name)
        plain = await (
            TdModel.objects.order_by("duration")
            .values_list("duration", flat=True)
            .all(using=db.name)
        )

        assert plain == [30.0, 90.0]
        for aggregate, seconds in ((Max, 90), (Min, 30)):
            values = await (
                TdModel.objects.annotate(duration=aggregate("duration"))
                .values_list("duration", flat=True)
                .all(using=db.name)
            )
            assert values == [timedelta(seconds=seconds)]

    @pytest.mark.asyncio
    async def test_sum_alias_matching_timedelta_column_is_not_dropped(self, db):
        for seconds in (90, 30):
            await TdModel.objects.create(duration=timedelta(seconds=seconds), using=db.name)

        values = await (
            TdModel.objects.annotate(duration=Sum("duration"))
            .values_list("duration", flat=True)
            .all(using=db.name)
        )
        # SUM of the BIGINT microseconds is NUMERIC; a Timedelta hint used to
        # read it as None.
        assert len(values) == 1
        assert Decimal(values[0]) == Decimal(120_000_000)

    async def _seed_suppliers(self, db):
        """Items (Acme, "x"), (Acme, "y"), (Bolt, "x"), in id order."""
        acme = await Supplier.objects.create(name="Acme", using=db.name)
        bolt = await Supplier.objects.create(name="Bolt", using=db.name)
        for supplier, cached in ((acme, "x"), (acme, "y"), (bolt, "x")):
            await SuppliedItem.objects.create(
                supplier_id=supplier.id, cached_supplier=cached, using=db.name
            )

    @pytest.mark.asyncio
    async def test_related_group_key_named_like_a_base_column(self, db):
        await self._seed_suppliers(db)

        names = await (
            SuppliedItem.objects.filter().group_by("supplier__name")
            .annotate(n=Count("id"))
            .order_by("supplier__name")
            .values_list("supplier__name", flat=True)
            .all(using=db.name)
        )
        assert names == ["Acme", "Bolt"]

        without_aggregates = await (
            SuppliedItem.objects.filter().group_by("supplier__name")
            .order_by("supplier__name")
            .values_list("supplier__name", flat=True)
            .all(using=db.name)
        )
        assert without_aggregates == ["Acme", "Bolt"]

    @pytest.mark.asyncio
    async def test_related_and_base_group_keys_both_projected(self, db):
        await self._seed_suppliers(db)

        rows = await (
            SuppliedItem.objects.filter().group_by("cached_supplier", "supplier__name")
            .annotate(n=Count("id"))
            .order_by("supplier__name", "cached_supplier")
            .values("cached_supplier", "supplier__name", "n")
            .all(using=db.name)
        )
        assert rows == [
            {"n": 1, "cached_supplier": "x", "supplier__name": "Acme"},
            {"n": 1, "cached_supplier": "y", "supplier__name": "Acme"},
            {"n": 1, "cached_supplier": "x", "supplier__name": "Bolt"},
        ]

    @pytest.mark.asyncio
    async def test_order_by_related_name_and_base_column_spellings(self, db):
        await self._seed_suppliers(db)

        by_relation = await (
            SuppliedItem.objects.join("supplier")
            .order_by("-supplier__name", "cached_supplier")
            .values_list("cached_supplier", flat=True)
            .all(using=db.name)
        )
        assert by_relation == ["x", "x", "y"]

        by_column = await (
            SuppliedItem.objects.order_by("-cached_supplier", "id")
            .values_list("cached_supplier", flat=True)
            .all(using=db.name)
        )
        assert by_column == ["y", "x", "x"]

    @pytest.mark.asyncio
    async def test_joined_rows_keep_base_column_named_like_relation(self, db):
        """A base column labeled "supplier__name" stays in the main rows when
        the supplier relation is joined, and keeps its own value."""
        await self._seed_suppliers(db)
        query = SuppliedItem.objects.join("supplier").order_by("id")

        items = await query.all(using=db.name)
        assert [(item.cached_supplier, item.supplier.name) for item in items] == [
            ("x", "Acme"),
            ("y", "Acme"),
            ("x", "Bolt"),
        ]

        rows = await query.values("cached_supplier").all(using=db.name)
        assert [row["cached_supplier"] for row in rows] == ["x", "y", "x"]

        client = await get_connection(db.name)
        raw_rows = await query.fetch_rows(client)
        assert [
            (row["cached_supplier"], row["supplier__name"]) for row in raw_rows
        ] == [("x", "Acme"), ("y", "Acme"), ("x", "Bolt")]

    @pytest.mark.asyncio
    async def test_nested_join_hydrates_each_level(self, db):
        comments = await (
            Comment.objects.join("post__author").order_by("id").all(using=db.name)
        )

        assert [
            (comment.post.title, comment.post.author.name) for comment in comments
        ] == [
            ("Rust Patterns", "Alice"),
            ("Rust Patterns", "Alice"),
            ("Quantum Computing", "Bob"),
        ]

    @pytest.mark.asyncio
    async def test_order_by_related_field_without_a_join(self, db):
        query = Post.objects.order_by("-author__name", "id")
        expected = [
            "ML Basics",
            "Unpublished",
            "Quantum Computing",
            "Draft Post",
            "Rust Patterns",
            "Async Python",
        ]

        posts = await query.all(using=db.name)
        assert [post.title for post in posts] == expected
        assert posts[0].author is not None and posts[0].author.name == "Charlie"

        titles = await query.values_list("title", flat=True).all(using=db.name)
        assert titles == expected

        rows = await query.values("title").all(using=db.name)
        assert [row["title"] for row in rows] == expected

        client = await get_connection(db.name)
        raw_rows = await query.fetch_rows(client)
        assert [row["title"] for row in raw_rows] == expected
        assert raw_rows[0]["author__name"] == "Charlie"


class TestAnnotateGroupBy:
    @pytest.mark.asyncio
    async def test_order_by_aggregate_alias_matching_mapped_field(self, aliased_db):
        midnight = await AliasedEvent.objects.get(title="Midnight", using=aliased_db.name)
        await AliasedEvent.objects.create(
            title="Another midnight", created=midnight.created, using=aliased_db.name
        )

        counts = await (
            AliasedEvent.objects.annotate(title=Count("id"))
            .group_by("created")
            .order_by("-title")
            .values_list("title", flat=True)
            .all(using=aliased_db.name)
        )
        assert counts == [2, 1]

    @pytest.mark.asyncio
    async def test_group_by_base_column_and_annotation_with_same_database_name(
        self, aliased_db
    ):
        midnight = await AliasedEvent.objects.get(title="Midnight", using=aliased_db.name)
        await AliasedEvent.objects.create(
            title="midnight", created=midnight.created, using=aliased_db.name
        )
        rows = await (
            AliasedEvent.objects.filter(title__in=["Midnight", "midnight"])
            .annotate(event_title=Lower("title"))
            .group_by("title", "event_title")
            .values("title", "event_title")
            .all(using=aliased_db.name)
        )

        assert sorted((row["title"], row["event_title"]) for row in rows) == [
            ("Midnight", "midnight"),
            ("midnight", "midnight"),
        ]

    @pytest.mark.asyncio
    async def test_group_by_related_field(self, db):
        rows = await (
            Post.objects.filter()
            .group_by("author__name")
            .annotate(n=Count("id"))
            .order_by("author__name")
            .values("author__name", "n")
            .all(using=db.name)
        )
        assert rows == [
            {"author__name": "Alice", "n": 2},
            {"author__name": "Bob", "n": 2},
            {"author__name": "Charlie", "n": 2},
        ]

    @pytest.mark.asyncio
    async def test_group_by_related_field_without_aggregates(self, db):
        rows = await (
            Post.objects.filter()
            .group_by("author__name")
            .order_by("author__name")
            .values("author__name")
            .all(using=db.name)
        )

        assert rows == [
            {"author__name": "Alice"},
            {"author__name": "Bob"},
            {"author__name": "Charlie"},
        ]

    @pytest.mark.asyncio
    async def test_grouped_union_orders_by_related_output_name(self, db):
        groups = (
            Post.objects.filter()
            .group_by("author__name")
            .values_list("author__name", flat=True)
        )
        rows = await (
            groups.filter(published=True)
            .union_all(groups.filter(published=False))
            .order_by("author__name")
            .all(using=db.name)
        )

        assert rows == ["Alice", "Bob", "Bob", "Charlie", "Charlie"]

    @pytest.mark.asyncio
    async def test_count_alias_matching_related_field_decodes_as_integer(self, db):
        query = Post.objects.annotate(author__name=Count("author__id")).values_list(
            "author__name", flat=True
        )

        assert await query.all(using=db.name) == [6]
        async with atomic(using=db.name):
            assert await query.all(using=db.name) == [6]

    @pytest.mark.asyncio
    async def test_sum_related_field(self, db):
        rows = await (
            Post.objects.annotate(total=Sum("author__id"))
            .group_by("author_id")
            .having(total__gt=2)
            .order_by("author_id")
            .values("author_id", "total")
            .all(using=db.name)
        )
        assert [(row["author_id"], Decimal(row["total"])) for row in rows] == [
            (2, Decimal(4)),
            (3, Decimal(6)),
        ]

    @pytest.mark.asyncio
    async def test_ungrouped_related_selection_raises(self, db):
        with pytest.raises(ValueError, match=r"author__name.*group_by\(\)"):
            await (
                Post.objects.join("author")
                .annotate(n=Count("id"))
                .values("author__name", "n")
                .all(using=db.name)
            )

    @pytest.mark.asyncio
    async def test_ungrouped_related_selection_without_a_join_raises(self, db):
        with pytest.raises(ValueError, match=r"category__name.*group_by\(\)"):
            await (
                Post.objects.annotate(n=Count("id"))
                .values("category__name", "n")
                .all(using=db.name)
            )

    @pytest.mark.asyncio
    @pytest.mark.parametrize(
        ("author", "expected"), [("Alice", [{"author_id": 1, "n": 2}]), ("Nobody", [])]
    )
    async def test_grouped_aggregate_with_related_filter(self, db, author, expected):
        rows = await (
            Post.objects.filter(author__name=author)
            .annotate(n=Count("id", distinct=True))
            .group_by("author_id")
            .having(n__gt=1)
            .order_by("-n")
            .values("author_id", "n")
            .all(using=db.name)
        )
        assert rows == expected

    @pytest.mark.asyncio
    async def test_count_group_by(self, db):
        """Count posts per author using values() + group_by."""
        rows = await (
            Post.objects.annotate(n=Count("id"))
            .group_by("author_id")
            .order_by("author_id")
            .values("author_id", "n")
            .all(using=db.name)
        )
        assert len(rows) == 3
        counts = [r["n"] for r in rows]
        assert counts == [2, 2, 2]  # Alice=2, Bob=2, Charlie=2

    @pytest.mark.asyncio
    async def test_group_by_without_values_raises(self, db):
        """group_by().all() without values() raises TypeError."""
        with pytest.raises(TypeError, match="group_by.*values"):
            await (
                Post.objects.annotate(n=Count("id"))
                .group_by("author_id")
                .all(using=db.name)
            )

    @pytest.mark.asyncio
    async def test_having(self, db):
        """Group by author_id, keep only those with sum views > 100."""
        rows = await (
            Post.objects.annotate(total=Sum("views"))
            .group_by("author_id")
            .having(total__gt=100)
            .values("author_id", "total")
            .all(using=db.name)
        )
        # Alice: 120+35=155, Bob: 80+0=80, Charlie: 200+0=200
        assert len(rows) == 2  # Alice and Charlie


async def _seed_typed_groups(db):
    # it always has); the decimals share their number of integer digits.
    for str_val, decimal_val, date_val in [
        ("a", Decimal("1.25"), date(2024, 1, 10)),
        ("a", Decimal("2.00"), date(2024, 2, 10)),
        ("b", Decimal("3.75"), date(2024, 3, 10)),
        ("b", Decimal("1.50"), date(2024, 1, 5)),
    ]:
        await AllTypes.objects.create(
            str_val=str_val, decimal_val=decimal_val, date_val=date_val, using=db.name
        )


class TestHavingAggregateTypes:
    """HAVING values bind by the aggregate's result type, so string-encoded
    values (Decimal, date) compare as that type."""

    @pytest.mark.asyncio
    @pytest.mark.parametrize("alias", ["top", "decimal_val"])
    async def test_max_decimal(self, db, alias):
        """The alias may also be the aggregated field's name."""
        await _seed_typed_groups(db)
        rows = await (
            AllTypes.objects.annotate(**{alias: Max("decimal_val")})
            .group_by("str_val")
            .having(**{f"{alias}__gt": Decimal("2.5")})
            .values("str_val")
            .all(using=db.name)
        )
        assert [r["str_val"] for r in rows] == ["b"]

    @pytest.mark.asyncio
    async def test_max_date(self, db):
        await _seed_typed_groups(db)
        rows = await (
            AllTypes.objects.annotate(last=Max("date_val"))
            .group_by("str_val")
            .having(last__gt=date(2024, 3, 1))
            .values("str_val")
            .all(using=db.name)
        )
        assert [r["str_val"] for r in rows] == ["b"]


LONG_REPORT_LABEL = (
    "customer_facing_reporting_label_that_exceeds_the_postgresql_identifier_limit"
)


class TestJoinedQueryReviewRegressions:
    async def _create_tenant_item(self, db):
        tenant = await Tenant.objects.create(
            name="Acme", duration=timedelta(seconds=90), using=db.name
        )
        item = await TenantItem.objects.create(
            tenant_uuid=tenant.uuid,
            exists=timedelta(seconds=15),
            customer_facing_reporting_label_that_exceeds_the_postgresql_identifier_limit="Acme West",
            using=db.name,
        )
        return tenant, item

    @pytest.mark.asyncio
    async def test_alternate_fk_column_is_used_for_grouping_and_hydration(self, db):
        tenant, item = await self._create_tenant_item(db)
        rows = await (
            TenantItem.objects.annotate(n=Count("*"))
            .group_by("tenant__name")
            .values("tenant__name", "n")
            .all(using=db.name)
        )
        assert rows == [{"tenant__name": "Acme", "n": 1}]

        joined = (
            await TenantItem.objects.join("tenant")
            .filter(tenant__name="Acme")
            .get(using=db.name)
        )
        assert joined.id == item.id
        assert joined.tenant is not None
        assert joined.tenant.uuid == tenant.uuid
        assert joined.tenant.id == tenant.id

    @pytest.mark.asyncio
    async def test_nested_joins_and_long_related_label(self, db):
        link = await ReviewLink.objects.create(
            post_id=1, post_author_id=2, using=db.name
        )
        rows = await (
            ReviewLink.objects.annotate(n=Count("*"))
            .group_by("post_author__name", "post__author__name")
            .values("post_author__name", "post__author__name", "n")
            .all(using=db.name)
        )
        assert rows == [
            {"post_author__name": "Bob", "post__author__name": "Alice", "n": 1}
        ]

        group = await ReviewGroup.objects.create(
            contributor_link_for_review_reports_id=link.id, using=db.name
        )
        await ReviewReport.objects.create(
            regional_reporting_group_for_review_id=group.id, using=db.name
        )
        path = (
            "regional_reporting_group_for_review"
            "__contributor_link_for_review_reports__post__author__name"
        )
        labels = await (
            ReviewReport.objects.filter()
            .group_by(path)
            .values_list(path, flat=True)
            .all(using=db.name)
        )
        assert labels == ["Alice"]

    @pytest.mark.asyncio
    async def test_long_base_field_label_round_trips(self, db):
        await self._create_tenant_item(db)
        labels = await (
            TenantItem.objects.filter()
            .group_by(LONG_REPORT_LABEL)
            .values_list(LONG_REPORT_LABEL, flat=True)
            .all(using=db.name)
        )
        assert labels == ["Acme West"]

    @pytest.mark.asyncio
    async def test_grouped_exists_uses_a_boolean_decode_type(self, db):
        await self._create_tenant_item(db)
        exists = await (
            TenantItem.objects.filter()
            .group_by("exists")
            .values("exists")
            .exists(using=db.name)
        )
        assert exists is True

    @pytest.mark.asyncio
    async def test_new_min_max_aliases_decode_as_timedeltas(self, db):
        await self._create_tenant_item(db)
        longest = await (
            TenantItem.objects.annotate(longest=Max("tenant__duration"))
            .values_list("longest", flat=True)
            .all(using=db.name)
        )
        assert longest == [timedelta(seconds=90)]

        for seconds in (30, 90):
            await TdModel.objects.create(
                duration=timedelta(seconds=seconds), using=db.name
            )
        assert await TdModel.objects.max("duration", using=db.name) == timedelta(
            seconds=90
        )
        assert await TdModel.objects.min("duration", using=db.name) == timedelta(
            seconds=30
        )

    @pytest.mark.asyncio
    async def test_dotted_base_column_with_join_filters_and_scalar_expression(self, db):
        supplier = await Supplier.objects.create(name="Acme", rating=11, using=db.name)
        for amount in (8, 7):
            await SuppliedItem.objects.create(
                supplier_id=supplier.id, amount=amount, using=db.name
            )
        query = SuppliedItem.objects.join("supplier")
        ordered = (
            await query.order_by("amount")
            .values_list("amount", flat=True)
            .all(using=db.name)
        )
        assert ordered == [7, 8]
        matched = await (
            query.filter(amount=7, supplier__rating=11)
            .annotate(x=F("amount"))
            .values("amount", "x")
            .all(using=db.name)
        )
        assert matched == [{"amount": 7, "x": 7}]

        grouped = await (
            SuppliedItem.objects.annotate(n=Count("*"))
            .group_by("supplier__rating")
            .having(supplier__rating=11)
            .values("supplier__rating", "n")
            .all(using=db.name)
        )
        assert grouped == [{"supplier__rating": 11, "n": 2}]

    @pytest.mark.asyncio
    async def test_distinct_values_deduplicate_across_joined_rows(self, db):
        for author_id in (1, 2):
            await Post.objects.create(
                title="Shared Title", author_id=author_id, using=db.name
            )
        query = Post.objects.join("author").filter(title="Shared Title").values("title")
        assert await query.distinct().all(using=db.name) == [{"title": "Shared Title"}]

    @pytest.mark.asyncio
    @pytest.mark.parametrize("fields", [("id", "title"), ("title",)])
    async def test_distinct_model_projection_loads_and_orders_joined_relations(
        self, db, fields
    ):
        posts = await (
            Post.objects.join("author")
            .filter(id__in=[1, 3])
            .select(*fields)
            .distinct()
            .order_by("-author__name")
            .all(using=db.name)
        )

        assert [post.id for post in posts] == [3, 1]
        assert [post.author.name for post in posts if post.author] == ["Bob", "Alice"]

    @pytest.mark.asyncio
    async def test_related_ordering_supports_row_locks(self, db):
        async with atomic(using=db.name):
            query = Post.objects.order_by("author__name").filter(id=1)
            for locked in (query.for_update(), query.for_share()):
                post = await locked.get(using=db.name)
                assert post.title == "Rust Patterns"
                assert post.author is not None
                assert post.author.name == "Alice"

    @pytest.mark.asyncio
    async def test_fetch_rows_only_returns_explicit_columns(self, db):
        client = await get_connection(db.name)
        query = Post.objects.order_by("author__name").filter(id=1).select("title")
        assert await query.fetch_rows(client) == [{"title": "Rust Patterns"}]
        post = await query.get(client=client)
        assert post.author is not None
        assert post.author.name == "Alice"

    @pytest.mark.asyncio
    async def test_union_related_order_hydrates_both_operands(self, db):
        first = Post.objects.filter(id=3)
        second = Post.objects.filter(id=1)
        rows = await first.union_all(second).order_by("author__name").all(using=db.name)
        assert [post.id for post in rows] == [1, 3]
        assert [post.author.name for post in rows if post.author] == ["Alice", "Bob"]

    @pytest.mark.asyncio
    @pytest.mark.parametrize(
        ("field", "expected_cached", "expected_related"),
        [
            ("supplier__name", ["Zebra", "Aardvark"], ["Aardvark", "Zebra"]),
            ("cached_supplier", ["Aardvark", "Zebra"], ["Zebra", "Aardvark"]),
        ],
    )
    async def test_union_orders_columns_with_colliding_labels(
        self, db, field, expected_cached, expected_related
    ):
        for name, cached_name in [("Zebra", "Aardvark"), ("Aardvark", "Zebra")]:
            supplier = await Supplier.objects.create(name=name, using=db.name)
            await SuppliedItem.objects.create(
                supplier_id=supplier.id, cached_supplier=cached_name, using=db.name
            )
        first = SuppliedItem.objects.join("supplier").filter(cached_supplier="Aardvark")
        second = SuppliedItem.objects.filter(cached_supplier="Zebra")
        items = await first.union(second).order_by(field).all(using=db.name)

        assert [item.cached_supplier for item in items] == expected_cached
        assert [item.supplier.name for item in items if item.supplier] == expected_related

    @pytest.mark.asyncio
    async def test_long_join_labels_hydrate_nested_fields(self, db):
        link = await ReviewLink.objects.create(post_id=1, post_author_id=2, using=db.name)
        group = await ReviewGroup.objects.create(
            contributor_link_for_review_reports_id=link.id, using=db.name
        )
        report = await ReviewReport.objects.create(
            regional_reporting_group_for_review_id=group.id, using=db.name
        )
        rows = await (
            ReviewReport.objects.filter(id=report.id)
            .join(
                "regional_reporting_group_for_review"
                "__contributor_link_for_review_reports__post__author"
            )
            .all(using=db.name)
        )
        (loaded,) = rows
        loaded_group = loaded.regional_reporting_group_for_review
        assert loaded_group is not None
        assert loaded_group.id == group.id
        loaded_link = loaded_group.contributor_link_for_review_reports
        assert loaded_link is not None
        assert loaded_link.id == link.id
        loaded_post = loaded_link.post
        assert loaded_post is not None
        assert loaded_post.id == 1
        loaded_author = loaded_post.author
        assert loaded_author is not None
        assert loaded_author.id == 1
        assert loaded_author.name == "Alice"
