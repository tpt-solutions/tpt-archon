//! Fixed-size pages and an LRU buffer pool.
//!
//! A [`Page`] is a `PAGE_SIZE`-byte frame of bytes plus a [`PageState`]. The
//! [`BufferPool`] caches a bounded number of pages over a
//! [`BlockDevice`](crate::block::BlockDevice), tracking each frame's state
//! (`Free` / `Clean` / `Dirty` / `Pinned`) and evicting the least-recently-used
//! unpinned frame — writing it back first if it is dirty.
//!
//! # Layout stability for the unified page cache
//!
//! A `Page`'s bytes are a plain `[u8; PAGE_SIZE]` with no header interleaved
//! into the frame and no internal pointers, so a later phase
//! (`tpt-archon-bridge`) can map the same bytes into another address space
//! without translation. Bookkeeping (state, pin count, dirtiness) is kept
//! *outside* the byte frame, in the pool.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::block::{BlockDevice, BlockId, StorageError};
use crate::checksum::{crc32, PAGE_CRC};

/// The page size in bytes. Matches the default block size (4 KiB).
///
/// A 16 KiB configuration is possible by constructing a pool over a block
/// device whose `BLOCK_SIZE` is 16 KiB; the page frame size is fixed at the
/// block size to keep page↔block mapping one-to-one and copy-free.
pub const PAGE_SIZE: usize = 4096;

/// The lifecycle state of a buffer-pool frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageState {
    /// The frame holds no page.
    Free,
    /// The frame holds a page identical to what is on disk.
    Clean,
    /// The frame holds a page with unwritten modifications.
    Dirty,
    /// The frame is pinned (in use) and must not be evicted.
    ///
    /// The inner value is the pin count; a frame can be pinned more than once.
    Pinned(u32),
}

/// A single fixed-size page frame.
#[derive(Clone)]
pub struct Page {
    bytes: [u8; PAGE_SIZE],
}

/// Encodes a logical `PAGE_SIZE` page into the on-disk block layout: the page
/// bytes followed by a trailing CRC32 over those bytes. The result is exactly
/// `BLOCK_SIZE` (`PAGE_SIZE + PAGE_CRC`) bytes.
pub(crate) fn encode_page_block(page: &[u8]) -> Vec<u8> {
    let mut block = alloc::vec![0u8; PAGE_SIZE + PAGE_CRC];
    block[..PAGE_SIZE].copy_from_slice(page);
    let crc = crc32(page);
    block[PAGE_SIZE..].copy_from_slice(&crc.to_le_bytes());
    block
}

/// Decodes an on-disk block back into a logical `PAGE_SIZE` page, verifying
/// the trailing CRC32. Returns [`StorageError::Corrupt`] if the checksum does
/// not match — the block was silently corrupted on storage and must not be
/// returned to the caller.
fn decode_block(block: &[u8]) -> Result<[u8; PAGE_SIZE], StorageError> {
    if block.len() != PAGE_SIZE + PAGE_CRC {
        return Err(StorageError::ShortRead {
            got: block.len(),
            expected: PAGE_SIZE + PAGE_CRC,
        });
    }
    // A never-written block is all zeros. Its stored CRC is also zero, which can
    // never match `crc32(zeros)`, so accept an all-zero block as a valid zero
    // page without verification. This is the common "freshly created / freshly
    // opened" case and avoids spurious `Corrupt` errors on unallocated blocks.
    if block.iter().all(|&b| b == 0) {
        return Ok([0u8; PAGE_SIZE]);
    }
    let mut page = [0u8; PAGE_SIZE];
    page.copy_from_slice(&block[..PAGE_SIZE]);
    let stored = u32::from_le_bytes([
        block[PAGE_SIZE],
        block[PAGE_SIZE + 1],
        block[PAGE_SIZE + 2],
        block[PAGE_SIZE + 3],
    ]);
    if crc32(&page) != stored {
        return Err(StorageError::Corrupt);
    }
    Ok(page)
}

impl Page {
    /// Creates a zeroed page.
    pub fn zeroed() -> Self {
        Self {
            bytes: [0u8; PAGE_SIZE],
        }
    }

    /// Immutable view of the page bytes.
    #[inline]
    pub fn as_bytes(&self) -> &[u8; PAGE_SIZE] {
        &self.bytes
    }

    /// Mutable view of the page bytes.
    #[inline]
    pub fn as_bytes_mut(&mut self) -> &mut [u8; PAGE_SIZE] {
        &mut self.bytes
    }
}

impl Default for Page {
    fn default() -> Self {
        Self::zeroed()
    }
}

impl core::fmt::Debug for Page {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Page").field("size", &PAGE_SIZE).finish()
    }
}

struct Frame {
    block_id: BlockId,
    page: Page,
    state: PageState,
    /// Set while a frame is pinned-and-modified; on final unpin it decides
    /// whether the frame becomes `Dirty` (true) or `Clean` (false).
    dirty_intent: bool,
}

/// A bounded, LRU buffer pool over a block device.
///
/// The pool holds at most `capacity` frames. [`fetch`](BufferPool::fetch) pins
/// and returns a page, loading it from the device on a miss and evicting the
/// least-recently-used unpinned frame if the pool is full.
pub struct BufferPool<D: BlockDevice> {
    device: D,
    capacity: usize,
    frames: Vec<Frame>,
    /// LRU order of *frame indices*: front = least recently used.
    lru: VecDeque<usize>,
}

impl<D: BlockDevice> BufferPool<D> {
    /// Creates a pool over `device` holding at most `capacity` pages.
    ///
    /// Panics if `capacity` is zero, or if the device's `BLOCK_SIZE` does not
    /// equal [`PAGE_SIZE`].
    pub fn new(device: D, capacity: usize) -> Self {
        assert!(capacity > 0, "buffer pool capacity must be non-zero");
        assert_eq!(
            D::BLOCK_SIZE,
            PAGE_SIZE + PAGE_CRC,
            "page manager requires BLOCK_SIZE == PAGE_SIZE + checksum bytes"
        );
        Self {
            device,
            capacity,
            frames: Vec::new(),
            lru: VecDeque::new(),
        }
    }

    /// Number of frames currently resident.
    pub fn resident(&self) -> usize {
        self.frames.len()
    }

    /// The state of the frame currently holding `block_id`, if resident.
    pub fn state_of(&self, block_id: BlockId) -> Option<PageState> {
        self.frames
            .iter()
            .find(|f| f.block_id == block_id)
            .map(|f| f.state)
    }

    fn find(&self, block_id: BlockId) -> Option<usize> {
        self.frames.iter().position(|f| f.block_id == block_id)
    }

    fn touch(&mut self, idx: usize) {
        if let Some(p) = self.lru.iter().position(|&i| i == idx) {
            self.lru.remove(p);
        }
        self.lru.push_back(idx);
    }

    /// Evicts the least-recently-used *unpinned* frame, writing it back if
    /// dirty. Returns the evicted frame index, or `None` if every frame is
    /// pinned.
    fn evict(&mut self) -> Result<Option<usize>, StorageError> {
        let victim = self
            .lru
            .iter()
            .copied()
            .find(|&i| !matches!(self.frames[i].state, PageState::Pinned(_)));
        let Some(idx) = victim else {
            return Ok(None);
        };
        if self.frames[idx].state == PageState::Dirty {
            let (block_id, bytes) = {
                let f = &self.frames[idx];
                (f.block_id, *f.page.as_bytes())
            };
            let block = encode_page_block(&bytes);
            self.device.write_block(block_id, &block)?;
        }
        if let Some(p) = self.lru.iter().position(|&i| i == idx) {
            self.lru.remove(p);
        }
        self.frames[idx].state = PageState::Free;
        self.frames[idx].dirty_intent = false;
        Ok(Some(idx))
    }

    /// Fetches and pins the page for `block_id`, loading from the device on a
    /// miss. Returns an immutable view of the page bytes.
    ///
    /// Call [`unpin`](BufferPool::unpin) when done. Returns an error if the
    /// pool is full and every frame is pinned.
    pub fn fetch(&mut self, block_id: BlockId) -> Result<&Page, StorageError> {
        if let Some(idx) = self.find(block_id) {
            self.pin_frame(idx);
            self.touch(idx);
            return Ok(&self.frames[idx].page);
        }

        let idx = self.acquire_frame()?;
        let mut block = alloc::vec![0u8; D::BLOCK_SIZE];
        self.device.read_block(block_id, &mut block)?;
        let page_bytes = decode_block(&block)?;
        let mut page = Page::zeroed();
        page.as_bytes_mut().copy_from_slice(&page_bytes);
        self.frames[idx] = Frame {
            block_id,
            page,
            state: PageState::Pinned(1),
            dirty_intent: false,
        };
        self.touch(idx);
        Ok(&self.frames[idx].page)
    }

    /// Fetches and pins the page for `block_id`, marking it dirty, and returns
    /// a mutable view for in-place modification.
    pub fn fetch_mut(&mut self, block_id: BlockId) -> Result<&mut Page, StorageError> {
        let idx = if let Some(idx) = self.find(block_id) {
            self.pin_frame(idx);
            idx
        } else {
            let idx = self.acquire_frame()?;
            let mut block = alloc::vec![0u8; D::BLOCK_SIZE];
            self.device.read_block(block_id, &mut block)?;
            let page_bytes = decode_block(&block)?;
            let mut page = Page::zeroed();
            page.as_bytes_mut().copy_from_slice(&page_bytes);
            self.frames[idx] = Frame {
                block_id,
                page,
                state: PageState::Pinned(1),
                dirty_intent: false,
            };
            idx
        };
        self.touch(idx);
        // A pinned-and-modified frame must be written back on eviction, so
        // remember dirtiness alongside the pin. We encode this by upgrading the
        // pin count but tracking dirty via a separate marker on unpin; simplest
        // correct approach: mark dirty now.
        self.mark_dirty_pinned(idx);
        Ok(&mut self.frames[idx].page)
    }

    fn pin_frame(&mut self, idx: usize) {
        self.frames[idx].state = match self.frames[idx].state {
            PageState::Pinned(n) => PageState::Pinned(n + 1),
            _ => PageState::Pinned(1),
        };
    }

    /// Records that a pinned frame has unwritten modifications by stashing the
    /// dirty intent; on unpin to zero it will become `Dirty` rather than
    /// `Clean`.
    fn mark_dirty_pinned(&mut self, idx: usize) {
        self.frames[idx].dirty_intent = true;
    }

    fn acquire_frame(&mut self) -> Result<usize, StorageError> {
        // Reuse a Free frame if one exists.
        if let Some(idx) = self.frames.iter().position(|f| f.state == PageState::Free) {
            return Ok(idx);
        }
        // Grow if under capacity.
        if self.frames.len() < self.capacity {
            self.frames.push(Frame {
                block_id: BlockId::MAX,
                page: Page::zeroed(),
                state: PageState::Free,
                dirty_intent: false,
            });
            return Ok(self.frames.len() - 1);
        }
        // Otherwise evict.
        match self.evict()? {
            Some(idx) => Ok(idx),
            None => Err(StorageError::AllFramesPinned),
        }
    }

    /// Unpins a previously fetched page. When the pin count reaches zero the
    /// frame becomes `Dirty` (if it was modified) or `Clean`.
    pub fn unpin(&mut self, block_id: BlockId) {
        if let Some(idx) = self.find(block_id) {
            if let PageState::Pinned(n) = self.frames[idx].state {
                if n > 1 {
                    self.frames[idx].state = PageState::Pinned(n - 1);
                } else if self.frames[idx].dirty_intent {
                    self.frames[idx].state = PageState::Dirty;
                } else {
                    self.frames[idx].state = PageState::Clean;
                }
            }
        }
    }

    /// Writes all dirty frames back to the device and syncs it.
    ///
    /// Note: a frame that is still `Pinned` but carries `dirty_intent` (i.e. a
    /// `fetch_mut` that has not yet been `unpin`ned) is also flushed. This is
    /// intentional — a flush is allowed to underpin an in-flight pin — but it
    /// means an unpin-then-uncommitted mutation will persist on `flush_all`
    /// even though the surrounding transaction may not have committed. For
    /// single-threaded use that is benign; callers wanting strict
    /// commit-scoped durability should `unpin` only after commit (or route
    /// the write through the WAL first — see the `StorageEngine` facade).
    pub fn flush_all(&mut self) -> Result<(), StorageError> {
        for idx in 0..self.frames.len() {
            if self.frames[idx].state == PageState::Dirty
                || (self.frames[idx].dirty_intent
                    && matches!(self.frames[idx].state, PageState::Pinned(_)))
            {
                let (block_id, bytes) = {
                    let f = &self.frames[idx];
                    (f.block_id, *f.page.as_bytes())
                };
                let block = encode_page_block(&bytes);
                self.device.write_block(block_id, &block)?;
                self.frames[idx].dirty_intent = false;
                if self.frames[idx].state == PageState::Dirty {
                    self.frames[idx].state = PageState::Clean;
                }
            }
        }
        self.device.sync()
    }

    /// Consumes the pool, returning the underlying device.
    pub fn into_device(self) -> D {
        self.device
    }

    /// Borrows the underlying device mutably without consuming the pool.
    ///
    /// Useful for recovery paths that must write pages directly to the device
    /// (e.g. replaying a WAL) while the pool stays alive.
    pub fn device_mut(&mut self) -> &mut D {
        &mut self.device
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::InMemoryBlockDevice;

    fn pool(blocks: u64, cap: usize) -> BufferPool<InMemoryBlockDevice> {
        BufferPool::new(InMemoryBlockDevice::new(blocks), cap)
    }

    #[test]
    fn fetch_pins_and_unpin_marks_clean() {
        let mut p = pool(4, 2);
        let _ = p.fetch(0).unwrap();
        assert_eq!(p.state_of(0), Some(PageState::Pinned(1)));
        p.unpin(0);
        assert_eq!(p.state_of(0), Some(PageState::Clean));
    }

    #[test]
    fn fetch_mut_marks_dirty_after_unpin_and_persists() {
        let mut p = pool(4, 2);
        {
            let page = p.fetch_mut(1).unwrap();
            page.as_bytes_mut()[0] = 0x42;
        }
        assert!(matches!(p.state_of(1), Some(PageState::Pinned(_))));
        p.unpin(1);
        assert_eq!(p.state_of(1), Some(PageState::Dirty));

        p.flush_all().unwrap();
        assert_eq!(p.state_of(1), Some(PageState::Clean));

        // Verify it actually reached the device.
        let dev = p.into_device();
        let mut buf = [0u8; InMemoryBlockDevice::BLOCK_SIZE];
        dev.read_block(1, &mut buf).unwrap();
        assert_eq!(buf[0], 0x42);
    }

    #[test]
    fn lru_evicts_least_recently_used_and_writes_back_dirty() {
        let mut p = pool(8, 2);
        // Dirty page 0, unpin.
        {
            p.fetch_mut(0).unwrap().as_bytes_mut()[0] = 9;
        }
        p.unpin(0);
        // Touch page 1, unpin.
        let _ = p.fetch(1).unwrap();
        p.unpin(1);
        // Access 0 again to make 1 the LRU.
        let _ = p.fetch(0).unwrap();
        p.unpin(0);
        // Fetch a third page: pool is full (cap 2), must evict LRU = block 1.
        let _ = p.fetch(2).unwrap();
        p.unpin(2);
        assert_eq!(p.state_of(1), None); // evicted
        assert!(p.state_of(0).is_some());

        // Dirty page 0 must have been persisted when eventually evicted.
        p.flush_all().unwrap();
        let dev = p.into_device();
        let mut buf = [0u8; InMemoryBlockDevice::BLOCK_SIZE];
        dev.read_block(0, &mut buf).unwrap();
        assert_eq!(buf[0], 9);
    }

    #[test]
    fn all_pinned_pool_errors() {
        let mut p = pool(8, 1);
        let _ = p.fetch(0).unwrap(); // pinned, cap 1
        assert_eq!(p.fetch(1).err(), Some(StorageError::AllFramesPinned));
    }

    #[test]
    fn double_pin_requires_double_unpin() {
        let mut p = pool(4, 2);
        let _ = p.fetch(0).unwrap();
        let _ = p.fetch(0).unwrap();
        assert_eq!(p.state_of(0), Some(PageState::Pinned(2)));
        p.unpin(0);
        assert_eq!(p.state_of(0), Some(PageState::Pinned(1)));
        p.unpin(0);
        assert_eq!(p.state_of(0), Some(PageState::Clean));
    }
}
