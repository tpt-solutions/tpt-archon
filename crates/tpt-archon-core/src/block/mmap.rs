//! Real OS-level memory-mapped, zero-copy **read** access (requires the
//! `mmap` feature).
//!
//! [`MmapBlockDevice`] wraps a real `mmap(2)`/`CreateFileMappingW` mapping
//! (via the cross-platform [`memmap2`] crate) over a file. [`page_ref`]
//! borrows a block's bytes directly out of that mapping — no allocation, no
//! copy, no `BufferPool` involved — unlike [`FileBlockDevice`](super::FileBlockDevice),
//! whose `read_block` always copies into a caller-supplied buffer.
//!
//! # Why read-only
//!
//! This module deliberately does not offer a writable mapping. `StorageEngine`
//! (see `crate::storage`) enforces a write-ahead invariant — a WAL record is
//! durable *before* the corresponding page write is considered committed — by
//! ordering two independent operations (`Wal::append`/`sync` then
//! `BlockDevice::write_block`/`sync`). A writable mmap's dirty pages are
//! flushed to disk by the OS on its own schedule, which would require
//! hand-rolled `msync` calls at exactly the right points to preserve that
//! ordering — a correctness hazard not worth taking for a database whose
//! stated differentiator is crash-recovery correctness, without a concrete
//! need driving it (the invariant does have a `tpt-telos`-formal backstop —
//! `formal-proofs/wal.telos` — plus `faultsim` fuzzing and unit tests). See
//! `TODO.md`'s Phase 2b entry for the full rationale. Writes continue to go
//! through the existing `write_block` path unchanged.
//!
//! # Snapshot, not a live view
//!
//! [`MmapBlockDevice::open`] maps the file as it exists at the moment of the
//! call. Bytes written by a separate writer afterwards are not guaranteed to
//! become visible (and never will if the file was extended, since the mapped
//! length is fixed at open time) — call `open` again for a fresh snapshot.
//! This is a deliberately conservative contract that avoids relying on mmap
//! coherence guarantees the OS doesn't promise.

use std::fs::File;
use std::path::Path;

use memmap2::Mmap;

use super::{BlockDevice, BlockId, StorageError};

fn io_err(e: std::io::Error) -> StorageError {
    StorageError::Io {
        kind: e.kind() as u8,
    }
}

/// A read-only [`BlockDevice`] backed by a real OS memory mapping.
///
/// Only available with the `mmap` Cargo feature (implies `std`).
pub struct MmapBlockDevice {
    // Keeps the file descriptor alive for the mapping's lifetime; never read
    // after construction (the mapping is the only access path).
    _file: File,
    mmap: Mmap,
    block_count: u64,
}

impl MmapBlockDevice {
    /// Opens `path` read-only and memory-maps its current contents.
    ///
    /// See the module docs for why this is a point-in-time snapshot rather
    /// than an auto-updating live view.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, StorageError> {
        let file = File::open(path).map_err(io_err)?;
        // SAFETY: `memmap2::Mmap::map`'s documented hazard is that another
        // process truncating or writing to the file while it's mapped can
        // produce a `SIGBUS`/inconsistent read. We accept the same
        // point-in-time-snapshot contract every caller of this type is
        // documented to rely on (see module docs); this device is read-only
        // and used for offline/snapshot access, not concurrent-with-writer
        // shared mapping.
        let mmap = unsafe { Mmap::map(&file) }.map_err(io_err)?;
        let block_count = mmap.len() as u64 / Self::BLOCK_SIZE as u64;
        Ok(Self {
            _file: file,
            mmap,
            block_count,
        })
    }

    /// Borrows block `block_id`'s bytes directly out of the OS mapping.
    ///
    /// Genuinely zero-copy: the returned reference points straight into the
    /// mapped memory, tied to `&self`'s lifetime — no allocation, no
    /// `BufferPool` frame, no pin/unpin bookkeeping to pair this call with.
    pub fn page_ref(&self, block_id: BlockId) -> Result<&[u8; Self::BLOCK_SIZE], StorageError> {
        if block_id >= self.block_count {
            return Err(StorageError::OutOfBounds {
                block_id,
                block_count: self.block_count,
            });
        }
        let start = block_id as usize * Self::BLOCK_SIZE;
        let end = start + Self::BLOCK_SIZE;
        self.mmap[start..end]
            .try_into()
            .map_err(|_| StorageError::ShortRead {
                got: self.mmap.len() - start,
                expected: Self::BLOCK_SIZE,
            })
    }
}

impl BlockDevice for MmapBlockDevice {
    fn read_block(&self, block_id: BlockId, buffer: &mut [u8]) -> Result<(), StorageError> {
        if buffer.len() != Self::BLOCK_SIZE {
            return Err(StorageError::ShortRead {
                got: buffer.len(),
                expected: Self::BLOCK_SIZE,
            });
        }
        // A compatibility shim for genericity over `D: BlockDevice` (tests,
        // faultsim) — this copies. The zero-copy path is `page_ref`.
        buffer.copy_from_slice(self.page_ref(block_id)?);
        Ok(())
    }

    fn write_block(&mut self, _block_id: BlockId, _data: &[u8]) -> Result<(), StorageError> {
        Err(StorageError::Unsupported)
    }

    fn sync(&mut self) -> Result<(), StorageError> {
        // Read-only: nothing is ever dirty.
        Ok(())
    }

    fn block_count(&self) -> u64 {
        self.block_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Database;

    fn temp_path(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "tpt-archon-core-mmap-{}-{}.bin",
            name,
            std::process::id()
        ));
        p
    }

    #[test]
    fn page_ref_is_stable_and_zero_copy() {
        let path = temp_path("stable");
        {
            let mut db = Database::create(&path, 4).unwrap();
            db.put(1, &[0xEEu8; MmapBlockDevice::BLOCK_SIZE]).unwrap();
        }
        let dev = MmapBlockDevice::open(&path).unwrap();
        let a = dev.page_ref(1).unwrap();
        let b = dev.page_ref(1).unwrap();
        // Same underlying mapping bytes, not a fresh copy each call.
        assert!(core::ptr::eq(a.as_ptr(), b.as_ptr()));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn written_then_committed_then_mmap_reads_it() {
        let path = temp_path("roundtrip");
        {
            let mut db = Database::create(&path, 4).unwrap();
            db.put(2, &[0xAB; MmapBlockDevice::BLOCK_SIZE]).unwrap();
        }
        let dev = MmapBlockDevice::open(&path).unwrap();
        assert_eq!(dev.page_ref(2).unwrap()[0], 0xAB);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rejects_out_of_bounds() {
        let path = temp_path("oob");
        {
            let _ = Database::create(&path, 1).unwrap();
        }
        let dev = MmapBlockDevice::open(&path).unwrap();
        assert!(matches!(
            dev.page_ref(5),
            Err(StorageError::OutOfBounds { .. })
        ));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn write_block_is_unsupported() {
        let path = temp_path("readonly");
        {
            let _ = Database::create(&path, 1).unwrap();
        }
        let mut dev = MmapBlockDevice::open(&path).unwrap();
        assert_eq!(
            dev.write_block(0, &[0u8; MmapBlockDevice::BLOCK_SIZE]),
            Err(StorageError::Unsupported)
        );
        let _ = std::fs::remove_file(&path);
    }
}
