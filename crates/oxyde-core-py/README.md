# Oxyde for PostgreSQL — Rust core

This crate builds `oxyde._oxyde_core`, the native extension bundled in [oxyde-postgres](https://github.com/ebrake/oxyde-postgres). It requires PostgreSQL 18+ and Python 3.15+.

It provides PostgreSQL SQL generation, typed parameter binding, row decoding, connection pools, transactions, and migration support.

The core includes the vendored SQLx PostgreSQL driver with cache-disabled unnamed statements and deferred custom-type metadata resolution. See the [first-release notes](https://github.com/ebrake/oxyde-postgres/blob/main/docs/changelog.md) and [connection guide](https://github.com/ebrake/oxyde-postgres/blob/main/docs/guide/connections.md).

Based on [Oxyde](https://github.com/mr-fatalyst/oxyde). MIT licensed; SQLx retains its MIT/Apache-2.0 licenses.
