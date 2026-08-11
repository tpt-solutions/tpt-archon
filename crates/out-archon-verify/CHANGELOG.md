# Changelog

All notable changes to this crate are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
### Changed
### Fixed

## [0.1.0] - 2026-08-05

### Added
- Verification harness (`out-archon-verify`): the home for external-ecosystem
  verification of the archon storage stack, pulling in the published TPT
  ecosystem verifiers (not runtime deps, never linked into the shippable crates).
- `eidos` module — proves the B-Link tree node-capacity invariant via the
  QF_LRA decision procedure (`tpt-eidos-verifier`).
- `telos` module — formal proof extraction + verification (`tpt-telos-parser` →
  `tpt-telos-ir` → `tpt-telos-verifier`) for the WAL replay, MVCC
  serializability, B-Link tree structural, and scheduler deadlock-freedom
  invariants.
- `gpu` module — smoke test of the `tpt-gpu-ir-spec` emitter lowering a
  vectorized top-k scan into TPTIR text (emitter only; never executes).
- `manifest` module — verifies every `.telos` source under `formal-proofs/` has
  a matching `<name>.telos.proof.json` whose recorded SHA-256 digest matches the
  source, failing CI on a missing or tampered manifest.

### Changed
- Renamed from `tpt-archon-verify` to `out-archon-verify` to carry the
  `out-archon-` not-published-crate prefix (see `AGENTS.md`); rejoined the
  default workspace now that `tpt-eidos-verifier`/`tpt-telos-*`/`tpt-gpu-ir-spec`
  are all published to crates.io, swapping git+rev pins for ordinary version
  requirements (Phase 9). Per ADR 0003 the proofs establish claims that are
  *tested now, proven later*; no zero-CVE / zero-corruption language is implied.
