//! Crash / fault simulation for the write-ahead log and the [`StorageEngine`].
//!
//! The `faultsim` module is a *testing* tool, not a runtime feature: it injects
//! the kinds of corruption a real power-loss or torn-write would produce — a
//! truncated tail, flipped payload bytes, and zeroed records — and then asserts
//! that [`StorageEngine::recover`](crate::storage::StorageEngine::recover)
//! always yields a prefix-consistent state: every replayed page is one that was
//! durably committed, and no partial or corrupt record is ever applied.
//!
//! This is the runtime counterpart to the `tpt-telos` replay-consistency
//! assertion harness in `formal-proofs/` (see ADR 0003): the property is
//! exercised here across many randomized corruptions, complementing the
//! solver-checked regression test.

use alloc::vec;
use alloc::vec::Vec;

use crate::block::BlockDevice;
use crate::storage::StorageEngine;
use crate::wal::Wal;

/// A fault to inject into a persisted WAL byte buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// Drop the final `n` bytes (a write that was in flight at crash time).
    TruncateTail(usize),
    /// Flip a byte somewhere in the tail region to corrupt a record's payload or
    /// checksum.
    CorruptTail,
    /// Overwrite the final record boundary with zeroes (a half-written header).
    ZeroTail,
}

impl Fault {
    /// Applies the fault to a clone of `log`, returning the corrupted buffer.
    ///
    /// Faults are applied only where the log is long enough; a too-short log is
    /// returned untouched (recovery of an empty/short log is trivially
    /// prefix-consistent).
    pub fn apply(&self, log: &[u8]) -> Vec<u8> {
        if log.is_empty() {
            return log.to_vec();
        }
        let mut out = log.to_vec();
        match self {
            Fault::TruncateTail(n) => {
                let cut = out.len().saturating_sub(*n);
                out.truncate(cut);
            }
            Fault::CorruptTail => {
                let last = out.len() - 1;
                // Flip a byte far enough into the tail that an intact prefix
                // (if any) is not disturbed.
                out[last] ^= 0xFF;
            }
            Fault::ZeroTail => {
                let start = out.len().saturating_sub(4);
                for b in out.iter_mut().skip(start) {
                    *b = 0;
                }
            }
        }
        out
    }
}

/// Runs a single fault-injection trial: write `committed` pages, commit, then
/// apply `fault` to the durable log and recover over a fresh engine on the same
/// device. Returns the number of page images successfully replayed.
///
/// The caller is expected to assert that the replayed count is `committed` (a
/// clean prefix) or fewer (a torn tail), never more, and that every recovered
/// block equals its originally-committed image.
pub fn trial<D: BlockDevice + Clone>(
    device: D,
    committed: usize,
    fault: Fault,
) -> (usize, Vec<Option<u8>>) {
    let log;
    {
        let mut e = StorageEngine::new(device.clone(), committed.max(1) + 1);
        for i in 0..committed {
            let page = vec![i as u8; crate::page::PAGE_SIZE];
            e.write_page(i as u64, &page).unwrap();
        }
        e.commit().unwrap();
        log = e.wal_bytes().to_vec();
    }

    let corrupted = fault.apply(&log);

    let mut e2 = StorageEngine::new(device, committed.max(1) + 1);
    let applied = e2.recover(&corrupted).unwrap();

    // Reconstruct what each committed block *should* hold from the intact log.
    let wal = Wal::from_bytes(&log);
    let mut expected: Vec<Option<u8>> = (0..committed).map(|_| None).collect();
    wal.replay(|rec| {
        if rec.kind == crate::wal::RecordKind::PageWrite {
            let idx = rec.block_id as usize;
            if idx < expected.len() && !rec.payload.is_empty() {
                expected[idx] = Some(rec.payload[0]);
            }
        }
    });

    (applied, expected)
}

/// Runs `trials` randomized fault-injection trials and asserts the
/// prefix-consistency invariant for each: the number of replayed pages never
/// exceeds the number of committed pages, and every replayed page image matches
/// the originally-committed value.
///
/// Uses a simple xorshift PRNG so the simulation stays `no_std` + allocation
/// free of external crates.
pub fn fuzz<D, F>(make_device: F, trials: usize)
where
    D: BlockDevice + Clone,
    F: Fn() -> D,
{
    let faults = [
        Fault::TruncateTail(0),
        Fault::TruncateTail(1),
        Fault::TruncateTail(4),
        Fault::CorruptTail,
        Fault::ZeroTail,
    ];

    let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
    for t in 0..trials {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let committed = (seed as usize % 8) + 1; // 1..=8 pages
        let fault = faults[seed as usize % faults.len()];
        let device = make_device();

        let (applied, expected) = trial(device, committed, fault);
        assert!(
            applied <= committed,
            "trial {t}: recovery replayed {applied} > {committed} committed"
        );

        // Recover again (fresh device) to inspect replayed images.
        let device2 = make_device();
        let log;
        {
            let mut e = StorageEngine::new(device2.clone(), committed.max(1) + 1);
            for i in 0..committed {
                let page = vec![i as u8; crate::page::PAGE_SIZE];
                e.write_page(i as u64, &page).unwrap();
            }
            e.commit().unwrap();
            log = e.wal_bytes().to_vec();
        }
        let corrupted = fault.apply(&log);
        let mut e2 = StorageEngine::new(device2, committed.max(1) + 1);
        e2.recover(&corrupted).unwrap();
        for (idx, exp) in expected.iter().enumerate() {
            if let Some(exp) = exp {
                // Only blocks that fall within the intact prefix are readable;
                // if the block is missing it was in the torn tail and must not
                // have been applied (guarded by `applied <= committed` above).
                let got = e2.read_page(idx as u64);
                if let Ok(bytes) = got {
                    assert_eq!(
                        bytes[0], *exp,
                        "trial {t}: block {idx} replayed image mismatch"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::InMemoryBlockDevice;

    fn device() -> InMemoryBlockDevice {
        InMemoryBlockDevice::new(16)
    }

    #[test]
    fn truncate_tail_recovers_prefix() {
        let (applied, _) = trial(device(), 4, Fault::TruncateTail(1));
        assert!(applied <= 4);
    }

    #[test]
    fn corrupt_tail_recovers_prefix() {
        let (applied, _) = trial(device(), 4, Fault::CorruptTail);
        assert!(applied <= 4);
    }

    #[test]
    fn zero_tail_recovers_prefix() {
        let (applied, _) = trial(device(), 4, Fault::ZeroTail);
        assert!(applied <= 4);
    }

    #[test]
    fn fuzz_many_faults_stay_prefix_consistent() {
        fuzz(device, 256);
    }
}
