//! `BEGIN` / `COMMIT` / `ROLLBACK` transaction control.
//!
//! Each table keeps its own [`mvcc::MvccStore`]; an open transaction lazily
//! begins a per-table [`mvcc::Transaction`] the first time that table is
//! touched (see `Database::ensure_txn`). Writes made while a transaction is
//! open are buffered in that table's store (not applied to the B-Link tree)
//! so `ROLLBACK` can discard them outright; `COMMIT` validates and applies
//! each table's buffered writes in turn. Because each table commits
//! independently, a conflict on one table during `COMMIT` does not roll back
//! writes already applied to tables committed earlier in the same `COMMIT`
//! — cross-table commit is not atomic. This is a known limitation, not a
//! subtle bug: true multi-table atomicity would need a two-phase commit
//! protocol this engine doesn't have.

use alloc::string::ToString;
use alloc::vec::Vec;

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
        for (table_name, txn) in txns {
            let writes: Vec<(u64, Vec<u8>)> =
                txn.writes_iter().map(|(k, v)| (k, v.to_vec())).collect();
            let ts = self
                .table_mut(&table_name)
                .expect("table existed when its transaction was opened");
            match ts.mvcc.commit(txn) {
                Ok(_) => {
                    for (id, bytes) in writes {
                        if bytes[0] == MVCC_TOMBSTONE {
                            ts.tree.delete(id);
                            maintain_vector_indexes_on_delete(ts, id);
                        } else {
                            ts.tree.insert(id, bytes[1..].to_vec());
                            let row =
                                decode_row_validated(id, &bytes[1..], ts.schema.columns.len())?;
                            maintain_vector_indexes_for_row(ts, id, &row);
                        }
                    }
                    maybe_build_vector_indexes(ts)?;
                }
                Err(mvcc::CommitError::Conflict) => {
                    return Err(DbError::TransactionError(alloc::format!(
                        "commit conflict on table '{table_name}'"
                    )));
                }
            }
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
