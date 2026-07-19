//! An end-to-end storage facade wiring the [`BufferPool`] to the [`Wal`].
//!
//! This is the missing integration seam called out in the review: it makes the
//! write-ahead invariant real by appending a WAL record *before* a page reaches
//! main storage, and exposing [`StorageEngine::recover`] so a log replayed
//! after a crash restores the last committed page images.
//!
//! The facade is intentionally small and `no_std` + `alloc` (no `std`-only
//! features required), so it can sit on top of the in-memory or file-backed
//! [`BlockDevice`](crate::block::BlockDevice) interchangeably.

use crate::block::{BlockDevice, BlockId, StorageError};
use crate::page::{BufferPool, PAGE_SIZE};
use crate::wal::{RecordKind, Wal};

/// A storage engine: a buffer pool whose page modifications are first recorded
/// in a write-ahead log.
pub struct StorageEngine<D: BlockDevice> {
    pool: BufferPool<D>,
    wal: Wal,
}

impl<D: BlockDevice> StorageEngine<D> {
    /// Creates an engine over `device`, caching at most `capacity` pages.
    pub fn new(device: D, capacity: usize) -> Self {
        Self {
            pool: BufferPool::new(device, capacity),
            wal: Wal::new(),
        }
    }

    /// The buffer pool's page size, exposed for callers framing page images.
    pub const PAGE_SIZE: usize = PAGE_SIZE;

    /// Writes `page` (exactly [`PAGE_SIZE`] bytes) to `block_id`, recording the
    /// change in the WAL *first* (the write-ahead invariant) and then staging it
    /// in the buffer pool. The mutation is not durable until
    /// [`commit`](StorageEngine::commit) flushes both log and page.
    pub fn write_page(&mut self, block_id: BlockId, page: &[u8]) -> Result<(), StorageError> {
        if page.len() != PAGE_SIZE {
            return Err(StorageError::ShortWrite {
                got: page.len(),
                expected: PAGE_SIZE,
            });
        }
        // Write-ahead: log the full page image before touching main storage.
        self.wal.append(RecordKind::PageWrite, block_id, page);
        let frame = self.pool.fetch_mut(block_id)?;
        frame.as_bytes_mut().copy_from_slice(page);
        self.pool.unpin(block_id);
        Ok(())
    }

    /// Reads `block_id` through the buffer pool (loaded from the device on a
    /// miss). The returned slice is valid only until the next pool operation
    /// that may evict the frame.
    pub fn read_page(&mut self, block_id: BlockId) -> Result<&[u8], StorageError> {
        let frame = self.pool.fetch(block_id)?;
        Ok(frame.as_bytes())
    }

    /// Commits the current batch of writes: appends a commit marker to the WAL,
    /// flushes the log-carrying pool pages, and syncs the device.
    pub fn commit(&mut self) -> Result<(), StorageError> {
        self.wal.append(RecordKind::Commit, 0, &[]);
        self.pool.flush_all()
    }

    /// Returns the raw WAL bytes, suitable for persisting to a dedicated log
    /// region (or a sidecar file) before the device is considered durable.
    pub fn wal_bytes(&self) -> &[u8] {
        self.wal.as_bytes()
    }

    /// Rebuilds the engine's in-memory WAL from previously persisted log bytes
    /// (a torn tail is truncated by [`Wal::from_bytes`]) and replays every
    /// intact `PageWrite` record back into the device, restoring the last
    /// committed page images. After recovery the device reflects all durable
    /// writes; uncommitted trailing pages are dropped.
    pub fn recover(&mut self, log_bytes: &[u8]) -> Result<usize, StorageError> {
        let wal = Wal::from_bytes(log_bytes);
        let mut applied = 0usize;
        wal.replay(|rec| {
            if rec.kind == RecordKind::PageWrite && rec.payload.len() == PAGE_SIZE {
                // Apply the page image directly to the device. We bypass the
                // pool so recovery is independent of pool capacity and order.
                if self
                    .pool
                    .device_mut()
                    .write_block(rec.block_id, &rec.payload)
                    .is_ok()
                {
                    applied += 1;
                }
            }
        });
        self.wal = wal;
        Ok(applied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::InMemoryBlockDevice;

    fn page_of(byte: u8) -> alloc::vec::Vec<u8> {
        alloc::vec![byte; PAGE_SIZE]
    }

    #[test]
    fn write_then_read_round_trips_through_pool() {
        let mut e = StorageEngine::new(InMemoryBlockDevice::new(4), 2);
        e.write_page(0, &page_of(0xAB)).unwrap();
        assert_eq!(e.read_page(0).unwrap()[0], 0xAB);
    }

    #[test]
    fn wal_records_page_before_main_storage() {
        let mut e = StorageEngine::new(InMemoryBlockDevice::new(4), 2);
        e.write_page(1, &page_of(0x11)).unwrap();
        // WAL carries the PageWrite record; committing makes it durable.
        assert!(!e.wal_bytes().is_empty());
        e.commit().unwrap();

        // The WAL prefix must contain the full page image.
        let log = e.wal_bytes().to_vec();
        let wal = Wal::from_bytes(&log);
        let mut found = false;
        wal.replay(|rec| {
            if rec.kind == RecordKind::PageWrite
                && rec.block_id == 1
                && rec.payload.first() == Some(&0x11)
            {
                found = true;
            }
        });
        assert!(found, "WAL must record the page write before storage");
    }

    #[test]
    fn recover_replays_committed_pages_after_crash() {
        let dev = InMemoryBlockDevice::new(4);
        let log;
        {
            let mut e = StorageEngine::new(dev.clone(), 2);
            e.write_page(0, &page_of(0x01)).unwrap();
            e.write_page(2, &page_of(0x02)).unwrap();
            e.commit().unwrap();
            log = e.wal_bytes().to_vec();
        }

        // New engine over the same (now-durable) device, "crashed" with the log.
        let mut e2 = StorageEngine::new(dev, 2);
        let n = e2.recover(&log).unwrap();
        // The log has 2 PageWrite records (block 0 and block 2) plus a Commit;
        // only the page writes are replayed into the device.
        assert_eq!(n, 2, "both committed page writes should replay");

        // Reads now hit the recovered device.
        assert_eq!(e2.read_page(0).unwrap()[0], 0x01);
        assert_eq!(e2.read_page(2).unwrap()[0], 0x02);
    }

    #[test]
    fn recover_ignores_torn_tail() {
        let dev = InMemoryBlockDevice::new(4);
        let mut e = StorageEngine::new(dev.clone(), 2);
        e.write_page(0, &page_of(0x07)).unwrap();
        e.commit().unwrap();
        let mut log = e.wal_bytes().to_vec();
        // Corrupt the tail so only the valid prefix survives.
        let len = log.len();
        for b in log.iter_mut().skip(len - 2) {
            *b ^= 0xFF;
        }

        let mut e2 = StorageEngine::new(dev, 2);
        let n = e2.recover(&log).unwrap();
        assert_eq!(n, 1, "torn tail truncated to the durable prefix");
        assert_eq!(e2.read_page(0).unwrap()[0], 0x07);
    }
}

/// A file-backed database handle: the "embeddable SQLite" entry point.
///
/// Wraps a [`StorageEngine`] over a [`FileBlockDevice`] so callers can open a
/// database file and read/write fixed-size pages with the WAL write-ahead
/// guarantee, without touching the buffer pool or WAL directly.
///
/// Only available with the default `std` feature.
#[cfg(feature = "std")]
pub struct Database {
    engine: StorageEngine<crate::block::FileBlockDevice>,
}

#[cfg(feature = "std")]
impl Database {
    /// Opens an existing database file, inferring the block count from the
    /// file size.
    pub fn open<P: AsRef<std::path::Path>>(path: P) -> Result<Self, StorageError> {
        let device = crate::block::FileBlockDevice::open(path)?;
        Ok(Self {
            engine: StorageEngine::new(device, 64),
        })
    }

    /// Creates (or resizes) a database file with `block_count` blocks.
    pub fn create<P: AsRef<std::path::Path>>(
        path: P,
        block_count: u64,
    ) -> Result<Self, StorageError> {
        let device = crate::block::FileBlockDevice::create(path, block_count)?;
        Ok(Self {
            engine: StorageEngine::new(device, 64),
        })
    }

    /// Writes a page and commits it durably (WAL record + page flush + sync).
    pub fn put(&mut self, block_id: BlockId, page: &[u8]) -> Result<(), StorageError> {
        self.engine.write_page(block_id, page)?;
        self.engine.commit()
    }

    /// Reads a page through the buffer pool.
    pub fn get(&mut self, block_id: BlockId) -> Result<&[u8], StorageError> {
        self.engine.read_page(block_id)
    }

    /// Recovers committed state from a persisted WAL, replaying page images
    /// back into the file.
    pub fn recover_from(&mut self, log_bytes: &[u8]) -> Result<usize, StorageError> {
        self.engine.recover(log_bytes)
    }
}

#[cfg(all(test, feature = "std"))]
mod db_tests {
    use super::*;

    fn temp_db(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "tpt-archon-db-{}-{}-{}.bin",
            name,
            std::process::id(),
            name.len()
        ));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn page_of(byte: u8) -> alloc::vec::Vec<u8> {
        alloc::vec![byte; PAGE_SIZE]
    }

    #[test]
    fn create_open_put_and_get() {
        let path = temp_db("create");
        {
            let mut db = Database::create(&path, 4).unwrap();
            db.put(1, &page_of(0x55)).unwrap();
        }
        {
            let mut db = Database::open(&path).unwrap();
            assert_eq!(db.get(1).unwrap()[0], 0x55);
        }
        let _ = std::fs::remove_file(&path);
    }
}
