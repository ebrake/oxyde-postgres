# Models

Models are the foundation of Oxyde. Each model class represents a database table, with class attributes defining columns.

## Basic Model Definition

```python
from oxyde import Model, Field

class User(Model):
    id: int | None = Field(default=None, db_pk=True)
    name: str
    email: str = Field(db_unique=True)
    age: int | None = Field(default=None)

    class Meta:
        is_table = True
```

## The Meta Class

The inner `Meta` class configures table-level settings:

```python
class User(Model):
    class Meta:
        is_table = True              # Required: marks this as a database table
        table_name = "users"         # Optional: custom table name (default: class name)
        schema = "public"            # Optional: database schema
```

### Required Settings

| Setting | Type | Description |
|---------|------|-------------|
| `is_table` | `bool` | Must be `True` for database tables |

### Optional Settings

| Setting | Type | Default | Description |
|---------|------|---------|-------------|
| `table_name` | `str` | Class name | Database table name |
| `schema` | `str` | None | Database schema |
| `indexes` | `list[Index]` | `[]` | Composite indexes |
| `constraints` | `list[Check]` | `[]` | CHECK constraints |
| `unique_together` | `list[tuple]` | `[]` | Composite unique constraints |

## Type Annotations

Oxyde uses Python type hints to infer SQL types:

```python
class Example(Model):
    # Required field
    name: str

    # Optional field (nullable)
    bio: str | None = Field(default=None)

    # With default value
    status: str = Field(default="active")

    class Meta:
        is_table = True
```

### Type Mapping

| Python Type | PostgreSQL |
|-------------|------------|
| `int` | BIGINT |
| `int` (PK) | BIGSERIAL |
| `str` | VARCHAR(255) |
| `float` | DOUBLE PRECISION |
| `bool` | BOOLEAN |
| `datetime` | TIMESTAMP |
| `date` | DATE |
| `time` | TIME |
| `UUID` | UUID |
| `Decimal` | NUMERIC |
| `bytes` | BYTEA |
| `dict` | JSONB |
| `list[T]` | T[] |
| `Enum` (str values) | native enum type |

!!! tip "Custom string length"
    `str` fields default to `VARCHAR(255)`. Use `max_length` to change: `Field(max_length=500)` → `VARCHAR(500)`.

!!! tip "Decimal precision"
    Use `max_digits` and `decimal_places` for precise numeric columns: `Field(max_digits=10, decimal_places=2)` → `NUMERIC(10,2)` / `DECIMAL(10,2)`.

!!! tip "Enums"
    A string-valued `Enum` annotation maps to a native enum type on PostgreSQL. See [Fields — Enum Fields](fields.md#enum-fields).

## Primary Keys

### Auto-increment Primary Key

```python
class User(Model):
    id: int | None = Field(default=None, db_pk=True)

    class Meta:
        is_table = True
```

The `id` will be auto-generated on insert.

### UUID Primary Key

```python
from uuid import UUID, uuid4

class User(Model):
    id: UUID = Field(default_factory=uuid4, db_pk=True)

    class Meta:
        is_table = True
```

## Indexes

### Single-Column Index

```python
class User(Model):
    email: str = Field(db_index=True)

    class Meta:
        is_table = True
```

### Composite Index

```python
from oxyde import Index

class Event(Model):
    city: str
    start_date: datetime

    class Meta:
        is_table = True
        indexes = [
            Index(("city", "start_date")),
        ]
```

### Partial Index

Partial indexes are supported by PostgreSQL migrations.

```python
class User(Model):
    email: str
    deleted_at: datetime | None = Field(default=None)

    class Meta:
        is_table = True
        indexes = [
            Index(("email",), unique=True, where="deleted_at IS NULL"),
        ]
```

### Unique Indexes and NULLs

By default, unique indexes allow multiple NULL values. Set `nulls_distinct=False`
to treat NULLs as equal. A single-column index then accepts at most one NULL;
a composite index rejects repeated combinations, including NULLs.

```python
Index(("email",), unique=True, nulls_distinct=False)
Index(
    ("tenant_id", "email"),
    unique=True,
    nulls_distinct=False,
    where="deleted_at IS NULL",
)
```

`nulls_distinct=False` requires `unique=True`. `None` (the default) or `True`
keeps PostgreSQL's default behavior. Changing this option generates a migration
that recreates the index.

### Expression Indexes

A field of `Index` can be an expression instead of a column name, for example
for case-insensitive lookups. An index with an expression needs an explicit
`name`. PostgreSQL truncates identifiers to 63 bytes; index names that become
equal after truncation, or that repeat across tables, are rejected by
`makemigrations`. So are names PostgreSQL has already used: a table name, and
the relations `CREATE TABLE` creates itself (`<table>_pkey` for a primary key,
`<table>_<column>_key` for a `unique` column, `<table>_<column>_seq` for a
serial column such as an integer primary key).

```python
from oxyde import Coalesce, Index, Lower

class User(Model):
    tenant_id: int
    email: str
    qty: int = 0

    class Meta:
        is_table = True
        indexes = [
            Index((Lower("email"),), unique=True, name="ux_user_email_lower"),
            Index(("tenant_id", Lower("email")), name="ix_user_tenant_email"),
            Index((Coalesce("qty", 1),), name="ix_user_qty_or_one"),
        ]
```

Things to know:

- `Lower`, `Upper`, `Coalesce`, `Greatest`, arithmetic on `F(...)`, `Value(...)`
  and `RawSQL(...)` are accepted. `Concat` is rejected: PostgreSQL marks
  `CONCAT_WS` as STABLE, and index expressions must be IMMUTABLE. The core
  allows only `coalesce`, `greatest`, `lower` and `upper` as functions in an
  index key.
- Names inside expressions are field names and are mapped to their `db_column`;
  a name that is not a field passes through unchanged.
- Literals are written inline into the `CREATE INDEX` statement and typed by
  PostgreSQL from the surrounding expression. Write the same expression in a
  query for the planner to use the index.
- `RawSQL` is sent verbatim with database column names. Migration ordering cannot
  see the columns it uses, so dropping such a column does not automatically
  drop the index first.
- A bare `F("col")` field is the same as `"col"`.
- Changing an expression recreates the index (drop, then create).
- A malformed key in a hand-written migration fails with a specific message,
  such as "index key must set exactly one of `column` and `expr`, got both", or
  an unknown-field error. The valid shapes are in
  [Migrations](migrations.md#create-index).
- The list of values in an enum column is not part of an index key. Adding a
  member to the enum does not recreate the index.

### Operator Classes

`opclasses` gives one operator class per field (`None` keeps the default):

```python
Index(("name",), opclasses=("text_pattern_ops",), name="ix_user_name_pattern")
Index(
    (Lower("name"),),
    method="gin",
    opclasses=("gin_trgm_ops",),
    name="ix_user_name_trgm",
)
```

The class name is written unquoted, so it must be an identifier, optionally
schema-qualified (`public.gin_trgm_ops`). Classes that come from an extension need
the extension first. Create it in an earlier migration:

```python
def upgrade(ctx):
    ctx.execute("CREATE EXTENSION IF NOT EXISTS pg_trgm")
```

An index with an operator class is a different index from a plain one on the same
column, so `Field(db_index=True)` and an `Index(...)` with `opclasses` can coexist.

### Index Methods

PostgreSQL supports different index methods:

```python
Index(("data",), method="gin")   # GIN index for JSONB
Index(("name",), method="hash")  # Hash index for equality
Index(("point",), method="spgist")
Index(("created_at",), method="brin")  # Block-range index for large, ordered tables
```

## Constraints

### UNIQUE Constraint

```python
# Single column
email: str = Field(db_unique=True)

# Multiple columns
class Meta:
    unique_together = [("user_id", "slug")]
```

### CHECK Constraint

```python
from oxyde import Check

class Event(Model):
    start_date: datetime
    end_date: datetime
    price: float

    class Meta:
        is_table = True
        constraints = [
            Check("start_date < end_date", name="valid_dates"),
            Check("price >= 0"),
        ]
```

## SQL Defaults

Set database-level default values:

```python
class User(Model):
    id: int | None = Field(default=None, db_pk=True)
    created_at: datetime = Field(db_default="CURRENT_TIMESTAMP")
    uuid: str = Field(db_default="gen_random_uuid()")  # PostgreSQL
    status: str = Field(db_default="'active'")  # Note: strings need quotes

    class Meta:
        is_table = True
```

!!! warning "Python vs SQL Defaults"
    - `default=value` — Python-side default, used when creating instances
    - `db_default="..."` — SQL-side default, used by the database

## Column Mapping

Override the database column name:

```python
class User(Model):
    created_at: datetime = Field(db_column="created_timestamp")

    class Meta:
        is_table = True
```

The Python attribute is `created_at`, but the database column is `created_timestamp`.

## Custom SQL Types

Override the inferred SQL type:

```python
class User(Model):
    id: int = Field(db_pk=True, db_type="BIGSERIAL")
    name: str = Field(db_type="VARCHAR(255)")
    data: dict = Field(db_type="JSONB")  # PostgreSQL

    class Meta:
        is_table = True
```

## Instance Methods

### save()

Insert or update a record:

```python
# Insert new record
user = User(name="Alice", email="alice@example.com")
await user.save()
print(user.id)  # Auto-generated ID

# Update existing record
user.name = "Alice Smith"
await user.save()

# Partial update (only specified fields)
user.age = 31
await user.save(update_fields=["age"])
```

### delete()

Delete a record:

```python
user = await User.objects.get(id=1)
await user.delete()
```

### refresh()

Reload from database:

```python
user = await User.objects.get(id=1)
# ... some other process updates the database ...
await user.refresh()  # Reload latest data
```

## Lifecycle Hooks

Override these methods to run code before/after database operations:

```python
class User(Model):
    id: int | None = Field(default=None, db_pk=True)
    name: str
    email: str
    created_at: datetime | None = Field(default=None)
    updated_at: datetime | None = Field(default=None)

    class Meta:
        is_table = True

    async def pre_save(self, *, is_create: bool, update_fields: set[str] | None = None):
        """Called before save()."""
        from datetime import datetime
        now = datetime.utcnow()
        if is_create:
            self.created_at = now
        self.updated_at = now

    async def post_save(self, *, is_create: bool, update_fields: set[str] | None = None):
        """Called after save()."""
        if is_create:
            print(f"Created user {self.id}")

    async def pre_delete(self):
        """Called before delete()."""
        print(f"About to delete user {self.id}")

    async def post_delete(self):
        """Called after delete()."""
        print(f"Deleted user {self.id}")
```

## Model Inheritance

### Abstract Models

Create base models without database tables:

```python
class TimestampMixin(Model):
    """Mixin for created_at/updated_at fields."""
    created_at: datetime = Field(db_default="CURRENT_TIMESTAMP")
    updated_at: datetime | None = Field(default=None)


class User(TimestampMixin):
    id: int | None = Field(default=None, db_pk=True)
    name: str

    class Meta:
        is_table = True
```

Only `User` creates a database table.

## Pydantic Integration

Model inherits from Pydantic's BaseModel, so you get:

### Validation

```python
class User(Model):
    id: int | None = Field(default=None, db_pk=True)
    age: int = Field(ge=0, le=150)  # Must be 0-150
    email: str = Field(pattern=r"^[\w.-]+@[\w.-]+\.\w+$")

    class Meta:
        is_table = True

# Raises ValidationError
user = User(age=200, email="invalid")
```

### Serialization

```python
user = await User.objects.get(id=1)

# To dict
data = user.model_dump()

# To JSON
json_str = user.model_dump_json()

# From dict
user = User.model_validate({"name": "Alice", "email": "alice@example.com"})
```

### JSON Aliases

```python
class User(Model):
    created_at: datetime = Field(
        alias="createdAt",        # JSON key
        db_column="created_at",   # Database column
    )

    class Meta:
        is_table = True
```

## Generated Type Stubs

Configure model modules in `oxyde_config.py`, then generate `.pyi` files beside
them for typed manager and query methods:

```python
# oxyde_config.py
MODELS = ["myapp.models"]
```

```bash
oxyde generate-stubs
```

Generated methods include field-specific filter operands, array and JSON
`contains` / `contained_by` / `overlap` lookups, and the keyword-only
`create(..., _skip_hooks=False)` parameter. Unknown field names remain type
errors.

A generated stub shadows the whole Python module, so it also preserves public
constants, aliases, supporting classes, intentional reexports, decorators,
PEP 695 type parameters, and imports guarded by `TYPE_CHECKING`. Async generator
signatures retain async iteration and `asynccontextmanager` compatibility;
Python 3.15 lazy imports become ordinary imports in portable stubs.

Regenerate stubs when model fields or module interfaces change. The repository
checks the generated interfaces and expected type errors with mypy, pyright,
and ty.

## Next Steps

- [Fields](fields.md) — Complete field reference
- [Queries](queries.md) — Query your models
- [Relations](relations.md) — Foreign keys and joins
