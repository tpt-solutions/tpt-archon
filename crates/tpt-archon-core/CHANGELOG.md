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
- Crate scaffold (Phase 1 of tpt-archon): a `#![no_std]`, zero-allocation
  storage engine providing crash-safe, concurrent access to fixed-size pages
  on a block device.
- `block` module — the `BlockDevice` trait and backends: `InMemoryBlockDevice`,
  `FileBlockDevice` (behind the `std` feature), and `MmapBlockDevice` (behind the
  opt-in `mmap` feature) for real OS-`mmap`-backed, zero-copy *read* access.
- `zerocopy` module — `FixedBuf`, and little-endian, bounds-checked
  `Cursor`/`Reader` for page/WAL framing with no heap allocation and no `serde`
  (replacing the never-built `tpt-zero-bytes`).
- `page` module — a fixed-size `Page` plus a `BufferPool` with a
  `Free`/`Clean`/`Dirty`/`Pinned` state machine and LRU eviction with
  dirty-page writeback.
- `wal` module — an append-only, LSN-ordered write-ahead log with CRC32 framing
  and crash-recovery replay that truncates a torn tail.
- `btree` module — a Lehman & Yao B-Link tree (right-links + high keys) with
  point lookups, range scans, and node-splitting inserts, with the node-capacity
  invariant checked at compile time to fit within a page.
- `storage` module — a `StorageEngine` facade wiring `BufferPool` to `Wal`: page
  writes go through the write-ahead log before main storage, and
  `StorageEngine::recover` replays committed page images after a crash; plus a
  file-backed `Database::open`/`create` convenience (`std` feature).
- `faultsim` module — a *testing* tool (not a runtime feature) that injects
  power-loss-shaped corruption and asserts recovery always yields a
  prefix-consistent state.
- `storage_tour` example walking through block device, buffer pool, WAL, and
  B-Link tree.
- B-Link property tests forcing ≥2 interior levels (sequential / reverse /
  shuffled insert orders) with full-key lookup assertions.
