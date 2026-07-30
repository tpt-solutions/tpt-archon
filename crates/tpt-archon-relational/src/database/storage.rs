//! [`TableStorage`]: a single table's schema, B-Link tree, per-table MVCC
//! store, and lazily-built vector indexes.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use tpt_archon_core::btree::BTree;

use crate::executor::Value;
use crate::mvcc;
use crate::vector_index;

use super::codec::decode_row_validated;
use super::schema::{ColumnType, DbError, Schema};

/// A table's storage: its schema, B-Link tree, and per-table MVCC store used
/// while a transaction is open on it.
#[derive(Debug)]
pub(super) struct TableStorage {
    pub(super) schema: Schema,
    pub(super) tree: BTree,
    pub(super) next_row_id: u64,
    pub(super) mvcc: mvcc::MvccStore,
    /// Column name -> IVFFlat index, built lazily once a vector column's live
    /// row count crosses `vector_index::MIN_ROWS_FOR_INDEX` and incrementally
    /// maintained from then on (see `maintain_vector_indexes_for_row`,
    /// `maintain_vector_indexes_on_delete`, and `maybe_build_vector_indexes`
    /// below).
    pub(super) vector_indexes: Vec<(String, vector_index::IvfFlatIndex)>,
}

/// Updates every vector index on `ts` to reflect `row`'s current value at
/// `id`: inserts/replaces if the indexed column holds a vector, removes if
/// not (e.g. set to `NULL` by an `UPDATE`). No-op if `ts` has no indexes yet.
pub(super) fn maintain_vector_indexes_for_row(ts: &mut TableStorage, id: u64, row: &[Value]) {
    if ts.vector_indexes.is_empty() {
        return;
    }
    let schema = &ts.schema;
    for (col_name, idx) in &mut ts.vector_indexes {
        match schema.index_of(col_name).map(|slot| &row[slot]) {
            Some(Value::Vector(v)) => idx.insert(id, v),
            _ => idx.remove(id),
        }
    }
}

/// Removes `id` from every vector index on `ts` (used by `DELETE`).
pub(super) fn maintain_vector_indexes_on_delete(ts: &mut TableStorage, id: u64) {
    for (_, idx) in &mut ts.vector_indexes {
        idx.remove(id);
    }
}

/// Builds an IVFFlat index for any vector column that doesn't have one yet,
/// once `ts`'s row-id counter crosses `vector_index::MIN_ROWS_FOR_INDEX`.
/// Scans the table once per column being built — a one-time cost paid once
/// per column, amortized by every vector query afterward; further writes
/// maintain the index incrementally via `maintain_vector_indexes_for_row` /
/// `maintain_vector_indexes_on_delete` instead of re-scanning.
pub(super) fn maybe_build_vector_indexes(ts: &mut TableStorage) -> Result<(), DbError> {
    if (ts.next_row_id as usize) < vector_index::MIN_ROWS_FOR_INDEX {
        return Ok(());
    }
    let pending_cols: Vec<(usize, String)> = ts
        .schema
        .columns
        .iter()
        .zip(ts.schema.types.iter())
        .enumerate()
        .filter(|(_, (_, t))| **t == ColumnType::Vector)
        .map(|(i, (name, _))| (i, name.clone()))
        .filter(|(_, name)| !ts.vector_indexes.iter().any(|(c, _)| c == name))
        .collect();
    if pending_cols.is_empty() {
        return Ok(());
    }
    let col_count = ts.schema.columns.len();
    let mut per_col: Vec<Vec<(u64, Vec<f32>)>> = vec![Vec::new(); pending_cols.len()];
    for id in 0..ts.next_row_id {
        let Some(bytes) = ts.tree.get(id) else {
            continue;
        };
        let row = decode_row_validated(id, bytes, col_count)?;
        for (bucket, (slot, _)) in per_col.iter_mut().zip(pending_cols.iter()) {
            if let Value::Vector(v) = &row[*slot] {
                bucket.push((id, v.clone()));
            }
        }
    }
    for ((_, name), vectors) in pending_cols.iter().zip(per_col) {
        if !vectors.is_empty() {
            ts.vector_indexes
                .push((name.clone(), vector_index::IvfFlatIndex::build(&vectors)));
        }
    }
    Ok(())
}
