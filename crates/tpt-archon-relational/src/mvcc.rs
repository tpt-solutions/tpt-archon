//! Multi-version concurrency control with snapshot isolation.
//!
//! Each write creates a new version tagged with the writing transaction's
//! commit timestamp; a reader sees the latest version committed at or before its
//! snapshot timestamp. A serializable-isolation check detects write-write
//! conflicts (first-committer-wins) and read-write conflicts against the
//! snapshot, aborting the losing transaction.
//!
//! # Serializability
//!
//! Transactions take a snapshot at begin, buffer their writes, and on commit
//! are validated: a transaction aborts if any key it read was modified by a
//! transaction that committed after its snapshot, or if any key it wrote was
//! modified after its snapshot. This is optimistic concurrency control giving
//! serializable execution — the property `tpt-telos` is intended to prove (see
//! `formal-proofs/`). Until then it is exercised by the conflict tests below.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

/// A logical timestamp used for versioning.
pub type Timestamp = u64;

/// A committed version of a value.
#[derive(Debug, Clone)]
struct Version {
    commit_ts: Timestamp,
    value: Vec<u8>,
}

/// The multi-version store.
#[derive(Debug, Default)]
pub struct MvccStore {
    /// key -> versions ordered by commit timestamp (ascending).
    versions: BTreeMap<u64, Vec<Version>>,
    /// The last commit timestamp handed out.
    clock: Timestamp,
}

/// A read-write transaction.
#[derive(Debug)]
pub struct Transaction {
    snapshot: Timestamp,
    reads: Vec<u64>,
    writes: BTreeMap<u64, Vec<u8>>,
    committed: bool,
}

impl Transaction {
    /// Peeks at this transaction's own buffered write for `key`, if any,
    /// without affecting its read-set — used for read-your-own-writes within
    /// an open transaction, before it commits.
    pub fn get_write(&self, key: u64) -> Option<&[u8]> {
        self.writes.get(&key).map(|v| v.as_slice())
    }

    /// Iterates this transaction's buffered writes — used to apply them to
    /// durable storage after a successful commit.
    pub fn writes_iter(&self) -> impl Iterator<Item = (u64, &[u8])> {
        self.writes.iter().map(|(k, v)| (*k, v.as_slice()))
    }
}

/// Result of attempting to commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitError {
    /// A conflicting transaction committed first; this one must retry.
    Conflict,
}

impl MvccStore {
    /// Creates an empty store.
    pub fn new() -> Self {
        Self {
            versions: BTreeMap::new(),
            clock: 0,
        }
    }

    /// Begins a transaction reading from the current snapshot.
    pub fn begin(&self) -> Transaction {
        Transaction {
            snapshot: self.clock,
            reads: Vec::new(),
            writes: BTreeMap::new(),
            committed: false,
        }
    }

    fn visible_value(&self, key: u64, as_of: Timestamp) -> Option<&[u8]> {
        let chain = self.versions.get(&key)?;
        chain
            .iter()
            .rev()
            .find(|v| v.commit_ts <= as_of)
            .map(|v| v.value.as_slice())
    }

    /// Reads `key` within `txn`, respecting its snapshot and its own buffered
    /// writes.
    pub fn read(&self, txn: &mut Transaction, key: u64) -> Option<Vec<u8>> {
        txn.reads.push(key);
        if let Some(v) = txn.writes.get(&key) {
            return Some(v.clone());
        }
        self.visible_value(key, txn.snapshot).map(|v| v.to_vec())
    }

    /// Buffers a write of `value` to `key` within `txn`.
    pub fn write(&self, txn: &mut Transaction, key: u64, value: Vec<u8>) {
        txn.writes.insert(key, value);
    }

    fn latest_commit_ts(&self, key: u64) -> Option<Timestamp> {
        self.versions
            .get(&key)
            .and_then(|c| c.last())
            .map(|v| v.commit_ts)
    }

    /// Attempts to commit `txn`. On success, its writes become a new version at
    /// a fresh commit timestamp; on conflict the transaction is aborted.
    pub fn commit(&mut self, mut txn: Transaction) -> Result<Timestamp, CommitError> {
        // Validate reads and writes against anything committed after the
        // snapshot (serializable check).
        for &key in txn.reads.iter().chain(txn.writes.keys()) {
            if let Some(latest) = self.latest_commit_ts(key) {
                if latest > txn.snapshot {
                    return Err(CommitError::Conflict);
                }
            }
        }

        self.clock += 1;
        let commit_ts = self.clock;
        for (key, value) in txn.writes.iter() {
            self.versions.entry(*key).or_default().push(Version {
                commit_ts,
                value: value.clone(),
            });
        }
        txn.committed = true;
        Ok(commit_ts)
    }

    /// Reads the latest committed value of `key` (for assertions/tests).
    pub fn latest(&self, key: u64) -> Option<Vec<u8>> {
        self.versions
            .get(&key)
            .and_then(|c| c.last())
            .map(|v| v.value.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_isolation_hides_later_commits() {
        let mut store = MvccStore::new();
        // Seed key 1.
        let mut t0 = store.begin();
        store.write(&mut t0, 1, alloc::vec![10]);
        store.commit(t0).unwrap();

        // Reader takes a snapshot.
        let mut reader = store.begin();

        // A concurrent writer commits a new version.
        let mut writer = store.begin();
        store.write(&mut writer, 1, alloc::vec![20]);
        store.commit(writer).unwrap();

        // Reader still sees the old snapshot value.
        assert_eq!(store.read(&mut reader, 1), Some(alloc::vec![10]));
    }

    #[test]
    fn write_write_conflict_is_detected() {
        let mut store = MvccStore::new();
        let mut seed = store.begin();
        store.write(&mut seed, 1, alloc::vec![0]);
        store.commit(seed).unwrap();

        let mut a = store.begin();
        let mut b = store.begin();
        store.write(&mut a, 1, alloc::vec![1]);
        store.write(&mut b, 1, alloc::vec![2]);

        // First committer wins.
        assert!(store.commit(a).is_ok());
        assert_eq!(store.commit(b), Err(CommitError::Conflict));
        assert_eq!(store.latest(1), Some(alloc::vec![1]));
    }

    #[test]
    fn read_write_conflict_aborts() {
        let mut store = MvccStore::new();
        let mut seed = store.begin();
        store.write(&mut seed, 1, alloc::vec![0]);
        store.commit(seed).unwrap();

        // t reads key 1 under its snapshot.
        let mut t = store.begin();
        let _ = store.read(&mut t, 1);

        // Another txn updates key 1 and commits.
        let mut other = store.begin();
        store.write(&mut other, 1, alloc::vec![9]);
        store.commit(other).unwrap();

        // t now writes something else and tries to commit: its read of key 1 is
        // stale, so it must abort.
        store.write(&mut t, 2, alloc::vec![5]);
        assert_eq!(store.commit(t), Err(CommitError::Conflict));
    }

    #[test]
    fn non_conflicting_transactions_both_commit() {
        let mut store = MvccStore::new();
        let mut a = store.begin();
        let mut b = store.begin();
        store.write(&mut a, 1, alloc::vec![1]);
        store.write(&mut b, 2, alloc::vec![2]);
        assert!(store.commit(a).is_ok());
        assert!(store.commit(b).is_ok());
        assert_eq!(store.latest(1), Some(alloc::vec![1]));
        assert_eq!(store.latest(2), Some(alloc::vec![2]));
    }
}
