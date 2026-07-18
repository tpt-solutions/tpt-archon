# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

tpt-archon is a vertically integrated storage/kernel/database stack: a
`no_std` zero-allocation storage engine (`tpt-archon-core`), a capability-based
microkernel with a unified page cache (`tpt-archon-bridge` +
`tpt-archon-kernel`), and a GPU-accelerated relational query engine
(`tpt-archon-relational`), built inside-out so each layer is designed around
the one beneath it. See `spec.txt` for the original design doc and `TODO.md`
for what's actually been built so far — this is a large, multi-phase project
under active bootstrap, not a finished system.

## Commands

```
cargo build --workspace                              # build everything
cargo test --workspace                                # run all unit + integration tests
cargo test -p tpt-archon-core                          # test a single crate
cargo build -p tpt-archon-core --no-default-features   # verify no_std build
cargo fmt --all -- --check                             # CI formatting check
cargo clippy --workspace --all-targets -- -D warnings  # CI lint check (matches RUSTFLAGS=-D warnings in CI)
```

## Workspace layout

Four crates under `crates/`, strictly layered — a crate may only depend on
crates below it in this list:

- **tpt-archon-core** — `#![no_std]`, zero-allocation storage engine.
  `block/` (the `BlockDevice` trait + `InMemoryBlockDevice` and a
  `std`-feature-gated file-backed device), `page/` (page manager, LRU
  buffer pool, Free/Clean/Dirty/Pinned states), `wal/` (LSN-ordered
  write-ahead log, crash recovery), `btree/` (concurrent B-Link tree,
  latch-free reads, node capacity enforced at the type level via
  `tpt-eidos`). No external workspace deps.
- **tpt-archon-bridge** — capability-based IPC types (strongly typed,
  unforgeable, revocable) and the unified page cache trait that lets the
  kernel map storage pages directly into a process's address space,
  eliminating double-buffering. Depends on `tpt-archon-core`.
- **tpt-archon-kernel** — async task scheduler (one `Task` per DB
  connection, not a process; `io_uring` backend on Linux), IPC message
  passing, user-space driver framework. Depends on `tpt-archon-core` +
  `tpt-archon-bridge`.
- **tpt-archon-relational** — hand-written SQL parser (PostgreSQL-compatible
  dialect), cost-based planner, vectorized executor, MVCC with serializable
  isolation, GPU acceleration for vector search/aggregations/UDFs via
  `tpt-gpu-primitives`/`tpt-gpu-runtime`. Depends on all three crates above.

### Dependency graph

```
tpt-archon-relational -> tpt-archon-kernel -> tpt-archon-bridge -> tpt-archon-core
```

Enforced by which crates appear in each `Cargo.toml`'s `[dependencies]` — do
not add a reverse-direction dependency.

### TPT ecosystem crates this workspace depends on

- `tpt-eidos-kernel` / `tpt-eidos-verifier` (compile-time invariants) —
  published on crates.io under the `tpt-eidos` project.
- `tpt-telos` / `tpt-telos-verifier` (formal verification of WAL, MVCC,
  scheduler deadlock-freedom) — published on crates.io under the
  `tpt-telos` project.
- `tpt-gpu-primitives` / `tpt-gpu-runtime` (GPU compute) — published on
  crates.io under the `tpt-gpu` project; only `tpt-archon-relational` uses
  these.
- **No `tpt-zero-bytes` crate exists** — the original design doc names it,
  but it was never built anywhere in the TPT ecosystem (confirmed absent
  both locally and on crates.io). `tpt-archon-core` implements its own
  zero-allocation I/O primitives instead of depending on it.
- `tpt-formal-lab` is mentioned only in `spec.txt`'s verification-strategy
  prose, never as an actual crate dependency of any of the four crates — it
  is correctly omitted from every `Cargo.toml`.

## Testing conventions

Unit tests are colocated in `src/` per module. Add `tests/` integration
suites once a crate's public API stabilizes. `benches/` (Criterion) tracks
the performance claims in `spec.txt`'s "Success Metrics" section (30% faster
than PostgreSQL for I/O-bound workloads, 2x SQLite for embedded, 10x pgvector
for vector search) — treat these as benchmarks to validate, not marketing
copy to assume true.

## Publishing

Crates publish to crates.io individually, in dependency order, once each
clears its "crates.io readiness" checklist in `TODO.md` (metadata, docs,
examples, `cargo publish --dry-run`). Publishing is manual
(`.github/workflows/release.yml`, `workflow_dispatch`) — nothing auto-publishes
on tag push yet, since not every crate is ready at the same time.
