//! A minimal end-to-end tour of `tpt-archon-core`'s public surface:
//! a block device, the buffer pool, the WAL, and the B-Link tree.
//!
//! Run with: `cargo run -p tpt-archon-core --example storage_tour`

use tpt_archon_core::block::{BlockDevice, InMemoryBlockDevice};
use tpt_archon_core::btree::BTree;
use tpt_archon_core::page::{BufferPool, PageState};
use tpt_archon_core::wal::{RecordKind, Wal};

fn main() {
    // 1. A block device: 64 blocks of 4 KiB each, held in memory.
    let device = InMemoryBlockDevice::new(64);
    println!(
        "block device: {} blocks x {} bytes",
        device.block_count(),
        InMemoryBlockDevice::BLOCK_SIZE
    );

    // 2. A buffer pool caching up to 8 pages over that device.
    let mut pool = BufferPool::new(device, 8);
    {
        let page = pool.fetch_mut(0).unwrap();
        page.as_bytes_mut()[..5].copy_from_slice(b"hello");
    }
    pool.unpin(0);
    println!("page 0 state after write+unpin: {:?}", pool.state_of(0));
    assert_eq!(pool.state_of(0), Some(PageState::Dirty));
    pool.flush_all().unwrap();
    println!("page 0 state after flush: {:?}", pool.state_of(0));

    // 3. A write-ahead log: append before the page reaches storage, then replay.
    let mut wal = Wal::new();
    wal.append(RecordKind::PageWrite, 0, b"hello");
    wal.append(RecordKind::Commit, 0, b"");
    let replayed = wal.replay(|rec| {
        println!(
            "  replay lsn={} kind={:?} block={}",
            rec.lsn, rec.kind, rec.block_id
        );
    });
    println!("replayed {replayed} WAL records");

    // 4. A B-Link tree index: insert, point lookup, range scan.
    let mut tree = BTree::new();
    for k in 0..1000u64 {
        tree.insert(k, alloc_value(k));
    }
    println!("tree holds {} keys", tree.len());
    println!("get(42) = {:?}", tree.get(42));
    let range = tree.range(100, 105);
    println!("range [100,105) = {} rows", range.len());
    assert_eq!(range.len(), 5);

    println!("storage tour complete.");
}

fn alloc_value(k: u64) -> Vec<u8> {
    k.to_le_bytes().to_vec()
}
