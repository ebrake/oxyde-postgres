# Oxyde for PostgreSQL — Rust core

`oxyde-postgres-core` is the native Rust extension for [Oxyde for PostgreSQL](https://github.com/ebrake/oxyde-postgres), requiring PostgreSQL 18+ and Python 3.13+.

It provides PostgreSQL SQL generation, typed parameter binding, row decoding, connection pools, transactions, and migration support through the `_oxyde_core` module. Install `oxyde-postgres` for the Python ORM API.

The core includes the vendored SQLx PostgreSQL driver with cache-disabled unnamed statements and deferred custom-type metadata resolution. See the [first-release notes](https://github.com/ebrake/oxyde-postgres/blob/main/docs/changelog.md) and [connection guide](https://github.com/ebrake/oxyde-postgres/blob/main/docs/guide/connections.md).

Based on [Oxyde](https://github.com/mr-fatalyst/oxyde). MIT licensed; SQLx retains its MIT/Apache-2.0 licenses.
