# Aggregation

Oxyde supports SQL aggregate functions and GROUP BY operations.

## Aggregate Functions

### Direct Aggregates

Execute aggregate queries directly:

```python
# COUNT
count = await User.objects.count()
active_count = await User.objects.filter(status="active").count()

# SUM
total = await Order.objects.sum("amount")

# AVG
average = await User.objects.avg("age")

# MAX / MIN
highest = await Product.objects.max("price")
lowest = await Product.objects.min("price")
```

### Aggregate Classes

For more control, use aggregate classes:

```python
from oxyde import Count, Sum, Avg, Max, Min

# With distinct
unique_count = await User.objects.annotate(
    unique_cities=Count("city", distinct=True)
).all()
```

## Available Aggregates

| Function | Description | Example |
|----------|-------------|---------|
| `Count` | Count rows | `Count("*")`, `Count("id", distinct=True)` |
| `Sum` | Sum values | `Sum("amount")`, `Sum("amount", distinct=True)` |
| `Avg` | Average | `Avg("price")`, `Avg("price", distinct=True)` |
| `Max` | Maximum | `Max("created_at")` |
| `Min` | Minimum | `Min("price")` |

## annotate()

Add computed columns to query results:

```python
from oxyde import Count

# Single annotation
results = await User.objects.annotate(
    post_count=Count("id")
).all()

# Multiple annotations
results = await Order.objects.annotate(
    total=Sum("amount"),
    avg_item=Avg("amount"),
).all()
```

## GROUP BY

Group results by one or more columns:

```python
# Posts per author
results = await Post.objects.values("author_id").annotate(
    count=Count("*")
).group_by("author_id").all()

# Result: [{"author_id": 1, "count": 5}, {"author_id": 2, "count": 3}]
```

### Multiple Group Columns

```python
# Sales by year and month
results = await Order.objects.values("year", "month").annotate(
    total=Sum("amount"),
    orders=Count("*")
).group_by("year", "month").all()
```

### With Filtering

```python
# Active users per city
results = await User.objects.filter(
    status="active"
).values("city").annotate(
    count=Count("*")
).group_by("city").all()
```

## HAVING

Filter on aggregate results:

```python
# Authors with more than 5 posts
results = await Post.objects.values("author_id").annotate(
    count=Count("*")
).group_by("author_id").having(count__gt=5).all()
```

!!! note "HAVING vs WHERE"
    - `filter()` → WHERE (filters rows before grouping)
    - `having()` → HAVING (filters groups after aggregation)

### Complex HAVING

```python
# High-value customers (total orders > $1000)
results = await Order.objects.values("customer_id").annotate(
    total=Sum("amount")
).group_by("customer_id").having(total__gte=1000).all()
```

## Ordering Aggregates

Order by aggregate values:

```python
# Top authors by post count
results = await Post.objects.values("author_id").annotate(
    count=Count("*")
).group_by("author_id").order_by("-count").limit(10).all()
```

## Scalar Expressions

Scalar expressions compute one value per row. They are annotated with
`annotate()` and read back with `.values()`, `.values_list()` or `fetch_all()`.
Model-returning `.all()`, `.first()` and `.get()` raise `TypeError`, because the
extra column has no model field to hydrate. A query cannot mix scalar
annotations with aggregates, and `having()` only accepts aggregate aliases.

Column names refer to the model's own fields (mapped to `db_column`);
related-field names such as `author__id` are not supported. An alias may not
equal a model field or column name, or start with a joined relation's
`<prefix>__` namespace. Aggregate arguments also map field names to `db_column`;
this applies to both `annotate(total=Sum("stock"))` and `sum("stock")`.

```python
from oxyde import F

query = Product.objects.annotate(double_stock=F("stock") * 2)
rows = await query.values("id", "double_stock").all()
numbers = await query.values_list("double_stock", flat=True).all()
raw_rows = await query.fetch_all()
```

On joined queries, `.values()` and `fetch_all()` without a field list also
include joined keys such as `author__name`. Scalar annotations require core
`>=0.3.1,<0.4.0`; an older core can silently discard the annotation IR.

### Coalesce

Return the first non-NULL value. `str` arguments are columns; use `Value` for a
string literal:

```python
from oxyde import Coalesce, Value

rows = await User.objects.annotate(
    display_name=Coalesce("nickname", "username", Value("anonymous"))
).values("id", "display_name").all()
```

### Greatest

```python
from oxyde import Coalesce, Greatest

rows = await Product.objects.annotate(
    eff_min=Greatest(Coalesce("minimum_quantity", 1), 1)
).values("id", "eff_min").order_by("-eff_min").all()
```

`Coalesce` and `Greatest` also work in `update()` values, and compose with `F`
arithmetic: `update(qty=Coalesce("qty", F("default_qty"), 0))`.

### Value

`Value(x)` is an explicit literal. A `str` inside `Value` is never read as a
column name.

### Result Types

Annotations that pass through a known column type preserve its decoding:
`F("tags")` and `Coalesce("tags", None)` return the array as a list, and an
interval field returns seconds as it does when selected directly. Enum aliases
are decoded as enum values, including when another argument is `RawSQL` or
arithmetic; PostgreSQL may reject incompatible argument types.

Expressions that can change the result type, such as `F` arithmetic,
`Greatest("integer_field", 1.5)`, `Concat`, and standalone `RawSQL`, use native
result decoding. Binding-only hints for custom SQL types do not change the
decoded result. See [Typed Literals and NULLs](expressions.md#typed-literals-and-nulls)
for enum, array, and NULL binding rules.

`Coalesce`, `Concat`, and `RawSQL` live in `oxyde.queries.expressions`.
Imports from `oxyde`, `oxyde.queries`, and `oxyde.queries.aggregates` remain
compatible. Their old aggregate interfaces (`to_ir()`, `.fields`, and
`.func_name`) are no longer supported.

### Concat

Join strings with an optional separator. Compiles to `CONCAT_WS(separator, ...)`,
so NULL arguments are skipped rather than turning the whole result NULL. `str`
arguments are columns; use `Value` for literals:

```python
from oxyde import Concat, Value

rows = await User.objects.annotate(
    full_name=Concat("first_name", "last_name", separator=" ")
).values("id", "full_name").all()
```

## RawSQL

For functions Oxyde doesn't wrap, embed a raw SQL fragment. It is a scalar
expression like the ones above, so it is read back with `.values()`:

```python
from oxyde import RawSQL

rows = await User.objects.annotate(
    name_length=RawSQL("LENGTH(name)")
).values("id", "name_length").all()
```

The fragment is sent verbatim, so column names are database column names (not
mapped from field names), and it takes no parameters.

!!! warning "SQL Injection"
    Be careful with user input in RawSQL. Never interpolate user data directly.

!!! note
    `RawSQL` is sent verbatim and never qualified. In a query with `join()`, qualify
    columns yourself (`RawSQL('LENGTH("posts"."name")')`); an unqualified name that
    exists in several tables is ambiguous.

## Examples

### Leaderboard

```python
# Top 10 players by score
leaderboard = await Score.objects.values("user_id").annotate(
    total=Sum("points")
).group_by("user_id").order_by("-total").limit(10).all()
```

### Sales Report

```python
# Daily sales totals
from datetime import date

report = await Order.objects.filter(
    created_at__gte=date(2024, 1, 1)
).values("date").annotate(
    orders=Count("*"),
    revenue=Sum("total"),
    avg_order=Avg("total")
).group_by("date").order_by("date").all()
```

### Category Statistics

```python
# Products per category with price stats
stats = await Product.objects.values("category_id").annotate(
    count=Count("*"),
    avg_price=Avg("price"),
    min_price=Min("price"),
    max_price=Max("price")
).group_by("category_id").all()
```

### User Activity

```python
# Most active users (by login count)
active_users = await LoginLog.objects.values("user_id").annotate(
    logins=Count("*")
).group_by("user_id").having(logins__gte=10).order_by("-logins").all()
```

### Time-based Grouping

```python
# Orders by year
yearly = await Order.objects.filter(
    created_at__year=2024
).values("created_at__year", "created_at__month").annotate(
    count=Count("*"),
    total=Sum("amount")
).group_by("created_at__year", "created_at__month").all()
```

## Complete Example

```python
import asyncio
from oxyde import Model, Field, db, Count, Sum, Avg

class Order(Model):
    class Meta:
        is_table = True

    id: int | None = Field(default=None, db_pk=True)
    customer_id: int
    amount: float
    status: str

async def main():
    async with db.connect("postgresql://localhost/app"):
        # Total orders and revenue
        total_orders = await Order.objects.count()
        total_revenue = await Order.objects.sum("amount")
        print(f"Orders: {total_orders}, Revenue: ${total_revenue}")

        # Revenue by status
        by_status = await Order.objects.values("status").annotate(
            count=Count("*"),
            total=Sum("amount")
        ).group_by("status").all()

        for row in by_status:
            print(f"{row['status']}: {row['count']} orders, ${row['total']}")

        # Top customers
        top = await Order.objects.values("customer_id").annotate(
            orders=Count("*"),
            spent=Sum("amount")
        ).group_by("customer_id").order_by("-spent").limit(5).all()

        print("\nTop customers:")
        for row in top:
            print(f"Customer {row['customer_id']}: ${row['spent']}")

asyncio.run(main())
```

## Next Steps

- [Relations](relations.md) — Foreign keys and joins
- [Transactions](transactions.md) — Atomic operations
- [Queries](queries.md) — Full query reference
