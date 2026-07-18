# tpt-archon-relational

Phase 3 of [tpt-archon](https://github.com/tpt-solutions/tpt-archon): an
AI-native relational query engine running on the lower `tpt-archon` layers.

## Modules

- [`parser`](src/parser.rs) — a hand-written, allocation-light recursive-descent
  parser for a PostgreSQL-leaning `SELECT` subset
  (`SELECT cols FROM table [WHERE col op int] [LIMIT n]`), with a zero-copy
  tokenizer.
- [`planner`](src/planner.rs) — a small cost-based planner producing a physical
  `Plan`. It estimates rows from `TableStats`, decides whether to vectorize a
  scan, and records a CPU-vs-GPU `Dispatch` decision.
- [`executor`](src/executor.rs) — a vectorized (batched) execution engine over
  an in-memory `Table`, plus a `vector_topk` CPU fallback for similarity search.
- [`mvcc`](src/mvcc.rs) — an `MvccStore` with snapshot isolation and optimistic
  validation that detects write-write and read-write conflicts
  (first-committer-wins).

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
