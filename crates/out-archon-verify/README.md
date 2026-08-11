# out-archon-verify

Verification harness for
[TPT Archon](https://github.com/tpt-solutions/tpt-archon). Package name
`out-archon-verify`. Not published (`publish = false`) — an internal
verification tool, not one of the layered architecture crates.

This crate is the home for *external-ecosystem* verification of the archon
storage stack. Every other crate must `cargo publish`-dry-run clean (no git
deps), so all verification work that pulls in the published TPT ecosystem
verifiers — `tpt-eidos-verifier` / `tpt-telos-*` / `tpt-gpu-ir-spec` — lives
here instead. They are verification/tooling deps, **not** runtime deps, and are
never pulled into the shippable crates.

## Three independent verifiers

- **`eidos`** — a QF_LRA decision procedure ([`tpt_eidos_verifier`]) used to
  prove the B-Link tree *node-capacity invariant*: a full node (header +
  `NODE_CAPACITY` key/value slots + right-link) can never overflow the
  `PAGE_SIZE` page it is serialized into. This is the formal counterpart to the
  compile-time node-fits-page check in `tpt-archon-core`'s `btree` module.
- **`telos`** — formal proof extraction + verification
  ([`tpt_telos_parser`] → [`tpt_telos_ir`] → [`tpt_telos_verifier`]) for the WAL
  replay invariant, the MVCC serializability invariant, the B-Link tree
  *structural* invariant (every leaf keeps `1 <= keys <= NODE_CAPACITY` across
  insert/replace/split), and the cooperative scheduler's deadlock-freedom /
  progress property. Proof sources live under `formal-proofs/`.
- **`gpu`** — a smoke test of the [`tpt_gpu_ir_spec`] emitter: the relational
  engine can lower a vectorized top-k scan into stable TPTIR text. This crate is
  an *emitter*, not a runtime — we only assert the IR is produced, never that it
  executes.

## Manifest integrity

- **`manifest`** — verifies every `.telos` source under `formal-proofs/` has a
  `<name>.telos.proof.json` manifest whose recorded SHA-256 digest matches the
  source file's current bytes, so CI fails on a missing or tampered manifest
  rather than silently trusting a drifted `.telos` file.

## Run

```sh
cargo test -p out-archon-verify
```

Note: this crate is a normal workspace member as of 2026-08-04 (it was
previously excluded while the ecosystem verifiers were git-hosted; they are now
all on crates.io — see `TODO.md` Phase 9).

Per ADR 0003 these proofs/tests establish claims that are *tested now, proven
later*; no zero-CVE / zero-corruption language is implied.

## License

Licensed under either of [MIT](../../LICENSE-MIT) or
[Apache-2.0](../../LICENSE-APACHE) at your option.
