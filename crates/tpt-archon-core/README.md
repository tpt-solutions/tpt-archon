# tpt-archon-core

Phase 1 of [tpt-archon](https://github.com/tpt-solutions/tpt-archon): a
`#![no_std]`, zero-allocation storage engine providing crash-safe,
concurrent access to fixed-size pages on a block device.

## Modules

- [`block`](src/block/mod.rs) — the [`BlockDevice`] backend abstraction with
  `InMemoryBlockDevice` and (behind the `std` feature) `FileBlockDevice`.
- [`zerocopy`](src/zerocopy.rs) — fixed-capacity byte buffers (`FixedBuf`) and
  little-endian, bounds-checked `Cursor`/`Reader` for page/WAL framing with no
  heap allocation and no `serde`.
- [`page`](src/page.rs) — a fixed-size `Page` plus a `BufferPool` with a
  `Free`/`Clean`/`Dirty`/`Pinned` state machine and LRU eviction with
  dirty-page writeback.
- [`wal`](src/wal.rs) — an append-only, LSN-ordered write-ahead log with CRC32
  framing and crash-recovery replay that truncates a torn tail.
- [`btree`](src/btree.rs) — a Lehman & Yao B-Link tree (right-links + high
  keys) with point lookups, range scans and node-splitting inserts. Node
  capacity is checked at compile time to fit within a page.
- [`storage`](src/storage.rs) — a `StorageEngine` facade wiring the
  `BufferPool` to the `Wal`: page writes go through the write-ahead log
  before main storage, and `StorageEngine::recover` replays it after a crash.
- [`faultsim`](src/faultsim.rs) — a *testing* tool, not a runtime feature:
  injects power-loss-shaped corruption (truncated tails, flipped bytes,
  zeroed records) and asserts recovery always yields a prefix-consistent
  state. The runtime counterpart to the `tpt-telos` replay-consistency
  harness in `formal-proofs/`.

> **No `tpt-zero-bytes`.** That crate was never built. The zero-allocation
> primitives live in [`zerocopy`](src/zerocopy.rs) on purpose — do not add a
> dependency on a crate by that name.

## Features

- `std` (default) — enables the file-backed `FileBlockDevice` (needs
  `std::fs`). Build with `--no-default-features` for a fully `no_std`
  configuration.

## Example

```bash
cargo run -p tpt-archon-core --example storage_tour
```

See [`examples/storage_tour.rs`](examples/storage_tour.rs) for a tour through
the block device, buffer pool, WAL, and B-Link tree.

## License

Licensed under either of [MIT](../../LICENSE-MIT) or
[Apache-2.0](../../LICENSE-APACHE) at your option.

[`BlockDevice`]: src/block/mod.rs
