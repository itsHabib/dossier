.PHONY: check fmt fmt-check lint test build release clean mutants mutants-quick

# `make check` is the single command CI runs and you run before commit.
# Same matrix locally and in CI so failures are reproducible.
check: fmt-check lint test

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

lint:
	cargo clippy --all-targets --all-features -- -D warnings

test:
	cargo test --all-features

build:
	cargo build --all-features

release:
	cargo build --release --all-features

clean:
	cargo clean

# Full mutation audit. Slow (~30-60 min on this size repo).
# Requires: cargo install cargo-mutants
mutants:
	cargo mutants --no-shuffle

# Mutate only files changed vs main. PR-triage variant.
# Uses a temp file (not bash process substitution) so this works under
# /bin/sh aka dash on Ubuntu CI runners.
mutants-quick:
	mkdir -p target
	git diff main..HEAD > target/.mutants-diff
	cargo mutants --in-diff target/.mutants-diff
	rm -f target/.mutants-diff
