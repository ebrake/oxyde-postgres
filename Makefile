.PHONY: test test-unit test-integration test-smoke test-rust coverage lint lint-rust format build-core build-core-dev

# The pinned toolchain (rust-toolchain.toml) and the project venv are not on a
# default PATH (rustup from Homebrew has no shims). Two things are needed:
# - PATH for the processes the recipes start (cargo finds rustc/clippy, maturin
#   finds cargo and python);
# - absolute tool paths: make looks a simple recipe command up in the PATH it
#   started with, not in the PATH exported below.
RUST_CHANNEL := $(shell sed -n 's/^channel *= *"\(.*\)"/\1/p' rust-toolchain.toml)
RUST_TOOLCHAIN_BIN := $(firstword $(wildcard $(HOME)/.rustup/toolchains/$(RUST_CHANNEL)-*/bin))
VENV_BIN := $(CURDIR)/.venv/bin
export PATH := $(if $(RUST_TOOLCHAIN_BIN),$(RUST_TOOLCHAIN_BIN):)$(VENV_BIN):$(PATH)

CARGO := $(or $(wildcard $(RUST_TOOLCHAIN_BIN)/cargo),cargo)
MATURIN := $(or $(wildcard $(VENV_BIN)/maturin),maturin)
PYTEST := $(or $(wildcard $(VENV_BIN)/pytest),pytest)

PYTHON_DIR = python
TEST_DIR = $(PYTHON_DIR)/oxyde/tests
COV_PKG = $(PYTHON_DIR)/oxyde

test:
	$(PYTEST) $(TEST_DIR)

test-unit:
	$(PYTEST) $(TEST_DIR)/unit

test-integration:
	$(PYTEST) $(TEST_DIR)/integration

test-smoke:
	$(PYTEST) $(TEST_DIR)/smoke

test-rust:
	$(CARGO) test --workspace --no-default-features

coverage:
	$(PYTEST) $(TEST_DIR) --cov=$(COV_PKG) --cov-report=term-missing

lint:
	pre-commit run --all-files

lint-rust:
	$(CARGO) clippy --workspace --all-targets -- -D warnings
	$(CARGO) fmt --all -- --check

format:
	cd $(PYTHON_DIR) && ruff format .
	$(CARGO) fmt --all

build-core:
	cd crates/oxyde-core-py && $(MATURIN) develop --release

build-core-dev:
	$(MATURIN) develop --manifest-path crates/oxyde-core-py/Cargo.toml
