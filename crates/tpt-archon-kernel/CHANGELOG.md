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
- Crate scaffold (Phase 2b of tpt-archon): a capability-based microkernel with a
  unified page cache (user-space-first; not yet running on bare metal).
- `scheduler` module — a cooperative, round-robin async `Scheduler` running one
  `Task` per DB connection.
- `ipc` module — capability-bearing `Message` passing via a `MessageRouter`
  (a message is only delivered if the sender holds a write capability for the
  destination channel).
- `memory` module — `UnifiedMemory`, where the kernel page cache *is* the
  database buffer pool: a single allocation shared through the bridge's
  `UnifiedPageCache` trait, with capability-checked access; also `map_read_zero_copy`
  (read-only, OS-`mmap`-backed zero-copy, `mmap` feature) over the bridge's
  `MmapPageSource` trait.
- `io_uring_backend` module (Linux only, `io-uring-backend` feature) — a real
  `io_uring` `Reactor` plus `IoReadTask`/`IoWriteTask` ordinary `Task`s that
  submit I/O and yield `Pending` until the kernel reports completion, plugging
  into `Scheduler` unchanged.
