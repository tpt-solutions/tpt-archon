# AGENTS.md — tpt-archon

Rust Cargo workspace, four crates under `crates/`, strictly layered
(`tpt-archon-core` → `tpt-archon-bridge` → `tpt-archon-kernel` →
`tpt-archon-relational`; a crate may only depend on crates below it). The
full task breakdown lives in `TODO.md`.

## Build & verify (these are the CI gates)
- `cargo fmt --all -- --check` — format gate.
- `cargo clippy --workspace --all-targets -- -D warnings` — warnings denied.
- `cargo test --workspace` — all tests.
- `cargo test -p tpt-archon-core` or `cargo test -p tpt-archon-core <name>` — single crate / single test.
- `cargo build -p tpt-archon-core --no-default-features` — must build `no_std` clean; don't let a `std`-only helper leak into the default feature set.
- `cargo publish -p tpt-archon-core --dry-run --allow-dirty` — crates.io packaging check. Only `tpt-archon-core` has zero workspace path-deps, so it's the only crate this can validate before its siblings are actually published (see `.github/workflows/release.yml`).

CI sets `RUSTFLAGS=-D warnings`, so keep the build warning-clean locally too.

## Architecture (dependency order, matches `crates/` build order)
`tpt-archon-core` (`no_std`, zero-alloc: `block`/`page`/`wal`/`btree`) →
`tpt-archon-bridge` (capability IPC + unified page cache traits, glues core to
the kernel) → `tpt-archon-kernel` (async scheduler, IPC, memory management,
user-space drivers) → `tpt-archon-relational` (SQL parser, planner, vectorized
executor, MVCC, GPU acceleration).

## Crate ownership
- `tpt-archon-core` — `BlockDevice` trait + backends (`InMemoryBlockDevice`,
  file-backed), page manager/buffer pool, WAL (LSN-ordered), B-Link tree.
  `#![no_std]`; a `std` feature gates the file-backed `BlockDevice`.
- `tpt-archon-bridge` — capability-based IPC types, unified page cache trait
  definitions shared between storage and kernel. Depends on `tpt-archon-core`.
- `tpt-archon-kernel` — async task scheduler, IPC message passing, user-space
  driver framework. Depends on `tpt-archon-core` + `tpt-archon-bridge`.
- `tpt-archon-relational` — SQL parser/planner/executor, MVCC, GPU
  acceleration via the `tpt-gpu-ir-spec` emitter (feature-gated, emits TPTIR
  for an external GPU backend; not a runtime). Depends on all three crates
  below it.

## External TPT ecosystem dependencies
These are verification/tooling deps, **not** runtime deps. None of them are
pulled into the shippable crates (which must stay `cargo publish`-dry-run
clean — crates.io rejects git deps even in dev-deps). They live exclusively in
the non-published `crates/out-archon-verify` harness.
- `tpt-eidos-verifier` — QF_LRA decision procedure; proves the B-Link tree
  node-capacity invariant (node fits the page). The bare `tpt-eidos` repo holds
  it (there is **no** `tpt-eidos-kernel` crate).
- `tpt-telos-parser` / `tpt-telos-ir` / `tpt-telos-verifier` — formal
  verification of the WAL replay and MVCC serializability invariants. Pulled
  from the `tpt-telos` repo. There is **no** standalone `tpt-telos` crate; the
  three sub-crates above are the package names.
- `tpt-gpu-ir-spec` — the TPTIR dialect **emitter** (lowers an IR region to
  stable TPTIR text). Used only to emit a vectorized top-k scan for an external
  GPU backend; it is **not** a runtime and does not execute anything. There is
  **no** `tpt-gpu-primitives` or `tpt-gpu-runtime` crate — they don't exist
  anywhere in the ecosystem.
- There is **no** `tpt-zero-bytes` crate — it doesn't exist anywhere in the
  ecosystem. Zero-allocation primitives are implemented inline in
  `tpt-archon-core`; do not add a dependency on a crate named that.

## Testing conventions
Every crate has unit tests colocated in `src/`. Add integration tests under
`tests/` once a crate's public surface stabilizes. `benches/` holds Criterion
benchmarks comparing against SQLite/PostgreSQL/pgvector per the success
metrics in `spec.txt`.
