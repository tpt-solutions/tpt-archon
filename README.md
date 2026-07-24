# tpt-archon

**tpt-archon** is a vertically integrated, proof-native computing stack that
eliminates the boundaries between storage, operating system, and database.
Built inside-out on Rust's ownership model and formal verification, it unifies
the page cache, kernel memory management, and database buffer pool into a
single, zero-copy address space — see [`spec.txt`](spec.txt) for the full
design document.

## Status

Early but functional across all four crates: each phase's core functionality is
implemented and tested. External verification crates (`tpt-eidos-verifier`,
`tpt-telos-*`, `tpt-gpu-ir-spec`) are wired in via the non-published
`crates/out-archon-verify` harness, not the shippable crates. GPU support is
IR-emission only (no runtime). Nothing here is production-ready; see
[`TODO.md`](TODO.md) for the live checklist and what is deferred (e.g. GPU
device execution, real `io_uring`/`mmap` backends, publishing).

| Phase | Crate | Purpose | Status |
|---|---|---|---|
| 1 | [`tpt-archon-core`](crates/tpt-archon-core) | `no_std`, zero-allocation storage engine (block device, page manager, WAL, B-Link tree) | Implemented |
| 2 | [`tpt-archon-bridge`](crates/tpt-archon-bridge) | Zero-copy IPC & unified page cache traits between storage and kernel | Implemented |
| 2 | [`tpt-archon-kernel`](crates/tpt-archon-kernel) | Capability-based microkernel (user-space first) with unified page cache | Implemented |
| 3 | [`tpt-archon-relational`](crates/tpt-archon-relational) | AI-native SQL query engine (GPU opt-in, CPU fallback) | Implemented |

The dependency graph is strict and one-directional:

```
tpt-archon-relational
    ↓
tpt-archon-kernel
    ↓
tpt-archon-bridge
    ↓
tpt-archon-core
```

## TPT ecosystem dependencies

Archon builds on sibling TPT Solutions crates rather than reimplementing
verification tooling from scratch:

- [`tpt-eidos`](https://github.com/tpt-solutions/tpt-eidos) (bare repo; the
  `tpt-eidos-verifier` sub-crate) — QF_LRA solver used to prove the B-Link
  node-capacity / page-fit invariant.
- [`tpt-telos`](https://github.com/tpt-solutions/tpt-telos) (`tpt-telos-verifier`
  / `tpt-telos-ir` / `tpt-telos-parser` sub-crates) — formal verification of
  critical invariants (WAL crash consistency, MVCC serializability, scheduler
  deadlock-freedom). There is no standalone `tpt-telos` crate; those three
  sub-crates are the package names.
- [`tpt-gpu`](https://github.com/tpt-solutions/tpt-gpu) (the `tpt-gpu-ir-spec`
  sub-crate) — a TPTIR dialect **emitter** (lowers an IR region to stable TPTIR
  text). It is **not** a runtime and does not execute anything. There is **no**
  `tpt-gpu-primitives` or `tpt-gpu-runtime` crate — they do not exist anywhere
  in the ecosystem. The GPU path in `tpt-archon-relational` only emits TPTIR
  for an external GPU backend; the CPU `vector_topk` remains the real executor.

These are verification/tooling deps, **not** runtime deps. None of them are
pulled into the shippable crates (which must stay `cargo publish`-dry-run
clean — crates.io rejects git deps even in dev-deps). They live exclusively in
the non-published `crates/out-archon-verify` harness. See AGENTS.md and ADR
0003.

There is no `tpt-zero-bytes` crate (referenced in the original design doc but
never built anywhere in the ecosystem); the zero-allocation I/O primitives
`tpt-archon-core` needs are implemented directly in that crate instead.

## Quick start

The fastest way to try Archon is the interactive SQL shell:

```sh
cargo run -p out-archon-sql
```

This drops you into a REPL where you can create tables, insert data, and run
queries:

```sql
CREATE TABLE users (id INT, name TEXT, age INT);
INSERT INTO users (id, name, age) VALUES (1, 'alice', 30);
INSERT INTO users (id, name, age) VALUES (2, 'bob', 25);
SELECT name, age FROM users WHERE age >= 25 ORDER BY age;
```

Or run a single statement non-interactively:

```sh
cargo run -p out-archon-sql -e "SELECT 1 + 2;"
```

**Which crate should I use?**

| What you want | Crate | Example |
|---|---|---|
| Embed a database in your app | `tpt-archon-relational` | [`examples/select_end_to_end.rs`](crates/tpt-archon-relational/examples/select_end_to_end.rs) |
| Use the storage engine directly | `tpt-archon-core` | [`examples/storage_tour.rs`](crates/tpt-archon-core/examples/storage_tour.rs) |
| Run SQL interactively | `archon-sql` (package `out-archon-sql`, not published) | `cargo run -p out-archon-sql` |
| Scaffold a new project | `template/` | `cargo generate --path template` |
| Formal-verification harness | `out-archon-verify` (not published) | `cargo test -p out-archon-verify` |

## Build

```sh
cargo build --workspace
cargo test --workspace
```

## License

Licensed under either of:

- [MIT License](LICENSE-MIT)
- [Apache License, Version 2.0](LICENSE-APACHE)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual-licensed as above, without any additional terms or conditions.
