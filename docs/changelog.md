# Changelog

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
- Put **E Brake <ebrake123@icloud.com>** first in both distribution author lists, retaining Nikita Ryzhenkov's author credit and the complete original Oxyde MIT license. Preserve SQLx's current license texts and its original LaunchBadge copyright notices.
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
