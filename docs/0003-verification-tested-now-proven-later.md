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
