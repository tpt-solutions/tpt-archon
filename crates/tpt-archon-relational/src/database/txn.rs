//! `BEGIN` / `COMMIT` / `ROLLBACK` transaction control.
//!
//! Each table keeps its own [`mvcc::MvccStore`]; an open transaction lazily
//! begins a per-table [`mvcc::Transaction`] the first time that table is
//! touched (see `Database::ensure_txn`). Writes made while a transaction is
//! open are buffered in that table's store (not applied to the B-Link tree)
//! so `ROLLBACK` can discard them outright. `COMMIT` is atomic across tables:
//! it validates (and MVCC-commits) every table's buffered writes *before*
//! applying any of them to the B-Link tree, so a conflict on any table during
//! `COMMIT` aborts the whole commit and leaves the database unchanged (no
//! partial application across tables). This holds under the engine's
//! single-writer mutex, which serializes commits.

use alloc::string::ToString;
use alloc::vec::Vec;

/// Buffered per-table writes captured at COMMIT time: the table name and its
/// `(row key, encoded row)` writes, snapshotted before MVCC commit/apply.
type CommittedWrites = Vec<(String, Vec<(u64, Vec<u8>)>)>;

use crate::mvcc;

use super::codec::{decode_row_validated, MVCC_TOMBSTONE};
use super::schema::DbError;
use super::storage::{
    maintain_vector_indexes_for_row, maintain_vector_indexes_on_delete, maybe_build_vector_indexes,
};
use super::Database;

impl Database {
    pub(super) fn run_begin(&mut self) -> Result<(), DbError> {
        if self.in_transaction {
            return Err(DbError::TransactionError(
                "transaction already in progress".to_string(),
            ));
        }
        self.in_transaction = true;
        self.active_txns.clear();
        Ok(())
    }

    pub(super) fn run_commit(&mut self) -> Result<(), DbError> {
        if !self.in_transaction {
            return Err(DbError::TransactionError(
                "no active transaction".to_string(),
            ));
        }
        let txns = core::mem::take(&mut self.active_txns);
        self.in_transaction = false;

        // Snapshot every table's buffered writes up front. MVCC `commit`
        // consumes the transaction, so we capture the writes first.
        let mut writes_by_table: CommittedWrites = Vec::with_capacity(txns.len());
        for (table_name, txn) in &txns {
            let writes: Vec<(u64, Vec<u8>)> =
                txn.writes_iter().map(|(k, v)| (k, v.to_vec())).collect();
            writes_by_table.push((table_name.clone(), writes));
        }

        // Validation phase: validate/commit every per-table MVCC transaction
        // *before* applying any write to the B-Link tree. User-visible state
        // lives in `ts.tree`; an MVCC commit that is not followed by a tree
        // write is invisible to readers, so bailing on the first conflict
        // here leaves the database exactly as it was before COMMIT. Under the
        // engine's single-writer mutex this is safe and makes a multi-table
        // COMMIT atomic (no partial application across tables).
        for (table_name, txn) in txns {
            let ts = self
                .table_mut(&table_name)
                .expect("table existed when its transaction was opened");
            if let Err(mvcc::CommitError::Conflict) = ts.mvcc.commit(txn) {
                return Err(DbError::TransactionError(alloc::format!(
                    "commit conflict on table '{table_name}'"
                )));
            }
        }

        // Apply phase: only reached if every validation above succeeded.
        for (table_name, writes) in writes_by_table {
            let ts = self
                .table_mut(&table_name)
                .expect("table existed when its transaction was opened");
            for (id, bytes) in writes {
                if bytes[0] == MVCC_TOMBSTONE {
                    ts.tree.delete(id);
                    maintain_vector_indexes_on_delete(ts, id);
                } else {
                    ts.tree.insert(id, bytes[1..].to_vec());
                    let row = decode_row_validated(id, &bytes[1..], ts.schema.columns.len())?;
                    maintain_vector_indexes_for_row(ts, id, &row);
                }
            }
            maybe_build_vector_indexes(ts)?;
        }
        Ok(())
    }

    pub(super) fn run_rollback(&mut self) -> Result<(), DbError> {
        if !self.in_transaction {
            return Err(DbError::TransactionError(
                "no active transaction".to_string(),
            ));
        }
        // Buffered per-table transactions are simply dropped without
        // committing, discarding every write made since BEGIN.
        self.active_txns.clear();
        self.in_transaction = false;
        Ok(())
    }
}
