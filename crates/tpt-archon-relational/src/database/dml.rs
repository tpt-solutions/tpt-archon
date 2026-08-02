//! `INSERT` / `UPDATE` / `DELETE` execution.

use alloc::string::ToString;
use alloc::vec;
use alloc::vec::Vec;

use crate::executor::Value;
use crate::parser::{DeleteStatement, Expr, InsertStatement, UpdateStatement};

use super::codec::{
    decode_row_validated, encode_row, literal_to_value, mvcc_wrap_row, mvcc_wrap_tombstone,
    MVCC_TOMBSTONE,
};
use super::schema::DbError;
use super::storage::{
    maintain_vector_indexes_for_row, maintain_vector_indexes_on_delete, maybe_build_vector_indexes,
};
use super::Database;

impl Database {
    pub(super) fn run_insert_stmt(&mut self, stmt: &InsertStatement) -> Result<u64, DbError> {
        let in_txn = self.in_transaction;
        if in_txn {
            self.ensure_txn(&stmt.table);
        }
        let Database {
            tables,
            active_txns,
            ..
        } = self;
        let ts = tables
            .iter_mut()
            .find(|(n, _)| n == &stmt.table)
            .map(|(_, t)| t)
            .ok_or_else(|| DbError::UnknownTable(stmt.table.clone()))?;
        let cols: Vec<usize> = if stmt.columns.is_empty() {
            (0..ts.schema.columns.len()).collect()
        } else {
            stmt.columns
                .iter()
                .map(|c| {
                    ts.schema
                        .index_of(c)
                        .ok_or_else(|| DbError::UnknownColumn(c.clone()))
                })
                .collect::<Result<_, _>>()?
        };
        if stmt.values.len() != cols.len() {
            return Err(DbError::ArityMismatch);
        }
        let mut row = vec![Value::Int(0); ts.schema.columns.len()];
        for (slot, lit) in cols.iter().zip(stmt.values.iter()) {
            row[*slot] = literal_to_value(&ts.schema, *slot, lit)?;
        }
        let id = ts.next_row_id;
        ts.next_row_id += 1;
        if !cols.contains(&0) {
            row[0] = Value::Int(id as i64);
        }
        if in_txn {
            let txn = active_txns
                .iter_mut()
                .find(|(n, _)| n == &stmt.table)
                .map(|(_, t)| t)
                .expect("ensure_txn guarantees a transaction exists");
            let wrapped = mvcc_wrap_row(&row);
            ts.mvcc.write(txn, id, wrapped);
        } else {
            let encoded = encode_row(&row);
            ts.tree.insert(id, encoded);
            maintain_vector_indexes_for_row(ts, id, &row);
            maybe_build_vector_indexes(ts)?;
        }
        Ok(1)
    }

    pub(super) fn run_update(&mut self, stmt: &UpdateStatement) -> Result<u64, DbError> {
        let matching: Vec<u64> = self.matching_row_ids(&stmt.table, stmt.filter.as_ref())?;
        let count = matching.len() as u64;
        let in_txn = self.in_transaction;
        if in_txn {
            self.ensure_txn(&stmt.table);
        }
        for id in matching {
            let Database {
                tables,
                active_txns,
                ..
            } = &mut *self;
            let ts = tables
                .iter_mut()
                .find(|(n, _)| n == &stmt.table)
                .map(|(_, t)| t)
                .ok_or_else(|| DbError::UnknownTable(stmt.table.clone()))?;
            let existing_txn = active_txns
                .iter_mut()
                .find(|(n, _)| n == &stmt.table)
                .map(|(_, t)| t);

            // Resolve the current row: this transaction's own buffered write
            // (read-your-own-writes) if any, else the committed tree.
            let mut row =
                if let Some(buffered) = existing_txn.as_deref().and_then(|t| t.get_write(id)) {
                    if buffered[0] == MVCC_TOMBSTONE {
                        continue;
                    }
                    decode_row_validated(id, &buffered[1..], ts.schema.columns.len())?
                } else {
                    let bytes = ts.tree.get(id).ok_or(DbError::RowNotFound(id))?.to_vec();
                    decode_row_validated(id, &bytes, ts.schema.columns.len())?
                };

            for a in &stmt.assignments {
                let slot = ts
                    .schema
                    .index_of(&a.column)
                    .ok_or_else(|| DbError::UnknownColumn(a.column.clone()))?;
                if slot == 0 {
                    continue;
                }
                row[slot] = literal_to_value(&ts.schema, slot, &a.value)?;
            }

            if in_txn {
                let txn = active_txns
                    .iter_mut()
                    .find(|(n, _)| n == &stmt.table)
                    .map(|(_, t)| t)
                    .expect("ensure_txn guarantees a transaction exists");
                let wrapped = mvcc_wrap_row(&row);
                ts.mvcc.write(txn, id, wrapped);
            } else {
                let encoded = encode_row(&row);
                ts.tree.insert(id, encoded);
                maintain_vector_indexes_for_row(ts, id, &row);
            }
        }
        Ok(count)
    }

    pub(super) fn run_delete(&mut self, stmt: &DeleteStatement) -> Result<u64, DbError> {
        let matching = self.matching_row_ids(&stmt.table, stmt.filter.as_ref())?;
        let count = matching.len() as u64;
        let in_txn = self.in_transaction;
        if in_txn {
            self.ensure_txn(&stmt.table);
        }
        let Database {
            tables,
            active_txns,
            ..
        } = self;
        let ts = tables
            .iter_mut()
            .find(|(n, _)| n == &stmt.table)
            .map(|(_, t)| t)
            .ok_or_else(|| DbError::UnknownTable(stmt.table.clone()))?;
        for id in matching {
            if in_txn {
                let txn = active_txns
                    .iter_mut()
                    .find(|(n, _)| n == &stmt.table)
                    .map(|(_, t)| t)
                    .expect("ensure_txn guarantees a transaction exists");
                ts.mvcc.write(txn, id, mvcc_wrap_tombstone());
            } else {
                ts.tree.delete(id);
                maintain_vector_indexes_on_delete(ts, id);
            }
        }
        Ok(count)
    }

    /// Returns row ids from `table_name` whose rows satisfy the predicate.
    fn matching_row_ids(
        &self,
        table_name: &str,
        filter: Option<&Expr>,
    ) -> Result<Vec<u64>, DbError> {
        let ts = self
            .table(table_name)
            .ok_or_else(|| DbError::UnknownTable(table_name.to_string()))?;
        let txn = self
            .active_txns
            .iter()
            .find(|(n, _)| n == table_name)
            .map(|(_, t)| t);
        let cache = match filter {
            Some(expr) => self.build_subquery_cache(expr, &ts.schema.columns, &[], &[])?,
            None => Vec::new(),
        };
        let mut out = Vec::new();
        for id in 0..ts.next_row_id {
            let row = if let Some(buffered) = txn.and_then(|t| t.get_write(id)) {
                if buffered[0] == MVCC_TOMBSTONE {
                    continue;
                }
                decode_row_validated(id, &buffered[1..], ts.schema.columns.len())?
            } else if let Some(bytes) = ts.tree.get(id) {
                decode_row_validated(id, bytes, ts.schema.columns.len())?
            } else {
                continue;
            };
            let keep = match filter {
                None => true,
                Some(expr) => self
                    .eval_where(
                        expr,
                        &ts.schema.columns,
                        &row,
                        &[],
                        &[],
                        &[],
                        &ts.schema.columns,
                        &cache,
                        &mut 0usize,
                    )?
                    .unwrap_or(false),
            };
            if keep {
                out.push(id);
            }
        }
        Ok(out)
    }
}
