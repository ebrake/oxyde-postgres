# Changelog

## Unreleased

- `db.listen()` subscribes to PostgreSQL notifications with one dedicated connection
  outside the named pool's limit. See [upstream PR #48](https://github.com/mr-fatalyst/oxyde/pull/48).
- Connection and pool failures, including acquire timeouts, now raise `DatabaseError`,
  a subclass of `RuntimeError`.
- Listener reconnect attempts back off and stop at the named pool's acquire timeout.
- Initial listener subscriptions are bounded by the named pool's acquire timeout.
- Closing a listener releases its dedicated connection immediately.
- Listeners stay open when a receive raises a transient connection error, including when
  a read-only standby rejects `LISTEN`. A terminal receive error raises `DatabaseError` and
  closes the listener. Later receives and iteration raise `db.ListenerClosedError` with
  the original error message. The listener example keeps one pending receive across
  periodic timeouts.

## 0.5.0 — Bundled Rust core and PostgreSQL 19 features

- `oxyde-postgres` now bundles the Rust core as `oxyde._oxyde_core` so the
  extension and Python code always ship together. After upgrading, run
  `pip uninstall oxyde-postgres-core` to remove the retired distribution.
  Native exceptions now report `oxyde._oxyde_core` as their module.
- CI tests PostgreSQL 18 and 19.
- PostgreSQL `oid` and `oid8` results, including array elements, decode as
  Python integers and preserve their full unsigned range.
- PostgreSQL 19 adds scan I/O statistics via `explain(analyze=True, io=True)`
  and single-statement `get_or_create_atomic()` via `ON CONFLICT DO SELECT`.
  Both features raise `UnsupportedServerVersionError` on PostgreSQL 18.
- Many-to-many prefetch deduplicates target IDs in linear time.
- The Rust core converts write values for `create()`, `bulk_create()`,
  `update()`, `bulk_update()`, and upserts. It handles strings, numbers,
  bytes, dates and times, UUIDs, and decimals directly. Expressions,
  containers, enums, subclasses, and types whose `TYPE_REGISTRY` serializer an
  application replaced still go through the Python serializer, so stored
  values are unchanged.

    Client-side time per operation with the previous write path (Before) and
    the Rust core (After), median of seven runs, using a stub client that
    MessagePack-encodes each query:

    | Operation                                   |  Before |   After | Speedup |
    | :------------------------------------------ | ------: | ------: | ------: |
    | Build and encode a bulk INSERT (1,000 rows) | 4.16 ms | 1.29 ms |    3.2× |
    | `bulk_create()` (1,000 rows)                | 6.65 ms | 3.68 ms |    1.8× |
    | `create()` (one row)                        | 15.6 µs | 12.5 µs |    1.3× |
    | Filtered `update()` (one integer field)     |  7.3 µs |  7.2 µs |    1.0× |
    | Filtered `update()`, 41-field model         | 10.1 µs |  9.9 µs |    1.0× |

## 0.4.1 — Foreign-key assignment and inserts without primary keys

- Passing or assigning a related instance copies its key to the foreign-key
  column once the key has been loaded, saved, or supplied explicitly.
  Client-generated keys such as `default_factory=uuid4` only fill the FK
  after the related object is saved. A required FK raises a validation error
  during construction when the related object's key is still a default.
- `bulk_update(posts, ["author"])` writes the scalar value from `author_id`.
- An explicit FK column, including `None`, takes precedence at construction
  and in copy updates. A relation-only copy update sets the FK column to the
  new key, or to `None` when the key is unavailable. Writes to an empty FK
  column raise `ValueError` if the assigned object has no available key.
  `save()` keeps the assigned relation when it matches the returned FK.
- Required FK columns can resolve keys from nested dictionaries or JSON via
  pydantic-core's alias path. With instances in `model_validate()` input or an
  initializer after `Model` in the MRO that lacks `**kwargs`, supply the
  required FK column explicitly.
- PostgreSQL `create()` and `bulk_create()` now support models with no primary
  key. The affected-row count confirms which submitted fields were persisted
  when the full insert succeeds.
- The ORM requires `oxyde-postgres-core>=0.4.1,<0.5.0` for the new insert behavior.

## 0.4.0 — Typed PostgreSQL query results

- **Breaking.** `values()`, `values_list()`, `fetch_all()`, and `fetch_rows()`
  return typed UUID, Decimal, date, time, datetime, and timedelta values,
  including array elements from model fields and untyped expressions such as
  `RawSQL`. PostgreSQL `TIME` returns a naive `time` and `TIMETZ` returns a
  timezone-aware `time`. The same rules apply to `first()`, `last()`, and
  `get()` in values mode. The core preserves strings for `str` and
  `list[str]` fields, including `Annotated` and optional forms, in direct
  projections, `F()` references, type-preserving `Coalesce()` and `Greatest()`
  calls, and `Min` and `Max`, including the terminal methods. Widening literals
  such as `Decimal("1.5")` produce a computed, typed result. Enums return their
  underlying values.

    When upgrading, remove `UUID()`, `Decimal()`, `fromisoformat()`, and
    `timedelta(seconds=…)` wrappers around results that now arrive typed.
    For example, `UUID()` raises when passed an existing `UUID` object.

- Values queries apply typed conversion, including for `Sum` and `Avg`
  annotations, when the core returns result kinds. Untagged values remain raw.
  Keep annotation aliases within 63 bytes. PostgreSQL truncates longer aliases
  in grouped and non-grouped queries. `values()` returns the wire value under
  the truncated name, and `values_list()` returns `None` for the requested alias.
- Preserve PostgreSQL text for out-of-range temporal values, including
  `infinity`, `-infinity`, and `24:00:00` with its offset for `TIMETZ`.
  Non-finite `NUMERIC` results travel as PostgreSQL text and convert to
  `Decimal` when tagged.
- Terminal `.sum()` and `.avg()` return `timedelta` on duration fields.
  The Rust core preserves whole microseconds and rounds fractional microseconds
  to the nearest even microsecond on ties.
- Model queries correctly decode `list[timedelta]` elements. Previously they
  treated the stored microsecond counts as seconds.

- Require Rust 1.100+ for source builds and pin development and CI to Rust 1.100.0.
- Upgrade [SQLx to 0.9.1](https://github.com/transact-rs/sqlx/blob/v0.9.1/CHANGELOG.md)
  and rebase the vendored PostgreSQL driver, preserving single-batch unnamed
  queries for PgBouncer transaction pooling. The upgrade fixes statement-cache
  leaks, stale cached plans, unnamed statement parsing after custom OID
  resolution, Bind result-format counts, type dependency resolution, and
  out-of-range date/time decoding. Connections now use TCP_NODELAY and reuse
  the TLS connector across attempts. `hostaddr`/`PGHOSTADDR` now bypasses DNS
  while retaining `host` as the TLS hostname, matching libpq.
- PostgreSQL TLS now uses the aws-lc-rs crypto provider for rustls. Certificate
  verification uses the operating system's trust store in place of the bundled
  Mozilla roots. `ssl_root_cert` can add a custom CA.
- macOS wheels target Apple Silicon. Intel Mac installations build from source.
- Upgrade both packages together. The ORM requires `oxyde-postgres-core>=0.4.0,<0.5.0` for typed query results.

## 0.3.5 — PostgreSQL aggregates, joins, and Python 3.15

- Require Python 3.15+ for both packages. Native wheels target standard and
  free-threaded CPython 3.15. Tests exercise both forms of native lazy import
  in model modules and verify that the generated stubs pass static type checks.
- Grouped output labels and related aggregate inputs require native core 0.3.5.
  The minimum supported core is `oxyde-postgres-core>=0.3.5,<0.4.0`.
  Native core 0.3.5 uses ABI 2. Upgrade both packages together because
  oxyde-postgres 0.3.4 expects ABI 1.

- Aggregates on PostgreSQL work with filters through foreign keys. Group keys,
  aggregate inputs and `order_by()` can name related fields
  (`group_by("author__name")`, `Sum("author__score")`,
  `order_by("author__name")`) and join the related table themselves. Grouped
  rows keep the requested names, including labels longer than PostgreSQL's
  63-byte identifier limit. Terminal `.min()`/`.max()` and `Min`/`Max`
  annotations now decode like the input field. For example,
  `.max("duration")` on a timedelta field returns a `timedelta`.
  `COUNT`/`SUM`/`AVG` aliases decode by their own result type. A column
  whose `db_column` contains a dot is quoted as one identifier. Low-level
  filters accept qualified references such as `posts.id`.
- In grouping, aggregates and ordering, a name that is both a relation path
  and a raw `db_column` (a `supplier` foreign key next to
  `db_column="supplier__name"`) now names the related field, as in `filter()`.
  Use the field name for the column. Non-grouped `values()` keeps the raw
  column meaning.
- Joined rows assign related columns to their relation by position. A base
  column named like a related one (`db_column="supplier__name"` next to a
  joined `supplier`) keeps its value and type, and nested joins
  (`join("post__author")`) load each level, including when PostgreSQL truncates
  long join labels. Flat results reject duplicate column labels across joins.
- `group_by()` accepts selected scalar annotations. Default projections can
  order a same-model UNION by a related field.
- Joins honor the target field specified by `db_fk` and deduplicate related
  rows by their primary key, including when the foreign key references an array.
  Nested join aliases use the full path, such as `post__author`.
  Update any `RawSQL` that refers to
  the former `post_author` alias. Conflicting aliases receive a suffix.
- `for_update()` and `for_share()` with joins lock the base table's rows
  using `FOR UPDATE OF` and `FOR SHARE OF`.
- Raw queries with an explicit projection omit joined result columns.
  Non-grouped `distinct()` model queries keep joined relations when their
  projection includes the primary key. A non-grouped `distinct()` query that
  omits joined columns raises `ValueError` if it orders by a related field.
- `UNION` operands use the same join projection. Flat row fetches raise
  `ValueError` if a joined result would overwrite a base model field.

## 0.3.4 — Primary key tracking and upserts

- Database hydration uses Pydantic's compiled validator without the
  primary-key tracking callback when the returned row supplies the key.
  Database field names apply at the row boundary, and nested JSON models use
  their own alias settings.
- Default primary keys are tracked using the fields Pydantic actually
  received, including on alias paths and attribute-based inputs. Inserts
  send populated primary keys that are excluded from application dumps.
- Conflict updates qualify expressions against the existing row and reject
  overlapping assignments. Bulk inserts confirm keys from the returned
  rows, leaving suppressed default-key rows retryable.
- In-place inserts refresh private containers that validation mutated and
  preserve untouched local private state.

- `save()` now inserts or updates on primary-key conflict when the key comes
  from `default` or `default_factory`. Omitted columns keep their database
  defaults on insert, and an empty `update_fields` leaves an existing row
  untouched. For a default key, `pre_save` receives `is_create=True` and
  `post_save` reports whether a row was inserted. Saving a partial model updates
  the loaded and assigned fields by default and requires a primary key. When
  the key is included in the projection, `distinct()` and `union()` treat rows
  with different keys as distinct. Raw fetches use the requested columns.
  `create(instance=…)` populates the passed instance in place. Both
  `create(instance=…)` and default-key `save()` refresh validator-derived
  private attributes from the returned row and keep default and
  factory-initialized ones. A key assigned by `model_post_init` or an
  after-validator still counts as a default key. A default key assigned by
  `pre_save` remains retryable until the insert or upsert returns a row. A
  conflict with empty `update_fields` or a trigger can suppress that row.
  The upsert works for a table named `excluded`, and `union()` no longer
  adds the key to aggregate branches.
- A partial select tracks the fields returned by the database. Assignments
  made during model validation to unselected fields are excluded from the
  default `save()` update.
- `save()` sends `update_columns` and value-less DO UPDATE IR that core 0.3.3
  rejects or ignores. The minimum supported core is now
  `oxyde-postgres-core>=0.3.4,<0.4.0`.
- `InsertQuery.bulk_values()` keeps `on_conflict_do_nothing()` and
  `on_conflict_update()` clauses.
- `Field(alias=...)` now works like `pydantic.Field(alias=...)`: the alias is
  accepted on input and used by `model_dump(by_alias=True)`. It was previously
  ignored. Models also accept field names on application input
  (`validate_by_name=True`). Database rows are validated by field name, even
  when an alias matches another field's name. Writes always use field names,
  even with `serialize_by_alias=True`.
- `model_validate()` honors `strict` and `context` and tracks default primary
  keys the same way as the constructor.

## 0.3.3 — Declared PostgreSQL extensions

- Declare requirements with `Meta.extensions = ("pg_trgm",)`. Requirements from
  table models are combined into a sorted, deduplicated snapshot set.
- Migrations and `create_tables()` install new requirements before enum types,
  tables and indexes using `CREATE EXTENSION IF NOT EXISTS`.
- Added `ensure_extension`, `remove_extension_requirement` (state only), and the
  explicit manual `drop_extension` operation (`DROP EXTENSION ... RESTRICT`).
  Removing a declaration or rolling back preserves the installed extension.
- Known extension operator classes and column types (`db_type`) are detected
  automatically, including pgvector types used by `hnsw` and `ivfflat` indexes.
  Quoted type names are matched case-sensitively, as in PostgreSQL (`'"Vector"'`
  is not pgvector).
  Use `Meta.extensions` for other requirements. Existing 0.3.2 models keep working
  without new declarations.
- **Migration note:** upgrading a 0.3.2 project using trigram, hstore, citext, cube,
  ltree, vector or PostGIS creates a pending `ensure_extension` migration.
  `CREATE EXTENSION IF NOT EXISTS` safely skips extensions already installed.
  Historical replay and squash remain compatible. The CLI has no `--check` mode;
  `makemigrations --dry-run` exits 0, so CI is unaffected.
- Generated migration names skip extension operations when naming structural
  changes. Inferred names are lowercase with punctuation and spaces replaced by
  underscores, including enum values and `uuid-ossp` (`extension_uuid_ossp`).
- Requires `oxyde-postgres-core>=0.3.3,<0.4.0`. Snapshots without extension
  requirements keep their existing serialized form.

## 0.3.2 — Expression and operator-class indexes

- `Index` fields can be expressions (`Index((Lower("email"),), name=...)`), and
  `Index(..., opclasses=(...))` sets an operator class per field. Expression
  indexes need an explicit name. `spgist` and `brin` join the accepted `method`
  values.
- `makemigrations` now checks index definitions in Rust: every column an index uses
  must exist, and index names must be unique across the schema, also after
  PostgreSQL's 63-byte truncation. Unnamed operator-class indexes include the
  operator class in their generated name (`users_email_text_pattern_ops_idx`), and
  plain indexes now honour `db_column` and foreign-key column names.
  An index name that PostgreSQL has already used (a table, `<table>_pkey`,
  `<table>_<column>_key` or `<table>_<column>_seq`) is rejected the same way.
- Replaying `rename_column` keeps index keys (including expressions) and foreign
  keys in step with the renamed column. Replaying `drop_column` removes the
  indexes and foreign keys that used the column, as PostgreSQL does, so a
  hand-written migration no longer leaves `makemigrations` proposing a
  `drop_index` that fails.
- Changing a column's `unique` flag now names the constraint the way PostgreSQL
  names an inline `UNIQUE` (`<table>_<column>_key`, shortened to fit 63 bytes), so
  `DROP CONSTRAINT` finds the constraint `CREATE TABLE` made on long table or
  column names. Names that already fit are unchanged.
- `makemigrations` now writes a `downgrade` for every column change
  (`unique`, `nullable`, type, length, default). Before, only a change involving
  `nulls_not_distinct` was reversed, so rolling back a migration that added or
  removed a `unique` constraint (or changed nullability) silently left the schema
  as it was. A change written as `column` plus `changes`, which has no previous
  value to restore, leaves a `# TODO` comment.
- Expression index keys no longer contain an enum's list of values, so adding an
  enum member does not recreate the index. Their `column_types` are written in a
  fixed order, so generated migration files are reproducible.
- Add `Lower` and `Upper` scalar expressions (also usable in `annotate()` and
  `update()`).
- Index keys are stored as a bare string for a plain column (unchanged, so existing
  snapshots and migrations are byte-identical) or as an object with `column` or
  `expr` and an optional `opclass`. A core older than 0.3.2 rejects such keys
  instead of dropping them. Index expressions may only call `coalesce`,
  `greatest`, `lower` and `upper` (PostgreSQL requires them to be IMMUTABLE;
  `concat_ws` is STABLE).
- Fix `makemigrations` output for a `drop_index` operation without its stored
  definition (it referenced a missing key).

## 0.3.1 — Scalar expressions (2026-11-12)

- Add `Greatest` and `Value`, and rebuild `Coalesce` as a scalar expression that
  composes with `F` arithmetic. `Coalesce` in `annotate()` previously failed at
  decode time; it now works, alongside `Greatest`, `Value` and `F` arithmetic.
- Scalar annotations are values-only: read them with `.values()`,
  `.values_list()` or `fetch_all()`. `.all()`, `.first()` and `.get()` raise
  `TypeError`. They cannot be mixed with aggregates in one query, `having()`
  rejects them, and an alias may not equal a model field or column name.
- `Concat` (`CONCAT_WS`, skips NULLs) and `RawSQL` are now scalar expressions and
  work in `annotate()` and `update()`. `Concat` accepts columns, `F` and `Value`
  arguments. `RawSQL` is sent verbatim with database column names and no
  parameters. `Concat.to_ir()` / `RawSQL.to_ir()` are gone.
- Map field names inside expressions to `db_column` in `update()`, `increment()`
  and `annotate()`. `increment()` on a field with a custom `db_column` now updates
  the right column. Related-field names (`a__b`) are rejected.
- `Coalesce`, `Concat` and `RawSQL` moved to `oxyde.queries.expressions`; imports
  from `oxyde`, `oxyde.queries` and `oxyde.queries.aggregates` still work.
  `to_ir()`, `.fields` and `.func_name` are gone from them.
- Map an aggregate's field name to its `db_column` (`annotate(Sum("stock"))`,
  `sum("stock")`); previously only the column name worked.
- Enum handling in expressions now lives in the core: a string or enum literal
  next to an enum column in `Coalesce`/`Greatest` (including nested calls) is
  cast to that column's enum type, a bare `Value(...)` assigned in `update()` or
  `create()` binds by the target column. A `str`-based `Enum` member passed to
  `Coalesce`/`Greatest` is a literal, not a column name.
- Annotation results are decoded by the type of the column they pass through:
  `annotate(x=F("tags"))` or `Coalesce("tags", None)` on an array column returns
  the list (it was `None`), `F("duration")` on a `timedelta` field returns
  seconds like the field itself (it was raw microseconds), and enum aliases are
  decoded as enums, also when another argument is `RawSQL` or arithmetic (the
  result is then the enum or a database error). Other expressions that can
  change the type (`F` arithmetic, a literal of another type such as
  `Greatest(int_col, 1.5)`, `Concat`, `RawSQL`) use native decoding.
  `count()` / `exists()` on an annotated query no longer
  decode their result through an annotation that shares the `_count` / `exists`
  name (they returned `0` / `False`).
- A `NULL` literal in `Coalesce`/`Greatest` now takes the type of a sibling column
  (`Coalesce("qty", None)` no longer fails with "bigint and text cannot be
  matched") or of the `update()` target column. String literals in a nested
  `Coalesce` under an enum `Greatest`, and in an `update()` expression assigned
  to an enum column, are now cast to the enum type. A bare
  `Value(Decimal(...))` assigned to a column with an unrecognised `db_type`
  (MONEY, CITEXT, domains) now binds by its own type instead of as text.
  `update(field=Value(...))` on an enum field validates the value like a raw one.
  A `NULL` with no sibling type (`Coalesce(None, 1)`, `Coalesce(F("qty") + 1,
  None)`, or next to an `Unknown`-typed column such as MONEY) is a bare SQL
  `NULL` that PostgreSQL types from context, instead of a text parameter that
  failed with "text and bigint cannot be matched". A MONEY-like column (binding
  hint only) never types a sibling or target, in `annotate()` or `update()`. A
  list literal next to an array column (`Coalesce("tags", Value([]))`) binds as
  that array type, so it works in `update()` and `annotate()`. A list whose
  elements do not fit the array's element type (`Value(["1", 2.5])` for a
  `BIGINT[]`, also as a bare `Value(...)` assigned to the column) now raises
  instead of storing NULL elements.
- A plain `Decimal` or `UUID` value assigned to or filtered on a column with an
  unrecognised `db_type` (MONEY, CITEXT, a domain) now binds by the field's
  annotation instead of as text. The core receives these as binding-only
  `bind_types`; they never change how results are decoded, and an explicit
  column spec wins. A `str` column no longer has an RFC 3339-looking value
  reinterpreted as a timestamp, and an enum field with an explicit `db_type`
  stays a plain verbatim column.
- An annotation alias that starts with a joined relation's `<prefix>__` is
  rejected; the encoder would have moved it into the relation. `fetch_all()` /
  `.values()` on a joined query without a field list now includes the joined
  `<prefix>__<column>` keys instead of silently omitting them.
- Expression nodes are decoded into a typed `ScalarExpr` in the core, and
  `QueryIR` gains `annotations`. The IR protocol is unchanged, so an older core
  would drop `annotations` silently: require `oxyde-postgres-core>=0.3.1,<0.4.0`.
- Expand the existing guides for scalar expressions, typed binding, migration
  replay, and generated stubs. Refresh installation and PostgreSQL requirements,
  query examples, and links to the fork's features.
- Fix `make build-core` to use the root package metadata and `make lint` to find
  the virtual environment's pre-commit executable.
- Validation: 1,548 Python tests, 233 Rust workspace tests, 16 PostgreSQL driver
  regressions, and 82 generated-stub type-check tests passed. All pre-commit
  checks, strict documentation builds, and package license/metadata checks
  passed; locally built wheels passed a fresh-environment PostgreSQL smoke test.

## 0.3.0 — Rust schema replay (2026-11-11)

- Replay migration schemas in Rust, sharing typed operations with schema diffing
  and SQL generation. Read individual fields when resolving alterations, avoiding
  serialization of the entire schema on every column change.
- **Breaking:** `SchemaState.tables` is a read-only canonical snapshot view. Each
  access returns a fresh dictionary; mutate state through `apply_operation`.
  Legacy metadata and omitted-key provenance are no longer included in snapshots.
- Replay histories strictly against the migration contract. Errors identify the
  migration filename, operation number (starting at 1), and operation type, and
  direct users to correct that historical file. Invalid operations previously
  recorded with `--fake` must also be corrected before replay or squash.
- Resolve `sqlmigrate` column alterations against history, sharing execute mode
  fallback behavior for untracked columns. Previews without column alterations
  no longer replay unrelated history.
- Track schema changes during execution: renaming a table or column and then
  altering it in the same migration now uses its current definition. For example,
  making a renamed NOT NULL column nullable emits `ALTER ... DROP NOT NULL`;
  0.2.x could omit that SQL.
- Preserve declaration key order in diff operations and generated migration files.
- Require `oxyde-postgres-core>=0.3.0,<0.4.0`. The core ABI remains version 1.

## 0.2.3 — async generator stubs and manager create (2026-11-11)

- Emit async generators as plain `def` declarations returning their annotated
  generator type, preserving `@asynccontextmanager` and `async for` compatibility
  in top-level functions, supporting classes, and custom model methods.
- Preserve decorators, signatures, and PEP 695 type parameters. Keep ordinary
  coroutines asynchronous and exclude yields in nested scopes from detection.
- Include the keyword-only `_skip_hooks: bool = False` parameter in generated
  manager `create` signatures while retaining typed field narrowing, so unknown
  field names remain errors.
- Check generated stubs with mypy, pyright, and ty, including strict ty suppression
  validation with type-ignore comments disabled. The supported ty rule name is
  `incompatible-method-override` as of the pinned ty 0.0.100.
- Validation: 1,050 unit tests passed; all generated-stub fixtures and negative
  checks passed with mypy, pyright, and ty, including 15 strict suppression checks.

## 0.2.2 — complete generated stubs (2026-11-11)

- Preserve public constants, typed declarations, supporting classes, `__all__`,
  and intentional reexports when a generated stub shadows a model module.
- Collect imports guarded by `TYPE_CHECKING`, including aliases and
  `typing_extensions`, and render Python 3.15 native lazy imports as ordinary
  imports in portable stubs.
- Preserve typing factories, functional enums, implicit type aliases, explicit
  `TypeAlias` declarations, and PEP 695 `type` statements as type declarations.
- Avoid duplicate bindings when functions, classes, or imports are reassigned
  through wrappers. Merge repeated imports and keep star imports separate.
- Retain the ALL_CAPS constant reexport convention while excluding runtime
  classes, functions, and modules. Avoid unnecessary skeleton imports for local
  declarations and follow private signature dependencies with identifier sets.
- Validation: 1,346 Python tests and 180 Rust tests passed; generated-stub checks
  passed with mypy, pyright, and ty. Six Python 3.15 native lazy-import tests,
  all pre-commit hooks, and the original reproduction cases passed.

## 0.2.1 — NULLS NOT DISTINCT (2026-11-11)

- Support PostgreSQL `NULLS NOT DISTINCT` for unique indexes with
  `Index(..., unique=True, nulls_distinct=False)` and unique columns with
  `Field(db_unique=True, db_nulls_distinct=False)`, including reversible migrations
  when the option changes.
- Preserve existing NULLs-distinct behavior with `None` (the default) or `True`.
  The serialized `nulls_not_distinct` key is omitted when false, so existing
  snapshots and migration files remain compatible and unchanged models produce
  no extra migrations.

- Require `oxyde-postgres-core>=0.2.1,<0.3.0` so the Python API always uses a core that supports the new constraint option.

- Validation: 180 Rust tests and 1,257 Python tests passed (two expected skips), including PostgreSQL 18 constraint enforcement, migration upgrades/rollbacks, and existing snapshot compatibility. Required lint/type checks, strict documentation build, and package license/metadata checks passed.

- Pin Linux CI and release hosts to Ubuntu 24.04 LTS; Linux wheel builds use the existing manylinux/musllinux containers.

## 0.2.0 — dependency refresh and Python 3.13 minimum (2026-11-11)

### Compatibility

- Require **Python 3.13+** for both packages; PostgreSQL **18+** remains required. Python 3.10–3.12 users can continue using 0.1.0.post1.
- Build native wheels for CPython 3.13, 3.14, 3.15, and free-threaded 3.14 on the existing Linux glibc/musl, macOS, and Windows targets.
- Require Rust 1.99+ for source builds and pin development/CI to stable Rust 1.99.1.
- Keep the `oxyde` import, `_oxyde_core` extension, CLI, and ORM APIs. Pair `oxyde-postgres` 0.2.0 with `oxyde-postgres-core>=0.2.0,<0.3.0`.

### SQLx and Rust dependencies

- Upgrade [SQLx to 0.9.0](https://github.com/transact-rs/sqlx/blob/v0.9.0/CHANGELOG.md) from 0.8.6. Rebase the vendored PostgreSQL driver on the published 0.9.0 source, including its current type resolver, authentication, TLS, connection, and transaction fixes.
- Preserve cache-disabled, single-batch unnamed Parse → Bind → Describe portal → Execute → Close → Sync execution. Cold custom result types resolve through SQLx's recursive resolver only after the original `ReadyForQuery`, on the owning connection, without replaying SQL.
- Adapt generated and explicit raw SQL entry points to SQLx's `SqlStr` / `AssertSqlSafe` API; ORM values remain separately bound parameters.
- Upgrade SeaQuery from 0.31.1 to **1.0.2**, adapting identifiers, expression operators, unboxed scalar values, array binding, and thread-safe values.
- SeaQuery now parenthesizes SQL default expressions (`DEFAULT (42)`, `DEFAULT (CURRENT_TIMESTAMP)`). The two affected golden DDL snapshots are intentionally updated; the SQL retains the same PostgreSQL default behavior and existing databases need no migration for this formatting change.
- Refresh `Cargo.lock` to the latest non-yanked versions allowed by the dependency graph. Direct dependency baselines are:

| Package | Version |
|---|---|
| SQLx / sqlx-postgres | 0.9.0 |
| SeaQuery | 1.0.2 |
| Tokio | 1.54.1 |
| serde / serde_json | 1.0.229 / 1.0.154 |
| anyhow / thiserror / tracing | 1.0.104 / 2.0.21 / 0.1.44 |
| async-trait / futures / once_cell | 0.1.92 / 0.3.34 / 1.21.4 |
| chrono / uuid / rust_decimal | 0.4.45 / 1.29.2 / 1.43.0 |
| rmp / rmp-serde / rmpv | 0.8.15 / 1.3.1 / 1.3.1 |
| PyO3 / pyo3-async-runtimes | 0.30.1 / 0.30.0 |
| mimalloc / tikv-jemallocator (optional) | 0.1.52 / 0.7.0 |
| insta / tempfile (tests) | 1.49.0 / 3.27.0 |

### Python, build, and documentation dependencies

| Package | Version baseline |
|---|---|
| Pydantic / MessagePack / Typer | 2.14.3 / 1.2.4 / 0.29.1 |
| pytest / pytest-asyncio / pytest-cov | 9.2.0 / 1.4.1.post1 / 7.1.0 |
| Ruff / pre-commit | 0.17.4 / 4.7.0 |
| mypy / pyright / ty (pinned) | 2.4.1 / 1.1.414 / 0.0.100 |
| testcontainers / psycopg2-binary | 4.15.0 / 2.9.13 |
| maturin / setuptools | 1.16.0 / 84.2.1 |
| MkDocs / Material for MkDocs | 1.6.1 / 9.8.1 |
| PyMdown Extensions / mike | 13.0 / 2.2.0 |

- Raise runtime and optional dependency baselines to their latest stable releases at preparation time. Update Ruff hooks, CI's mypy version, and native release builds to match.
- Use standard-library `Self`, `dataclass_transform`, and `TimeoutError` with the Python 3.13 minimum. Keep explicit generic variance and runtime typing metadata.
- Adapt primary-key narrowing for mypy 2 and emit the current `ty` override diagnostic name in generated stubs.
- Validate `Requires-Python` alongside package version, description links, authorship, and bundled licenses before publishing.

### Validation

- 173 Rust workspace tests and 16 explicitly enabled PostgreSQL driver tests passed, including the wire-order, cold custom-type, cancellation, and no-replay regressions.
- 1,226 Python tests passed on CPython 3.15 (two expected skips). All 47 generated-stub tests passed on CPython 3.13 with the upgraded mypy, pyright, and ty.
- All pre-commit hooks, optional allocator builds, strict documentation build, nine release-verifier tests, local wheel imports, and artifact license/metadata checks passed.

## 0.1.0.post1 — package description corrections (2026-11-11)

- Replace relative links in both PyPI package descriptions with absolute GitHub URLs for the repository, first-release notes, connection guide, documentation, contributor guide, and original MIT license.
- Reject relative Markdown links in release artifact descriptions before publishing.
- Correct release validation for Windows CRLF license files and complete the existing combined GitHub release without resending an older workflow commit as its target.
- Keep runtime behavior and PostgreSQL 18 / Python 3.10 minimum versions unchanged.

## 0.1.0 — first fork release (2026-11-11)

Based on upstream [Oxyde](https://github.com/mr-fatalyst/oxyde) at `26f3e9d470134d218ca70051fb48627e90fd6d6e` (upstream Python 0.8.0 / core 0.7.1). This release combines the following additions; overlapping fixes are included once.

### PostgreSQL-focused fork

- Require **PostgreSQL 18 or newer**, checked before a newly connected pool is registered. Accept only `postgres://` and `postgresql://` URLs.
- Remove the SQLite and MySQL drivers, binders, decoders, EXPLAIN implementations, SQL dialects, migration fallbacks, pool settings, and backend dependencies.
- Use PostgreSQL SQL and native `RETURNING` throughout; remove MySQL's emulated update-returning path and SQLite table rebuilds.
- Package the fork as **oxyde-postgres 0.1.0** and **oxyde-postgres-core 0.1.0**, retaining the `oxyde` imports, `_oxyde_core` native module, and `oxyde` CLI.
- Move smoke, integration, migration, lifecycle, and driver tests to disposable PostgreSQL 18 databases/schemas. CI targets PostgreSQL 18 and Python 3.10/3.15.
- Update documentation, examples, metadata, and release workflows for this fork, preserving upstream authorship and licenses.
- Disable Cargo symbol stripping in development and release profiles to avoid unloadable macOS native modules (`mis-aligned LINKEDIT string pool`) with the pinned Rust toolchain. Verify native wheel imports on macOS ARM64 and Windows in the release workflow.
- Put native package metadata at the workspace root and explicitly include the patched SQLx sources and licenses in source releases, so source installs preserve the driver fixes.

### Transaction nesting depth

Source: [upstream PR #52](https://github.com/mr-fatalyst/oxyde/pull/52).

- Expose `get_transaction_depth(using="default")` through `oxyde.db` and `oxyde.db.transaction`: 0 outside `atomic()`, 1 in the outer block, and 2+ for nested savepoints.
- Keep depth independent per database alias and owning task; child tasks and worker threads do not inherit their parent's reported depth. Standalone `AsyncTransaction` usage is excluded.
- Cover rollback, failed operations, nesting, aliases, child tasks, threads, and calls without a running event loop.

### Array, JSON, decimal, and typed-filter fixes

Source: [upstream PR #53 / array-field-fixes](https://github.com/mr-fatalyst/oxyde/pull/53). The smallint fix also incorporates the behavior and regression coverage from [oxyde-core PR #1](https://github.com/rclayer/oxyde-core/pull/1).

- Decode `SMALLINT` and `SMALLINT[]` as Python integers, including negative values, 16-bit boundaries, nulls, empty arrays, and raw scalar queries.
- Carry column `db_type` overrides through selects, joins, updates, and deletes so array comparisons use the actual column type, such as `INTEGER[]` instead of `BIGINT[]`.
- Strip size/precision modifiers from filter casts to prevent truncation or rounding into a match. Validate type identifiers and reject unreadable overrides; preserve `FLOAT(24)`/`REAL` range semantics and non-finite floats.
- Treat out-of-range array equality operands as nonmatches, preserve NULL semantics, omit impossible `IN` operands, and reject unsupported ordering operands.
- Bind and decode `list[bytes]` as `BYTEA[]`.
- Normalize mixed aware/naive datetime array elements through UTC to the field's timezone flavor.
- Bind exponent-form decimals numerically, including decimal arrays.
- Preserve each JSON array element's own JSON type: strings remain strings even when they contain JSON-looking text, and numeric/boolean elements remain numeric/boolean.
- Coerce integer elements in float and decimal array filters to the declared element type. Reject incompatible filter elements instead of silently binding them as NULL.
- Apply string lookups to the decoded string inside JSON fields, preserving document equality behavior for `exact`.
- Resolve joined-column types only from their own qualified metadata; aggregate aliases do not inherit unrelated same-named column types.
- Bind `HAVING` values by aggregate result type, including decimals, dates, datetimes, UUIDs, and enums; preserve column casts for `Min`/`Max`.
- Serialize tuple filter operands recursively, including UUID, Decimal, and datetime values.

### Lookup parsing

Source: [lookup-parsing](https://github.com/ebrake/oxyde/tree/lookup-parsing).

- Resolve direct and related filter keys in one place and validate lookups consistently.
- Match `having()` keys against annotation aliases before splitting lookup suffixes, including aliases that end in lookup names.
- Report unsupported aggregate lookups against the correct alias.

### Array and JSON containment lookups

Source: [array-lookups-v2](https://github.com/ebrake/oxyde/commit/be855e8b6c693c67c1a408614a1564029ff4f3c4).

- Add `contains`, `contained_by`, and `overlap` for typed arrays and top-level JSON arrays.
- Use native PostgreSQL `@>`, `<@`, and `&&` with operands cast to the column's array type. Compare NULL elements explicitly; ignore order and duplicates in the outer array.
- Compare JSON elements as complete JSONB values, supporting objects and nested JSON arrays. Cast plain JSON columns when necessary and preserve SQL NULL/non-array behavior.
- Validate operands by element type, including exact decimal representability and array range constraints.
- Keep string `contains` as a string lookup and preserve related fields named `overlap` or `contained_by` when those names are not valid lookups on the preceding field.
- Generate typed filter stubs with the new lookups' operand types.

### Python 3.15 wheels

Source: [upstream PR #54](https://github.com/mr-fatalyst/oxyde/pull/54).

- Upgrade PyO3 and pyo3-async-runtimes to 0.30 and adapt the renamed bridge APIs.
- Add CPython 3.15 to wheel builds, package classifiers, and CI while retaining Python 3.10 as the minimum. Use macOS 15 runners for macOS wheel builds.

### Cache-disabled PostgreSQL execution and custom types

Source: [oxyde-core PR #2](https://github.com/rclayer/oxyde-core/pull/2), including its final custom-result correction at `58e7ae4efb2b28c9dfa31516b1d93d4bd0581399`.

- Vendor SQLx PostgreSQL 0.8.6 and retain its license files and source-preservation hygiene exclusions.
- When `pg_statement_cache_capacity=0`, send unnamed Parse → Bind → Describe portal → Execute → Close → Sync in one round trip, avoiding stale named plans across DDL and PgBouncer transaction pooling.
- Resolve cold custom result OIDs after the original `ReadyForQuery`, using the same checked-out connection and recursive type cache. Native enum arrays, composites, NULL/empty arrays, and uncommitted types remain decodable.
- Stage only results with unresolved custom OIDs; built-in and cache-warm results continue streaming. Cached queries, explicit preparation, and simple-query behavior retain their existing execution paths.
- Never replay the original statement during metadata resolution, including after an autocommit write. A metadata failure can therefore be reported after a write has committed; callers must not blindly retry writes.
- Add regressions for wire message ordering, DDL, custom types, transaction visibility, cancellation, and catalog failure after `INSERT RETURNING` without duplicate writes.

Cold custom-type results buffer until execution completes, which can increase latency and memory use for large result sets. See [connection settings](https://github.com/ebrake/oxyde-postgres/blob/main/docs/guide/connections.md).

### Coordinated publishing

- Publish both PyPI projects at the same version through their dedicated trusted-publishing workflows.
- Use one GitHub `v0.1.0` release containing the Python package, native core wheels for every supported platform, and both source distributions. Publish the core first, then the dependent Python package, and make the combined GitHub release public when both complete.

### License preservation

- Preserve upstream Oxyde’s complete MIT license and original `Copyright (c) 2025 Nikita Ryzhenkov` notice unchanged, with author credit in package metadata and the README.
- Include the original MIT text in both Python wheels and both source distributions, and include the vendored SQLx MIT/Apache notices in native wheels as well as native source distributions.
- Check release artifacts for the complete license texts, copyright notice, author credit, package identity, and version before publishing.

### Initial validation

- Python 3.13: 1,226 tests passed, including all 47 generated-stub type-check tests; two PostgreSQL fixture/feature cases are skipped.
- Rust: 173 workspace tests and 16 explicitly enabled PostgreSQL driver tests passed. The latter include PostgreSQL 18 operation, pre-18 rejection, failed-overwrite preservation, and all seven unnamed-statement regressions.
- All repository pre-commit checks pass, including Ruff, mypy, Rust formatting, cargo check, Clippy, and file checks. Documentation builds in strict mode.
- Built both Python distributions and a CPython 3.15 release wheel from the extracted source archive; installed the wheels in a fresh Python 3.15 environment and verified a PostgreSQL array round trip.
- Python LSP diagnostics were checked against the upstream checkout; existing typing/native-extension diagnostics remain. Rust LSP diagnostics were unavailable on this host; Cargo compilation and Clippy passed.
