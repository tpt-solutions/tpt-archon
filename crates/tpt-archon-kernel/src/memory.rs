//! Unified memory management: the kernel page cache *is* the DB buffer pool.
//!
//! [`UnifiedMemory`] holds a single [`UnifiedPageCache`] (from
//! `tpt-archon-bridge`) and exposes it as both the kernel's page cache and the
//! database's buffer pool. There is deliberately no second allocation: memory
//! mapping a storage page and buffering it for the database are the same
//! operation over the same bytes, gated by the same capability system.
//!
//! Bare-metal memory mapping and real `mmap` come later; this validates the
//! model in user space (Risk 1 mitigation from `spec.txt`).

use tpt_archon_bridge::capability::Capability;
use tpt_archon_bridge::page_cache::{CacheError, UnifiedPageCache};
use tpt_archon_core::page::Page;

/// The kernel's memory manager, wrapping the unified page cache.
///
/// Generic over any [`UnifiedPageCache`] so the same manager works over the
/// core buffer pool today and a real mmap-backed cache later.
pub struct UnifiedMemory<C: UnifiedPageCache> {
    cache: C,
}

impl<C: UnifiedPageCache> UnifiedMemory<C> {
    /// Wraps a unified page cache.
    pub fn new(cache: C) -> Self {
        Self { cache }
    }

    /// Maps a page for reading (capability-checked). Same bytes the storage
    /// engine holds.
    pub fn map_read(&mut self, cap: &Capability, block_id: u64) -> Result<&Page, CacheError> {
        self.cache.map_read(cap, block_id)
    }

    /// Maps a page for writing (capability-checked).
    pub fn map_write(&mut self, cap: &Capability, block_id: u64) -> Result<&mut Page, CacheError> {
        self.cache.map_write(cap, block_id)
    }

    /// Releases a mapping.
    pub fn unmap(&mut self, block_id: u64) {
        self.cache.unmap(block_id);
    }

    /// Consumes the manager, returning the wrapped cache.
    pub fn into_cache(self) -> C {
        self.cache
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tpt_archon_bridge::capability::{CapabilityIssuer, Resource, Right};
    use tpt_archon_bridge::page_cache::CorePageCache;
    use tpt_archon_core::block::InMemoryBlockDevice;
    use tpt_archon_core::page::BufferPool;

    #[test]
    fn unified_memory_shares_storage_pages() {
        let mut issuer = CapabilityIssuer::new();
        let rw = issuer.mint(Resource::Page(1), Right::ReadWrite);

        let cache = CorePageCache::new(BufferPool::new(InMemoryBlockDevice::new(4), 2));
        let mut mem = UnifiedMemory::new(cache);

        {
            let page = mem.map_write(&rw, 1).unwrap();
            page.as_bytes_mut()[0] = 0x5A;
        }
        mem.unmap(1);

        let page = mem.map_read(&rw, 1).unwrap();
        assert_eq!(page.as_bytes()[0], 0x5A);
        mem.unmap(1);
    }
}
