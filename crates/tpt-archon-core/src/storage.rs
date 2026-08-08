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

use alloc::vec::Vec;

use crate::block::{BlockDevice, BlockId, StorageError};
use crate::page::{encode_page_block, BufferPool, PAGE_SIZE};
use crate::wal::{RecordKind, Wal};

#[cfg(feature = "std")]
use std::io::{Read, Seek, Write};

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

    /// Appends a `Commit` marker to the in-memory WAL — the closing record of
    /// the write-ahead sequence — without flushing any storage. Durable
    /// persistence of the log itself and of the data pages is left to the
    /// caller (see the `Database` std facade, which fsyncs the WAL sidecar file
    /// *before* flushing the data device so the write-ahead invariant holds at
    /// the fsync boundary, not just logically).
    pub fn append_commit(&mut self) -> Result<(), StorageError> {
        self.wal.append(RecordKind::Commit, 0, &[]);
        Ok(())
    }

    /// Flushes all dirty pool frames to the device and syncs it (the data-
    /// durable half of a commit).
    pub fn flush_data(&mut self) -> Result<(), StorageError> {
        self.pool.flush_all()
    }

    /// Replaces the in-memory WAL with a fresh, empty log. Used after a
    /// checkpoint (e.g. on `Database::open` after replaying a sidecar WAL) so
    /// subsequent appends start clean and the persisted sidecar stays bounded.
    pub fn reset_wal(&mut self) {
        self.wal = Wal::new();
    }

    /// Commits the current batch of writes: appends a commit marker and flushes
    /// the data device. The WAL remains in-memory; callers wanting a durable,
    /// fsynced log (crash-recoverable file database) must persist
    /// [`wal_bytes`](StorageEngine::wal_bytes) to a sidecar and fsync it
    /// *before* calling this (see the `Database` std facade).
    pub fn commit(&mut self) -> Result<(), StorageError> {
        self.append_commit()?;
        self.flush_data()
    }

    /// Returns the raw WAL bytes, suitable for persisting to a dedicated log
    /// region (or a sidecar file) before the device is considered durable.
    pub fn wal_bytes(&self) -> &[u8] {
        self.wal.as_bytes()
    }

    /// Rebuilds the engine's in-memory WAL from previously persisted log bytes
    /// (a torn tail is truncated by [`Wal::from_bytes`]) and replays every
    /// `PageWrite` record that is followed by a `Commit` (or `Checkpoint`)
    /// record back into the device, restoring the last committed page images.
    ///
    /// A `PageWrite` is only applied once an intact `Commit`/`Checkpoint`
    /// record for it is seen later in the log; any `PageWrite`s with no such
    /// record following them (a crash between [`write_page`](Self::write_page)
    /// and [`commit`](Self::commit) leaves a fully-formed, non-torn
    /// `PageWrite` record with no trailing commit marker) are dropped, same as
    /// a torn/corrupt tail is. After recovery the device reflects exactly the
    /// durable, committed writes.
    pub fn recover(&mut self, log_bytes: &[u8]) -> Result<usize, StorageError> {
        let wal = Wal::from_bytes(log_bytes);
        let mut applied = 0usize;
        // Writes are buffered until a Commit/Checkpoint proves them durable;
        // a PageWrite with no later commit marker (crash before commit, or a
        // torn tail that swallowed the commit marker itself) is never
        // flushed to the device.
        let mut pending: Vec<(BlockId, Vec<u8>)> = Vec::new();
        wal.replay(|rec| match rec.kind {
            RecordKind::PageWrite if rec.payload.len() == PAGE_SIZE => {
                pending.push((rec.block_id, rec.payload.clone()));
            }
            RecordKind::Commit | RecordKind::Checkpoint => {
                for (block_id, payload) in pending.drain(..) {
                    // Apply the page image directly to the device. We bypass
                    // the pool so recovery is independent of pool capacity
                    // and order. The page is encoded with its CRC so the
                    // on-disk block matches the format the pool reads.
                    let block = encode_page_block(&payload);
                    if self.pool.device_mut().write_block(block_id, &block).is_ok() {
                        applied += 1;
                    }
                }
            }
            _ => {}
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
        // The corruption below lands on the trailing Commit record itself (a
        // single PageWrite + Commit is a short log), so with commit-gated
        // replay this is indistinguishable from "crashed before commit": the
        // page write must be dropped, not applied.
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
        assert_eq!(
            n, 0,
            "the only page write's commit marker was torn, so it must not be replayed"
        );
        // Device was never touched, so this reads the pool's zero-initialized
        // default rather than the uncommitted 0x07 image.
        assert_eq!(e2.read_page(0).unwrap()[0], 0x00);
    }

    #[test]
    fn recover_drops_page_write_with_no_following_commit() {
        // Regression test for security-audit finding 2: `write_page` appends
        // the WAL record before `commit` is ever called, so a crash between
        // the two leaves a fully-formed, non-torn `PageWrite` record with no
        // trailing `Commit`. Replay must drop it, not apply it as if durable.
        let dev = InMemoryBlockDevice::new(4);
        let log;
        {
            let mut e = StorageEngine::new(dev.clone(), 2);
            e.write_page(0, &page_of(0x01)).unwrap();
            e.commit().unwrap();
            // A second write with no matching commit — simulates a crash
            // between `write_page` and `commit`.
            e.write_page(1, &page_of(0x02)).unwrap();
            log = e.wal_bytes().to_vec();
        }

        let mut e2 = StorageEngine::new(dev, 2);
        let n = e2.recover(&log).unwrap();
        assert_eq!(n, 1, "only the committed write (block 0) should replay");
        assert_eq!(e2.read_page(0).unwrap()[0], 0x01);
        // Block 1's write was never committed, so it must not have reached
        // the device — this reads back the zero-initialized default.
        assert_eq!(e2.read_page(1).unwrap()[0], 0x00);
    }
}

/// A file-backed database handle: the "embeddable SQLite" entry point.
///
/// Wraps a [`StorageEngine`] over a [`FileBlockDevice`] so callers can open a
/// database file and read/write fixed-size pages with the WAL write-ahead
/// guarantee, without touching the buffer pool or WAL directly.
///
/// Durability model: every [`put`](Database::put) records the page in the
/// in-memory WAL, **fsyncs the WAL to a sidecar `<path>.wal` file first**, and
/// only then flushes + fsyncs the data file. That is the write-ahead invariant
/// enforced at the `fsync` boundary (not just logically) — a crash between the
/// two fsyncs leaves a durable WAL record whose page image is replayed on the
/// next [`open`](Database::open). [`open`] replays any committed records found
/// in the sidecar WAL into the data file, then checkpoints (truncates) it.
///
/// Only available with the default `std` feature.
#[cfg(feature = "std")]
pub struct Database {
    engine: StorageEngine<crate::block::FileBlockDevice>,
    wal_file: std::fs::File,
}

#[cfg(feature = "std")]
impl Database {
    /// Maps a database path to its sidecar WAL file (`<path>.wal`).
    fn wal_path_for<P: AsRef<std::path::Path>>(path: P) -> std::path::PathBuf {
        let p = path.as_ref();
        let mut s = p.as_os_str().to_os_string();
        s.push(".wal");
        std::path::PathBuf::from(s)
    }

    fn io_err(e: std::io::Error) -> StorageError {
        StorageError::Io {
            kind: e.kind() as u8,
        }
    }

    /// Opens an existing database file, inferring the block count from the
    /// file size, and replays + checkpoints any committed records left in the
    /// sidecar WAL from a previous run.
    pub fn open<P: AsRef<std::path::Path>>(path: P) -> Result<Self, StorageError> {
        let device = crate::block::FileBlockDevice::open(&path)?;
        let wal_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(Self::wal_path_for(&path))
            .map_err(Self::io_err)?;
        let mut db = Self {
            engine: StorageEngine::new(device, 64),
            wal_file,
        };
        // Replay any committed records from a previous run, then checkpoint.
        let mut buf = Vec::new();
        db.wal_file
            .seek(std::io::SeekFrom::Start(0))
            .map_err(Self::io_err)?;
        db.wal_file.read_to_end(&mut buf).map_err(Self::io_err)?;
        if !buf.is_empty() {
            db.engine.recover(&buf)?;
            // `recover` writes pages via `write_block` (no sync) — make the
            // replayed state durable before we truncate the WAL.
            db.engine.flush_data()?;
            // Checkpoint: discard the replayed log and start a fresh in-memory
            // WAL so subsequent puts don't re-replay old records.
            db.engine.reset_wal();
            db.wal_file
                .seek(std::io::SeekFrom::Start(0))
                .map_err(Self::io_err)?;
            db.wal_file.set_len(0).map_err(Self::io_err)?;
        }
        Ok(db)
    }

    /// Creates (or resizes) a database file with `block_count` blocks, and a
    /// fresh (truncated) sidecar WAL.
    pub fn create<P: AsRef<std::path::Path>>(
        path: P,
        block_count: u64,
    ) -> Result<Self, StorageError> {
        let device = crate::block::FileBlockDevice::create(&path, block_count)?;
        let wal_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(Self::wal_path_for(&path))
            .map_err(Self::io_err)?;
        Ok(Self {
            engine: StorageEngine::new(device, 64),
            wal_file,
        })
    }

    /// Persists the in-memory WAL to the sidecar file and fsyncs it.
    fn persist_wal(&mut self) -> Result<(), StorageError> {
        let bytes = self.engine.wal_bytes().to_vec();
        self.wal_file
            .seek(std::io::SeekFrom::Start(0))
            .map_err(Self::io_err)?;
        self.wal_file.write_all(&bytes).map_err(Self::io_err)?;
        self.wal_file
            .set_len(bytes.len() as u64)
            .map_err(Self::io_err)?;
        self.wal_file
            .sync_all()
            .map_err(|_| StorageError::SyncFailed)
    }

    /// Writes a page and commits it durably: WAL record → **fsync WAL sidecar
    /// first** → flush + fsync data file. This honors the write-ahead invariant
    /// at the `fsync` boundary, so a crash between the two fsyncs is recovered
    /// by replaying the durable WAL on the next [`open`](Database::open).
    pub fn put(&mut self, block_id: BlockId, page: &[u8]) -> Result<(), StorageError> {
        self.engine.write_page(block_id, page)?;
        self.engine.append_commit()?;
        self.persist_wal()?;
        self.engine.flush_data()
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
        let wal = Database::wal_path_for(&path);
        {
            let mut db = Database::create(&path, 4).unwrap();
            db.put(1, &page_of(0x55)).unwrap();
        }
        {
            let mut db = Database::open(&path).unwrap();
            assert_eq!(db.get(1).unwrap()[0], 0x55);
        }
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&wal);
    }

    #[test]
    fn reopen_replays_committed_writes_from_wal_file() {
        let path = temp_db("walreplay");
        let wal = Database::wal_path_for(&path);
        {
            let mut db = Database::create(&path, 4).unwrap();
            db.put(0, &page_of(0x01)).unwrap();
            db.put(2, &page_of(0x02)).unwrap();
        }
        // The sidecar WAL was written and fsynced by the puts above.
        assert!(std::fs::metadata(&wal).unwrap().len() > 0);
        {
            let mut db = Database::open(&path).unwrap();
            assert_eq!(db.get(0).unwrap()[0], 0x01);
            assert_eq!(db.get(2).unwrap()[0], 0x02);
        }
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&wal);
    }

    #[test]
    fn torn_wal_tail_is_tolerated_on_reopen() {
        let path = temp_db("waltorn");
        let wal = Database::wal_path_for(&path);
        {
            let mut db = Database::create(&path, 4).unwrap();
            db.put(0, &page_of(0xAB)).unwrap();
        }
        // Corrupt the tail of the sidecar WAL to simulate a crash mid-append.
        {
            let mut f = std::fs::OpenOptions::new().write(true).open(&wal).unwrap();
            let len = std::fs::metadata(&wal).unwrap().len();
            f.seek(std::io::SeekFrom::Start(len - 3)).unwrap();
            f.write_all(&[0xFF, 0xFF, 0xFF]).unwrap();
        }
        // Reopen must not panic and must keep the committed page (already
        // durable on the data device; the torn WAL is simply ignored).
        let mut db = Database::open(&path).unwrap();
        assert_eq!(db.get(0).unwrap()[0], 0xAB);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&wal);
    }

    #[test]
    fn wal_replay_populates_data_device_after_crash_before_data_flush() {
        // Models the write-ahead window: the WAL was fsynced but the data pages
        // were NOT yet flushed (crash between log-durable and data-durable).
        // Reopening must recover the page image from the on-disk WAL.
        let path = temp_db("walonly");
        let wal = Database::wal_path_for(&path);
        {
            // Create the data file (all zeros), then plant a committed WAL in
            // the sidecar (simulating a WAL fsynced before the data flush).
            let _ = Database::create(&path, 4).unwrap();
            let dev = crate::block::InMemoryBlockDevice::new(4);
            let mut e = StorageEngine::new(dev, 2);
            e.write_page(1, &page_of(0x5A)).unwrap();
            e.append_commit().unwrap();
            let bytes = e.wal_bytes().to_vec();
            let mut f = std::fs::OpenOptions::new().write(true).open(&wal).unwrap();
            f.seek(std::io::SeekFrom::Start(0)).unwrap();
            f.write_all(&bytes).unwrap();
            f.set_len(bytes.len() as u64).unwrap();
            f.sync_all().unwrap();
        }
        // Open replays the WAL into the zeroed data device.
        let mut db = Database::open(&path).unwrap();
        assert_eq!(db.get(1).unwrap()[0], 0x5A);
        assert_eq!(db.get(0).unwrap()[0], 0x00);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&wal);
    }
}
