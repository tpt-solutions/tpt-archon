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
//!
//! [`MmapPageSource`]/[`MmapPageCache`] (opt-in `mmap` feature) offer a second,
//! genuinely OS-`mmap`-backed zero-copy path for reads — real shared virtual
//! memory, not just an in-process reference. Deliberately a separate,
//! additive trait rather than a `UnifiedPageCache` impl: see its docs for why.

use tpt_archon_core::block::{BlockDevice, StorageError};
use tpt_archon_core::page::{BufferPool, Page};

use crate::capability::{Capability, Resource, Right, SharedIssuer};

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
    issuer: SharedIssuer,
}

impl<D: BlockDevice> CorePageCache<D> {
    /// Wraps a core buffer pool, gating every access against `issuer` so a
    /// capability revoked after being minted is rejected here — not just in
    /// `CapabilityIssuer::validate` calls the enforcement path never reaches.
    pub fn new(pool: BufferPool<D>, issuer: SharedIssuer) -> Self {
        Self { pool, issuer }
    }

    /// Returns the wrapped pool.
    pub fn into_pool(self) -> BufferPool<D> {
        self.pool
    }
}

impl<D: BlockDevice> UnifiedPageCache for CorePageCache<D> {
    fn map_read(&mut self, cap: &Capability, block_id: u64) -> Result<&Page, CacheError> {
        if !self
            .issuer
            .borrow()
            .authorizes(cap, Resource::Page(block_id), Right::Read)
        {
            return Err(CacheError::Denied);
        }
        Ok(self.pool.fetch(block_id)?)
    }

    fn map_write(&mut self, cap: &Capability, block_id: u64) -> Result<&mut Page, CacheError> {
        if !self
            .issuer
            .borrow()
            .authorizes(cap, Resource::Page(block_id), Right::Write)
        {
            return Err(CacheError::Denied);
        }
        Ok(self.pool.fetch_mut(block_id)?)
    }

    fn unmap(&mut self, block_id: u64) {
        self.pool.unpin(block_id);
    }
}

/// Read-only, zero-copy page access backed by a real OS memory mapping
/// (requires the `mmap` feature).
///
/// Deliberately *not* a sub/supertrait of [`UnifiedPageCache`]: a type that
/// implements only `MmapPageSource` (e.g. [`MmapPageCache`]) has no write
/// capability at all — the type system, not a runtime check or convention,
/// is what prevents a reader-only mmap cache from being used to mutate
/// storage. A type is free to implement both traits if it legitimately
/// supports both access modes.
#[cfg(all(feature = "std", feature = "mmap"))]
pub trait MmapPageSource {
    /// Borrows page `block_id` directly out of the OS mapping (no copy), if
    /// `cap` authorizes read access.
    ///
    /// Takes `&self`, not `&mut self`: unlike [`UnifiedPageCache::map_read`]
    /// (which mutates `BufferPool`'s LRU/pin state), a real mmap read needs
    /// no exclusive access, so multiple concurrent read borrows are possible
    /// with ordinary shared references. There is deliberately no paired
    /// `unmap` — the borrow's lifetime *is* the release.
    fn map_read_zero_copy(
        &self,
        cap: &Capability,
        block_id: u64,
    ) -> Result<&[u8; tpt_archon_core::page::PAGE_SIZE], CacheError>;
}

/// Adapts a `tpt-archon-core` [`MmapBlockDevice`](tpt_archon_core::block::MmapBlockDevice)
/// to [`MmapPageSource`] (requires the `mmap` feature).
///
/// Pages are borrowed straight out of the OS mapping — genuinely zero-copy,
/// not just "no extra copy inside this process" the way [`CorePageCache`]'s
/// `BufferPool`-backed pages are.
#[cfg(all(feature = "std", feature = "mmap"))]
pub struct MmapPageCache {
    device: tpt_archon_core::block::MmapBlockDevice,
    issuer: SharedIssuer,
}

#[cfg(all(feature = "std", feature = "mmap"))]
impl MmapPageCache {
    /// Wraps a memory-mapped block device, gating every access against
    /// `issuer` so a capability revoked after being minted is rejected here.
    pub fn new(device: tpt_archon_core::block::MmapBlockDevice, issuer: SharedIssuer) -> Self {
        Self { device, issuer }
    }

    /// Re-opens the mapping over `path`, so a reader can observe writes
    /// committed by a writer since this cache was created (mmap is a
    /// point-in-time snapshot — see `MmapBlockDevice`'s docs).
    pub fn refresh<P: AsRef<std::path::Path>>(&mut self, path: P) -> Result<(), StorageError> {
        self.device = tpt_archon_core::block::MmapBlockDevice::open(path)?;
        Ok(())
    }
}

#[cfg(all(feature = "std", feature = "mmap"))]
impl MmapPageSource for MmapPageCache {
    fn map_read_zero_copy(
        &self,
        cap: &Capability,
        block_id: u64,
    ) -> Result<&[u8; tpt_archon_core::page::PAGE_SIZE], CacheError> {
        if !self
            .issuer
            .borrow()
            .authorizes(cap, Resource::Page(block_id), Right::Read)
        {
            return Err(CacheError::Denied);
        }
        Ok(self.device.page_ref(block_id)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::CapabilityIssuer;
    use alloc::rc::Rc;
    use core::cell::RefCell;
    use tpt_archon_core::block::InMemoryBlockDevice;

    fn cache(blocks: u64, cap: usize, issuer: SharedIssuer) -> CorePageCache<InMemoryBlockDevice> {
        CorePageCache::new(
            BufferPool::new(InMemoryBlockDevice::new(blocks), cap),
            issuer,
        )
    }

    #[test]
    fn write_then_read_is_zero_copy_visible() {
        let issuer = Rc::new(RefCell::new(CapabilityIssuer::new()));
        let rw = issuer
            .borrow_mut()
            .mint(Resource::Page(2), Right::ReadWrite);
        let mut c = cache(8, 4, issuer);

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
        let issuer = Rc::new(RefCell::new(CapabilityIssuer::new()));
        let read_only = issuer.borrow_mut().mint(Resource::Page(0), Right::Read);
        let mut c = cache(4, 2, issuer);
        assert_eq!(c.map_write(&read_only, 0).err(), Some(CacheError::Denied));
        // Wrong page.
        assert_eq!(c.map_read(&read_only, 1).err(), Some(CacheError::Denied));
    }

    #[test]
    fn revoked_capability_is_denied_at_map_read_and_map_write() {
        // Regression test for security-audit finding 1: `revoke` must have an
        // effect at the real enforcement point, not just when
        // `CapabilityIssuer::validate` is called directly.
        let issuer = Rc::new(RefCell::new(CapabilityIssuer::new()));
        let rw = issuer
            .borrow_mut()
            .mint(Resource::Page(0), Right::ReadWrite);
        let mut c = cache(4, 2, issuer.clone());

        // Live capability works.
        c.map_write(&rw, 0).unwrap();
        c.unmap(0);
        c.map_read(&rw, 0).unwrap();
        c.unmap(0);

        // Revoke, then the *same* structurally-valid capability must be
        // rejected by the cache itself.
        issuer.borrow_mut().revoke(&rw);
        assert_eq!(c.map_read(&rw, 0).err(), Some(CacheError::Denied));
        assert_eq!(c.map_write(&rw, 0).err(), Some(CacheError::Denied));
    }
}

#[cfg(all(test, feature = "std", feature = "mmap"))]
mod mmap_tests {
    use super::*;
    use crate::capability::CapabilityIssuer;
    use alloc::rc::Rc;
    use core::cell::RefCell;
    use tpt_archon_core::block::MmapBlockDevice;
    use tpt_archon_core::page::PAGE_SIZE;
    use tpt_archon_core::storage::Database;

    fn temp_db(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "tpt-archon-bridge-mmap-{}-{}.bin",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn mmap_cache(path: &std::path::Path, issuer: SharedIssuer) -> MmapPageCache {
        MmapPageCache::new(MmapBlockDevice::open(path).unwrap(), issuer)
    }

    #[test]
    fn mmap_cache_write_then_read_is_zero_copy_visible() {
        let path = temp_db("visible");
        let mut db = Database::create(&path, 4).unwrap();
        db.put(2, &[0xCCu8; PAGE_SIZE]).unwrap();

        let issuer = Rc::new(RefCell::new(CapabilityIssuer::new()));
        let ro = issuer.borrow_mut().mint(Resource::Page(2), Right::Read);
        let c = mmap_cache(&path, issuer);

        assert_eq!(c.map_read_zero_copy(&ro, 2).unwrap()[0], 0xCC);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn mmap_cache_access_without_capability_is_denied() {
        let path = temp_db("denied");
        let _ = Database::create(&path, 4).unwrap();

        let issuer = Rc::new(RefCell::new(CapabilityIssuer::new()));
        let read_only = issuer.borrow_mut().mint(Resource::Page(0), Right::Read);
        let c = mmap_cache(&path, issuer);

        // Wrong page.
        assert_eq!(
            c.map_read_zero_copy(&read_only, 1).err(),
            Some(CacheError::Denied)
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn mmap_cache_revoked_capability_is_denied() {
        let path = temp_db("revoked");
        let _ = Database::create(&path, 4).unwrap();

        let issuer = Rc::new(RefCell::new(CapabilityIssuer::new()));
        let ro = issuer.borrow_mut().mint(Resource::Page(0), Right::Read);
        let c = mmap_cache(&path, issuer.clone());

        c.map_read_zero_copy(&ro, 0).unwrap();
        issuer.borrow_mut().revoke(&ro);
        assert_eq!(c.map_read_zero_copy(&ro, 0).err(), Some(CacheError::Denied));
        let _ = std::fs::remove_file(&path);
    }
}
