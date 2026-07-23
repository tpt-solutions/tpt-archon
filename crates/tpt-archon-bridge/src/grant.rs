//! Zero-copy capability grants — the contract Telos-compiled user-space code
//! relies on to read/write kernel-mapped storage pages.
//!
//! [`CapabilityGrant`] is a thin, safe convenience layer over
//! [`UnifiedPageCache`]: it bundles the capability check and the page borrow
//! into one call, returning a [`MemoryView`]/[`MemoryViewMut`] instead of a
//! raw `&Page`/`&mut Page`. No new unsafe code or raw pointers are introduced
//! — the view is backed by the same safe, borrowed reference `map_read`/
//! `map_write` already return.

use tpt_archon_core::page::Page;

use crate::capability::Capability;
use crate::page_cache::{CacheError, UnifiedPageCache};

/// A zero-copy, read-only view into a kernel-managed storage page.
pub struct MemoryView<'a> {
    page: &'a Page,
}

impl<'a> MemoryView<'a> {
    /// Borrows the page's bytes. No copy is made.
    pub fn as_slice(&self) -> &[u8] {
        self.page.as_bytes()
    }
}

/// A zero-copy, read-write view into a kernel-managed storage page.
pub struct MemoryViewMut<'a> {
    page: &'a mut Page,
}

impl<'a> MemoryViewMut<'a> {
    /// Borrows the page's bytes for reading. No copy is made.
    pub fn as_slice(&self) -> &[u8] {
        self.page.as_bytes()
    }

    /// Borrows the page's bytes for writing. No copy is made.
    pub fn as_slice_mut(&mut self) -> &mut [u8] {
        self.page.as_bytes_mut()
    }
}

/// Grants zero-copy access to pages in a [`UnifiedPageCache`], gated by a
/// [`Capability`].
///
/// Blanket-implemented for every `UnifiedPageCache`, so any existing cache
/// (e.g. [`CorePageCache`](crate::page_cache::CorePageCache)) gets this
/// contract for free.
pub trait CapabilityGrant: UnifiedPageCache {
    /// Grants a read-only view of `block_id` if `cap` authorizes it.
    fn grant_read<'a>(
        &'a mut self,
        cap: &Capability,
        block_id: u64,
    ) -> Result<MemoryView<'a>, CacheError>;

    /// Grants a read-write view of `block_id` if `cap` authorizes it.
    fn grant_write<'a>(
        &'a mut self,
        cap: &Capability,
        block_id: u64,
    ) -> Result<MemoryViewMut<'a>, CacheError>;
}

impl<C: UnifiedPageCache> CapabilityGrant for C {
    fn grant_read<'a>(
        &'a mut self,
        cap: &Capability,
        block_id: u64,
    ) -> Result<MemoryView<'a>, CacheError> {
        Ok(MemoryView {
            page: self.map_read(cap, block_id)?,
        })
    }

    fn grant_write<'a>(
        &'a mut self,
        cap: &Capability,
        block_id: u64,
    ) -> Result<MemoryViewMut<'a>, CacheError> {
        Ok(MemoryViewMut {
            page: self.map_write(cap, block_id)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{CapabilityIssuer, Resource, Right};
    use crate::page_cache::CorePageCache;
    use tpt_archon_core::block::InMemoryBlockDevice;
    use tpt_archon_core::page::BufferPool;

    fn cache(blocks: u64, cap: usize) -> CorePageCache<InMemoryBlockDevice> {
        CorePageCache::new(BufferPool::new(InMemoryBlockDevice::new(blocks), cap))
    }

    #[test]
    fn grant_write_then_grant_read_is_zero_copy_visible() {
        let mut issuer = CapabilityIssuer::new();
        let rw = issuer.mint(Resource::Page(2), Right::ReadWrite);
        let mut c = cache(8, 4);

        {
            let mut view = c.grant_write(&rw, 2).unwrap();
            view.as_slice_mut()[0] = 0xCC;
        }
        c.unmap(2);

        let view = c.grant_read(&rw, 2).unwrap();
        assert_eq!(view.as_slice()[0], 0xCC);
        c.unmap(2);
    }

    #[test]
    fn grant_without_capability_is_denied() {
        let mut issuer = CapabilityIssuer::new();
        let read_only = issuer.mint(Resource::Page(0), Right::Read);
        let mut c = cache(4, 2);
        assert_eq!(c.grant_write(&read_only, 0).err(), Some(CacheError::Denied));
        assert_eq!(c.grant_read(&read_only, 1).err(), Some(CacheError::Denied));
    }
}
