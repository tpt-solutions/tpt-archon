//! File-backed [`BlockDevice`] backend (requires the `std` feature).

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use super::{BlockDevice, BlockId, StorageError};

fn io_err(e: std::io::Error) -> StorageError {
    StorageError::Io {
        kind: e.kind() as u8,
    }
}

/// A [`BlockDevice`] backed by a regular file on disk.
///
/// The file is treated as a fixed array of `block_count`
/// [`BLOCK_SIZE`](BlockDevice::BLOCK_SIZE)-byte blocks. On creation the file is
/// grown to exactly that size. Reads and writes seek to the block offset; a
/// stack buffer keeps the read/write hot path allocation-free.
///
/// Only available when the `std` feature (enabled by default) is on.
#[derive(Debug)]
pub struct FileBlockDevice {
    file: File,
    block_count: u64,
}

impl FileBlockDevice {
    /// Opens (creating if necessary) a file-backed device with `block_count`
    /// blocks at `path`.
    ///
    /// The file is resized to exactly `block_count * BLOCK_SIZE` bytes.
    pub fn create<P: AsRef<Path>>(path: P, block_count: u64) -> Result<Self, StorageError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(io_err)?;
        let len =
            block_count
                .checked_mul(Self::BLOCK_SIZE as u64)
                .ok_or(StorageError::OutOfBounds {
                    block_id: block_count,
                    block_count,
                })?;
        file.set_len(len).map_err(io_err)?;
        Ok(Self { file, block_count })
    }

    /// Opens an existing file-backed device, inferring `block_count` from the
    /// file's current length (rounded down to whole blocks).
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, StorageError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(io_err)?;
        let len = file.metadata().map_err(io_err)?.len();
        let block_count = len / Self::BLOCK_SIZE as u64;
        Ok(Self { file, block_count })
    }

    fn offset(&self, block_id: BlockId) -> Result<u64, StorageError> {
        if block_id >= self.block_count {
            return Err(StorageError::OutOfBounds {
                block_id,
                block_count: self.block_count,
            });
        }
        Ok(block_id * Self::BLOCK_SIZE as u64)
    }
}

impl BlockDevice for FileBlockDevice {
    fn read_block(&self, block_id: BlockId, buffer: &mut [u8]) -> Result<(), StorageError> {
        if buffer.len() != Self::BLOCK_SIZE {
            return Err(StorageError::ShortRead {
                got: buffer.len(),
                expected: Self::BLOCK_SIZE,
            });
        }
        let offset = self.offset(block_id)?;
        // `File` reads don't require `&mut self`; take our own handle position
        // by cloning the fd via `try_clone` would allocate — instead use
        // read_exact_at semantics through a positioned read.
        let mut f = &self.file;
        f.seek(SeekFrom::Start(offset)).map_err(io_err)?;
        f.read_exact(buffer).map_err(io_err)?;
        Ok(())
    }

    fn write_block(&mut self, block_id: BlockId, data: &[u8]) -> Result<(), StorageError> {
        if data.len() != Self::BLOCK_SIZE {
            return Err(StorageError::ShortWrite {
                got: data.len(),
                expected: Self::BLOCK_SIZE,
            });
        }
        let offset = self.offset(block_id)?;
        self.file.seek(SeekFrom::Start(offset)).map_err(io_err)?;
        self.file.write_all(data).map_err(io_err)?;
        Ok(())
    }

    fn sync(&mut self) -> Result<(), StorageError> {
        self.file.sync_all().map_err(|_| StorageError::SyncFailed)
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
            "tpt-archon-core-{}-{}.bin",
            name,
            std::process::id()
        ));
        p
    }

    #[test]
    fn round_trips_and_persists() {
        let path = temp_path("roundtrip");
        {
            let mut dev = FileBlockDevice::create(&path, 4).unwrap();
            let mut data = [0u8; FileBlockDevice::BLOCK_SIZE];
            data[0] = 0xEE;
            data[FileBlockDevice::BLOCK_SIZE - 1] = 0x11;
            dev.write_block(1, &data).unwrap();
            dev.sync().unwrap();
        }
        {
            let dev = FileBlockDevice::open(&path).unwrap();
            assert_eq!(dev.block_count(), 4);
            let mut out = [0u8; FileBlockDevice::BLOCK_SIZE];
            dev.read_block(1, &mut out).unwrap();
            assert_eq!(out[0], 0xEE);
            assert_eq!(out[FileBlockDevice::BLOCK_SIZE - 1], 0x11);
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rejects_out_of_bounds() {
        let path = temp_path("oob");
        let dev = FileBlockDevice::create(&path, 1).unwrap();
        let mut out = [0u8; FileBlockDevice::BLOCK_SIZE];
        assert!(matches!(
            dev.read_block(5, &mut out),
            Err(StorageError::OutOfBounds { .. })
        ));
        let _ = std::fs::remove_file(&path);
    }
}
