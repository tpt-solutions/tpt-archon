# tpt-archon-relational

Phase 3 of [tpt-archon](https://github.com/tpt-solutions/tpt-archon): an
AI-native relational query engine running on the lower `tpt-archon` layers.

## Modules

- [`parser`](src/parser.rs) — a hand-written, allocation-light recursive-descent
  parser for a PostgreSQL-leaning SQL dialect: `SELECT` (`WHERE` with
  `AND`/`OR`/`NOT`, `BETWEEN`, `IN`, `LIKE`, `IS [NOT] NULL`, subqueries),
  `JOIN`, `GROUP BY`/`HAVING`, `ORDER BY`, CTEs (`WITH`, including
  correlated/recursive-reference checks), `CREATE`/`ALTER`/`DROP TABLE`,
  `CREATE`/`DROP VIEW`, `INSERT`/`UPDATE`/`DELETE`, `BEGIN`/`COMMIT`/`ROLLBACK`,
  and the vector-search extension `ORDER BY cosine(col, ?) LIMIT k`.
- [`planner`](src/planner.rs) — a small cost-based planner producing a physical
  `Plan`. It estimates rows from `TableStats`, decides whether to vectorize a
  scan, and records a CPU-vs-GPU `Dispatch` decision.
- [`executor`](src/executor.rs) — a vectorized (batched) execution engine over
  an in-memory `Table` (joins, aggregates, subqueries), plus a `vector_topk`
  brute-force CPU fallback for similarity search.
- [`vector_index`](src/vector_index.rs) — an `IvfFlatIndex` approximate
  nearest-neighbor index (k-means clustering, `nprobe`-cluster probing) that
  `Database` builds lazily once a vector column crosses
  `vector_index::MIN_ROWS_FOR_INDEX` live rows, then maintains incrementally —
  closes the gap `vector_topk`'s brute-force scan has against pgvector at
  scale (see `benches/README.md` and `TODO.md` §5.5 for measured numbers).
- [`database`](src/database.rs) — the persistent `Database`: every table's
  rows live in a `tpt-archon-core` B-Link tree (not just in-memory), so
  `INSERT`/`UPDATE`/`DELETE`/`SELECT` exercise the full storage stack. Also
  owns DDL (`CREATE`/`ALTER`/`DROP TABLE`, views), and `BEGIN`/`COMMIT`/
  `ROLLBACK` transaction control backed by `mvcc` (one `MvccStore` per table).
- [`mvcc`](src/mvcc.rs) — an `MvccStore` with snapshot isolation and optimistic
  validation that detects write-write and read-write conflicts
  (first-committer-wins).
- [`explain`](src/explain.rs) — `EXPLAIN` support: renders a query's physical
  plan and dispatch decision; with the `gpu` feature, also renders the TPTIR
  text a `Dispatch::Gpu` plan would emit (emission only, no GPU execution).

## Features

- `std` (default) — forwards to the lower crates' `std` features.
- `gpu` — opt-in GPU acceleration. **The engine has a full CPU-only fallback**;
  GPU is never forced on consumers. The `tpt-gpu-*` integration behind this flag
  is a later milestone (see the repo-root `TODO.md`, Phase 3).

## Example

```bash
cargo run -p tpt-archon-relational --example select_end_to_end
```

See [`examples/select_end_to_end.rs`](examples/select_end_to_end.rs) for a full
parse → plan → execute `SELECT`.

## Publishing note

All three internal dependencies are path dependencies during development.
Switch them to version requirements before publishing.

## License

Licensed under either of [MIT](../../LICENSE-MIT) or
[Apache-2.0](../../LICENSE-APACHE) at your option.
