"""Scalar expression annotations: Value / Coalesce / Greatest and annotate()."""

from __future__ import annotations

from decimal import Decimal
from enum import Enum

import pytest

import oxyde
import oxyde.queries
import oxyde.queries.aggregates as aggregates_mod
from oxyde import (
    Coalesce,
    Concat,
    Count,
    F,
    Field,
    Greatest,
    Model,
    RawSQL,
    Sum,
    Value,
)
from oxyde.queries.expressions import (
    _map_expression_columns,
    _serialize_value_for_ir,
)
from oxyde.queries.insert import InsertQuery


class ScalarItem(Model):
    id: int | None = Field(default=None, db_pk=True)
    name: str = ""
    minimum_quantity: int | None = Field(default=None, db_nullable=True)
    stock: int = Field(default=0, db_column="stock_qty")

    class Meta:
        is_table = True


class Color(str, Enum):
    RED = "red"


def eff_min():
    return Greatest(Coalesce("minimum_quantity", 1), 1)


class TestExpressionNodes:
    def test_import_paths(self):
        assert oxyde.Coalesce is oxyde.queries.Coalesce is aggregates_mod.Coalesce
        assert oxyde.Greatest is oxyde.queries.Greatest
        assert oxyde.Value is oxyde.queries.Value

    def test_value_str_is_a_literal(self):
        node = Value("draft")._expr
        assert node["type"] == "value"
        assert node["value"] == "draft"

    def test_value_carries_type_spec(self):
        node = Value(Decimal("2.50"))._expr
        assert node["value_type"]["kind"] == "decimal"

    def test_value_rejects_expressions(self):
        with pytest.raises(TypeError):
            Value(F("a"))

    def test_function_arg_coercion(self):
        expr = Coalesce("a", F("b"), 0, Value("x"))._expr
        assert [a["type"] for a in expr["args"]] == [
            "column",
            "column",
            "value",
            "value",
        ]
        assert expr["args"][3]["value"] == "x"

    def test_function_requires_args(self):
        with pytest.raises(ValueError):
            Coalesce()

    def test_function_composes_with_arithmetic(self):
        expr = Greatest("a", 1) + 2
        assert expr._expr["type"] == "op"
        assert expr._expr["lhs"]["name"] == "greatest"


class TestColumnMapping:
    def test_serialize_maps_db_column(self):
        out = _serialize_value_for_ir(F("stock") + 1, ScalarItem)
        assert out["__expr__"]["lhs"] == {"type": "column", "name": "stock_qty"}

    def test_unknown_names_pass_through(self):
        out = _serialize_value_for_ir(F("stock_qty") + 1, ScalarItem)
        assert out["__expr__"]["lhs"]["name"] == "stock_qty"

    def test_related_names_rejected(self):
        with pytest.raises(ValueError, match="Related-field"):
            _serialize_value_for_ir(F("author__id") + 1, ScalarItem)

    def test_without_model_names_untouched(self):
        out = _serialize_value_for_ir(F("stock") + 1)
        assert out["__expr__"]["lhs"]["name"] == "stock"

    def test_nested_in_func_mapped(self):
        out = _serialize_value_for_ir(Coalesce("stock", 0), ScalarItem)
        assert out["__expr__"]["args"][0]["name"] == "stock_qty"


class TestExpressionEdgeCases:
    def test_str_enum_member_is_a_literal_not_a_column(self):
        node = Coalesce("name", Color.RED)._expr
        assert node["args"][0] == {"type": "column", "name": "name"}
        assert node["args"][1]["type"] == "value"
        assert node["args"][1]["value"] is Color.RED

    def test_column_mapping_skips_literal_payloads(self):
        lookalike = {"type": "column", "name": "stock"}
        node = Coalesce("stock", Value(lookalike))._expr
        mapped = _map_expression_columns(node, lambda name: f"db_{name}")
        assert mapped["args"][0]["name"] == "db_stock"
        assert mapped["args"][1]["value"] == lookalike

    def test_insert_query_maps_db_column_in_expressions(self):
        ir = InsertQuery(ScalarItem).values(name=Coalesce("stock", 0)).to_ir()
        assert ir["values"]["name"]["__expr__"]["args"][0]["name"] == "stock_qty"
        bulk = InsertQuery(ScalarItem).bulk_values([{"name": F("stock") + 1}]).to_ir()
        assert bulk["bulk_values"][0]["name"]["__expr__"]["lhs"]["name"] == "stock_qty"


class TestToIR:
    def test_values_with_alias(self):
        ir = (
            ScalarItem.objects.annotate(eff_min=eff_min())
            .values("id", "eff_min")
            .to_ir()
        )
        assert ir["cols"] == ["id"]
        assert [a["alias"] for a in ir["annotations"]] == ["eff_min"]
        assert "aggregates" not in ir
        assert ir["annotations"][0]["expr"]["name"] == "greatest"

    def test_alias_only_sends_empty_cols(self):
        ir = ScalarItem.objects.annotate(eff_min=eff_min()).values("eff_min").to_ir()
        assert ir["cols"] == []
        assert len(ir["annotations"]) == 1

    def test_values_without_args_returns_all_annotations(self):
        ir = ScalarItem.objects.annotate(eff_min=eff_min()).values().to_ir()
        assert "id" in ir["cols"]
        assert "eff_min" not in ir["cols"]
        assert len(ir["annotations"]) == 1

    def test_column_names_mapped(self):
        ir = ScalarItem.objects.annotate(s=Coalesce("stock", 0)).values("s").to_ir()
        assert ir["annotations"][0]["expr"]["args"][0]["name"] == "stock_qty"

    def test_function_alias_adds_no_python_side_column_type(self):
        # Result types for enum-valued annotations are inferred in Rust;
        # Python only forwards the model's own column types.
        ir = (
            ScalarItem.objects.annotate(
                m=Coalesce("minimum_quantity", 0), a=F("stock") + 1
            )
            .values("m", "a")
            .to_ir()
        )
        assert "m" not in ir["column_types"]
        assert "a" not in ir["column_types"]

    def test_concat_and_rawsql_annotations(self):
        ir = (
            ScalarItem.objects.annotate(
                c=Concat("name", "stock", separator="-"), r=RawSQL("LENGTH(name)")
            )
            .values("c", "r")
            .to_ir()
        )
        by_alias = {a["alias"]: a["expr"] for a in ir["annotations"]}
        assert by_alias["c"]["args"][2]["name"] == "stock_qty"
        assert by_alias["r"] == {"type": "raw", "sql": "LENGTH(name)"}
        # concat_ws yields text: no column-type inheritance
        assert "c" not in ir["column_types"]

    def test_f_arithmetic_annotation(self):
        ir = ScalarItem.objects.annotate(x=F("stock") + 1).values("x").to_ir()
        assert ir["annotations"][0]["expr"]["type"] == "op"

    def test_order_by_alias_not_remapped(self):
        ir = (
            ScalarItem.objects.annotate(eff_min=eff_min())
            .values("id", "eff_min")
            .order_by("-eff_min", "stock")
            .to_ir()
        )
        assert ir["order_by"] == [("eff_min", "DESC"), ("stock_qty", "ASC")]

    def test_order_by_unselected_alias_rejected(self):
        q = (
            ScalarItem.objects.annotate(eff_min=eff_min())
            .values("id")
            .order_by("eff_min")
        )
        with pytest.raises(ValueError, match="not selected"):
            q.to_ir()

    def test_aggregate_annotations_still_route_to_aggregates(self):
        ir = ScalarItem.objects.annotate(n=Count("id")).to_ir()
        assert ir["aggregates"][0]["alias"] == "n"
        assert "annotations" not in ir


class TestGuards:
    def test_mixing_with_aggregates_rejected(self):
        with pytest.raises(ValueError, match="mix"):
            ScalarItem.objects.annotate(n=Count("id")).annotate(e=eff_min())
        with pytest.raises(ValueError, match="mix"):
            ScalarItem.objects.annotate(e=eff_min()).annotate(n=Count("id"))

    def test_alias_conflicting_with_field_or_column(self):
        with pytest.raises(ValueError, match="conflicts"):
            ScalarItem.objects.annotate(name=Coalesce("name", "x"))
        with pytest.raises(ValueError, match="conflicts"):
            ScalarItem.objects.annotate(stock_qty=Coalesce("stock", 0))

    def test_having_rejects_scalar_alias(self):
        q = ScalarItem.objects.annotate(e=eff_min())
        with pytest.raises(ValueError, match="aggregate"):
            q.having(e__gt=1)

    @pytest.mark.asyncio
    async def test_models_raise_typeerror(self):
        q = ScalarItem.objects.annotate(e=eff_min())
        with pytest.raises(TypeError, match="values"):
            await q.fetch_models(client=None)  # type: ignore[arg-type]

    def test_terminal_aggregate_drops_scalar_annotations(self):
        q = ScalarItem.objects.annotate(e=eff_min())
        agg = q._clone()
        agg._annotations = {a: o for a, o in agg._annotations.items() if a != "e"}
        # the same transformation _aggregate applies must permit adding Sum
        assert agg.annotate(_sum=Sum("stock")).to_ir()["aggregates"]


class TestAggregateFieldMapping:
    def test_aggregate_field_mapped_to_db_column(self):
        ir = ScalarItem.objects.annotate(t=Sum("stock"), n=Count("*")).to_ir()
        fields = {a["alias"]: a["field"] for a in ir["aggregates"]}
        assert fields == {"t": "stock_qty", "n": "*"}

    def test_db_column_name_passes_through(self):
        ir = ScalarItem.objects.annotate(t=Sum("stock_qty")).to_ir()
        assert ir["aggregates"][0]["field"] == "stock_qty"


class TextBackedStatus(str, Enum):
    DRAFT = "draft"
    LIVE = "live"


class TextEnumItem(Model):
    id: int | None = Field(default=None, db_pk=True)
    status: TextBackedStatus = Field(default=TextBackedStatus.DRAFT, db_type="TEXT")

    class Meta:
        is_table = True


class TestUpdateValueEnumValidation:
    def test_value_literal_is_validated_like_a_raw_value(self):
        from pydantic import ValidationError

        from oxyde.queries.mixins.mutation import _validate_enum_update_values

        with pytest.raises(ValidationError):
            _validate_enum_update_values(TextEnumItem, {"status": "bogus"})
        with pytest.raises(ValidationError):
            _validate_enum_update_values(TextEnumItem, {"status": Value("bogus")})

    def test_valid_value_literal_passes_and_stays_a_value(self):
        from oxyde.queries.mixins.mutation import _validate_enum_update_values

        out = _validate_enum_update_values(
            TextEnumItem, {"status": Value("live")}
        )
        assert isinstance(out["status"], Value)
        assert out["status"]._expr["value"] == TextBackedStatus.LIVE

    def test_other_expressions_are_not_validated(self):
        from oxyde.queries.mixins.mutation import _validate_enum_update_values

        expr = Coalesce("status", Value("anything"))
        assert _validate_enum_update_values(TextEnumItem, {"status": expr})[
            "status"
        ] is expr
