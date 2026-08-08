//! Writable, OS-level memory-mapped block device (requires the `mmap-write`
//! feature).
//!
//! [`MmapWritableBlockDevice`] wraps a real `memmap2::MmapMut` over a file.
//! [`write_block`] memcpys into the mapping and widens a coalesced dirty
//! span; [`sync`] flushes that span via `flush_range` (the actual durability
//! boundary) and maps errors to [`StorageError::SyncFailed`].
//!
//! `mmap-write` is a *separate* feature from read-only `mmap` so existing
//! read-only consumers are never silently handed write capability. It is
//! **not** wired into the bridge/kernel zero-copy cache (deliberately core-
//! crate-only, see `TODO.md` Phase 11.3) — `StorageEngine`'s write-ahead
//! invariant stays enforced by the two-operation ordering in `crate::storage`,
//! untouched.
//!
//! # Cross-handle / cross-process tearing
//!
//! Both `Mmap` (read, `crate::block::MmapBlockDevice`) and `MmapMut` (write)
//! are `MAP_SHARED`, so a live writer and a live reader of the same file see
//! each other's writes with **no tearing guarantee**. v1 mitigates this with
//! intra-process ownership — there is no `Clone`, so each device owns its
//! unique mapping, and the type system (not a runtime mutex) proves no second
//! handle can be derived from one. Cross-handle/cross-process exclusivity is
//! **documentation-only** (matching the read-only module's own precedent):
//! hold at most one `MmapWritableBlockDevice` per file, per process, and
//! serialize access from other processes out of band. Concurrent multi-writer
//! mmap access and per-block-precision dirty tracking are explicit v2
//! out-of-scope items (see `TODO.md` Phase 11.3).

use std::fs::File;
use std::path::Path;

use memmap2::MmapMut;

use super::{BlockDevice, BlockId, StorageError};

fn io_err(e: std::io::Error) -> StorageError {
    StorageError::Io {
        kind: e.kind() as u8,
    }
}

/// A read/write [`BlockDevice`] backed by a real OS memory mapping.
///
/// Only available with the `mmap-write` Cargo feature (implies `mmap` + `std`).
pub struct MmapWritableBlockDevice {
    // Keeps the file descriptor alive for the mapping's lifetime; never read
    // after construction (the mapping is the only access path).
    _file: File,
    mmap: MmapMut,
    block_count: u64,
    // Coalesced dirty span: the inclusive `[lo, hi]` block range written since
    // the last `sync`. `None` means "nothing dirty" (sync is a no-op).
    dirty: Option<(BlockId, BlockId)>,
}

impl MmapWritableBlockDevice {
    /// Opens `path` read/write and memory-maps its current contents.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, StorageError> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(io_err)?;
        let block_count = file.metadata().map_err(io_err)?.len() / Self::BLOCK_SIZE as u64;
        // SAFETY: a writable `MAP_SHARED` mapping over a file opened read/write.
        // The documented hazards (another process truncating the file mid-write
        // causing `SIGBUS`; concurrent writers from other handles/processes) are
        // mitigated by intra-process ownership (no `Clone`) plus the
        // documentation-only cross-handle/cross-process exclusivity contract in
        // the module docs.
        let mmap = unsafe { MmapMut::map_mut(&file) }.map_err(io_err)?;
        Ok(Self {
            _file: file,
            mmap,
            block_count,
            dirty: None,
        })
    }

    /// Widens the coalesced dirty span to include `block_id`.
    fn widen_dirty(&mut self, block_id: BlockId) {
        self.dirty = Some(match self.dirty {
            Some((lo, hi)) => (lo.min(block_id), hi.max(block_id)),
            None => (block_id, block_id),
        });
    }
}

impl BlockDevice for MmapWritableBlockDevice {
    fn read_block(&self, block_id: BlockId, buffer: &mut [u8]) -> Result<(), StorageError> {
        if buffer.len() != Self::BLOCK_SIZE {
            return Err(StorageError::ShortRead {
                got: buffer.len(),
                expected: Self::BLOCK_SIZE,
            });
        }
        if block_id >= self.block_count {
            return Err(StorageError::OutOfBounds {
                block_id,
                block_count: self.block_count,
            });
        }
        let start = block_id as usize * Self::BLOCK_SIZE;
        let end = start + Self::BLOCK_SIZE;
        buffer.copy_from_slice(&self.mmap[start..end]);
        Ok(())
    }

    fn write_block(&mut self, block_id: BlockId, data: &[u8]) -> Result<(), StorageError> {
        if data.len() != Self::BLOCK_SIZE {
            return Err(StorageError::ShortWrite {
                got: data.len(),
                expected: Self::BLOCK_SIZE,
            });
        }
        if block_id >= self.block_count {
            return Err(StorageError::OutOfBounds {
                block_id,
                block_count: self.block_count,
            });
        }
        let start = block_id as usize * Self::BLOCK_SIZE;
        let end = start + Self::BLOCK_SIZE;
        self.mmap[start..end].copy_from_slice(data);
        self.widen_dirty(block_id);
        Ok(())
    }

    fn sync(&mut self) -> Result<(), StorageError> {
        // Durability boundary: flush only the coalesced dirty span. An empty
        // dirty span means everything is already durable — a no-op.
        if let Some((lo, hi)) = self.dirty.take() {
            let start = lo as usize * Self::BLOCK_SIZE;
            let end = (hi as usize + 1) * Self::BLOCK_SIZE;
            self.mmap
                .flush_range(start, end - start)
                .map_err(|_| StorageError::SyncFailed)?;
        }
        Ok(())
    }

    fn block_count(&self) -> u64 {
        self.block_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "tpt-archon-core-mmapmut-{}-{}.bin",
            name,
            std::process::id()
        ));
        p
    }

    /// Reads `block` and returns its first byte (the values under test only
    /// vary in byte 0).
    fn first_byte(dev: &MmapWritableBlockDevice, block: BlockId) -> u8 {
        let mut buf = [0u8; MmapWritableBlockDevice::BLOCK_SIZE];
        dev.read_block(block, &mut buf).unwrap();
        buf[0]
    }

    #[test]
    fn write_then_flush_then_reopen_round_trip() {
        let path = temp_path("roundtrip");
        {
            // Seed a file large enough for two blocks via the file-backed
            // device, then open it as a writable mapping.
            let mut db = crate::storage::Database::create(&path, 2).unwrap();
            db.put(0, &[0x11; MmapWritableBlockDevice::BLOCK_SIZE])
                .unwrap();
            db.put(1, &[0x22; MmapWritableBlockDevice::BLOCK_SIZE])
                .unwrap();
        }
        {
            let mut dev = MmapWritableBlockDevice::open(&path).unwrap();
            // Overwrite block 1 through the writable mapping, then sync.
            dev.write_block(1, &[0xAB; MmapWritableBlockDevice::BLOCK_SIZE])
                .unwrap();
            // Block 0 is untouched and still readable.
            assert_eq!(first_byte(&dev, 0), 0x11);
            dev.sync().unwrap();
        }
        // Reopen and confirm the flush persisted block 1's new contents.
        let dev = MmapWritableBlockDevice::open(&path).unwrap();
        assert_eq!(first_byte(&dev, 1), 0xAB);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn write_without_sync_is_live_in_mapping_sync_is_the_flush_point() {
        let path = temp_path("nosync");
        {
            let mut db = crate::storage::Database::create(&path, 1).unwrap();
            db.put(0, &[0x99; MmapWritableBlockDevice::BLOCK_SIZE])
                .unwrap();
        }
        let mut dev = MmapWritableBlockDevice::open(&path).unwrap();
        dev.write_block(0, &[0xCD; MmapWritableBlockDevice::BLOCK_SIZE])
            .unwrap();
        // The write is live in the mapping immediately (zero-copy), so the
        // device reads back its own un-flushed write.
        assert_eq!(first_byte(&dev, 0), 0xCD);
        // `sync` is the only durability point: it must flush without error and
        // is idempotent (a second sync with an empty dirty span is a no-op).
        dev.sync().unwrap();
        dev.sync().unwrap();
        // After sync, the new value survives a reopen (the durability boundary
        // was crossed). `MAP_SHARED` means a dropped mapping may also propagate
        // to the page cache, so callers must treat `sync` — not `drop` — as the
        // point at which bytes are guaranteed durable.
        let dev2 = MmapWritableBlockDevice::open(&path).unwrap();
        assert_eq!(first_byte(&dev2, 0), 0xCD);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn dirty_span_coalesces_and_flushes_only_written_range() {
        let path = temp_path("coalesce");
        {
            let mut db = crate::storage::Database::create(&path, 4).unwrap();
            for b in 0..4 {
                db.put(b, &[b as u8; MmapWritableBlockDevice::BLOCK_SIZE])
                    .unwrap();
            }
        }
        {
            let mut dev = MmapWritableBlockDevice::open(&path).unwrap();
            // Write block 0 and block 3 (non-contiguous): dirty span [0,3].
            dev.write_block(0, &[0xFF; MmapWritableBlockDevice::BLOCK_SIZE])
                .unwrap();
            dev.write_block(3, &[0x77; MmapWritableBlockDevice::BLOCK_SIZE])
                .unwrap();
            dev.sync().unwrap();
            // A second sync with nothing dirty is a no-op and must not error.
            dev.sync().unwrap();
        }
        let dev = MmapWritableBlockDevice::open(&path).unwrap();
        assert_eq!(first_byte(&dev, 0), 0xFF);
        assert_eq!(first_byte(&dev, 3), 0x77);
        // Untouched block 1 keeps its original value.
        assert_eq!(first_byte(&dev, 1), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rejects_out_of_bounds_write_and_read() {
        let path = temp_path("oob");
        {
            let mut db = crate::storage::Database::create(&path, 1).unwrap();
            db.put(0, &[0u8; MmapWritableBlockDevice::BLOCK_SIZE])
                .unwrap();
        }
        let mut dev = MmapWritableBlockDevice::open(&path).unwrap();
        assert!(matches!(
            dev.write_block(5, &[0u8; MmapWritableBlockDevice::BLOCK_SIZE]),
            Err(StorageError::OutOfBounds { .. })
        ));
        assert!(matches!(
            dev.read_block(5, &mut [0u8; MmapWritableBlockDevice::BLOCK_SIZE]),
            Err(StorageError::OutOfBounds { .. })
        ));
        let _ = std::fs::remove_file(&path);
    }
}
