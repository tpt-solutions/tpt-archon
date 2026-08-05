# ADR 0003: Verification-adjacent invariants are tested now, proven later

## Status

Accepted.

## Context

`spec.txt` and `TODO.md` call for `tpt-eidos` (compile-time dependent-type
invariants) and `tpt-telos` (Coq/Lean formal proofs) to guarantee properties
like WAL crash-consistency, B-Tree structural integrity, MVCC serializability,
and scheduler deadlock-freedom. Neither `tpt-eidos` nor `tpt-telos` is
available as a published crate today, and `spec.txt`'s "zero CVE / zero silent
corruption / zero race condition" claims are marketing language until real
proofs exist.

## Decision

Build the invariants into the code and exercise them with tests now, while
keeping a clean seam for the formal tools later:

- **Compile-time checks that can be done without `tpt-eidos`** are done with
  `const` assertions. B-Link node capacity is verified to fit within a page via
  a `const fn` evaluated in a `const` context
  (`btree::assert_node_fits_page`), which fails the build if violated — a
  genuine compile-time guarantee.
- **Properties that need `tpt-telos`** (WAL replay consistency, MVCC
  serializability, scheduler progress) are implemented to hold, documented as
  the intended proof target with a pointer to `formal-proofs/`, and covered by
  targeted tests (torn-tail WAL truncation, write-write / read-write MVCC
  conflicts, round-robin scheduler fairness).

Crate descriptions and docs do **not** repeat the zero-CVE/zero-corruption
claims; they describe what is implemented and what is tested.

## Consequences

- The properties are enforced in practice today and regression-guarded by
  tests, without blocking on unpublished dependencies.
- When `tpt-eidos`/`tpt-telos` land, they slot in against a codebase already
  organized around these invariants, and the corresponding `formal-proofs/`
  artifacts can be generated and linked.
- Until proofs exist, no marketing correctness claim is made in any published
  crate metadata.

## Update (2026-08-04)

The Context section's premise that "neither `tpt-eidos` nor `tpt-telos` is
available as a published crate today" is no longer true: `tpt-eidos-verifier`
(0.2.0), `tpt-telos-verifier`/`tpt-telos-ir`/`tpt-telos-parser` (0.1.1), and
`tpt-gpu-ir-spec` (0.1.0) are all published to crates.io as of late July/early
August 2026. `crates/out-archon-verify`'s dependencies were switched from
`git`+`rev` pins to ordinary version requirements, and the crate rejoined the
default workspace (see `TODO.md` Phase 9) since the "crates.io rejects git
deps" constraint that justified excluding it no longer applies.

This does **not** change the Decision above: `tpt-telos` still has no Coq/Lean
backend (its codegen targets Rust/Go), so the QF_LRA solver-checked assertion
harnesses in `formal-proofs/` remain what's actually being produced, not a
stepping stone that publication alone resolves. The publish status was never
the load-bearing reason for this ADR's approach — it was one of two.
