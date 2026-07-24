# Benchmarks

Criterion benchmarks for `tpt-archon-core` and `tpt-archon-relational`.

## Running

```bash
# From the repo root
cd benches
cargo bench
```

Or from the workspace root:

```bash
cargo bench --bench storage     # B-Link tree + WAL micro-benchmarks
cargo bench --bench query       # SQL parse/plan/execute pipeline
cargo bench --bench vector_compare  # Vector search (Archon only)
```

## Benchmark Targets

### `storage` (`benches/storage.rs`)
- `btree_insert_10k` — B-Link tree bulk insert throughput
- `btree_lookup_hit` — point lookup latency
- `wal_append_1k` — WAL append throughput

### `query` (`benches/query.rs`)
- `select_filter_project_100k` — full parse-plan-execute pipeline on 100k rows
- `parse_plan` — parse + plan phase only (no execution)
- `vector_topk_10k_dim128` — vector search over 10k 128-dim embeddings

### `vector_compare` (`benches/vector_compare.rs`)
Compares Archon's exact brute-force `vector_topk`, Archon's approximate
`IvfFlatIndex` (see `tpt-archon-relational::vector_index`), and a plain
brute-force baseline against external databases. Embeddings are generated
with a deterministic xorshift64* PRNG (not a low-cardinality modular
formula) so cluster/recall behavior is representative of real embeddings —
see `TODO.md` §5.5 for why that distinction mattered here:

```bash
# Archon only (always runs)
cargo bench --bench vector_compare

# + pgvector comparison (requires a running PostgreSQL with pgvector)
cargo bench --bench vector_compare --features pgvector-bench
POSTGRES_URL="postgres://user:pass@localhost:5432/dbname" cargo bench --bench vector_compare --features pgvector-bench

# + SQLite comparison
cargo bench --bench vector_compare --features sqlite-bench
SQLITE_PATH="/tmp/test.db" cargo bench --bench vector_compare --features sqlite-bench
```

The pgvector and SQLite comparisons are feature-gated and require external database instances.
