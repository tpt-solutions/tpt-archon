# Getting Started with tpt-archon

This guide takes you from zero to a running query across the four-crate stack,
and then tells you — honestly — what each crate is *not* (yet), per ADR 0003
(verification tested-now-proven-later, no over-promising).

## Prerequisites

- A recent stable Rust toolchain (`cargo`, `rustc`).
- `git` (the repo is a Cargo workspace).

## 1. Build & test the workspace

```sh
git clone https://github.com/tpt-solutions/tpt-archon
cd tpt-archon
cargo build --workspace
cargo test --workspace
```

The CI gates you should keep green locally:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## 2. Run an end-to-end SQL query (in-memory)

The `select_end_to_end` example parses a SQL statement, plans it, and executes
it with the vectorized engine:

```sh
cargo run -p tpt-archon-relational --example select_end_to_end
```

## 3. Open a real file-backed database

`tpt-archon-core` exposes an embeddable `Database` (the "embeddable SQLite"
story) over a file-backed block device, gated behind the default `std`
feature:

```rust
use tpt_archon_core::storage::Database;

let mut db = Database::create("demo.bin", 64).unwrap();
let page = vec![0xAB; tpt_archon_core::page::PAGE_SIZE];
db.put(1, &page).unwrap();          // WAL record + page flush + sync
let read = db.get(1).unwrap();
assert_eq!(read[0], 0xAB);
```

## 4. Run a full relational workload (INSERT/SELECT/UPDATE/DELETE)

`tpt-archon-relational::database::Database` stores rows in the core B-Link tree
(no separate buffer pool), supports `INSERT` / `UPDATE` / `DELETE`, and a vector
`f32[]` column with `ORDER BY cosine(...) LIMIT k`:

```rust
use tpt_archon_relational::database::{ColumnType, Database, Schema};
use tpt_archon_relational::parser::parse_statement;

let schema = Schema {
    columns: vec!["id".into(), "name".into(), "age".into()],
    types: vec![ColumnType::Int, ColumnType::Text, ColumnType::Int],
};
let mut db = Database::new(schema);

db.execute(&parse_statement("INSERT INTO t (id,name,age) VALUES (1,'alice',30)").unwrap(), &[]).unwrap();
let r = db.execute(&parse_statement("SELECT id FROM t WHERE age >= 30").unwrap(), &[]).unwrap();
assert_eq!(r.rows.len(), 1);
```

## 5. See capability-scoped multi-tenant isolation

```sh
cargo run -p tpt-archon-bridge --example multi_tenant
```

Two tenants share one unified page cache; each is issued a capability scoped to
its own pages, and cross-tenant access is denied.

## 6. Run the fault simulator

```sh
cargo test -p tpt-archon-core faultsim
```

This injects randomized WAL tail corruptions (truncation, byte flips, zeroing)
and asserts `StorageEngine::recover` always yields a prefix-consistent state.

---

## What each crate IS and is NOT (yet) — ADR 0003 honesty

### `tpt-archon-core`
- **IS:** a `no_std`, zero-allocation storage engine (block device, page manager
  + buffer pool, write-ahead log, B-Link tree), plus a file-backed `Database`.
- **NOT (yet):** a full transactional engine with group commit / checkpoints /
  background compaction; formally *proven* correct beyond the in-repo
  `tpt-telos`/`tpt-eidos` assertion harnesses.

### `tpt-archon-bridge`
- **IS:** capability types (unforgeable, revocable) and the unified page-cache
  trait that lets the kernel and storage share pages with no copy.
- **NOT (yet):** a production IPC transport. Capability *unforgeability* is
  enforced by Rust privacy today, not by the (deferred) `tpt-eidos` dependent
  types. There is no network/process boundary.

### `tpt-archon-kernel`
- **IS:** a user-space, capability-based scheduler + IPC model with a unified
  page cache that *is* the DB buffer pool.
- **NOT (yet):** a bare-metal microkernel. No real `io_uring`, no real `mmap`,
  no hardware device drivers (per the user-space-first risk mitigation in
  `spec.txt` and AGENTS.md). "Microkernel" here means a user-space process
  model first.

### `tpt-archon-relational`
- **IS:** a PostgreSQL-dialect SQL parser, cost-based planner, vectorized
  executor, MVCC, and a CPU `vector_topk` with an optional GPU **TPTIR emitter**.
- **NOT (yet):** GPU-*executing*. The `gpu` feature only emits TPTIR text for an
  external backend; CPU is the real executor. No complex GPU aggregations or ML
  UDFs yet. `EXPLAIN` renders the plan and, on the `gpu` feature, the emitted
  TPTIR.

See [`TODO.md`](TODO.md) for the live, per-phase checklist and what remains
deferred.
