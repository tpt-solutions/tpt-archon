//! Unified memory management: the kernel page cache *is* the DB buffer pool.
//!
//! [`UnifiedMemory`] holds a single [`UnifiedPageCache`] (from
//! `tpt-archon-bridge`) and exposes it as both the kernel's page cache and the
//! database's buffer pool. There is deliberately no second allocation: memory
//! mapping a storage page and buffering it for the database are the same
//! operation over the same bytes, gated by the same capability system.
//!
//! A real Linux/Windows OS-level `mmap` read path now exists too (opt-in
//! `mmap` feature, see [`tpt_archon_bridge::page_cache::MmapPageSource`]) —
//! genuine shared virtual memory for reads, not just an in-process reference.
//! Writes still go through the `UnifiedPageCache`/`BufferPool` path above;
//! real `mmap`-backed *writes* and bare-metal work remain deferred (Risk 1
//! mitigation from `spec.txt`) — see `TODO.md` Phase 2b for the rationale.

use tpt_archon_bridge::capability::Capability;
use tpt_archon_bridge::page_cache::{CacheError, UnifiedPageCache};
use tpt_archon_core::page::Page;

/// The kernel's memory manager, wrapping a page cache.
///
/// Generic over the cache type; the bound lives on the `impl` blocks that
/// actually need it (not the struct) so `UnifiedMemory<C>` also works over a
/// cache that only implements the read-only, `mmap`-backed
/// [`MmapPageSource`](tpt_archon_bridge::page_cache::MmapPageSource) trait
/// rather than the full read/write [`UnifiedPageCache`].
pub struct UnifiedMemory<C> {
    cache: C,
}

impl<C> UnifiedMemory<C> {
    /// Wraps a page cache.
    pub fn new(cache: C) -> Self {
        Self { cache }
    }

    /// Consumes the manager, returning the wrapped cache.
    pub fn into_cache(self) -> C {
        self.cache
    }
}

impl<C: UnifiedPageCache> UnifiedMemory<C> {
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
}

#[cfg(all(feature = "std", feature = "mmap"))]
impl<C: tpt_archon_bridge::page_cache::MmapPageSource> UnifiedMemory<C> {
    /// Genuinely zero-copy: the kernel and the storage layer read the same
    /// OS-mapped bytes, no `BufferPool`, no copy. See
    /// [`MmapPageSource`](tpt_archon_bridge::page_cache::MmapPageSource)'s
    /// docs for why this is a separate, read-only path.
    pub fn map_read_zero_copy(
        &self,
        cap: &Capability,
        block_id: u64,
    ) -> Result<&[u8; tpt_archon_core::page::PAGE_SIZE], CacheError> {
        self.cache.map_read_zero_copy(cap, block_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::rc::Rc;
    use core::cell::RefCell;
    use tpt_archon_bridge::capability::{CapabilityIssuer, Resource, Right};
    use tpt_archon_bridge::page_cache::{CacheError, CorePageCache};
    use tpt_archon_core::block::InMemoryBlockDevice;
    use tpt_archon_core::page::BufferPool;

    #[test]
    fn unified_memory_shares_storage_pages() {
        let issuer = Rc::new(RefCell::new(CapabilityIssuer::new()));
        let rw = issuer
            .borrow_mut()
            .mint(Resource::Page(1), Right::ReadWrite);

        let cache = CorePageCache::new(BufferPool::new(InMemoryBlockDevice::new(4), 2), issuer);
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

    #[test]
    fn unified_memory_denies_revoked_capability() {
        // Regression test for security-audit finding 1: `UnifiedMemory`
        // delegates straight to the underlying `UnifiedPageCache`, so a
        // revoked capability must be denied here too, not just when the
        // issuer is consulted directly.
        let issuer = Rc::new(RefCell::new(CapabilityIssuer::new()));
        let rw = issuer
            .borrow_mut()
            .mint(Resource::Page(1), Right::ReadWrite);

        let cache = CorePageCache::new(
            BufferPool::new(InMemoryBlockDevice::new(4), 2),
            issuer.clone(),
        );
        let mut mem = UnifiedMemory::new(cache);

        mem.map_write(&rw, 1).unwrap();
        issuer.borrow_mut().revoke(&rw);
        assert_eq!(mem.map_read(&rw, 1).err(), Some(CacheError::Denied));
        assert_eq!(mem.map_write(&rw, 1).err(), Some(CacheError::Denied));
    }
}

#[cfg(all(test, feature = "std", feature = "mmap"))]
mod mmap_tests {
    use super::*;
    use alloc::rc::Rc;
    use core::cell::RefCell;
    use tpt_archon_bridge::capability::{CapabilityIssuer, Resource, Right};
    use tpt_archon_bridge::page_cache::{CacheError, MmapPageCache};
    use tpt_archon_core::block::MmapBlockDevice;
    use tpt_archon_core::page::PAGE_SIZE;
    use tpt_archon_core::storage::Database;

    fn temp_db(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "tpt-archon-kernel-mmap-{}-{}.bin",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn unified_memory_zero_copy_read_over_mmap() {
        let path = temp_db("shared");
        let mut db = Database::create(&path, 4).unwrap();
        db.put(1, &[0x5Au8; PAGE_SIZE]).unwrap();

        let issuer = Rc::new(RefCell::new(CapabilityIssuer::new()));
        let ro = issuer.borrow_mut().mint(Resource::Page(1), Right::Read);
        let cache = MmapPageCache::new(MmapBlockDevice::open(&path).unwrap(), issuer);
        let mem = UnifiedMemory::new(cache);

        assert_eq!(mem.map_read_zero_copy(&ro, 1).unwrap()[0], 0x5A);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn unified_memory_mmap_denies_revoked_capability() {
        let path = temp_db("revoked");
        let _ = Database::create(&path, 4).unwrap();

        let issuer = Rc::new(RefCell::new(CapabilityIssuer::new()));
        let ro = issuer.borrow_mut().mint(Resource::Page(0), Right::Read);
        let cache = MmapPageCache::new(MmapBlockDevice::open(&path).unwrap(), issuer.clone());
        let mem = UnifiedMemory::new(cache);

        mem.map_read_zero_copy(&ro, 0).unwrap();
        issuer.borrow_mut().revoke(&ro);
        assert_eq!(
            mem.map_read_zero_copy(&ro, 0).err(),
            Some(CacheError::Denied)
        );
        let _ = std::fs::remove_file(&path);
    }
}
