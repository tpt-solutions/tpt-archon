//! `tpt-archon-core`: a `no_std`, zero-allocation storage engine.
//!
//! This is Phase 1 of the `tpt-archon` stack (see the workspace-root
//! `spec.txt` and `TODO.md`). It is a single-crate storage engine, embeddable
//! like SQLite's storage layer, providing:
//!
//! - [`block`] — the [`BlockDevice`](block::BlockDevice) backend abstraction
//!   with in-memory and (behind the `std` feature) file-backed backends.
//! - [`zerocopy`] — fixed-capacity byte buffers and zero-copy
//!   (de)serialization helpers used on the read/write hot path.
//! - [`page`] — a fixed-size page abstraction and an LRU buffer pool with a
//!   `Free`/`Clean`/`Dirty`/`Pinned` state machine and dirty-page writeback.
//! - [`wal`] — an append-only, LSN-ordered write-ahead log with crash-recovery
//!   replay.
//! - [`btree`] — a B-Link tree with point lookups, range scans and concurrent
//!   inserts.
//! - [`storage`] — a [`StorageEngine`](storage::StorageEngine) facade wiring
//!   the buffer pool to the WAL, so page writes actually go through the
//!   write-ahead log before main storage (not just each piece tested alone).
//! - [`faultsim`] — a *testing* tool (not a runtime feature): injects
//!   power-loss-shaped corruption (truncated tails, flipped bytes, zeroed
//!   records) and asserts [`StorageEngine::recover`](storage::StorageEngine::recover)
//!   always yields a prefix-consistent state.
//!
//! # Zero-allocation, and the missing `tpt-zero-bytes`
//!
//! The [`zerocopy`] module exists *because there is no `tpt-zero-bytes`
//! crate* anywhere in the TPT ecosystem — it was never built. Do not "helpfully"
//! add a dependency on a crate by that name; the primitives live here on
//! purpose so the read/write hot path allocates nothing.
//!
//! # `no_std`
//!
//! The crate is `#![no_std]` by default (it uses `alloc`). The default `std`
//! feature only adds the file-backed [`BlockDevice`](block::BlockDevice); build
//! with `--no-default-features` for a fully `no_std` configuration.
#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_op_in_unsafe_fn)]

extern crate alloc;

pub mod block;
pub mod btree;
pub mod faultsim;
pub mod page;
pub mod storage;
pub mod wal;
pub mod zerocopy;
