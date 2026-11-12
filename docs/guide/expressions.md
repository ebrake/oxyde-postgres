# Expressions

Expressions compute values in PostgreSQL for annotations and atomic updates.
`F` references a column and supports arithmetic; `Coalesce`, `Greatest`,
`Concat`, `Value`, and `RawSQL` compose with it. Scalar annotations require
`oxyde-postgres-core>=0.3.1,<0.4.0`.

## Why F Expressions?

Without F expressions, you risk race conditions:

```python
# BAD: Race condition
post = await Post.objects.get(id=1)
post.views = post.views + 1  # Read old value
await post.save()            # Another request might have incremented too
```

With F expressions, the operation is atomic:

```python
# GOOD: Atomic
from oxyde import F
await Post.objects.filter(id=1).update(views=F("views") + 1)
```

The SQL generated is `UPDATE posts SET views = views + 1 WHERE id = 1`.

## Basic Usage

```python
from oxyde import F

# Reference a column
F("views")

# Arithmetic
F("views") + 1
F("price") * 0.9
F("balance") - 100
F("total") / F("count")
```

## Supported Operations

| Operation | Example | SQL |
|-----------|---------|-----|
| Addition | `F("x") + 1` | `x + 1` |
| Subtraction | `F("x") - 1` | `x - 1` |
| Multiplication | `F("x") * 2` | `x * 2` |
| Division | `F("x") / 2` | `x / 2` |
| Negation | `-F("x")` | `-x` |

## Column + Column

Combine multiple columns:

```python
# total = price * quantity
await Order.objects.filter(id=1).update(total=F("price") * F("quantity"))

# score = base_score + bonus
await Player.objects.filter(id=1).update(score=F("base_score") + F("bonus"))
```

## Column Names

Names inside an expression are field names. `update()`, `increment()` and
`annotate()` map them to the field's `db_column`; a name that is not a model
field passes through unchanged, so `F("db_col")` keeps working. Related-field
names (`F("author__id")`) are rejected.

## Coalesce, Greatest and Value

`Coalesce(...)`, `Greatest(...)` and `Value(x)` are expressions too. `str`
arguments are columns, other objects are literals, and `Value("text")` forces a
string literal. Enum members are literals, including string-based enums:

```python
from oxyde import Coalesce, F, Greatest, Value

await Item.objects.query().update(qty=Greatest(Coalesce("qty", F("default_qty")), 1))
```

They also work in `annotate()`; see [Aggregation](aggregation.md).

## Use with annotate()

Compute a value for each row and select it with `.values()`:

```python
from oxyde import Coalesce, F, Greatest

rows = await Product.objects.annotate(
    available=Greatest(Coalesce("stock", 0) - F("reserved"), 0),
).values("id", "available").order_by("-available").all()
```

Use `.values(...).all()`, `.values_list(...).all()`, or `fetch_all()` for
scalar results. Model-returning `.all()`, `.first()`, and `.get()` raise
`TypeError`. A query cannot mix scalar annotations and aggregates, and
`having()` accepts aggregate aliases only. See
[Aggregation](aggregation.md#scalar-expressions) for alias and decoding rules.

## Strings and Raw SQL

`Concat` uses `CONCAT_WS`, skipping NULL arguments:

```python
from oxyde import Concat, Value

await User.objects.query().update(
    display_name=Concat("first_name", "last_name", Value("(member)"), separator=" ")
)
```

`RawSQL("LENGTH(name)")` is a scalar expression usable in annotations and
updates. It uses database column names verbatim, does not map `db_column`, and
takes no parameters. In joined queries, qualify ambiguous columns yourself.
Use bound literals for data; never interpolate user input into the fragment.

## Typed Literals and NULLs

The core types sibling literals in `Coalesce` and `Greatest` from known column
types, including nested calls. Enum literals are cast to the column's native
enum type. A bare `Value(...)` assigned in `create()` or `update()` uses the
target column type, and enum values in `update()` are validated like raw values.

An array column can supply the type of an empty list fallback:

```python
await Article.objects.query().update(tags=Coalesce("tags", Value([])))
```

Incompatible array elements raise an error instead of silently becoming NULL
elements. NULL literals use a known sibling or target type when available;
otherwise the compiler emits SQL `NULL` and lets PostgreSQL resolve the type.
PostgreSQL still requires compatible argument types for each function.

A custom `db_type` with no recognized column spec (such as MONEY, CITEXT, or
a domain) supplies no sibling expression type. Python Decimal and UUID hints
guide parameter binding on those fields without changing result decoding.
Known explicit column specs take precedence. String values remain strings,
including text that looks like a timestamp.

## Use with update()

The most common use case:

```python
# Increment
await Post.objects.filter(id=1).update(views=F("views") + 1)

# Decrement
await Product.objects.filter(id=1).update(stock=F("stock") - 1)

# Percentage increase
await Product.objects.filter(category="sale").update(
    price=F("price") * 1.1  # 10% increase
)

# Percentage discount
await Product.objects.filter(category="clearance").update(
    price=F("price") * 0.5  # 50% off
)
```

## Use with increment()

Shortcut for common increment pattern:

```python
# These are equivalent:
await Post.objects.filter(id=1).update(views=F("views") + 1)
await Post.objects.filter(id=1).increment("views", by=1)

# Decrement
await Post.objects.filter(id=1).increment("views", by=-1)
```

## Bulk Updates

F expressions work with bulk updates:

Use `Model.objects.query().update(...)` to update every row, or start with
`filter(...)` to limit the rows. `update()` belongs to the query builder.

```python
# Give everyone a 5% raise
await Employee.objects.query().update(salary=F("salary") * 1.05)

# Reset all counters
await Stats.objects.query().update(count=F("count") * 0)  # Set to 0
```

## Complex Expressions

Chain operations:

```python
# (price * quantity) - discount
await Order.objects.filter(id=1).update(
    total=F("price") * F("quantity") - F("discount")
)

# Nested: ((base + bonus) * multiplier)
await Score.objects.filter(id=1).update(
    final=((F("base") + F("bonus")) * F("multiplier"))
)
```

## With Value Constants

Mix F expressions with constants:

```python
# Add flat fee
await Order.objects.filter(id=1).update(total=F("subtotal") + 9.99)

# Apply tax rate
await Order.objects.filter(id=1).update(total=F("subtotal") * 1.08)
```

## Reverse Operations

F expressions support reverse operations:

```python
# 100 - balance (instead of balance - 100)
await Account.objects.filter(id=1).update(remaining=100 - F("spent"))

# 2 * multiplier
await Score.objects.filter(id=1).update(doubled=2 * F("base"))
```

## Examples

### Page View Counter

```python
async def view_post(post_id: int):
    await Post.objects.filter(id=post_id).increment("views")
```

### Inventory Management

```python
async def purchase(product_id: int, quantity: int):
    await Product.objects.filter(id=product_id).update(
        stock=F("stock") - quantity
    )

async def restock(product_id: int, quantity: int):
    await Product.objects.filter(id=product_id).update(
        stock=F("stock") + quantity
    )
```

### Balance Transfer

```python
from oxyde.db import transaction

async def transfer(from_id: int, to_id: int, amount: float):
    async with transaction.atomic():
        await Account.objects.filter(id=from_id).update(
            balance=F("balance") - amount
        )
        await Account.objects.filter(id=to_id).update(
            balance=F("balance") + amount
        )
```

### Pricing Rules

```python
# Apply 20% discount to all sale items
await Product.objects.filter(on_sale=True).update(
    price=F("price") * 0.8
)

```

### Score Calculation

```python
# Calculate weighted score
await Score.objects.query().update(
    total=F("correct") * 10 - F("wrong") * 5
)
```

## Limitations

### No Filtering with F

F expressions work in update values and scalar annotations. They are not
accepted as filter operands:

```python
# This won't work
# await User.objects.filter(balance__gt=F("credit_limit")).all()

# Use raw SQL or application logic instead
```

### No String Operations

F expressions only support numeric operations:

```python
# These won't work:
# F("first_name") + " " + F("last_name")  # String concat
# F("name").lower()  # String methods

# Use Concat for string concatenation or RawSQL for database functions
```

### Scalar Results Need a Projection

Use `.values()` or `.values_list()` before executing a scalar-annotated query,
or use `fetch_all()` directly. Extra computed columns cannot hydrate model fields.

## Next Steps

- [Aggregation](aggregation.md) — Aggregate functions and GROUP BY
- [Queries](queries.md) — Full query reference
- [Transactions](transactions.md) — Atomic operations with transactions
