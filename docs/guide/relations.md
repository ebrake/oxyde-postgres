# Relations

Oxyde supports foreign key relationships and eager loading.

## Foreign Keys

### Defining a Foreign Key

Foreign keys are defined using type annotations:

```python
class Post(Model):
    id: int | None = Field(default=None, db_pk=True)
    title: str
    author: Author | None = Field(default=None, db_on_delete="CASCADE")

    class Meta:
        is_table = True
```

The FK target class must be defined **before** the model that references it. See [Type annotations and forward references](#type-annotations-and-forward-references) for details.

This creates:
- A column `author_id` in the database
- A foreign key constraint to `Author.id`

### Working with Foreign Keys

```python
# Create with FK ID
post = await Post.objects.create(
    title="Hello World",
    author_id=1  # Use the _id suffix
)

# Access FK value
print(post.author_id)  # 1
```

You can also pass a related instance or assign it directly.

```python
post = await Post.objects.create(title="Hello World", author=author)
post.author = other_author
await post.save()
```

`Post(...)`, `model_construct()`, and `model_copy(update=...)` can copy the
related object's key to the FK column. Oxyde uses the target field and column
configured by `db_fk` and `db_column` and records the copied column in
`model_fields_set`. A key is available once it has been loaded, saved, or
supplied explicitly. A client-generated key such as `default_factory=uuid4`
only fills the FK after the related object is saved. This also applies to
defaults on non-primary-key targets.

A required FK needs an available key when you construct the model. Supply a
related object with an available key or pass the FK column explicitly.
Construction with a related object whose key is still a default raises a
validation error. For synthesized FK columns, `model_validate()` and
`model_validate_json()` use pydantic-core's alias path to read keys from nested
dictionaries and JSON. With a model instance in `model_validate()` input,
supply the required FK column explicitly. A base class after `Model` in the
MRO whose `__init__` lacks `**kwargs` also requires the FK column to be passed
explicitly.

An explicit FK column, including `None`, takes precedence. Construction and
copy updates keep a supplied or cached relation only if its available key
matches the FK column. A copy update that supplies only the relation updates
the FK column to the new key, or to `None` if the key is unavailable.
Assigning `None` to `post.author` clears the loaded relation while retaining
the FK value. Set `post.author_id = None` to unlink a nullable relation,
including an assigned object that has yet to be saved. After `save()`, the
assigned relation stays attached if it still matches the stored FK.

If a write includes an empty FK column and the assigned related object has
no available key, it raises `ValueError`. With a nullable FK or when assigning
the relation to an already constructed model, you can save the related
object after assigning it. The next save of the parent fills in the missing
FK. An explicit update such as `post.save(update_fields=["title"])` only
checks the columns it writes. Both `save(update_fields=["author"])` and
`Post.objects.bulk_update(posts, ["author"])` write `author_id`.

### FK to Non-PK Field

Reference a different field:

```python
class Resource(Model):
    id: int | None = Field(default=None, db_pk=True)
    tenant: Tenant | None = Field(
        default=None,
        db_fk="uuid",  # Target Tenant.uuid instead of Tenant.id
        db_on_delete="CASCADE"
    )

    class Meta:
        is_table = True
```

This creates column `tenant_uuid` referencing `Tenant.uuid`.

On PostgreSQL, `create()` and `bulk_create()` can insert a model without a
primary key. A successful insert makes its submitted unique key available
as an FK target.

### ON DELETE Actions

| Action | Description |
|--------|-------------|
| `CASCADE` | Delete related rows |
| `SET NULL` | Set FK to NULL |
| `RESTRICT` | Prevent deletion |
| `NO ACTION` | Deferred check |

```python
# CASCADE - delete posts when author deleted
author: Author | None = Field(default=None, db_on_delete="CASCADE")

# SET NULL - keep post, set author to NULL
author: Author | None = Field(default=None, db_on_delete="SET NULL")

# RESTRICT - prevent deletion if posts exist
author: Author | None = Field(default=None, db_on_delete="RESTRICT")
```

## Eager Loading

### join()

Load related models in a single query (LEFT JOIN):

```python
# Load author with each post
posts = await Post.objects.join("author").all()

for post in posts:
    print(f"{post.title} by {post.author.name}")
```

Without `join()`, accessing `post.author` would require another query.

### Multiple Joins

```python
posts = await Post.objects.join("author", "category").all()
```

### Nested Joins

Use `__` to traverse relations:

```python
# Post -> Author -> Company
posts = await Post.objects.join("author__company").all()
for post in posts:
    print(f"{post.title} by {post.author.name} at {post.author.company.name}")
```

## Reverse Relations

### Defining Reverse FK

On the "one" side of a one-to-many:

```python
class Author(Model):
    id: int | None = Field(default=None, db_pk=True)
    name: str
    posts: list["Post"] = Field(db_reverse_fk="author")  # Virtual field for reverse relation

    class Meta:
        is_table = True


class Post(Model):
    id: int | None = Field(default=None, db_pk=True)
    title: str
    author: Author | None = Field(default=None, db_on_delete="CASCADE")

    class Meta:
        is_table = True
```

### prefetch()

Load reverse relations:

```python
# Load all posts for each author
authors = await Author.objects.prefetch("posts").all()

for author in authors:
    print(f"{author.name} has {len(author.posts)} posts")
    for post in author.posts:
        print(f"  - {post.title}")
```

`prefetch()` executes a separate query and attaches results.

## Many-to-Many

### Defining M2M

Use a junction table:

```python
class Post(Model):
    id: int | None = Field(default=None, db_pk=True)
    title: str
    tags: list["Tag"] = Field(db_m2m=True, db_through="PostTag")

    class Meta:
        is_table = True


class Tag(Model):
    id: int | None = Field(default=None, db_pk=True)
    name: str = Field(db_unique=True)

    class Meta:
        is_table = True


class PostTag(Model):
    id: int | None = Field(default=None, db_pk=True)
    post: Post | None = Field(default=None, db_on_delete="CASCADE")
    tag: Tag | None = Field(default=None, db_on_delete="CASCADE")

    class Meta:
        is_table = True
```

### Working with M2M

M2M relations are supported by `prefetch()`. The through model must have FK fields to both models:

```python
# Load posts with their tags
posts = await Post.objects.prefetch("tags").all()
for post in posts:
    print(f"{post.title}: {[t.name for t in post.tags]}")
```

You can also work with the junction table directly:

```python
# Add tag to post
await PostTag.objects.create(post_id=post.id, tag_id=tag.id)

# Remove tag from post
await PostTag.objects.filter(post_id=post.id, tag_id=tag.id).delete()

# Get all tags for a post (alternative approach)
post_tags = await PostTag.objects.filter(post_id=post.id).join("tag").all()
tags = [pt.tag for pt in post_tags]
```

## Automatic FK Column Naming

Oxyde automatically creates FK columns:

| Field Definition | Created Column |
|-----------------|----------------|
| `author: Author` | `author_id` |
| `author: Author` (PK is `uuid`) | `author_uuid` |
| `category: Category` (FK to `code`) | `category_code` |

## Common Patterns

### Self-Referential FK

```python
from __future__ import annotations

class Category(Model):
    id: int | None = Field(default=None, db_pk=True)
    name: str
    parent: Category | None = Field(default=None, db_on_delete="SET NULL")

    class Meta:
        is_table = True
```

`from __future__ import annotations` is required here because `Category` references itself.

### Polymorphic Relations (Manual)

```python
class Comment(Model):
    id: int | None = Field(default=None, db_pk=True)
    content: str
    commentable_type: str  # "post" or "photo"
    commentable_id: int

    class Meta:
        is_table = True

# Query
post_comments = await Comment.objects.filter(
    commentable_type="post",
    commentable_id=post_id
).all()
```

### Soft Delete with Relations

```python
class Post(Model):
    id: int | None = Field(default=None, db_pk=True)
    title: str
    deleted_at: datetime | None = Field(default=None)
    author: Author | None = Field(default=None, db_on_delete="SET NULL")

    class Meta:
        is_table = True

# Query active posts with author
posts = await Post.objects.filter(
    deleted_at__isnull=True
).join("author").all()
```

## Complete Example

```python
from datetime import datetime
from oxyde import Model, Field, db

class Author(Model):
    id: int | None = Field(default=None, db_pk=True)
    name: str
    posts: list["Post"] = Field(db_reverse_fk="author")

    class Meta:
        is_table = True
        table_name = "authors"


class Post(Model):
    id: int | None = Field(default=None, db_pk=True)
    title: str
    content: str
    author: Author | None = Field(default=None, db_on_delete="CASCADE")
    created_at: datetime = Field(db_default="CURRENT_TIMESTAMP")

    class Meta:
        is_table = True
        table_name = "posts"


async def main():
    async with db.connect("postgresql://localhost/app"):
        # Create author
        author = await Author.objects.create(name="Alice")

        # Create posts
        await Post.objects.create(
            title="First Post",
            content="Hello!",
            author_id=author.id
        )
        await Post.objects.create(
            title="Second Post",
            content="World!",
            author_id=author.id
        )

        # Load posts with author (JOIN)
        posts = await Post.objects.join("author").all()
        for post in posts:
            print(f"{post.title} by {post.author.name}")

        # Load author with posts (prefetch)
        authors = await Author.objects.prefetch("posts").all()
        for author in authors:
            print(f"{author.name}: {len(author.posts)} posts")
```

## Type Annotations and Forward References

Oxyde uses type annotations to define relationships. Python defers evaluating
annotations, so a field can reference its own model or one defined later in
the module:

```python
class Post(Model):
    author: Author | None = Field(default=None, db_on_delete="CASCADE")

class Author(Model):
    posts: list[Post] = Field(db_reverse_fk="author")
```

## Limitations

### No M2M in join()

M2M relations are supported by `prefetch()` but not by `join()`. Use `prefetch()` for M2M.

## Next Steps

- [Transactions](transactions.md) — Atomic operations
- [Queries](queries.md) — Full query reference
- [Fields](fields.md) — Field options reference
