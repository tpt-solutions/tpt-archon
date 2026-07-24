# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Workspace bootstrap: `tpt-archon-core`, `tpt-archon-bridge`, `tpt-archon-kernel`,
  `tpt-archon-relational` crate scaffolds.
- `tpt-archon-core`: `BlockDevice` trait and `InMemoryBlockDevice`.
- `tpt-archon-core`: `FileBlockDevice` (file-backed, `std`-feature gated); expanded
  `StorageError` (out-of-bounds, short read/write, I/O, sync, all-frames-pinned).
- `tpt-archon-core`: `zerocopy` module — `FixedBuf`, `Cursor`/`Reader` zero-allocation
  primitives (replacing the never-built `tpt-zero-bytes`).
- `tpt-archon-core`: `page` module — `Page`, `BufferPool` with Free/Clean/Dirty/Pinned
  state machine and LRU eviction with dirty-page writeback.
- `tpt-archon-core`: `wal` module — append-only, LSN-ordered, CRC32-framed write-ahead
  log with crash-recovery replay (torn-tail truncation).
- `tpt-archon-core`: `btree` module — Lehman & Yao B-Link tree with point lookups,
  range scans, node-splitting inserts, and a compile-time node-fits-page check.
- `tpt-archon-core`: `storage_tour` example.
- `tpt-archon-bridge`: `capability` (unforgeable, revocable capabilities) and
  `page_cache` (`UnifiedPageCache` trait + `CorePageCache` adapter with zero-copy
  integration test).
- `tpt-archon-kernel`: cooperative `scheduler`, capability-bearing `ipc`, and
  `memory` (unified page cache as buffer pool). User-space-first.
- `tpt-archon-relational`: `parser` (PostgreSQL-leaning SELECT subset), `planner`
  (cost-based, vectorization + CPU/GPU dispatch), vectorized `executor`
  (+ `vector_topk` CPU fallback), and snapshot-isolation `mvcc` with conflict
  detection. GPU support is opt-in behind the `gpu` feature.
- `tpt-archon-relational`: `select_end_to_end` example.
- `benches/`: Criterion benchmark scaffold (excluded from the main workspace) for
  storage and query hot paths and vector search.
- Docs: ADR 0002 (zero-allocation primitives in-crate) and ADR 0003 (verification
  tested-now-proven-later); `formal-proofs/README.md` tracking intended proof targets.

### Changed
- `out-archon-verify` (formerly `tpt-archon-verify`; renamed to carry the
  `out-archon-` not-published-crate prefix) moved out of the default workspace
  (`exclude`) so
  `cargo test --workspace` is offline-clean; it now runs in an opt-in CI job
  with network access for its git-hosted ecosystem verifiers.
- `README.md` §"TPT ecosystem dependencies" corrected to match `AGENTS.md`:
  no `tpt-gpu-primitives`/`tpt-gpu-runtime`; `tpt-gpu-ir-spec` is an IR emitter
  (no runtime). `formal-proofs/README.md` now states the `.telos` artifacts are
  QF_LRA solver-checked assertion harnesses, not machine-checked Coq/Lean proofs.

### Added
- `tpt-archon-core`: `storage` module — `StorageEngine` facade wiring `BufferPool`
  + `Wal` with write-ahead (WAL record before page) and `recover()` replaying
  committed page images after a crash; plus a file-backed `Database::open`/`create`
  convenience (`std` feature).
- `tpt-archon-core`: B-Link property tests forcing ≥2 interior levels (sequential /
  reverse / shuffled insert orders) with full-key lookup assertions.
- `tpt-archon-relational`: `explain` module — `explain_plan` (always) renders the
  physical plan + dispatch; `explain_gpu` (gated on the `gpu` feature) prints the
  emitted TPTIR for a GPU-dispatched scan.
