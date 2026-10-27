.DEFAULT_GOAL := all

CARGO ?= cargo
CLIPPY_DENY := -D warnings -D clippy::pedantic -D clippy::nursery
RUSTDOCFLAGS := -D warnings

.PHONY: all check fmt fmt-check typecheck lint test doc build release clean help

# Build the CLI and run the full deterministic quality gate.
# rust-toolchain.toml pins the formatter toolchain.
all: build check release

check: fmt-check typecheck lint test doc

fmt:
	$(CARGO) fmt --all

fmt-check:
	$(CARGO) fmt --all -- --check

typecheck:
	$(CARGO) check --locked --all-targets --all-features

lint:
	$(CARGO) clippy --locked --all-targets --all-features -- $(CLIPPY_DENY)

test:
	$(CARGO) test -q --locked --all-targets --all-features
	$(CARGO) test -q --locked --doc --all-features

doc:
	RUSTDOCFLAGS="$(RUSTDOCFLAGS)" $(CARGO) doc --locked --all-features --no-deps

build:
	$(CARGO) build --locked --all-targets --all-features

release:
	$(CARGO) build --locked --release --all-features

clean:
	$(CARGO) clean

help:
	@printf '%s\n' \
	  'all       Run full quality gate (default)' \
	  'check     Run formatting, typecheck, lint, tests, and docs' \
	  'fmt       Apply pinned rustfmt formatting' \
	  'fmt-check Verify formatting without changing files' \
	  'typecheck Run cargo check for all targets and features' \
	  'lint      Run strict Clippy checks' \
	  'test      Run all tests, including doctests' \
	  'doc       Build docs with rustdoc warnings denied' \
	  'build     Build all targets and features' \
	  'release   Build optimized host binary' \
	  'help      Show this help message' \
	  'clean     Remove Cargo build artifacts'
