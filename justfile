# Convenience recipes wrapping the commands documented in AGENTS.md.
# Requires `just` (https://github.com/casey/just); optional — every recipe
# is just a thin wrapper around a plain `cargo` invocation, so contributors
# without `just` installed can always run the underlying command directly.

# Run the full CI gate sequence locally.
check: fmt-check clippy test no-std

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

clippy:
    cargo clippy --workspace --all-targets -- -D warnings

test:
    cargo test --workspace

# Run tests for a single crate, optionally filtered by name.
test-one crate name="":
    cargo test -p {{crate}} {{name}}

# tpt-archon-core must build clean with no default features (no_std).
no-std:
    cargo build -p tpt-archon-core --no-default-features

# Formal-verification harness (excluded from the default workspace).
verify:
    cargo test -p out-archon-verify

# crates.io packaging dry-run for the one crate with zero workspace path-deps.
publish-check:
    cargo publish -p tpt-archon-core --dry-run --allow-dirty
