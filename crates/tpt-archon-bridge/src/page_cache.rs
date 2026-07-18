//! The unified page-cache trait shared between storage and kernel.
//!
//! The whole point of `tpt-archon` is that the kernel's page cache and the
//! database's buffer pool are *the same allocation*. [`UnifiedPageCache`] is the
//! interface that makes that possible: it lets a holder borrow a storage page
//! in place (no copy), gated by a [`Capability`].
//!
//! [`CorePageCache`] adapts `tpt-archon-core`'s buffer pool to this trait,
//! demonstrating that a page written through the core engine is visible through
//! the bridge with no intervening copy.

use tpt_archon_core::block::{BlockDevice, StorageError};
use tpt_archon_core::page::{BufferPool, Page};

use crate::capability::{Capability, Resource, Right};

/// Error accessing a page through the unified cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheError {
    /// The presented capability does not authorize the requested access.
    Denied,
    /// The underlying storage engine failed.
    Storage(StorageError),
}

impl From<StorageError> for CacheError {
    fn from(e: StorageError) -> Self {
        CacheError::Storage(e)
    }
}

/// A page cache whose pages can be mapped/borrowed in place across the
/// storage/kernel boundary.
///
/// Access is capability-gated: a caller must present a [`Capability`] that
/// authorizes the operation on the target page.
pub trait UnifiedPageCache {
    /// Borrows page `block_id` for reading, in place, if `cap` authorizes it.
    ///
    /// The returned bytes are the *same* bytes the storage engine holds — no
    /// copy is made.
    fn map_read(&mut self, cap: &Capability, block_id: u64) -> Result<&Page, CacheError>;

    /// Borrows page `block_id` for writing, in place, if `cap` authorizes it.
    fn map_write(&mut self, cap: &Capability, block_id: u64) -> Result<&mut Page, CacheError>;

    /// Releases a previously mapped page.
    fn unmap(&mut self, block_id: u64);
}

/// Adapts a `tpt-archon-core` [`BufferPool`] to [`UnifiedPageCache`].
///
/// Pages fetched here are borrowed straight out of the core buffer pool, so a
/// page written via `tpt-archon-core` is observable through this cache with no
/// copy.
pub struct CorePageCache<D: BlockDevice> {
    pool: BufferPool<D>,
}

impl<D: BlockDevice> CorePageCache<D> {
    /// Wraps a core buffer pool.
    pub fn new(pool: BufferPool<D>) -> Self {
        Self { pool }
    }

    /// Returns the wrapped pool.
    pub fn into_pool(self) -> BufferPool<D> {
        self.pool
    }
}

impl<D: BlockDevice> UnifiedPageCache for CorePageCache<D> {
    fn map_read(&mut self, cap: &Capability, block_id: u64) -> Result<&Page, CacheError> {
        if !cap.authorizes(Resource::Page(block_id), Right::Read) {
            return Err(CacheError::Denied);
        }
        Ok(self.pool.fetch(block_id)?)
    }

    fn map_write(&mut self, cap: &Capability, block_id: u64) -> Result<&mut Page, CacheError> {
        if !cap.authorizes(Resource::Page(block_id), Right::Write) {
            return Err(CacheError::Denied);
        }
        Ok(self.pool.fetch_mut(block_id)?)
    }

    fn unmap(&mut self, block_id: u64) {
        self.pool.unpin(block_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::CapabilityIssuer;
    use tpt_archon_core::block::InMemoryBlockDevice;

    fn cache(blocks: u64, cap: usize) -> CorePageCache<InMemoryBlockDevice> {
        CorePageCache::new(BufferPool::new(InMemoryBlockDevice::new(blocks), cap))
    }

    #[test]
    fn write_then_read_is_zero_copy_visible() {
        let mut issuer = CapabilityIssuer::new();
        let rw = issuer.mint(Resource::Page(2), Right::ReadWrite);
        let mut c = cache(8, 4);

        {
            let page = c.map_write(&rw, 2).unwrap();
            page.as_bytes_mut()[0] = 0xCC;
        }
        c.unmap(2);

        let page = c.map_read(&rw, 2).unwrap();
        assert_eq!(page.as_bytes()[0], 0xCC);
        c.unmap(2);
    }

    #[test]
    fn access_without_capability_is_denied() {
        let mut issuer = CapabilityIssuer::new();
        let read_only = issuer.mint(Resource::Page(0), Right::Read);
        let mut c = cache(4, 2);
        assert_eq!(c.map_write(&read_only, 0).err(), Some(CacheError::Denied));
        // Wrong page.
        assert_eq!(c.map_read(&read_only, 1).err(), Some(CacheError::Denied));
    }
}
