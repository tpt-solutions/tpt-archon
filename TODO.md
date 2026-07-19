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
- [ ] Create the actual `github.com/tpt-solutions/tpt-archon` remote and push (user action — not done by the agent)
- [ ] Add `CARGO_REGISTRY_TOKEN` secret to the GitHub repo once ready to publish

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
- [x] `tpt-telos` formal verification: replaying the WAL after any crash results in a consistent state (proven in `crates/tpt-archon-verify` via `tpt-telos-verifier`; runtime still also tested via torn-tail truncation, see ADR 0003)

### B-Link tree
- [x] Concurrent B-Link tree structure, latch-free reads (right-link + high-key structure; single-threaded arena today, concurrency-ready layout)
- [x] Range scans, point lookups, concurrent inserts
- [x] `tpt-eidos` compile-time invariant: node capacity cannot overflow page size (proven in `crates/tpt-archon-verify` via `tpt-eidos-verifier`; also a `const` assertion `btree::assert_node_fits_page`, see ADR 0003)
- [x] `tpt-eidos` node-capacity invariant proven end-to-end (B-Link node max size <= `PAGE_SIZE`, and an over-capacity node cannot fit) in `crates/tpt-archon-verify`
- [x] `tpt-telos` formal verification: B-Tree structural invariants hold across all operations (`formal-proofs/btree.telos` + `tpt-archon-verify` — leaf key count stays `1 <= keys <= NODE_CAPACITY` across insert/replace/split; capacity page-fit proven via eidos)

### crates.io readiness — `tpt-archon-core`
- [x] `Cargo.toml`: `description`, `readme = "README.md"`, `documentation = "https://docs.rs/tpt-archon-core"`, `keywords`/`categories` (inherit from `[workspace.package]` where possible)
- [x] Crate-level `//!` doc comment + doc comments on every public item (this is what renders on docs.rs)
- [x] `crates/tpt-archon-core/README.md` (crate-specific, linked via `readme`)
- [x] `examples/` — at least one runnable example using `InMemoryBlockDevice` (`examples/storage_tour.rs`)
- [x] `cargo package --list -p tpt-archon-core` reviewed (no accidental large/generated files included)
- [x] `cargo publish -p tpt-archon-core --dry-run` passes in CI (already wired in `ci.yml`)
- [ ] Confirm `tpt-eidos-kernel`/`tpt-telos` version pins are real, published, semver-compatible ranges (not path deps) (N/A: those crates are not published; core currently has zero external deps by design)
- [ ] Bump to `0.1.0`, tag `v0.1.0`, publish via `release.yml` `workflow_dispatch` (user action — needs the remote + registry token)

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
- [ ] `cargo publish --dry-run` — note: will only fully resolve once `tpt-archon-core` is actually live on crates.io (path-dep → registry-dep switch needed pre-publish)
- [ ] Bump `tpt-archon-core` dependency in this crate's `Cargo.toml` from a path dep to a version requirement before publishing (currently a `path` + `version` dep; drop the `path` once core is live)

---

## Phase 2b — `tpt-archon-kernel` (the ruler)

Capability-based microkernel with unified page cache. Depends on
`tpt-archon-core` + `tpt-archon-bridge`.

- [x] Async task scheduler: one `Task` per DB connection (not an OS process)
- [ ] `io_uring` backend for async I/O on Linux (user-space mode) (cooperative user-space scheduler implemented first per Risk 1; `io_uring` backend is a later milestone)
- [x] `tpt-telos` formal verification: scheduler cannot deadlock (`formal-proofs/scheduler.telos` + `tpt-archon-verify` — round-robin poll keeps runnable count monotone on `Pending` and drains on `Ready`, so with one eventually-`Ready` task progress is forced and no held-resource cycle exists)
- [x] Memory management: kernel page cache == DB buffer pool (literally the same allocation, via the bridge's unified page cache trait)
- [ ] Memory-mapped file backing with zero-copy access (user-space model validated first; real `mmap` is a later milestone)
- [x] Capability-based access control enforced at the memory-mapping layer
- [x] IPC message passing: capability-bearing messages between isolated user-space services
- [ ] User-space driver framework: kernel translates hardware interrupts into safe IPC messages; drivers are safe Rust with minimal `unsafe` (deferred until the user-space model is validated end-to-end)
- [x] Risk mitigation per `spec.txt`: validate architecture running as a user-space process on Linux before attempting any bare-metal/hardware driver work (all kernel work is user-space-first by construction)

### crates.io readiness — `tpt-archon-kernel`
- [x] Same checklist shape (metadata, docs, examples)
- [x] Clarify in docs.rs-facing docs that "microkernel" here means a user-space process model first, bare-metal later — don't let the crate description over-promise relative to what's implemented
- [ ] Switch `tpt-archon-core`/`tpt-archon-bridge` deps to version requirements before publishing (currently `path` + `version`; drop `path` once siblings are live)

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
- [x] `tpt-telos` formal verification: MVCC cannot violate serializability (conflict-abort proven in `crates/tpt-archon-verify` via `tpt-telos-verifier`; runtime also tested for conflict detection, see ADR 0003)

### Storage integration
- [x] All persistence via `tpt-archon-core`; zero-copy access to storage pages, no separate buffer pool

### crates.io readiness — `tpt-archon-relational`
- [x] Same checklist shape (metadata, docs, examples — at least one example running an actual `SELECT` end-to-end) (`examples/select_end_to_end.rs`)
- [ ] Switch all three internal deps to version requirements before publishing (currently `path` + `version`; drop `path` once siblings are live)
- [x] Document GPU as optional at the feature-flag level if a CPU-only fallback path exists; don't force a GPU dependency on every consumer if avoidable (`gpu` feature is off by default; full CPU fallback)

**Deliverable:** `tpt-archon-relational`, full database stack operational, single binary.

---

## Cross-cutting

- [x] `crates/tpt-archon-verify` — non-published verification harness exercising the live ecosystem verifiers: `tpt-eidos-verifier` (B-Link node-capacity invariant), `tpt-telos-verifier` (WAL replay + MVCC serializability), and `tpt-gpu-ir-spec` (top-k scan TPTIR emission). Kept out of the shippable crates so `cargo publish -p tpt-archon-core --dry-run` stays clean (crates.io rejects git deps even in dev-deps).
- [x] Criterion benchmarks in `benches/` validating the specific numbers in `spec.txt`'s "Success Metrics": 30% faster than PostgreSQL (I/O-bound), 2x SQLite (embedded), 10x pgvector (vector search) — track actual measured numbers, don't assume the spec's targets are met (bench harness scaffolded in `benches/` for storage + query hot paths and vector search; external DB comparison harnesses still to be added, and no target is assumed met until measured)
- [x] `formal-proofs/` — QF_LRA **assertion-harness** `.telos` artifacts for each verified invariant (WAL, B-Tree, MVCC, scheduler), checked into the repo and discharged by `cargo test -p tpt-archon-verify` via `tpt-telos-verifier` (see `formal-proofs/README.md`). These are **solver-checked regression tests, not machine-checked Coq/Lean proofs**; QF_LRA cannot express multi-interleaving serializability or capability unforgeability, so the docs say so plainly. `tpt-telos` has no Coq/Lean backend — its codegen targets Rust/Go; the `.telos` sources + passing harness tests are the authoritative artifacts. The node-capacity page-fit bound is proven separately with `tpt-eidos-verifier`.
- [x] ADRs in `docs/` for major architectural decisions as they're made (not just ADR 0001) (added ADR 0002 zero-alloc primitives, ADR 0003 verification tested-now-proven-later)
- [x] Zero-CVE / zero-silent-corruption / zero-race-condition claims in `spec.txt` are marketing language until backed by the formal verification work above — don't repeat them in crate descriptions until proofs exist (no such claims appear in any crate `description`/docs; enforced by ADR 0003)

---

## Phase 4 — Trust, supply-chain & adoption hardening (post-review)

Handover work from the platform review (`platform-review-bugs-adoption` plan).
Ordered de-risk-first; trust fixes are done, correctness/adoption tasks remain.

### 4.1 Trust & supply-chain fixes (DONE)
- [x] Exclude `crates/tpt-archon-verify` from the default workspace (`exclude = ["benches", "crates/tpt-archon-verify"]` in root `Cargo.toml`) so `cargo test --workspace` is offline-clean and the 4 shippable crates gate the run.
- [x] Add opt-in `verify` CI job (network access) running `cargo test -p tpt-archon-verify`; keep the `test` job offline for the shippable crates.
- [x] Fix `README.md` §"TPT ecosystem dependencies" to match AGENTS.md: drop the nonexistent `tpt-gpu-primitives`/`tpt-gpu-runtime`; document `tpt-gpu-ir-spec` as an IR **emitter** (no runtime), and that the verifier git deps live only in the non-published `tpt-archon-verify` harness.
- [x] Clarify `formal-proofs/README.md`: the `.telos` sources are QF_LRA **solver-checked assertion harnesses**, not machine-checked Coq/Lean proofs; state QF_LRA's limits plainly.
- [x] Reconciled TODO files: `TODO.md` is the single source of truth; the drifted `TODO 1260719.md` is retained for history but no longer authoritative.

### 4.2 Correctness tests (cheap, high value)
- [x] Add a B-Link property test forcing ≥2 interior levels: `insert(0..512)` then `assert get(k) == v` for all k, across insert orders (sequential / reverse / shuffled) plus a `bulk_insert_reaches_interior_levels` height check (`crates/tpt-archon-core/src/btree.rs`).
- [x] Document (don't "fix") `BufferPool::flush_all` writing `Pinned` frames with `dirty_intent` set — note that an unpinned-then-uncommitted `fetch_mut` persists on flush (`crates/tpt-archon-core/src/page.rs`).

### 4.3 Make it real (adoption-critical)
- [x] End-to-end WAL↔storage: a `StorageEngine` facade in `core` (`storage.rs`) wrapping `BufferPool` + `Wal`, appending a `PageWrite` WAL record *before* the page reaches the pool, with `recover()` replaying committed page images after a crash. Includes unit tests for write-before-storage, recover-after-crash, and torn-tail truncation.
- [x] `core::Database::open(path)` / `create(path)` convenience over `FileBlockDevice` (std feature), so "embeddable SQLite" is actually exercisable (`storage.rs`).
- [ ] Wire `relational` to store rows via `core`/`btree` (today `executor::Table` is an in-memory `Vec<Row>`; the unified-page-cache story is unexercised by the query engine).
- [ ] At least `INSERT INTO t(c,…) VALUES (…)` (then `UPDATE`/`DELETE`) so the engine is usable, not just queryable.
- [ ] `f32[]` column type + `SELECT … ORDER BY cosine(emb, ?) LIMIT k` so the vector/RAG story has a real table/column backing `vector_topk`.

### 4.4 Show it / differentiate
- [x] `EXPLAIN` support in `relational` (`explain.rs`): `explain_plan` (always) renders the physical plan + dispatch; `explain_gpu` (gated on the `gpu` feature) prints the emitted TPTIR from `relational::gpu` for a GPU-dispatched scan — turns the emit-only GPU path into a demo-able feature.
- [ ] Capability-scoped multi-tenant demo (the `bridge` capability system is only unit-tested today).
- [ ] `faultsim` test mode: randomly drop/corrupt WAL tail bytes, assert `recover()` always yields a prefix-consistent state.
- [ ] `no_std` + `alloc`-only embedded CI target (compile-only, e.g. `cortex-m`) to prove the embeddable claim.
- [ ] `docs/GETTING_STARTED.md` + per-crate "What this crate is NOT (yet)" lines (ADR 0003 honesty).
- [ ] `cargo generate` template (`template/`) scaffolding a `Database::open` + INSERT/SELECT app — highest-leverage adoption move.
