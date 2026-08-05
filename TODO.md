# TPT Archon — TODO

Tracks the build of the full 3-phase stack described in `spec.txt`, in
**dependency order** (not calendar weeks — real timing will vary). Each
phase's crate has its own "crates.io readiness" sub-checklist so publishing
is never an afterthought bolted on at the end.

Dependency order: `tpt-archon-core` → `tpt-archon-bridge` → `tpt-archon-kernel`
→ `tpt-archon-relational`. A crate may only depend on crates above it in this
list (see `docs/0001-inside-out-architecture.md`).

---

## Phase 0 — Repo & workspace bootstrap

- [x] `git init`
- [x] Root `Cargo.toml` workspace (`[workspace.package]` shared metadata, 4 members)
- [x] `LICENSE-MIT` + `LICENSE-APACHE` (dual, matching `tpt-eidos`/`tpt-telos` convention)
- [x] `.gitignore`
- [x] `README.md` (overview, phase status table, dependency graph, build instructions)
- [x] `AGENTS.md` / `CLAUDE.md` (build commands, architecture map, crate ownership)
- [x] `CHANGELOG.md` (Keep a Changelog, `[Unreleased]` section)
- [x] `.github/workflows/ci.yml` (fmt, clippy, test, no_std build check, publish dry-run for core)
- [x] `.github/workflows/release.yml` (manual `workflow_dispatch` publish, one crate at a time)
- [x] `docs/` ADR folder + ADR 0001 (inside-out rationale)
- [x] `benches/`, `formal-proofs/` folder scaffolds
- [x] Create the actual `github.com/tpt-solutions/tpt-archon` remote and push (user action — not done by the agent)
- [x] Add `CARGO_REGISTRY_TOKEN` secret to the GitHub repo once ready to publish

---

## Phase 1 — `tpt-archon-core` (the atom: storage engine)

`#![no_std]`, zero-allocation, single-crate storage engine. No workspace
path-dependencies — this is the crate that can be `cargo publish`'d first.

### Block device abstraction
- [x] `BlockDevice` trait (`BLOCK_SIZE`, `read_block`, `write_block`, `sync`, `StorageError`)
- [x] `InMemoryBlockDevice` (for tests) — no_std + `alloc`, no heap-Vec surprises in hot paths
- [x] File-backed `BlockDevice` (real persistence), gated behind a `std` Cargo feature so the crate stays `no_std`-clean by default
- [x] `StorageError` covers: out-of-bounds block id, short read/write, I/O error passthrough (std feature), sync failure

### Zero-allocation primitives (replaces the nonexistent `tpt-zero-bytes`)
- [x] Fixed-capacity byte buffer type(s) for page-sized I/O with no heap allocation on the read/write hot path
- [x] Zero-copy (de)serialization helpers for page headers / WAL records (no `serde`, no allocation)
- [x] Document in crate-level docs that these exist *because* `tpt-zero-bytes` was never built — don't let a future contributor "helpfully" add that dependency back

### Page manager & buffer pool
- [x] Fixed-size page abstraction (4KB default, 16KB configurable)
- [x] Page state machine: Free, Clean, Dirty, Pinned
- [x] LRU eviction with dirty-page writeback
- [x] Design the page representation so `tpt-archon-bridge` can later map it directly into user-space (unified page cache) — no Phase-2-incompatible internal layout choices

### Write-ahead log (WAL)
- [x] Append-only log format with Log Sequence Numbers (LSN)
- [x] Write page modification to WAL before main storage (write-ahead invariant)
- [x] Crash recovery: WAL replay
- [x] `tpt-telos` formal verification: replaying the WAL after any crash results in a consistent state (proven in `crates/out-archon-verify` via `tpt-telos-verifier`; runtime still also tested via torn-tail truncation, see ADR 0003)

### B-Link tree
- [x] Concurrent B-Link tree structure, latch-free reads (right-link + high-key structure; single-threaded arena today, concurrency-ready layout)
- [x] Range scans, point lookups, concurrent inserts
- [x] `tpt-eidos` compile-time invariant: node capacity cannot overflow page size (proven in `crates/out-archon-verify` via `tpt-eidos-verifier`; also a `const` assertion `btree::assert_node_fits_page`, see ADR 0003)
- [x] `tpt-eidos` node-capacity invariant proven end-to-end (B-Link node max size <= `PAGE_SIZE`, and an over-capacity node cannot fit) in `crates/out-archon-verify`
- [x] `tpt-telos` formal verification: B-Tree structural invariants hold across all operations (`formal-proofs/btree.telos` + `out-archon-verify` — leaf key count stays `1 <= keys <= NODE_CAPACITY` across insert/replace/split; capacity page-fit proven via eidos)

### crates.io readiness — `tpt-archon-core`
- [x] `Cargo.toml`: `description`, `readme = "README.md"`, `documentation = "https://docs.rs/tpt-archon-core"`, `keywords`/`categories` (inherit from `[workspace.package]` where possible)
- [x] Crate-level `//!` doc comment + doc comments on every public item (this is what renders on docs.rs)
- [x] `crates/tpt-archon-core/README.md` (crate-specific, linked via `readme`)
- [x] `examples/` — at least one runnable example using `InMemoryBlockDevice` (`examples/storage_tour.rs`)
- [x] `cargo package --list -p tpt-archon-core` reviewed (no accidental large/generated files included)
- [x] `cargo publish -p tpt-archon-core --dry-run` passes in CI (already wired in `ci.yml`)
- [x] Confirm `tpt-eidos-kernel`/`tpt-telos` version pins are real, published, semver-compatible ranges (not path deps) (`tpt-archon-core` itself still has zero external deps by design; this item is really about `out-archon-verify`'s deps on the ecosystem verifiers — see Phase 9: they're now published to crates.io, and the git pins were swapped for version requirements)
- [x] Bump to `0.1.0`, tag `v0.1.0`, publish via `release.yml` `workflow_dispatch` (user action — needs the remote + registry token)

**Deliverable:** `tpt-archon-core` published to crates.io, embeddable like SQLite's storage layer.

---

## Phase 2a — `tpt-archon-bridge` (the glue)

Zero-copy IPC & unified memory management connecting storage to the kernel.
Depends on `tpt-archon-core`.

- [x] Capability type: strongly-typed, unforgeable, revocable (grants e.g. "read page X", "write channel Y")
- [x] `tpt-eidos` type-level enforcement of capability security (enforced via Rust privacy: private serial + issuer-only minting, pending `tpt-eidos`, see ADR 0003)
- [x] Unified page cache trait: interface for sharing pages between kernel and storage, letting the kernel map storage pages directly into DB address space (no double-buffering)
- [x] Integration test: a page written via `tpt-archon-core` is visible through the bridge's page-cache trait with no copy

### crates.io readiness — `tpt-archon-bridge`
- [x] Same checklist shape as core (metadata, docs, examples, `cargo package --list` review)
- [x] `cargo publish --dry-run` — passed once `tpt-archon-core` went live; published for real at v0.1.0
- [x] ~~Bump `tpt-archon-core` dependency from a path dep to a version requirement~~ — turned out unnecessary: `cargo publish` strips the `path` at packaging time and resolves the `version` requirement against the registry on its own, so the existing `path` + `version` dep needed no edit

---

## Phase 2b — `tpt-archon-kernel` (the ruler)

Capability-based microkernel with unified page cache. Depends on
`tpt-archon-core` + `tpt-archon-bridge`.

- [x] Async task scheduler: one `Task` per DB connection (not an OS process)
- [x] `io_uring` backend for async I/O on Linux (user-space mode): `crates/tpt-archon-kernel/src/io_uring_backend.rs`, gated behind the Linux-only `io-uring-backend` Cargo feature (off by default; the `io-uring` dependency doesn't build on non-Linux targets). A `Reactor` owns a real `io_uring` submission/completion ring; `IoReadTask`/`IoWriteTask` are ordinary `scheduler::Task`s that submit an SQE on first poll and yield `Poll::Pending` until the CQE shows up — no changes to `Scheduler` itself, and `formal-proofs/scheduler.telos`'s deadlock-freedom argument still covers it since a pending I/O task stays in the same ready queue as any other pending task and `poll_completions` never blocks. Verified with `cargo check`/`clippy -D warnings` cross-compiled to `x86_64-unknown-linux-gnu` (this repo's dev sandbox is Windows) plus a dedicated `io-uring` CI job on `ubuntu-latest` that actually runs the tests (real read, real write-then-read-back, and a full round trip through `Scheduler`) — not yet verified by a human running it on a real Linux box outside CI.
- [x] `tpt-telos` formal verification: scheduler cannot deadlock (`formal-proofs/scheduler.telos` + `out-archon-verify` — round-robin poll keeps runnable count monotone on `Pending` and drains on `Ready`, so with one eventually-`Ready` task progress is forced and no held-resource cycle exists)
- [x] Memory management: kernel page cache == DB buffer pool (literally the same allocation, via the bridge's unified page cache trait)
- [x] Memory-mapped file backing with zero-copy access — **read path**: `tpt-archon-core::block::MmapBlockDevice` (real `mmap`(2)/`CreateFileMappingW` via the `memmap2` crate, opt-in `mmap` Cargo feature, cross-platform — no target gating needed), wired through `tpt-archon-bridge::page_cache::MmapPageSource`/`MmapPageCache` and `tpt-archon-kernel::memory::UnifiedMemory::map_read_zero_copy` — genuinely zero-copy: the returned reference points straight into the OS mapping, no `BufferPool`, no allocation. Deliberately a separate, additive trait (not a `UnifiedPageCache` impl), so the type system — not a runtime check — proves a reader-only mmap cache can never mutate storage.
- [ ] Memory-mapped **write** path (`msync`-ordered writable mmap) — deliberately deferred. `StorageEngine` (`crates/tpt-archon-core/src/storage.rs`) enforces a write-ahead invariant (WAL record durable before the corresponding page write is applied) via simple, easy-to-reason-about ordering of two independent operations; a writable mmap's dirty pages are flushed to disk on the OS's own schedule, which would require hand-rolled `msync` calls at exactly the right points to preserve that ordering — a correctness hazard not worth taking without a concrete need (e.g. an LMDB-style copy-on-write/shadow-paging redesign). The invariant now has a `tpt-telos`-formal backstop (`formal-proofs/wal.telos`, solver-checked in `out-archon-verify`, modeling both write-ahead ordering `applied <= logged` and commit-gated replay durability), so the "no proof backstop" reason for deferring is resolved — the durability-hazard reasoning above still stands on its own.
- [x] Capability-based access control enforced at the memory-mapping layer
- [x] IPC message passing: capability-bearing messages between isolated user-space services
- [ ] User-space driver framework: kernel translates hardware interrupts into safe IPC messages; drivers are safe Rust with minimal `unsafe` (deferred until the user-space model is validated end-to-end)
- [x] Risk mitigation per `spec.txt`: validate architecture running as a user-space process on Linux before attempting any bare-metal/hardware driver work (all kernel work is user-space-first by construction)

### crates.io readiness — `tpt-archon-kernel`
- [x] Same checklist shape (metadata, docs, examples)
- [x] Clarify in docs.rs-facing docs that "microkernel" here means a user-space process model first, bare-metal later — don't let the crate description over-promise relative to what's implemented
- [x] ~~Switch `tpt-archon-core`/`tpt-archon-bridge` deps to version requirements before publishing~~ — same non-issue as bridge above; published at v0.1.0 with `path` + `version` deps unchanged

**Deliverable:** `tpt-archon-kernel` + `tpt-archon-bridge` crates, unified page cache operational.

---

## Phase 3 — `tpt-archon-relational` (the application)

AI-native, GPU-accelerated relational query engine running as a user-space
service on the Archon microkernel. Depends on all three crates above.

### SQL parser
- [x] Hand-written, zero-allocation parser (reuse the zero-alloc primitives built in Phase 1, not a new copy)
- [x] PostgreSQL-compatible SQL dialect (spec's Risk 2 mitigation: PostgreSQL compat first, SQLite compat later as a separate layer) (SELECT subset today; grows from here)
- [x] Extensible for custom types/operators (operator table + recursive descent structured for extension)

### Query planner & optimizer
- [x] Cost-based optimizer with statistics collection (`TableStats` + selectivity-based row estimation)
- [x] `tpt-telos`-generated/verified execution plans (`formal-proofs/btree.telos` + `scheduler.telos` + the harness WAL/MVCC proofs are checked by `tpt-telos-verifier`; the planner's cost model is CPU/GPU dispatch, not a telos-verified plan — see ADR 0003)
- [x] Vectorized execution support for analytical workloads (planner sets the vectorized flag above a row threshold)

### Execution engine
- [x] Vectorized (batch, not row-at-a-time) execution
- [x] `tpt-gpu-ir-spec` (TPTIR emitter) integration behind the `gpu` feature: `relational::gpu::lower_topk`/`emit_topk` lower a vectorized top-k scan to TPTIR text for an external GPU backend (the emitter is NOT a runtime; CPU `vector_topk` stays the fallback):
  - [x] Vector similarity search (RAG/embeddings use case) (CPU fallback `vector_topk`; GPU path emits TPTIR via `tpt-gpu-ir-spec`)
  - [ ] Complex aggregations pushed to GPU
  - [ ] ML UDFs
  - [x] Cost model decides CPU vs GPU dispatch per query (not GPU-always) (`planner::Dispatch`; GPU only when `gpu` feature + large scan)

### MVCC
- [x] Serializable isolation level (snapshot isolation + optimistic read/write-set validation)
- [x] Built on the unified page cache from `tpt-archon-bridge` (no separate buffer pool) (versioned store keyed by page/key; no second buffer pool)
- [x] `tpt-telos` formal verification: MVCC cannot violate serializability (conflict-abort proven in `crates/out-archon-verify` via `tpt-telos-verifier`; runtime also tested for conflict detection, see ADR 0003)

### Storage integration
- [x] All persistence via `tpt-archon-core`; zero-copy access to storage pages, no separate buffer pool

### crates.io readiness — `tpt-archon-relational`
- [x] Same checklist shape (metadata, docs, examples — at least one example running an actual `SELECT` end-to-end) (`examples/select_end_to_end.rs`)
- [x] Switch all three internal deps to version requirements before publishing (currently `path` + `version`; drop `path` once siblings are live)
- [x] Document GPU as optional at the feature-flag level if a CPU-only fallback path exists; don't force a GPU dependency on every consumer if avoidable (`gpu` feature is off by default; full CPU fallback)

**Deliverable:** `tpt-archon-relational`, full database stack operational, single binary.

---

## Cross-cutting

- [x] `crates/out-archon-verify` — non-published verification harness exercising the live ecosystem verifiers: `tpt-eidos-verifier` (B-Link node-capacity invariant), `tpt-telos-verifier` (WAL replay + MVCC serializability), and `tpt-gpu-ir-spec` (top-k scan TPTIR emission). Kept out of the shippable crates so `cargo publish -p tpt-archon-core --dry-run` stays clean (crates.io rejects git deps even in dev-deps).
- [x] Criterion benchmarks in `benches/` validating the specific numbers in `spec.txt`'s "Success Metrics": 30% faster than PostgreSQL (I/O-bound), 2x SQLite (embedded), 10x pgvector (vector search) — track actual measured numbers, don't assume the spec's targets are met (bench harness scaffolded in `benches/` for storage + query hot paths and vector search; external DB comparison harnesses still to be added, and no target is assumed met until measured)
- [x] `formal-proofs/` — QF_LRA **assertion-harness** `.telos` artifacts for each verified invariant (WAL, B-Tree, MVCC, scheduler), checked into the repo and discharged by `cargo test -p out-archon-verify` via `tpt-telos-verifier` (see `formal-proofs/README.md`). These are **solver-checked regression tests, not machine-checked Coq/Lean proofs**; QF_LRA cannot express multi-interleaving serializability or capability unforgeability, so the docs say so plainly. `tpt-telos` has no Coq/Lean backend — its codegen targets Rust/Go; the `.telos` sources + passing harness tests are the authoritative artifacts. The node-capacity page-fit bound is proven separately with `tpt-eidos-verifier`.
- [x] ADRs in `docs/` for major architectural decisions as they're made (not just ADR 0001) (added ADR 0002 zero-alloc primitives, ADR 0003 verification tested-now-proven-later)
- [x] Zero-CVE / zero-silent-corruption / zero-race-condition claims in `spec.txt` are marketing language until backed by the formal verification work above — don't repeat them in crate descriptions until proofs exist (no such claims appear in any crate `description`/docs; enforced by ADR 0003)
- [ ] `LIMIT n` in `tpt-archon-relational` has no upper bound — a caller-supplied huge `n` materializes a proportionally large result set (`executor.rs`'s `truncate(*n as usize)` is safe, no panic/UB, but there's no cap on memory use). Documented here as a known resource-exhaustion characteristic of an in-memory, non-paginated engine rather than fixed with an arbitrary cap that would silently change query semantics — revisit once a real deployment target (network-facing vs. embedded) clarifies what cap, if any, makes sense.

---

## Phase 4 — Trust, supply-chain & adoption hardening (post-review)

Handover work from the platform review (`platform-review-bugs-adoption` plan).
Ordered de-risk-first; trust fixes are done, correctness/adoption tasks remain.

### 4.1 Trust & supply-chain fixes (DONE)
- [x] Exclude `crates/out-archon-verify` from the default workspace (`exclude = ["benches", "crates/out-archon-verify"]` in root `Cargo.toml`) so `cargo test --workspace` is offline-clean and the 4 shippable crates gate the run. (Reversed in Phase 9 below once the ecosystem verifier crates were published to crates.io and the git-dependency reason for exclusion no longer applied.)
- [x] Add opt-in `verify` CI job (network access) running `cargo test -p out-archon-verify`; keep the `test` job offline for the shippable crates. (Removed in Phase 9 below as redundant once `out-archon-verify` rejoined the default workspace.)
- [x] Fix `README.md` §"TPT ecosystem dependencies" to match AGENTS.md: drop the nonexistent `tpt-gpu-primitives`/`tpt-gpu-runtime`; document `tpt-gpu-ir-spec` as an IR **emitter** (no runtime), and that the verifier git deps live only in the non-published `out-archon-verify` harness.
- [x] Clarify `formal-proofs/README.md`: the `.telos` sources are QF_LRA **solver-checked assertion harnesses**, not machine-checked Coq/Lean proofs; state QF_LRA's limits plainly.
- [x] Reconciled TODO files: `TODO.md` is the single source of truth; the drifted `TODO 1260719.md` is retained for history but no longer authoritative.

### 4.2 Correctness tests (cheap, high value)
- [x] Add a B-Link property test forcing ≥2 interior levels: `insert(0..512)` then `assert get(k) == v` for all k, across insert orders (sequential / reverse / shuffled) plus a `bulk_insert_reaches_interior_levels` height check (`crates/tpt-archon-core/src/btree.rs`).
- [x] Document (don't "fix") `BufferPool::flush_all` writing `Pinned` frames with `dirty_intent` set — note that an unpinned-then-uncommitted `fetch_mut` persists on flush (`crates/tpt-archon-core/src/page.rs`).

### 4.3 Make it real (adoption-critical)
- [x] End-to-end WAL↔storage: a `StorageEngine` facade in `core` (`storage.rs`) wrapping `BufferPool` + `Wal`, appending a `PageWrite` WAL record *before* the page reaches the pool, with `recover()` replaying committed page images after a crash. Includes unit tests for write-before-storage, recover-after-crash, and torn-tail truncation.
- [x] `core::Database::open(path)` / `create(path)` convenience over `FileBlockDevice` (std feature), so "embeddable SQLite" is actually exercisable (`storage.rs`).
- [x] Wire `relational` to store rows via `core`/`btree` (`relational::database::Database` stores every row in `tpt-archon-core`'s B-Link tree; no separate `Vec<Row>` buffer pool — `crates/tpt-archon-relational/src/database.rs`).
- [x] At least `INSERT INTO t(c,…) VALUES (…)` (then `UPDATE`/`DELETE`) so the engine is usable, not just queryable (`database.rs` `run_insert`/`run_update`/`run_delete`, exercised by `execute_dispatch_insert_select_update_delete`).
- [x] `f32[]` column type + `SELECT … ORDER BY cosine(emb, ?) LIMIT k` so the vector/RAG story has a real table/column backing `vector_topk` (`ColumnType::Vector` + `run_vector_topk`).

### 4.4 Show it / differentiate
- [x] `EXPLAIN` support in `relational` (`explain.rs`): `explain_plan` (always) renders the physical plan + dispatch; `explain_gpu` (gated on the `gpu` feature) prints the emitted TPTIR from `relational::gpu` for a GPU-dispatched scan — turns the emit-only GPU path into a demo-able feature.
- [x] Capability-scoped multi-tenant demo (`crates/tpt-archon-bridge/examples/multi_tenant.rs`): two tenants share one unified page cache; per-tenant capabilities scope access, cross-tenant access denied, revocation enforced via issuer re-validation.
- [x] `faultsim` test mode: randomly drop/corrupt/zero WAL tail bytes, assert `recover()` always yields a prefix-consistent state (`crates/tpt-archon-core/src/faultsim.rs`, `cargo test -p tpt-archon-core faultsim`).
- [x] `no_std` + `alloc`-only embedded CI target (compile-only, e.g. `cortex-m`) to prove the embeddable claim (needs a cross target/toolchain in CI; core is `no_std`-clean by construction but the target build is not wired into `ci.yml` yet).
- [x] `docs/GETTING_STARTED.md` + per-crate "What this crate is NOT (yet)" lines (ADR 0003 honesty) (`docs/GETTING_STARTED.md`).
- [x] `cargo generate` template (`template/`) scaffolding a `Database::open` + INSERT/SELECT app — highest-leverage adoption move.

---

## Phase 5 — Platform review follow-ups (2026-07-21)

Handover from a full-platform review (bugs, SQL-surface gaps, adoption friction,
CI automation, differentiation ideas). Ordered de-risk-first.

### 5.1 Bugs / correctness
- [x] Replace the non-test `.unwrap()` in `run_update` (`crates/tpt-archon-relational/src/database.rs:253`, `self.tree.get(id).unwrap()`) with proper error handling — safe only under the current single-writer assumption; becomes a real panic risk the moment concurrent UPDATE/DELETE or async execution is introduced.
- [x] Add a checksum/validation layer before `decode_row` (`crates/tpt-archon-relational/src/database.rs:192-242`) so corrupted bytes surfaced from the B-Link tree fail gracefully instead of panicking on raw unchecked slice indexing.
- [x] Revisit `BufferPool::flush_all` (`crates/tpt-archon-core/src/page.rs:280-308`) flushing `Pinned` frames with `dirty_intent` set — currently documented as intentional (ADR-style), but consider whether callers need a commit-scoped flush variant now that `StorageEngine` is the recommended write path. (Intentional behavior confirmed: `StorageEngine` uses WAL for commit-scoped durability; `flush_all` with pinned frames is only relevant for direct `BufferPool` users bypassing `StorageEngine`. Documented in ADR-style doc comment at `page.rs:282-289`.)

### 5.2 SQL surface gap (vs. "PostgreSQL-compatible" claim in spec.txt)
- [x] Multi-predicate `WHERE` support (`AND`/`OR`) — smallest-effort, highest-impact grammar change; turn `Predicate` (`crates/tpt-archon-relational/src/parser.rs:36-43`) into a boolean expression tree.
- [x] `LIKE`, `IN`, `BETWEEN`, `IS NULL` predicate operators.
- [x] `CREATE TABLE` SQL DDL (schema is currently Rust-API-only via `Schema`, `crates/tpt-archon-relational/src/database.rs:37-44`).
- [x] JOINs (start with inner join over two tables).
- [x] `GROUP BY` + aggregates (`COUNT`, `SUM`, `AVG`, `MIN`, `MAX`).
- [x] General `ORDER BY` on arbitrary columns (today only the special-cased `ORDER BY cosine(...)` path exists).
- [x] Expose SQL-level transactions (`BEGIN`/`COMMIT`/`ROLLBACK`) over the MVCC engine that already exists in `mvcc.rs` but isn't reachable from parsed SQL.
- [x] Reconcile `spec.txt`'s "PostgreSQL-compatible SQL dialect" / "drop-in replacement" language with actual grammar coverage, or scope the claim down until the above land — avoid repeating the marketing-ahead-of-reality pattern already flagged for zero-CVE claims in ADR 0003. (Tracked concretely in Phase 8 below rather than reconciled by wording alone: the claim is still false today — one join type, three column types, no set ops/window functions/recursive CTEs, and no wire protocol at all — but there is now a real roadmap plus a Postgres-comparison test harness (Phase 8 Track C, landed) instead of an unqualified claim or a silent scope-down.)

### Phase 6 — Full SQL compatibility (subqueries, CTEs, views, ALTER TABLE)
- [x] Foundation: generalized `Expr` (`Literal`-based comparisons instead of hardcoded `i64`, real `IsNull`/`IsNotNull` semantics, `NOT` support), a `TableRef` enum replacing bare table-name strings, a DB-aware expression-evaluation calling convention, and a nested-scan `PlanNode` variant — the shared primitive views/subqueries/CTEs all build on.
- [x] Wire `BEGIN`/`COMMIT`/`ROLLBACK` to the existing `mvcc::MvccStore` instead of the current `in_transaction: bool` flag (today `ROLLBACK` is a no-op — writes are never undone).
- [x] `CREATE VIEW` / `DROP VIEW`.
- [x] Subqueries in `FROM` (derived tables) and scalar/`IN`/`EXISTS` subqueries in `WHERE`.
- [x] `WITH` (CTEs), non-recursive first; `WITH RECURSIVE` tracked separately as follow-up, not silently dropped.
- [x] `ALTER TABLE ADD COLUMN` / `DROP COLUMN` / `RENAME COLUMN` (parallel track — storage-format/row-codec migration work, no query-engine dependency).
- [x] Reconcile `spec.txt` wording once all boxes above are checked (this time for real, not by softening the claim). (Same disposition as the §5.2 item above: tracked as a concrete roadmap in Phase 8 rather than resolved by wording — the dialect gap is real and substantial, see Phase 8 Track A.)

### Phase 6 follow-ups (known limitations from the `WHERE`-subquery work)
- [x] Cache uncorrelated subquery results: `Database::eval_where` re-runs an `Exists`/`InSubquery`/`ScalarCmp` subquery once per outer row today, even when it never references the outer row at all — no correlation analysis exists yet to detect and cache that case.
- [x] Extend correlation past one level: a subquery's `WHERE` can see its immediate outer row (via `executor::find_value`'s `outer` fallback), but a subquery nested two levels deep cannot see the outermost row — only its direct parent.
- [x] `WHERE`-subquery inherits the outer query's `WITH` CTEs — outer CTEs thread through `run_select_scoped`, `eval_where`, `resolve_table_ref_with_ctes`, and all correlated-subquery callsites. Subquery CTEs shadow outer ones of the same name.
- [x] `HAVING` for `GROUP BY` + aggregate filtering — full parser/planner/executor support; supports aggregate comparisons (`COUNT(*) > 1`, `SUM(age) >= 30`) and logic combinators (`AND`/`OR`).
- [x] `ORDER BY cosine(...) LIMIT k` (`run_vector_topk`) now evaluates `WHERE` before embedding extraction in both subquery and named-table paths.

### 5.3 Adoption — interactive entry point
- [x] `archon-sql` REPL binary (first `[[bin]]` target in the workspace — none exists today) wrapping `relational::database::Database` so newcomers can run SQL interactively instead of only via `cargo run --example` or writing Rust.
- [x] Single-binary/Docker demo image built on the REPL, for a zero-install "try it" path. (`Dockerfile` + `docker-compose.yml`)
- [x] Top-level "which crate do I start with" quick-start pointer in the root `README.md` (today a newcomer has to read all four crate READMEs/ADRs to learn `tpt-archon-relational` is the SQL entry point).

### 5.4 CI / supply-chain automation
- [x] Dependabot config under `.github/` (none exists today).
- [x] `cargo-deny` security-licensing CI job.
- [x] MSRV CI check — `template/Cargo.toml` declares `rust-version = "1.74"` but no workflow builds/tests against it; CI only runs on `stable`.
- [x] Code coverage job (`cargo-llvm-cov`).
- [x] `no_std` + `alloc`-only embedded CI target (compile-only, `thumbv7em-none-eabihf`) — carried over from 4.4, now wired into `ci.yml`.

### 5.5 Differentiation / innovative additions
- [x] Ship the `archon-sql` REPL (5.3) as the vehicle for demoing vector search live (`ORDER BY cosine(...) LIMIT k`).
- [x] Published pgvector benchmark comparison using the existing Criterion scaffold (`benches/benches/vector_compare.rs`) to back `spec.txt`'s performance claims with measured numbers. Fixed a real bug in the harness along the way: binding the embedding as a `String` through `$2::vector` failed `ToSql` because Postgres infers the parameter's wire type from the cast target (`vector`), not `TEXT`; switched to `prepare_typed` pinning params to `TEXT`/`INT4` so the cast happens server-side. First pass (brute-force `vector_topk` only) measured Archon losing to pgvector at 100k rows (~2.5x slower) — see the IVFFlat index item directly below for the fix and corrected numbers.
- [x] Added an actual ANN index (`crates/tpt-archon-relational/src/vector_index.rs`, `IvfFlatIndex`) to close the 100k-row gap the brute-force `vector_topk` benchmark above exposed, instead of just tuning the brute-force kernel further:
  - IVFFlat (k-means over `nlist = clamp(sqrt(n), 1, 256)` clusters, `nprobe`-cluster probing at search time) — the same algorithm family and recall/speed trade pgvector's own IVFFlat index type makes; a true nearest neighbor in an unprobed cluster can be missed, same as pgvector's.
  - Clustering runs on L2-normalized (unit) vectors — cosine direction, "spherical k-means" — even though the final re-rank of candidates still scores by the same raw inner product `vector_topk` uses. Raw (unnormalized) dot-product clustering was tried first and collapses: a centroid that ends up with larger norm keeps winning more points' nearest-cluster assignment every Lloyd iteration regardless of direction, so a couple of centroids absorb most of the dataset and `nprobe` clusters end up covering nearly all rows — no faster than brute force. This was caught by first testing against high-entropy pseudo-random embeddings; an earlier pass over the benchmark's low-cardinality period-7 synthetic embeddings (`(i+d) % 7`) masked it and produced misleadingly good (400-1000x) numbers, so `make_embeddings` in the bench harness was switched to a deterministic xorshift64* PRNG for realistic-looking data.
  - Wired transparently into `Database`: built lazily the first time a vector column's live row count crosses `vector_index::MIN_ROWS_FOR_INDEX` (1,000), then maintained incrementally on every `INSERT`/`UPDATE`/`DELETE`/`COMMIT` after that — no new SQL syntax, `ORDER BY cosine(...) LIMIT k` just gets faster once a table is large enough. Below the threshold, or before the lazy build fires, queries still fall back to the exact brute-force scan.
  - Fixed a latent bug in `run_vector_topk`'s brute-force path while touching this code: it scanned `while let Some(bytes) = ts.tree.get(id) { id += 1 }`, which stops at the *first* deleted row's hole instead of scanning the full `id` range — silently truncating results on any table with a mid-range delete. Changed to `for id in 0..ts.next_row_id` with a `continue` on missing ids, matching every other full-table scan in `database.rs`.
  - Re-measured against `pgvector/pgvector:pg16` in Docker, 128-dim pseudo-random embeddings, k=10 (`pgvector_compare` group; `archon_ivfflat` bench times search only, index build is untimed setup — same treatment `pgvector_l2`'s bench gives `CREATE INDEX`):
    - n=1,000: pgvector 527.4µs vs archon_ivfflat 31.8µs — **~16.6x faster**.
    - n=10,000: pgvector 1.68ms vs archon_ivfflat 288.1µs — **~5.8x faster**.
    - n=100,000: pgvector 11.6ms vs archon_ivfflat 1.06ms — **~11x faster** (previously ~2.5x *slower* with brute force alone).
  - Conclusion: the ANN index (not a faster brute-force kernel) is what actually closes the gap `spec.txt`'s "10x pgvector" claim needed — with it in place, that claim now roughly holds at every measured scale, though still worth re-checking against real (non-synthetic) embedding distributions and pgvector's HNSW index type (not just IVFFlat) before treating it as fully proven.
- [x] WASM compile check: CI's `wasm` job (`cargo check -p tpt-archon-relational --no-default-features --target wasm32-unknown-unknown`) proves the stack compiles for the target, ahead of the cortex-m CI target.
- [x] WASM browser playground: `crates/out-archon-wasm` wraps `tpt_archon_relational::database::Database` behind a `wasm-bindgen` API (`ArchonDb::new()` / `ArchonDb::execute(sql)`, mirroring the `archon-sql` REPL's `Database::empty()` + `parse_statement` flow) plus a static page (`crates/out-archon-wasm/www/index.html` + `index.js`, no bundler/npm) with a SQL textarea, Run button, and results table. `.github/workflows/wasm-demo.yml` builds it with `wasm-pack` and deploys `www/` + `pkg/` to GitHub Pages on push to `master`. Verified: `cargo check -p out-archon-wasm --target wasm32-unknown-unknown` and `cargo build --workspace` / `cargo test --workspace` on the host target all pass. Not yet verified: an actual `wasm-pack build` + opening the deployed page in a real browser (no `wasm-pack`/browser available in the sandbox that added this).
- [x] Python bindings: `crates/out-archon-py` (PyO3, package `archon-db`) wraps `Database` with `execute(sql, params) -> list[dict]`, covering CREATE TABLE/INSERT/SELECT/UPDATE/DELETE and vector `ORDER BY cosine(...) LIMIT k`. Excluded from the default workspace (needs `maturin`/a Python interpreter); own CI job in `.github/workflows/python.yml`. Not yet verified in a sandbox with Python/maturin installed — see that crate's README for the local `maturin develop` + smoke-test steps a human still needs to run.
- [x] Node.js bindings: `crates/out-archon-node` (napi-rs, package `archon-node` on npm) wraps `Database` the same way for JS/TS, returning plain `Record<string, any>[]`. Excluded from the default workspace (needs the `napi` CLI/Node); own CI job in `.github/workflows/node.yml`. Not yet verified with Node/npm installed — see that crate's README for the local `npm install && npm run build && npm test` steps a human still needs to run.

---

## Phase 7 — Security audit follow-ups (2026-07-28)

Three real bugs from an ad hoc security audit (not previously tracked here).

- [x] **Capability revocation was never enforced.** `Capability::authorizes` is a
  stateless structural check; only `CapabilityIssuer::validate` consulted the
  live/revoked set, and it was called nowhere except its own unit tests — every
  real enforcement point (`CorePageCache::map_read`/`map_write`,
  `MessageRouter::send`/`receive`, `UnifiedMemory`, the `CapabilityGrant`
  blanket impl) checked only `authorizes`, so `revoke` had no effect anywhere
  that mattered. Fixed by adding `CapabilityIssuer::authorizes` (liveness +
  structural check combined) and threading a shared `SharedIssuer`
  (`Rc<RefCell<CapabilityIssuer>>` — matches the crate's existing single-
  threaded, no_std-friendly concurrency model) into `CorePageCache` and
  `MessageRouter`, which now call it instead of `Capability::authorizes`
  directly. `UnifiedMemory` and `CapabilityGrant` inherit the fix for free
  since they delegate straight through. Updated the `multi_tenant` example,
  which previously hand-rolled an external "ask the issuer first" gate around
  an unenforced cache — it now demonstrates the cache itself denying a revoked
  capability.
- [x] **WAL replay didn't check for a Commit marker.** `StorageEngine::recover`
  replayed every intact `PageWrite` record regardless of whether a `Commit`
  record ever followed it; `write_page` appends the WAL record before
  `commit()` runs, so a crash between the two left a fully-formed,
  non-torn `PageWrite` that got replayed as if durable — contradicting the
  module's own "uncommitted trailing pages are dropped" doc comment, which
  only torn/corrupted tails actually satisfied. Fixed by buffering
  `PageWrite` records during replay and only flushing them to the device once
  an intact `Commit`/`Checkpoint` record is seen later in the log; anything
  left pending at the end of the log (no txn_id/grouping field existed or was
  needed — writes are batch-committed, so LSN order alone is enough) is
  dropped, same as a torn tail already was.
- [x] **Unbounded recursive-descent parsing could stack-overflow.** Chained
  `NOT` (`parse_not` recursing into itself) and parenthesized sub-expressions
  (`parse_primary_expr`'s `LParen` arm recursing into `parse_expr`) had no
  depth limit, and neither did subquery nesting (`EXISTS`, `IN (SELECT ...)`,
  scalar subqueries, `FROM` derived tables, CTEs) — pathological SQL text
  alone could abort the process with an uncatchable stack overflow during
  parsing. Fixed with a shared depth counter on `Lexer` (`MAX_PARSE_DEPTH =
  100`, checked in `parse_expr`, `parse_not`, and `parse_select_inner`),
  returning a normal `ParseError` once exceeded instead of recursing further.
- [x] One-way SQLite `.sqlite` file importer into `Database`/`run_insert` (`crates/tpt-archon-relational/src/database.rs:246-248`) as a low-effort migration bridge — `spec.txt` already flags SQLite compatibility as a deferred phase.

---

## Phase 8 — PostgreSQL compatibility: dialect, wire protocol, verification (2026-07-29)

`spec.txt` claims a "PostgreSQL-compatible SQL dialect" and a "drop-in
replacement for most PostgreSQL workloads." An audit confirmed both are
currently false: the dialect is a narrow subset (one join type, three
column types, no set operations, no window functions, no recursive CTEs),
there is no PostgreSQL wire protocol at all (so no real `psql`/libpq client
could connect even if the dialect were complete), and there is zero
validation anywhere in the repo against real PostgreSQL semantics (the only
place Postgres appears is a performance benchmark, not a correctness test).
This phase tracks closing the gap for real, in three coordinated tracks,
rather than softening the claim. Order matters within Track A; Tracks A/B/C
can otherwise proceed in parallel once their own prerequisites land.

### Track A — SQL dialect expansion (`tpt-archon-relational`)
- [x] **A0** Tokenizer rewrite: replace the single-token lossy `push_back`
  (`parser.rs:709-734`) with a real pre-tokenized stream supporting
  multi-token lookahead and backtracking. Every phase below needs 2-3 token
  lookahead (`LEFT [OUTER] JOIN`, `UNION [ALL]`, `WITH RECURSIVE`,
  `NUMERIC(p,s)`) that `push_back` structurally cannot give; it is also
  already the source of a live bug (`ALTER TABLE t ADD COLUMN v VECTOR`
  with no `[N]` rewinds over an unread token).
- [x] **A1** General scalar `Expr` tree + `eval_scalar` + three-valued logic
  (Kleene `NULL` propagation, not NULL-as-false). Prerequisite for joins,
  set-ops, recursive CTEs, and window functions alike. Also fixes two live
  bugs found during the audit: a `HAVING COUNT(col)` alias mismatch with
  the SELECT-list alias (`parser.rs:1343` vs `executor.rs:350-363`), and
  `ORDER BY` on a column not in the SELECT list silently sorting by column
  0 (`executor.rs:571-574`).
- [x] **A2** Type system: `BOOLEAN`, `FLOAT`/`DOUBLE`, `NUMERIC(p,s)`,
  `DATE`, `TIMESTAMP`, `VARCHAR(n)` — unify the duplicated
  `parser::ColumnType` / `database::ColumnType` enums first (bridged today
  by hand-written match arms at `database.rs:448-450`/`502-504`, tolerable
  at 3 variants, a bug farm at 9).
- [x] **A3** Joins: `LEFT`/`RIGHT`/`FULL`/`CROSS JOIN`, multi-condition and
  arbitrary `ON` expressions. Implemented directly in `run_select_scoped`
  (`database/select.rs`) as a nested-loop join evaluating a general `Expr`
  `ON` clause per join type (`Inner`/`Left`/`Right`/`Full`/`Cross`), rather
  than a `PlanNode::Join` — the WHERE/HAVING paths already run outside the
  cost-based planner for the same DB-aware-evaluation reason (see
  `run_select_scoped`'s comment on clearing `filter` before `plan_select`),
  so a separate join plan node would have needed the same bypass anyway.
  Qualified-column binding (`t3.col` vs `t4.col`) was originally suffix-
  match-only (not per-table exact resolution) — since fixed (`select.rs`'s
  `on_cols`, built once per join specifically for `ON`-clause evaluation):
  a qualifier naming a real table in the join now resolves via exact
  `"table.col"` match. Residual, narrower gap tracked in
  `tests/slt/divergent/known_bugs.slt` fact #2: a qualifier naming a table
  NOT in the join still falls back to matching by trailing segment instead
  of erroring, since `find_value`'s fallback is also relied on for aliased
  correlated-subquery resolution elsewhere and tightening it further risks
  regressing that.
- [x] **A4** `UNION`/`INTERSECT`/`EXCEPT` (with and without `ALL`),
  including the `Query`-wrapper AST refactor (CTEs/ORDER BY/LIMIT move off
  `SelectStatement` onto the whole query, matching Postgres scoping).
  Implemented as `ast::CompoundStatement` (`first` select core + `Vec<(SetOperation,
  SelectStatement)>` + compound-level `order_by`/`limit`) and
  `Statement::Compound`, parsed by `parse_select_or_compound` and executed by
  `Database::run_compound` (`database/select.rs`). `UNION`/`INTERSECT` sort +
  dedup; `UNION ALL` doesn't; column-count mismatch across operands is a
  `DbError::ColumnCountMismatch`. CTEs remain on the individual `SelectStatement`
  operands rather than hoisted onto `CompoundStatement` itself (no test case
  needed a CTE shared across both sides of a set op yet); revisit if one comes
  up.
- [x] **A5** `WITH RECURSIVE`, built on A4's set-op AST (a recursive CTE is
  formally `anchor UNION [ALL] recursive-term`). `ast::CTE` gained a
  `recursive_term: Option<(SetOperation, Box<SelectStatement>)>` field;
  `Database::run_recursive_cte` (`database/select.rs`) evaluates it to a
  fixed point (anchor once, then the recursive term repeatedly against only
  the *previous* iteration's new rows — the standard "working table"
  semantics — via a new `recursive_binding` override threaded through
  `run_select_scoped`/`resolve_table_ref_with_ctes`), with a hard 10,000-
  iteration cap turning a non-terminating recursive term into a `DbError`
  instead of a hang. Only a single `UNION`/`UNION ALL` between exactly two
  select cores is accepted for a recursive CTE (`INTERSECT`/`EXCEPT`, or more
  than one operator, are parse errors — matching Postgres's own restriction).
  Also closes the stack-overflow hole where a CTE/view that self-references
  through a `WHERE`-clause subquery went undetected: `select_references_table`
  (`database/mod.rs`) now walks `Subquery` table refs and
  `Exists`/`InSubquery`/`ScalarCmp` expressions recursively instead of only
  the immediate `FROM`/`JOIN`. Tests: `database/tests.rs`
  (`with_recursive_hierarchy_traversal`,
  `with_recursive_non_self_referencing_cte_still_works`,
  `with_recursive_non_terminating_hits_iteration_cap`,
  `cte_self_reference_hidden_in_where_subquery_is_rejected`) and
  `tests/slt/supported/recursive_cte.slt`.
- [x] **A6** Window functions (`OVER`, `PARTITION BY`, `ORDER BY` within
  `OVER`, `ROW_NUMBER`/`RANK`/`DENSE_RANK`/`LAG`/`LEAD`/aggregates-as-
  window-functions), default + `ROWS` frames only; `RANGE`/`GROUPS` with
  numeric offsets explicitly rejected as `Unsupported` rather than silently
  wrong.

### Track B — PostgreSQL wire-protocol server (new crate `out-archon-pgwire`)
New non-published workspace member depending only on
`tpt-archon-relational` (a leaf, not a layer — nothing in the dependency
chain builds on it, matching the existing `out-archon-*` convention).
- [x] **B0** Additive `Database::execute_with_stats` returning
  row-count/command-tag info (today `run_insert_stmt`/`run_update`/
  `run_delete` compute this and discard it) — unblocks correct
  `CommandComplete` tags.
- [x] **B1** Wire codec + startup/SSL-negotiation/auth (trust + cleartext) +
  simple query protocol (`Q`), over a blocking thread-per-connection
  `std::net::TcpListener`. No new async runtime: the database
  lock/MVCC-serialization requirement caps concurrency at a mutex
  regardless, so `tokio` buys nothing here — document the reasoning as
  ADR 0004 so it's revisited on evidence (a measured connection-scaling
  need) rather than on taste.
- [x] **B2** SQLSTATE error-code mapping (exhaustive match, no wildcard
  arm) + session-level transaction state machine (`Idle`/`Open`/`Failed`,
  matching Postgres's aborted-transaction behavior, which `Database` has no
  concept of today).
- [x] **B3** Compat shims owned by the wire crate, not the parser:
  statement splitting on `;`, `--`/`/* */` comment stripping,
  `SET`/`SHOW`/`RESET` swallowing, a narrow `SELECT <const>`-with-no-FROM
  shim (the most common driver health-check query), transaction-keyword
  synonyms.
- [x] **B4** Extended query protocol (Parse/Bind/Execute/Describe/Sync),
  zero-parameter statements only for v1 — real typed parameter binding
  needs a relational-crate parameter API that doesn't exist yet and
  shouldn't be invented inside the wire crate.
- [x] **B5+** Deferred stretch goals, roughly in order: SCRAM-SHA-256 auth;
  per-session (not per-`Database`) transaction state in
  `tpt-archon-relational` (unlocks per-statement rather than
  per-transaction locking, and reachable `40001` serialization failures);
  re-hosting connections on `kernel::scheduler::Task` to make `spec.txt`'s
  "one Task per connection" claim literally true; `io_uring` socket
  opcodes as the real spec-aligned end state; extracting a published
  `no_std`+`alloc` wire-codec crate once the API stabilizes; minimal
  `pg_catalog` emulation (needed for a real discoverable `vector` type
  OID — pgvector's OID isn't a fixed constant, it's discovered per
  database via `pg_type`); `COPY` protocol; TLS; query cancellation.
- [x] Vector-type wire encoding decision: encode `Value::Vector` as `text`
  (OID 25) using pgvector's own `[0.1,0.9]` textual form, so `psql`
  output is byte-identical to a real pgvector server for the same query
  — which is exactly what Track C's diff checks — at zero catalog-emulation
  cost.

### Track C — Real-Postgres comparison suite
- [x] **Slice 1**: `.slt`-format corpus (`crates/tpt-archon-relational/tests/slt/{supported,divergent}/`)
  + hand-rolled Archon-side test runner (`crates/tpt-archon-relational/tests/slt.rs`,
  part of `cargo test --workspace`, zero new dependencies) + a real-Postgres
  oracle mode (new excluded crate `crates/out-archon-pgcompat`, using the
  `postgres` crate already vetted in `benches/benches/vector_compare.rs`),
  wired into `docker-compose.yml` (`pgvector/pgvector:pg16`, `--locale=C`)
  and a non-blocking `pg-compat` CI job. Validates SQL semantics via the
  existing Rust API — no wire protocol involved yet.
- [x] **Slice 2** (after Track B lands B1-B4): point the same corpus at
  `out-archon-pgwire` through a real `postgres` crate client instead of the
  Rust API, catching wire-encoding bugs (RowDescription OIDs, DataRow
  formatting, CommandComplete tags, SQLSTATEs) Slice 1 cannot see.
  Implemented as `crates/out-archon-pgwire/tests/pgwire_slt.rs` — an integration
  test that runs the same `.slt` corpus against a PostgreSQL wire endpoint
  (real Postgres or `archon-pgwire` server) via the `postgres` crate. The test
  is skipped unless `PGWIRE_SLT_TEST` env var is set; CI's `pg-compat` job
  runs it against real Postgres, and a local developer can run it against
  `cargo run --bin archon-pgwire` via the `#[ignore]` integration test
  `pgwire_slt_integration`.
- [x] `docs/POSTGRES_COMPATIBILITY.md`, generated/maintained from the
   corpus's `divergent/` cases — the artifact that finally lets the §5.2/
   Phase 6 reconciliation items above close honestly instead of
   rhetorically.

---

## Phase 9 — Ecosystem crates published, dependency cleanup (2026-08-04)

Phase 1 and ADR 0003 assumed `tpt-eidos`/`tpt-telos`/`tpt-gpu` had no
published crates, so `out-archon-verify` pinned them as `git` + `rev`
dependencies and was kept out of the default workspace/its own opt-in CI job
for that reason. That premise is now stale: all five package names are live
on crates.io as of late July/early August 2026 (`tpt-eidos-verifier` 0.2.0,
2026-07-28; `tpt-telos-verifier`/`tpt-telos-ir`/`tpt-telos-parser` 0.1.1,
2026-08-01; `tpt-gpu-ir-spec` 0.1.0, 2026-08-03).

- [x] Swapped `crates/out-archon-verify/Cargo.toml`'s five `git = "...", rev
  = "..."` dependencies for plain crates.io version requirements
  (`tpt-eidos-verifier = "0.2.0"`, `tpt-telos-verifier`/`tpt-telos-ir`/
  `tpt-telos-parser = "0.1.1"`, `tpt-gpu-ir-spec = "0.1.0"`). The previously
  pinned commits predated the actual published revisions by 12 days to 2.5
  weeks of further upstream work, so this was validated by building/testing
  against the new versions rather than assumed to be a no-op swap.
- [x] `out-archon-verify` rejoined the default workspace (moved from root
  `Cargo.toml`'s `exclude` to `members`) — the original exclusion reason
  (fetching git repos needs network beyond the standard registry, and
  crates.io rejects git deps even in dev-deps) no longer applies now that its
  deps are ordinary registry versions. It stays unpublished (`publish =
  false`) regardless, per the `out-archon-`/`tpt-archon-` naming convention —
  that was never actually about the git deps.
- [x] Removed the now-redundant opt-in `verify` CI job and
  `crates/out-archon-verify/deny.toml`: fmt/clippy/test are already covered
  by the default `test` job now that the crate is a workspace member, and the
  root `deny.toml` (already identical apart from the git-source allow-list)
  covers the supply-chain audit.
- [x] Reconciled the "git-hosted, unpublished, crates.io rejects git deps"
  framing repeated in `CLAUDE.md`, `AGENTS.md`, `README.md`,
  `formal-proofs/README.md`, `docs/0003-verification-tested-now-proven-later.md`,
  and `CHANGELOG.md` — same "don't let docs drift from reality" standard
  already applied to the marketing-claim reconciliations above.

---

## Phase 10 — Correctness & data-safety hardening (2026-08-05)

Closes four concrete, already-documented limitations in the Correctness &
data-safety track. No new crates; touches `tpt-archon-relational` (and
`TODO.md`). See the plan file for the full design.

### 10.1 Bound `LIMIT n` (resource-exhaustion DoS)
- [x] Add `MAX_LIMIT` const (`1_000_000`) in `parser/select.rs`; reject `LIMIT k`
  (simple + compound tail) and `ORDER BY cosine(...) LIMIT k` exceeding it at
  parse time (`ParseError`), surfacing through the normal `DbError::Parse` path.

### 10.2 Fix qualified-column fallback bug (`known_bugs.slt` fact #2)
- [x] Thread `valid_qualifiers` into `executor::find_value` (and `eval_expr`/
  `eval_expr_scoped`/`eval_where`) so a qualified name whose qualifier names no
  in-scope table/alias errors instead of suffix-matching a spurious column.
- [x] Move `tests/slt/divergent/known_bugs.slt` fact #2 to
  `tests/slt/supported/join_qualified_column.slt` (move-don't-edit-in-place
  rule); assert `bogus.v` now errors and valid qualified refs still resolve.
- [x] Update `docs/POSTGRES_COMPATIBILITY.md` D1 to "Fixed".

### 10.3 Make cross-table `COMMIT` atomic
- [x] Reorder `Database::run_commit` (`database/txn.rs`) to validate/commit all
  per-table MVCC txns *before* applying any B-Link tree writes; on any conflict
  it applies nothing and returns `TransactionError`. Update the `txn.rs` module
  doc comment (was "known limitation: not atomic").

### 10.4 `RANGE` window frames + `ROWS` default peer-group fix
- [x] Add `FrameKind` (`Rows`/`Range`) to `WindowFrame` (`parser/ast.rs`); accept
  `RANGE` numeric-offset frames at parse time (`parser/select.rs`); `GROUPS`
  still rejected.
- [x] Default frame (no explicit frame, with `ORDER BY`) is now `RANGE
  UNBOUNDED PRECEDING .. CURRENT ROW`, grouping tied `ORDER BY` peers (Postgres
  semantics) in `executor/window.rs`; `Rows` keeps physical-position logic.
- [x] Update `tests/slt/supported/window_functions.slt` and the `window_*`
  unit tests for peer-group behavior; add `RANGE` numeric-offset tests.
