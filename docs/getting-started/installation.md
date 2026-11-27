# Installation

## Requirements

- PostgreSQL 18+
- Python 3.15+
- Rust 1.100+ (for building from source)

## Install from PyPI

```bash
pip install oxyde-postgres
```

`oxyde-postgres` installs the Python ORM and its Rust extension together.
Import the ORM as `oxyde` and the extension as `oxyde._oxyde_core`. The CLI
command is `oxyde`. Use a separate virtual environment from upstream Oxyde
because both distributions provide the `oxyde` package.

Native wheel builds cover CPython 3.15 and free-threaded CPython 3.15 on
Linux (glibc and musl, x86_64 and ARM64), macOS (Apple Silicon), and
Windows (x86_64). If no compatible wheel is available, installation builds
the core from source and requires Rust.

## Install from Source

If you need to build from source (e.g., for development or customization):

### 1. Clone the Repository

```bash
git clone https://github.com/ebrake/oxyde-postgres.git
cd oxyde-postgres
```

### 2. Create Virtual Environment

```bash
python -m venv .venv
source .venv/bin/activate  # Linux/macOS
# or
.venv\Scripts\activate     # Windows
```

### 3. Build and Install the Package

```bash
# Run from the repository root. rust-toolchain.toml pins the Rust toolchain.
pip install maturin
maturin develop --release
```

## Verify Installation

```python
import oxyde
import oxyde._oxyde_core
print(oxyde.__version__)
```

## Database Drivers

Oxyde uses SQLx under the hood. Database drivers are included — no additional installation required.

| Database | Connection URL |
|----------|----------------|
| PostgreSQL | `postgresql://user:pass@host:5432/db` |
| PostgreSQL | `postgres://user:pass@host:5432/db` |


## Next Steps

- [Quick Start](quickstart.md) — Build your first app in 5 minutes
- [First Project](first-project.md) — Complete tutorial with a real example
- [Expressions](../guide/expressions.md) — Scalar annotations and atomic updates
- [Changelog](../changelog.md) — Fork additions and upgrade notes
