# formal-proofs

Verified invariants for `tpt-archon`, expressed in the `tpt-telos` QF_LRA
proof language and checked by `tpt-telos-verifier` (via the
`tpt-archon-verify` harness).

## Proof artifacts (`.telos` sources)

| Source                 | Invariant                                                      | Mirrors                          | Status   |
| ---------------------- | -------------------------------------------------------------- | -------------------------------- | -------- |
| `wal.telos` (in harness) | WAL replay restores durable state (`durable' == flushed'`)   | `tpt-archon-core::wal`           | proven   |
| `mvcc.telos` (in harness) | MVCC commit conflict keeps `<= 1` committed txn             | `tpt-archon-relational::mvcc`    | proven   |
| `btree.telos`          | Every leaf keeps `1 <= keys <= NODE_CAPACITY` across insert/split | `tpt-archon-core::btree`   | proven   |
| `scheduler.telos`      | Round-robin progress / no held-resource cycle (deadlock-free) | `tpt-archon-kernel::scheduler`   | proven   |

The node-capacity **page-fit** bound (a full node fits in `PAGE_SIZE`) is proven
separately with `tpt-eidos-verifier` in `crates/tpt-archon-verify/src/eidos.rs`,
complementing the structural `btree.telos` proof above.

## How to verify

The proofs run as part of the workspace test suite:

```sh
cargo test -p tpt-archon-verify
```

This compiles the `.telos` sources under this directory, extracts verification
problems (`tpt-telos-parser` → `tpt-telos-ir`), and discharges them with
`tpt-telos-verifier`. All structural invariants for the B-Link tree and the
cooperative scheduler are now covered, not merely runtime-tested.

To verify a single file with the standalone `tpt-telos` frontend
(built from `github.com/tpt-solutions/tpt-telos` at the rev pinned in
`crates/tpt-archon-verify/Cargo.toml`):

```sh
telos verify formal-proofs/btree.telos
telos verify formal-proofs/scheduler.telos
```

## On Coq/Lean artifacts

`tpt-telos` does **not** emit Coq or Lean source — its codegen backends target
Rust and Go (and a C-ABI FFI bridge), and its verification path is the internal
QF_LRA solver used above. There is therefore no machine-generated `.v` / `.lean`
artifact to check in. The authoritative, machine-checked proof artifacts are the
`.telos` sources plus the passing `tpt-archon-verify` tests in this repository.
If a Coq/Lean backend is added to `tpt-telos` later, regenerate from these
sources and check the outputs in here.

See [ADR 0003](../docs/0003-verification-tested-now-proven-later.md). Until the
proofs in this directory exist (they now do), `spec.txt`'s zero-CVE /
zero-silent-corruption / zero-race-condition claims are not repeated in any
published crate description.
