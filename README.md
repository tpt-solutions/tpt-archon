# tpt-archon

**tpt-archon** is a vertically integrated, proof-native computing stack that
eliminates the boundaries between storage, operating system, and database.
Built inside-out on Rust's ownership model and formal verification, it unifies
the page cache, kernel memory management, and database buffer pool into a
single, zero-copy address space — see [`spec.txt`](spec.txt) for the full
design document.

## Status

Early but functional across all four crates: each phase's core functionality is
implemented and tested (CPU-only where GPU/external verification crates are not
yet available). Nothing here is production-ready; see [`TODO.md`](TODO.md) for the
live checklist and what is deferred (formal-verification and `tpt-gpu`
integrations, publishing).

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

- [`tpt-eidos`](https://github.com/tpt-solutions/tpt-eidos) — compile-time,
  dependently-typed structural invariants (B-Tree node capacity, capability
  security).
- [`tpt-telos`](https://github.com/tpt-solutions/tpt-telos) — formal
  verification of critical invariants (WAL crash consistency, MVCC
  serializability, scheduler deadlock-freedom).
- [`tpt-gpu`](https://github.com/tpt-solutions/tpt-gpu) (via
  `tpt-gpu-primitives` / `tpt-gpu-runtime`) — hardware-agnostic GPU compute
  for vector search, aggregations, and ML UDFs in `tpt-archon-relational`.

There is no `tpt-zero-bytes` crate (referenced in the original design doc but
never built anywhere in the ecosystem); the zero-allocation I/O primitives
`tpt-archon-core` needs are implemented directly in that crate instead.

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
