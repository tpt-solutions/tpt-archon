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
- `tpt-archon-kernel` — async task scheduler, IPC message passing, memory
  management (unified page cache), capability-based access control. Depends on
  `tpt-archon-core` + `tpt-archon-bridge`. The user-space *driver framework*
  (interrupt→IPC translation) is **deferred** — all kernel work is user-space-
  first by construction (see `TODO.md` Phase 2b).
- `tpt-archon-relational` — SQL parser/planner/executor, MVCC, GPU
  acceleration via the `tpt-gpu-ir-spec` emitter (feature-gated, emits TPTIR
  for an external GPU backend; not a runtime). Depends on all three crates
  below it.

## Non-published `out-archon-*` crates
Crates prefixed `out-archon-` are never published (dev/demo/verification
tooling); publish intent is visible from the name. The 4 `tpt-archon-*` crates
above are the shippable ones. Workspace membership (root `Cargo.toml`):
- `out-archon-sql` — the `archon-sql` REPL binary (workspace member).
- `out-archon-wasm` — wasm-bindgen browser playground (workspace member;
  builds on the host target, real `wasm32-unknown-unknown` + `wasm-pack` build
  per its README).
- `out-archon-pgwire` — PostgreSQL wire-protocol server (workspace member; the
  `pgwire_slt` integration test is `#[ignore]` unless `PGWIRE_SLT_TEST` is set).
- `out-archon-verify` — formal-verification harness exercising the live
  ecosystem verifiers (workspace member since Phase 9; was `exclude`d before the
  ecosystem crates were published to crates.io, see `TODO.md` Phase 9).
- `out-archon-py` / `out-archon-node` — PyO3 / napi-rs language bindings,
  `exclude`d (need Python/Node tooling; own CI jobs `python.yml` / `node.yml`).
- `out-archon-pgcompat` — real-Postgres oracle for the `.slt` corpus (Phase 8
  / Track C), `exclude`d (needs a live PostgreSQL; own `pg-compat` CI job).

## External TPT ecosystem dependencies
These are verification/tooling deps, **not** runtime deps. None of them are
pulled into the shippable crates. They now live in the `crates/out-archon-verify`
harness (a regular workspace member since Phase 9) as ordinary published
crates.io version requirements — `tpt-eidos-verifier` 0.2.0,
`tpt-telos-verifier`/`tpt-telos-ir`/`tpt-telos-parser` 0.1.1,
`tpt-gpu-ir-spec` 0.1.0 (all five published; see `TODO.md` Phase 9).
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

### PostgreSQL-compatibility corpus (Phase 8 / Track C)
- `crates/tpt-archon-relational/tests/slt.rs` runs the `.slt` corpus
  (`tests/slt/supported/` **and** `tests/slt/divergent/`) against `Database`
  directly — a normal `cargo test --workspace` integration test, zero new deps,
  offline-clean. `supported/` asserts behaviors validated against real Postgres;
  `divergent/` asserts Archon's *own* (documented-divergent) behavior stays
  stable. Run with `cargo test -p tpt-archon-relational slt_corpus`.
- `crates/out-archon-pgcompat` is the real-Postgres oracle half: it runs the
  same `supported/*.slt` against a live PostgreSQL (`pgvector/pgvector:pg16`,
  `--locale=C` per `docker-compose.yml`) and asserts Postgres agrees with the
  corpus's expected rows. Skipped unless `POSTGRES_URL` is set; runs in CI's
  opt-in `pg-compat` job. It deliberately skips `divergent/` (Postgres is not
  expected to match a documented Archon divergence).
- When a `divergent/` case is fixed, move its test to `supported/` (do **not**
  edit the divergent assertion in place) and delete its entry from
  `docs/POSTGRES_COMPATIBILITY.md` — the corpus is the source of truth, the doc
  is its human-readable mirror.
- `docs/POSTGRES_COMPATIBILITY.md` is the honest, machine-tracked divergence
  catalog generated from `tests/slt/divergent/`; it closes the `spec.txt`
  "PostgreSQL-compatible / drop-in replacement" claim with documented divergences
  rather than rhetoric.
