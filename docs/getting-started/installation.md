# Installation

## Requirements

- PostgreSQL 18+
- Python 3.10+
- Rust 1.88+ (for building from source)

## Install from PyPI

```bash
pip install oxyde-postgres
```

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

### 3. Build Rust Core

```bash
# Build the Rust workspace
cargo build --release

# Install the Python extension (PyO3 bindings)
cd crates/oxyde-core-py
pip install maturin
maturin develop --release
```

### 4. Install Python Package

```bash
cd ../../python
pip install -e .
```

## Verify Installation

```python
import oxyde
print(oxyde.__version__)
```

## Database Drivers

Oxyde uses SQLx under the hood. Database drivers are included — no additional installation required.

| Database | Connection URL |
|----------|----------------|
| PostgreSQL | `postgresql://user:pass@host:5432/db` |


## Next Steps

- [Quick Start](quickstart.md) — Build your first app in 5 minutes
- [First Project](first-project.md) — Complete tutorial with a real example
