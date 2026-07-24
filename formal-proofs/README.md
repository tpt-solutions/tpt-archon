# formal-proofs

Solver-checked **assertion harnesses** for `tpt-archon`, expressed in the
`tpt-telos` QF_LRA proof language and checked by `tpt-telos-verifier` (via the
`out-archon-verify` harness).

**Accuracy note:** these `.telos` sources are QF_LRA assertion harnesses — they
encode the numeric/structural invariants the algorithms rely on and discharge
them with `tpt-telos`'s built-in solver. They are **not** machine-checked
proofs in the Coq/Lean sense, and QF_LRA cannot express properties such as full
multi-interleaving serializability or capability unforgeability. Treat the
passing `out-archon-verify` tests as **strong regression checks**, not
end-to-end formal guarantees. The authoritative guarantee for node page-fit is
the `const` assertion `btree::assert_node_fits_page` in `tpt-archon-core`;
everything here is verification-tested, not proven in a foundational proof
assistant. See ADR 0003.

## Proof artifacts (`.telos` sources)

| Source                 | Invariant                                                      | Mirrors                          | Status   |
| ---------------------- | -------------------------------------------------------------- | -------------------------------- | -------- |
| `wal.telos` (in harness) | WAL replay restores durable state (`durable' == flushed'`)   | `tpt-archon-core::wal`           | solver-checked |
| `mvcc.telos` (in harness) | MVCC commit conflict keeps `<= 1` committed txn             | `tpt-archon-relational::mvcc`    | solver-checked |
| `btree.telos`          | Every leaf keeps `1 <= keys <= NODE_CAPACITY` across insert/split | `tpt-archon-core::btree`   | solver-checked |
| `scheduler.telos`      | Round-robin progress / no held-resource cycle (deadlock-free) | `tpt-archon-kernel::scheduler`   | solver-checked |

The node-capacity **page-fit** bound (a full node fits in `PAGE_SIZE`) is proven
separately with `tpt-eidos-verifier` in `crates/out-archon-verify/src/eidos.rs`,
complementing the structural `btree.telos` check above.

## How to verify

The proofs run as part of the workspace test suite:

```sh
cargo test -p out-archon-verify
```

This compiles the `.telos` sources under this directory, extracts verification
problems (`tpt-telos-parser` → `tpt-telos-ir`), and discharges them with
`tpt-telos-verifier`. The structural invariants for the B-Link tree and the
cooperative scheduler are exercised here as solver-checked regression tests
(see the accuracy note above).

To verify a single file with the standalone `tpt-telos` frontend
(built from `github.com/tpt-solutions/tpt-telos` at the rev pinned in
`crates/out-archon-verify/Cargo.toml`):

```sh
telos verify formal-proofs/btree.telos
telos verify formal-proofs/scheduler.telos
```

## On Coq/Lean artifacts

`tpt-telos` does **not** emit Coq or Lean source — its codegen backends target
Rust and Go (and a C-ABI FFI bridge), and its verification path is the internal
QF_LRA solver used above. There is therefore no machine-generated `.v` / `.lean`
artifact to check in. The authoritative, machine-checked proof artifacts are the
`.telos` sources plus the passing `out-archon-verify` tests in this repository.
If a Coq/Lean backend is added to `tpt-telos` later, regenerate from these
sources and check the outputs in here.

See [ADR 0003](../docs/0003-verification-tested-now-proven-later.md). Until a
foundational proof assistant backend exists for `tpt-telos`, `spec.txt`'s
zero-CVE / zero-silent-corruption / zero-race-condition claims are not
repeated in any published crate description, and these harnesses are described
as solver-checked, not proven.
