//! In-memory [`BlockDevice`] backend, primarily for tests.

use alloc::vec;
use alloc::vec::Vec;

use super::{BlockDevice, BlockId, StorageError};

/// A [`BlockDevice`] backed by a contiguous heap buffer.
///
/// Every block is preallocated at construction, so reads and writes never
/// allocate. `no_std`-compatible via `alloc`.
///
/// # Examples
///
/// ```
/// use tpt_archon_core::block::{BlockDevice, InMemoryBlockDevice};
///
/// let mut device = InMemoryBlockDevice::new(4);
/// let mut buf = [0u8; InMemoryBlockDevice::BLOCK_SIZE];
/// buf[0] = 7;
/// device.write_block(2, &buf).unwrap();
///
/// let mut out = [0u8; InMemoryBlockDevice::BLOCK_SIZE];
/// device.read_block(2, &mut out).unwrap();
/// assert_eq!(out[0], 7);
/// ```
#[derive(Debug, Clone)]
pub struct InMemoryBlockDevice {
    blocks: Vec<u8>,
    block_count: u64,
}

impl InMemoryBlockDevice {
    /// Creates a device with `block_count` zeroed blocks.
    pub fn new(block_count: u64) -> Self {
        let len = (block_count as usize)
            .checked_mul(<Self as BlockDevice>::BLOCK_SIZE)
            .expect("block_count * BLOCK_SIZE overflows usize");
        Self {
            blocks: vec![0u8; len],
            block_count,
        }
    }

    #[inline]
    fn range(&self, block_id: BlockId) -> Result<core::ops::Range<usize>, StorageError> {
        if block_id >= self.block_count {
            return Err(StorageError::OutOfBounds {
                block_id,
                block_count: self.block_count,
            });
        }
        let start = (block_id as usize) * <Self as BlockDevice>::BLOCK_SIZE;
        Ok(start..start + <Self as BlockDevice>::BLOCK_SIZE)
    }
}

impl BlockDevice for InMemoryBlockDevice {
    fn read_block(&self, block_id: BlockId, buffer: &mut [u8]) -> Result<(), StorageError> {
        if buffer.len() != Self::BLOCK_SIZE {
            return Err(StorageError::ShortRead {
                got: buffer.len(),
                expected: Self::BLOCK_SIZE,
            });
        }
        let range = self.range(block_id)?;
        buffer.copy_from_slice(&self.blocks[range]);
        Ok(())
    }

    fn write_block(&mut self, block_id: BlockId, data: &[u8]) -> Result<(), StorageError> {
        if data.len() != Self::BLOCK_SIZE {
            return Err(StorageError::ShortWrite {
                got: data.len(),
                expected: Self::BLOCK_SIZE,
            });
        }
        let range = self.range(block_id)?;
        self.blocks[range].copy_from_slice(data);
        Ok(())
    }

    fn sync(&mut self) -> Result<(), StorageError> {
        // Nothing to flush: writes are already visible in the backing buffer.
        Ok(())
    }

    fn block_count(&self) -> u64 {
        self.block_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_block() {
        let mut dev = InMemoryBlockDevice::new(8);
        let mut data = [0u8; InMemoryBlockDevice::BLOCK_SIZE];
        data[0] = 1;
        data[InMemoryBlockDevice::BLOCK_SIZE - 1] = 255;
        dev.write_block(3, &data).unwrap();

        let mut out = [0u8; InMemoryBlockDevice::BLOCK_SIZE];
        dev.read_block(3, &mut out).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn rejects_out_of_bounds() {
        let dev = InMemoryBlockDevice::new(2);
        let mut out = [0u8; InMemoryBlockDevice::BLOCK_SIZE];
        assert_eq!(
            dev.read_block(2, &mut out),
            Err(StorageError::OutOfBounds {
                block_id: 2,
                block_count: 2
            })
        );
    }

    #[test]
    fn rejects_wrong_sized_buffers() {
        let mut dev = InMemoryBlockDevice::new(1);
        let mut small = [0u8; 16];
        assert!(matches!(
            dev.read_block(0, &mut small),
            Err(StorageError::ShortRead { .. })
        ));
        assert!(matches!(
            dev.write_block(0, &small),
            Err(StorageError::ShortWrite { .. })
        ));
    }

    #[test]
    fn blocks_are_independent() {
        let mut dev = InMemoryBlockDevice::new(3);
        let mut a = [0u8; InMemoryBlockDevice::BLOCK_SIZE];
        a[0] = 0xAA;
        dev.write_block(0, &a).unwrap();

        let mut zero = [0u8; InMemoryBlockDevice::BLOCK_SIZE];
        dev.read_block(1, &mut zero).unwrap();
        assert!(zero.iter().all(|&b| b == 0));
    }
}
