//! Block device abstraction: the storage backend the rest of the engine
//! builds on.
//!
//! A [`BlockDevice`] exposes fixed-size blocks (default 4 KiB) addressed by a
//! [`BlockId`]. Backends provided here:
//!
//! - [`InMemoryBlockDevice`] — a heap-backed device for tests. `no_std`, uses
//!   `alloc`. Available in every configuration.
//! - [`FileBlockDevice`] — a `std::fs::File`-backed device for real
//!   persistence, gated behind the default `std` feature so the crate stays
//!   `no_std`-clean without it.

use core::fmt;

/// Identifies a single fixed-size block within a [`BlockDevice`].
pub type BlockId = u64;

/// Errors returned by [`BlockDevice`] operations.
///
/// Every fallible operation on a block device returns one of these; the set is
/// intentionally exhaustive so callers can reason about each failure mode.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum StorageError {
    /// A `block_id` outside the device's capacity was addressed.
    OutOfBounds {
        /// The offending block id.
        block_id: BlockId,
        /// The number of blocks the device actually has.
        block_count: u64,
    },
    /// A read supplied a buffer whose length is not exactly `BLOCK_SIZE`.
    ShortRead {
        /// The buffer length that was supplied.
        got: usize,
        /// The length that was required (`BLOCK_SIZE`).
        expected: usize,
    },
    /// A write supplied data whose length is not exactly `BLOCK_SIZE`.
    ShortWrite {
        /// The data length that was supplied.
        got: usize,
        /// The length that was required (`BLOCK_SIZE`).
        expected: usize,
    },
    /// An underlying I/O error occurred (only produced by `std` backends).
    ///
    /// The raw `std::io::Error` is reduced to its [`std::io::ErrorKind`] as a
    /// `u8` so this variant is representable without `std`.
    Io {
        /// The `std::io::ErrorKind` discriminant, best-effort.
        kind: u8,
    },
    /// A `sync`/flush to durable storage failed.
    SyncFailed,
    /// The buffer pool is full and every frame is pinned, so no page could be
    /// evicted to make room.
    AllFramesPinned,
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StorageError::OutOfBounds {
                block_id,
                block_count,
            } => write!(
                f,
                "block id {block_id} out of bounds (device has {block_count} blocks)"
            ),
            StorageError::ShortRead { got, expected } => {
                write!(f, "short read: buffer len {got}, expected {expected}")
            }
            StorageError::ShortWrite { got, expected } => {
                write!(f, "short write: data len {got}, expected {expected}")
            }
            StorageError::Io { kind } => write!(f, "i/o error (kind {kind})"),
            StorageError::SyncFailed => write!(f, "sync to durable storage failed"),
            StorageError::AllFramesPinned => {
                write!(f, "buffer pool full: all frames are pinned")
            }
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for StorageError {}

/// A fixed-block-size storage backend.
///
/// All backends use the same [`BLOCK_SIZE`](BlockDevice::BLOCK_SIZE). Reads and
/// writes operate on exactly one block at a time and require the caller's
/// buffer to be exactly `BLOCK_SIZE` bytes — this keeps the hot path
/// allocation-free and the layout predictable for the unified page cache in
/// later phases.
pub trait BlockDevice {
    /// The size of a single block, in bytes. 4 KiB by default.
    const BLOCK_SIZE: usize = 4096;

    /// Reads block `block_id` into `buffer`.
    ///
    /// `buffer` must be exactly [`BLOCK_SIZE`](BlockDevice::BLOCK_SIZE) bytes.
    fn read_block(&self, block_id: BlockId, buffer: &mut [u8]) -> Result<(), StorageError>;

    /// Writes `data` to block `block_id`.
    ///
    /// `data` must be exactly [`BLOCK_SIZE`](BlockDevice::BLOCK_SIZE) bytes.
    fn write_block(&mut self, block_id: BlockId, data: &[u8]) -> Result<(), StorageError>;

    /// Flushes all pending writes to durable storage.
    fn sync(&mut self) -> Result<(), StorageError>;

    /// The total number of blocks addressable on this device.
    fn block_count(&self) -> u64;
}

mod memory;
pub use memory::InMemoryBlockDevice;

#[cfg(feature = "std")]
mod file;
#[cfg(feature = "std")]
pub use file::FileBlockDevice;
