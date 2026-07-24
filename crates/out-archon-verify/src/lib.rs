//! Verification harness for `tpt-archon`.
//!
//! This crate is the home for *external-ecosystem* verification of the
//! archon storage stack. It is intentionally **not** published: every other
//! crate must `cargo publish`-dry-run clean (no git deps), so all verification
//! work that pulls in `tpt-eidos-verifier` / `tpt-telos-*` / `tpt-gpu-ir-spec`
//! lives here instead.
//!
//! Three independent verifiers are exercised:
//!
//! * **`eidos`** — a QF_LRA decision procedure ([`tpt_eidos_verifier`]) used to
//!   prove the B-Link tree *node-capacity invariant*: a full node (header +
//!   `NODE_CAPACITY` key/value slots + right-link) can never overflow the
//!   `PAGE_SIZE` page it is serialized into.
//! * **`telos`** — formal proof extraction + verification ([`tpt_telos_parser`]
//!   → [`tpt_telos_ir`] → [`tpt_telos_verifier`]) for the WAL replay invariant,
//!   the MVCC serializability invariant, the B-Link tree *structural* invariant
//!   (every leaf keeps `1 <= keys <= NODE_CAPACITY` across insert/replace/split,
//!   see `formal-proofs/btree.telos`), and the cooperative scheduler's
//!   deadlock-freedom / progress property (see `formal-proofs/scheduler.telos`).
//! * **`gpu`** — a smoke test of the [`tpt_gpu_ir_spec`] emitter: the
//!   relational engine can lower a vectorized top-k scan into TPTIR text. This
//!   crate is an *emitter*, not a runtime — we only assert the IR is produced,
//!   never that it executes.
//! * **`manifest`** — verifies every `.telos` source under `formal-proofs/`
//!   has a `<name>.telos.proof.json` manifest whose recorded SHA-256 digest
//!   matches the source file's current bytes, so CI fails on a missing or
//!   tampered manifest rather than silently trusting a drifted `.telos` file.
//!
//! Per ADR 0003 these proofs/tests establish claims that are *tested now,
//! proven later*; no zero-CVE / zero-corruption language is implied.

pub mod eidos;
pub mod gpu;
pub mod manifest;
pub mod telos;
