# Changelog

All notable changes to this crate are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
### Changed
### Fixed

## [0.1.0] - 2026-08-05

### Added
- Crate scaffold (Phase 3 of tpt-archon): an AI-native relational query engine
  running on the lower `tpt-archon` layers.
- `parser` module — a hand-written, allocation-light recursive-descent parser
  for a PostgreSQL-leaning SQL dialect (`SELECT`/`JOIN`/`GROUP BY`/`ORDER BY`,
  CTEs, DDL/DML, transactions, and the vector-search extension
  `ORDER BY cosine(col, ?) LIMIT k`).
- `planner` module — a small cost-based planner producing a physical `Plan`,
  deciding whether to vectorize a scan and recording a CPU-vs-GPU `Dispatch`.
- `executor` module — a vectorized (batched) execution engine over an in-memory
  `Table`, plus a `vector_topk` brute-force CPU fallback for similarity search.
- `vector_index` module — an `IvfFlatIndex` approximate nearest-neighbor index
  (k-means clustering, `nprobe` probing) built lazily and maintained
  incrementally once a vector column crosses a live-row threshold.
- `database` module — the persistent `Database`: every table's rows live in a
  `tpt-archon-core` B-Link tree, owning DDL and `BEGIN`/`COMMIT`/`ROLLBACK`
  transaction control.
- `mvcc` module — an `MvccStore` with snapshot isolation and optimistic
  validation detecting write-write and read-write conflicts
  (first-committer-wins).
- `explain` module — `EXPLAIN` rendering the physical plan and dispatch
  decision; with the `gpu` feature, also renders the emitted TPTIR for a
  GPU-dispatched scan (emission only, no GPU execution).
- `select_end_to_end` example (parse → plan → execute `SELECT`).
