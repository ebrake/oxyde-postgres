# Changelog

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
