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
use super::schema::{DbError, Schema};
use super::storage::{
    maintain_vector_indexes_for_row, maintain_vector_indexes_on_delete, maybe_build_vector_indexes,
    TableStorage,
};
use super::Database;
/// Returns the column groups that must hold distinct values: the primary key (if
/// any) followed by each declared `UNIQUE` group.
fn unique_groups(schema: &Schema) -> Vec<Vec<String>> {
    let mut groups: Vec<Vec<String>> = Vec::new();
    if let Some(pk) = &schema.primary_key {
        groups.push(pk.clone());
    }
    groups.extend(schema.unique.iter().cloned());
    groups
}

impl Database {
    pub(super) fn run_insert_stmt(&mut self, stmt: &InsertStatement) -> Result<u64, DbError> {
        let in_txn = self.in_transaction;
        if in_txn {
            self.ensure_txn(&stmt.table);
        }

        // Resolve the schema and column slots (immutable access is enough here).
        let schema = self
            .table(&stmt.table)
            .ok_or_else(|| DbError::UnknownTable(stmt.table.clone()))?
            .schema
            .clone();
        let cols: Vec<usize> = if stmt.columns.is_empty() {
            (0..schema.columns.len()).collect()
        } else {
            stmt.columns
                .iter()
                .map(|c| {
                    schema
                        .index_of(c)
                        .ok_or_else(|| DbError::UnknownColumn(c.clone()))
                })
                .collect::<Result<_, _>>()?
        };
        for row_values in &stmt.values {
            if row_values.len() != cols.len() {
                return Err(DbError::ArityMismatch);
            }
        }

        // Build the candidate rows. Omitted columns default to NULL (the engine
        // has no column defaults); the implicit `id` column is filled in below.
        let mut rows: Vec<Vec<Value>> = Vec::new();
        for row_values in &stmt.values {
            let mut row = vec![Value::Null; schema.columns.len()];
            for (slot, lit) in cols.iter().zip(row_values.iter()) {
                row[*slot] = literal_to_value(&schema, *slot, lit)?;
            }
            rows.push(row);
        }

        // Enforce constraints against the committed state and against other rows in
        // this batch (so a multi-row INSERT with an internal duplicate is rejected).
        let ts_ref = self.table(&stmt.table).expect("table checked above");
        for (i, row) in rows.iter().enumerate() {
            self.enforce_constraints(&stmt.table, ts_ref, row, None)?;
            for group in unique_groups(&schema) {
                let key: Vec<Value> = group
                    .iter()
                    .map(|c| row[schema.index_of(c).expect("constraint column must exist")].clone())
                    .collect();
                if key.contains(&Value::Null) {
                    continue;
                }
                for (j, other) in rows.iter().enumerate() {
                    if j == i {
                        continue;
                    }
                    let other_key: Vec<Value> = group
                        .iter()
                        .map(|c| {
                            other[schema.index_of(c).expect("constraint column must exist")].clone()
                        })
                        .collect();
                    if other_key == key {
                        return Err(DbError::UniqueViolation(group.join(",")));
                    }
                }
            }
        }

        // Apply.
        let mut inserted = 0u64;
        for row in rows {
            let ts = self
                .tables
                .iter_mut()
                .find(|(n, _)| n == &stmt.table)
                .map(|(_, t)| t)
                .ok_or_else(|| DbError::UnknownTable(stmt.table.clone()))?;
            let id = ts.next_row_id;
            ts.next_row_id += 1;
            let mut row = row;
            // The implicit `id` column (slot 0) is auto-assigned unless the
            // caller supplied it explicitly.
            if !cols.contains(&0) {
                row[0] = Value::Int(id as i64);
            }
            if in_txn {
                let txn = self
                    .active_txns
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
            inserted += 1;
        }
        Ok(inserted)
    }

    pub(super) fn run_update(&mut self, stmt: &UpdateStatement) -> Result<u64, DbError> {
        let matching: Vec<u64> = self.matching_row_ids(&stmt.table, stmt.filter.as_ref())?;
        let count = matching.len() as u64;
        let in_txn = self.in_transaction;
        if in_txn {
            self.ensure_txn(&stmt.table);
        }
        for id in matching {
            let mut row = match self.current_row(&stmt.table, id)? {
                Some(r) => r,
                None => continue,
            };
            for a in &stmt.assignments {
                let slot = self
                    .table(&stmt.table)
                    .expect("table checked above")
                    .schema
                    .index_of(&a.column)
                    .ok_or_else(|| DbError::UnknownColumn(a.column.clone()))?;
                if slot == 0 {
                    continue;
                }
                row[slot] = literal_to_value(
                    &self.table(&stmt.table).expect("table checked above").schema,
                    slot,
                    &a.value,
                )?;
            }
            let ts_ref = self.table(&stmt.table).expect("table checked above");
            self.enforce_constraints(&stmt.table, ts_ref, &row, Some(id))?;
            let ts = self
                .tables
                .iter_mut()
                .find(|(n, _)| n == &stmt.table)
                .map(|(_, t)| t)
                .ok_or_else(|| DbError::UnknownTable(stmt.table.clone()))?;
            if in_txn {
                let txn = self
                    .active_txns
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
        for id in matching {
            let row = match self.current_row(&stmt.table, id)? {
                Some(r) => r,
                None => continue,
            };
            self.enforce_fk_on_delete(&stmt.table, &row)?;
            let ts = self
                .tables
                .iter_mut()
                .find(|(n, _)| n == &stmt.table)
                .map(|(_, t)| t)
                .ok_or_else(|| DbError::UnknownTable(stmt.table.clone()))?;
            if in_txn {
                let txn = self
                    .active_txns
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

    /// Returns every live row of `ts` (committed tree plus the current
    /// transaction's buffered writes) as `(id, values)`, skipping `exclude_id`
    /// and any tombstoned rows. Used by constraint enforcement to scan for
    /// duplicate keys and dangling foreign keys.
    fn all_rows(
        &self,
        table_name: &str,
        ts: &TableStorage,
        exclude_id: Option<u64>,
    ) -> Result<Vec<(u64, Vec<Value>)>, DbError> {
        let txn = self
            .active_txns
            .iter()
            .find(|(n, _)| n == table_name)
            .map(|(_, t)| t);
        let ncols = ts.schema.columns.len();
        let mut out = Vec::new();
        for id in 0..ts.next_row_id {
            if Some(id) == exclude_id {
                continue;
            }
            let values = if let Some(buffered) = txn.and_then(|t| t.get_write(id)) {
                if buffered[0] == MVCC_TOMBSTONE {
                    continue;
                }
                decode_row_validated(id, &buffered[1..], ncols)?
            } else if let Some(bytes) = ts.tree.get(id) {
                decode_row_validated(id, bytes, ncols)?
            } else {
                continue;
            };
            out.push((id, values));
        }
        Ok(out)
    }

    /// Reads the current value of row `id` from `table_name`, honoring the active
    /// transaction's buffered writes. Returns `Ok(None)` if the row is absent or a
    /// tombstone.
    fn current_row(&self, table_name: &str, id: u64) -> Result<Option<Vec<Value>>, DbError> {
        let ts = self
            .table(table_name)
            .ok_or_else(|| DbError::UnknownTable(table_name.to_string()))?;
        let txn = self
            .active_txns
            .iter()
            .find(|(n, _)| n == table_name)
            .map(|(_, t)| t);
        let ncols = ts.schema.columns.len();
        let values = if let Some(buffered) = txn.and_then(|t| t.get_write(id)) {
            if buffered[0] == MVCC_TOMBSTONE {
                return Ok(None);
            }
            decode_row_validated(id, &buffered[1..], ncols)?
        } else if let Some(bytes) = ts.tree.get(id) {
            decode_row_validated(id, bytes, ncols)?
        } else {
            return Ok(None);
        };
        Ok(Some(values))
    }

    /// Validates a candidate `row` (values, with the implicit `id` already set at
    /// slot 0) against every declared constraint on `ts`. `exclude_id` skips one
    /// existing row (the row being updated) when checking uniqueness.
    fn enforce_constraints(
        &self,
        table_name: &str,
        ts: &TableStorage,
        row: &[Value],
        exclude_id: Option<u64>,
    ) -> Result<(), DbError> {
        // NOT NULL.
        for col in &ts.schema.not_null {
            let idx = ts.schema.index_of(col).expect("not_null column must exist");
            if row[idx] == Value::Null {
                return Err(DbError::NotNullViolation(col.clone()));
            }
        }

        // PRIMARY KEY + UNIQUE uniqueness (NULLs in a key are not considered
        // duplicates, matching PostgreSQL).
        let mut groups: Vec<Vec<String>> = Vec::new();
        if let Some(pk) = &ts.schema.primary_key {
            groups.push(pk.clone());
        }
        groups.extend(ts.schema.unique.iter().cloned());
        for group in &groups {
            let key: Vec<Value> = group
                .iter()
                .map(|c| {
                    let idx = ts.schema.index_of(c).expect("constraint column must exist");
                    row[idx].clone()
                })
                .collect();
            if key.contains(&Value::Null) {
                continue;
            }
            for (id, vals) in self.all_rows(table_name, ts, exclude_id)? {
                if Some(id) == exclude_id {
                    continue;
                }
                let other: Vec<Value> = group
                    .iter()
                    .map(|c| {
                        let idx = ts.schema.index_of(c).expect("constraint column must exist");
                        vals[idx].clone()
                    })
                    .collect();
                if other == key {
                    return Err(DbError::UniqueViolation(group.join(",")));
                }
            }
        }

        // CHECK expressions (evaluated against the candidate row).
        for expr in &ts.schema.checks {
            let cache = self.build_subquery_cache(expr, &ts.schema.columns, &[], &[])?;
            let ok = self
                .eval_where(
                    expr,
                    &ts.schema.columns,
                    row,
                    &[],
                    &[],
                    &[],
                    &ts.schema.columns,
                    &cache,
                    &mut 0usize,
                    &[],
                )?
                .unwrap_or(false);
            if !ok {
                return Err(DbError::CheckViolation(
                    "CHECK constraint evaluated to false or NULL".to_string(),
                ));
            }
        }

        // FOREIGN KEY: each referenced key must exist in the referenced table.
        for fk in &ts.schema.foreign_keys {
            let key: Vec<Value> = fk
                .columns
                .iter()
                .map(|c| {
                    let idx = ts.schema.index_of(c).expect("fk column must exist");
                    row[idx].clone()
                })
                .collect();
            if key.contains(&Value::Null) {
                continue;
            }
            let ref_ts = self.table(&fk.ref_table).ok_or_else(|| {
                DbError::ForeignKeyViolation(format!(
                    "referenced table '{}' does not exist",
                    fk.ref_table
                ))
            })?;
            let mut found = false;
            for (_, vals) in self.all_rows(&fk.ref_table, ref_ts, None)? {
                let other: Vec<Value> = fk
                    .ref_columns
                    .iter()
                    .map(|c| {
                        let idx = ref_ts.schema.index_of(c).expect("fk ref column must exist");
                        vals[idx].clone()
                    })
                    .collect();
                if other == key {
                    found = true;
                    break;
                }
            }
            if !found {
                return Err(DbError::ForeignKeyViolation(format!(
                    "no matching key {} in referenced table '{}'",
                    key.iter()
                        .map(|v| format!("{:?}", v))
                        .collect::<Vec<_>>()
                        .join(","),
                    fk.ref_table
                )));
            }
        }
        Ok(())
    }

    /// Enforces `ON DELETE RESTRICT` semantics: a row in `table_name` identified by
    /// `row` may not be deleted while another table has a foreign key pointing at
    /// it.
    fn enforce_fk_on_delete(&self, table_name: &str, row: &[Value]) -> Result<(), DbError> {
        for (other_name, other_ts) in &self.tables {
            for fk in &other_ts.schema.foreign_keys {
                if fk.ref_table != table_name {
                    continue;
                }
                let ref_idx: Vec<usize> = fk
                    .ref_columns
                    .iter()
                    .map(|c| {
                        self.table(table_name)
                            .expect("referenced table must exist")
                            .schema
                            .index_of(c)
                            .expect("fk ref column must exist")
                    })
                    .collect();
                let key: Vec<Value> = ref_idx.iter().map(|i| row[*i].clone()).collect();
                if key.contains(&Value::Null) {
                    continue;
                }
                for (_, vals) in self.all_rows(other_name, other_ts, None)? {
                    let other_keys: Vec<Value> = fk
                        .columns
                        .iter()
                        .map(|c| {
                            let idx = other_ts.schema.index_of(c).expect("fk column must exist");
                            vals[idx].clone()
                        })
                        .collect();
                    if other_keys == key {
                        return Err(DbError::ForeignKeyViolation(format!(
                            "row is still referenced by '{}.{}'",
                            other_name,
                            fk.columns.join(",")
                        )));
                    }
                }
            }
        }
        Ok(())
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
                        &[],
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
