# Changelog

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

Cold custom-type results buffer until execution completes, which can increase latency and memory use for large result sets. See [connection settings](guide/connections.md).

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
