//! Storage-engine micro-benchmarks for `tpt-archon-core`.
//!
//! These measure the hot paths (B-Link tree insert/lookup, buffer-pool
//! fetch, WAL append/replay) so the `spec.txt` success metrics can be tracked
//! against real numbers rather than assumed. Comparisons against SQLite belong
//! alongside these once a SQLite harness is added.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use tpt_archon_core::btree::BTree;
use tpt_archon_core::wal::{RecordKind, Wal};

fn bench_btree(c: &mut Criterion) {
    c.bench_function("btree_insert_10k", |b| {
        b.iter(|| {
            let mut t = BTree::new();
            for k in 0..10_000u64 {
                t.insert(black_box(k), black_box(k.to_le_bytes().to_vec()));
            }
            black_box(t.len());
        });
    });

    let mut tree = BTree::new();
    for k in 0..10_000u64 {
        tree.insert(k, k.to_le_bytes().to_vec());
    }
    c.bench_function("btree_lookup_hit", |b| {
        b.iter(|| black_box(tree.get(black_box(4242))));
    });
}

fn bench_wal(c: &mut Criterion) {
    c.bench_function("wal_append_1k", |b| {
        b.iter(|| {
            let mut wal = Wal::new();
            for i in 0..1000u64 {
                wal.append(RecordKind::PageWrite, i, black_box(b"payload"));
            }
            black_box(wal.next_lsn());
        });
    });
}

criterion_group!(benches, bench_btree, bench_wal);
criterion_main!(benches);
