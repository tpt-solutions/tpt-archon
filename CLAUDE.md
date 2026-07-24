# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

tpt-archon is a vertically integrated storage/kernel/database stack: a
`no_std` zero-allocation storage engine (`tpt-archon-core`), a capability-based
microkernel with a unified page cache (`tpt-archon-bridge` +
`tpt-archon-kernel`), and a GPU-opt-in relational query engine
(`tpt-archon-relational`), built inside-out so each layer is designed around
the one beneath it. See `spec.txt` for the original design doc and `TODO.md`
for what's actually been built so far (tracked phase-by-phase, in dependency
order) — this is a large, multi-phase project under active bootstrap, not a
finished system.

## Commands

```
cargo build --workspace                                   # build everything
cargo test --workspace                                    # run all unit + integration tests
cargo test -p tpt-archon-core                              # test a single crate
cargo test -p tpt-archon-core <name>                        # run a single test
cargo build -p tpt-archon-core --no-default-features        # verify no_std build
cargo fmt --all -- --check                                 # CI formatting gate
cargo clippy --workspace --all-targets -- -D warnings      # CI lint gate (CI also sets RUSTFLAGS=-D warnings)
cargo publish -p tpt-archon-core --dry-run --allow-dirty   # crates.io packaging check (see note below)
```

Only `tpt-archon-core` has zero workspace path-dependencies, so it's the only
crate `cargo publish --dry-run` can validate before its siblings are actually
published — the others depend on sibling crates via `path`, and cargo needs
to resolve those against the real crates.io index (see
`.github/workflows/release.yml` and `ci.yml`).

## Workspace layout

Six crates under `crates/`, strictly layered — a crate may only depend on
crates below it in this list. Naming convention: crates published to
crates.io are prefixed `tpt-archon-`; crates that are never published
(dev/demo tools, the verification harness) are prefixed `out-archon-`
instead, so publish intent is visible from the crate/directory name alone.

- **tpt-archon-core** — `#![no_std]`, zero-allocation storage engine.
  `block/` (the `BlockDevice` trait + `InMemoryBlockDevice` and a
  `std`-feature-gated file-backed device), `page/` (page manager, LRU
  buffer pool, Free/Clean/Dirty/Pinned states), `wal/` (LSN-ordered
  write-ahead log, crash recovery), `btree/` (concurrent B-Link tree,
  latch-free reads, node capacity enforced via a `const fn` assertion,
  `btree::assert_node_fits_page`). No external workspace deps — this keeps
  it the one crate that can be published first.
- **tpt-archon-bridge** — capability-based IPC types (strongly typed,
  unforgeable, revocable — enforced via Rust privacy today: private serial +
  issuer-only minting) and the unified page cache trait that lets the
  kernel map storage pages directly into a process's address space,
  eliminating double-buffering. Depends on `tpt-archon-core`.
- **tpt-archon-kernel** — async task scheduler (one `Task` per DB
  connection, not a process; `io_uring` backend on Linux), IPC message
  passing, user-space driver framework. Depends on `tpt-archon-core` +
  `tpt-archon-bridge`.
- **tpt-archon-relational** — hand-written SQL parser (PostgreSQL-compatible
  dialect), cost-based planner, vectorized executor, MVCC with serializable
  isolation. GPU support is opt-in and only emits TPTIR (via the
  `tpt-gpu-ir-spec` emitter) for an external GPU backend to consume — it is
  not itself a GPU runtime. Depends on all three crates above.
- **out-archon-sql** — *not published* (`publish = false`); the `archon-sql`
  interactive SQL REPL binary (the package is named `out-archon-sql`, but the
  built executable is still `archon-sql` — that's what users type). A demo/
  adoption tool wrapping `tpt-archon-relational::database::Database`, not one
  of the layered architecture crates. Depends on `tpt-archon-relational`.
- **out-archon-verify** — *not published* (`publish = false`); a verification
  harness that pulls in the git-hosted ecosystem verifiers
  (`tpt-eidos-verifier`, `tpt-telos-*`, `tpt-gpu-ir-spec`) to prove invariants
  (B-Link node-capacity fit, WAL replay consistency, MVCC serializability)
  against `tpt-archon-core`/`tpt-archon-relational`. Kept separate because
  crates.io rejects git dependencies even as dev-dependencies, and this lets
  the shippable crates stay `cargo publish --dry-run` clean (see ADR 0003).

### Dependency graph

```
tpt-archon-relational -> tpt-archon-kernel -> tpt-archon-bridge -> tpt-archon-core
```

`out-archon-sql` depends only on `tpt-archon-relational` (it's a thin REPL
wrapper, not a layer anything else builds on). `out-archon-verify` sits off
to the side, depending on `tpt-archon-core` and (dev-dep)
`tpt-archon-relational` but consumed by nothing. Enforced by which crates
appear in each `Cargo.toml`'s `[dependencies]` — do not add a
reverse-direction dependency.

### TPT ecosystem crates this workspace depends on

These are verification/tooling deps, pulled in only by the non-published
`crates/out-archon-verify` harness (git deps, pinned to exact commits in that
crate's `Cargo.toml` — bump deliberately when an upstream release is
validated). None of them are runtime dependencies of the shippable crates.

- `tpt-eidos-verifier` (from the `tpt-eidos` repo) — QF_LRA decision
  procedure; proves the B-Link tree node-capacity invariant. There is **no**
  `tpt-eidos-kernel` crate.
- `tpt-telos-verifier` / `tpt-telos-ir` / `tpt-telos-parser` (from the
  `tpt-telos` repo) — formal verification of WAL replay and MVCC
  serializability invariants. There is **no** standalone `tpt-telos` crate;
  these three sub-crates are the actual package names.
- `tpt-gpu-ir-spec` (from the `tpt-gpu` repo) — the TPTIR dialect *emitter*
  (lowers an IR region to stable TPTIR text) for a vectorized top-k scan;
  it emits IR for an external backend and does not itself execute anything.
  There is **no** `tpt-gpu-primitives` or `tpt-gpu-runtime` crate — they
  don't exist anywhere in the ecosystem.
- **No `tpt-zero-bytes` crate exists** — the original design doc names it,
  but it was never built anywhere in the TPT ecosystem. `tpt-archon-core`
  implements its own zero-allocation I/O primitives instead of depending on
  it; don't add a dependency on a crate named that.

See ADR `docs/0003-verification-tested-now-proven-later.md` for the full
rationale: invariants are implemented and tested/proven in
`out-archon-verify` + `formal-proofs/` now, since `tpt-eidos`/`tpt-telos`
aren't published crates yet, and crate docs deliberately avoid repeating
`spec.txt`'s "zero CVE / zero silent corruption" claims until real proofs
exist.

## Testing conventions

Unit tests are colocated in `src/` per module. Add `tests/` integration
suites once a crate's public API stabilizes. `benches/` (Criterion, its own
excluded workspace member so it isn't built unless `cargo bench` is run
directly from `benches/`) tracks the performance claims in `spec.txt`'s
"Success Metrics" section (30% faster than PostgreSQL for I/O-bound
workloads, 2x SQLite for embedded, 10x pgvector for vector search) — treat
these as benchmarks to validate, not marketing copy to assume true.

## Publishing

Crates publish to crates.io individually, in dependency order, once each
clears its "crates.io readiness" checklist in `TODO.md` (metadata, docs,
examples, `cargo publish --dry-run`). Publishing is manual
(`.github/workflows/release.yml`, `workflow_dispatch`) — nothing auto-publishes
on tag push yet, since not every crate is ready at the same time.
`out-archon-sql` and `out-archon-verify` are never published
(`publish = false`) — see the crate-naming convention note above.
