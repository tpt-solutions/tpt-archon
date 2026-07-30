//! A persistent relational [`Database`] wired to `tpt-archon-core`.
//!
//! This is the section 4.3 integration: instead of keeping rows only in an
//! in-memory [`Table`](crate::executor::Table), the engine now stores every row
//! in a [`BTree`](tpt_archon_core::btree::BTree) from `tpt-archon-core` (which
//! sits on the unified page cache / `StorageEngine`). `INSERT` / `UPDATE` /
//! `DELETE` mutate the index; `SELECT` scans it, so the full storage stack is
//! exercised end-to-end rather than only the in-memory path.
//!
//! Row encoding is a tiny, allocation-light tag-length-value codec — no serde,
//! consistent with the zero-alloc primitives in `tpt-archon-core`.
//!
//! Submodules: [`schema`] (`ColumnType`/`Schema`/`DbError`), [`codec`] (the
//! row TLV codec and MVCC write-buffer tagging), [`storage`]
//! ([`TableStorage`] and its vector-index maintenance), [`txn`]
//! (`BEGIN`/`COMMIT`/`ROLLBACK`), [`ddl`] (`CREATE`/`ALTER TABLE`,
//! `CREATE`/`DROP VIEW`), [`dml`] (`INSERT`/`UPDATE`/`DELETE`), [`subquery`]
//! (correlated-subquery detection and the uncorrelated-subquery cache used by
//! `WHERE`/`HAVING`), and [`select`] (`SELECT`, including vector top-k). Only
//! the items re-exported below are part of the crate's public surface;
//! everything else is `pub(super)` so it can be shared across these
//! submodules without leaking outside `database`.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tpt_archon_core::btree::BTree;

use crate::executor::ResultSet;
use crate::mvcc;
use crate::parser::{SelectStatement, Statement, TableRef};

mod codec;
mod ddl;
mod dml;
mod schema;
mod select;
mod storage;
mod subquery;
#[cfg(test)]
mod tests;
mod txn;

pub use schema::{ColumnType, DbError, Schema};

use storage::TableStorage;

/// A small relational database backed by `tpt-archon-core`'s B-Link tree.
///
/// Supports multiple tables, SQL DDL (`CREATE TABLE`), multi-predicate
/// `WHERE`, `JOIN`s, `GROUP BY` + aggregates, `ORDER BY`, and
/// `BEGIN`/`COMMIT`/`ROLLBACK` transaction control backed by [`mvcc`].
///
/// Each table keeps its own [`mvcc::MvccStore`]; an open transaction lazily
/// begins a per-table [`mvcc::Transaction`] the first time that table is
/// touched. Writes made while a transaction is open are buffered in that
/// table's store (not applied to the B-Link tree) so `ROLLBACK` can discard
/// them outright; `COMMIT` validates and applies each table's buffered writes
/// in turn. Because each table commits independently, a conflict on one
/// table during `COMMIT` does not roll back writes already applied to
/// tables committed earlier in the same `COMMIT` — cross-table commit is not
/// atomic. This is a known limitation, not a subtle bug: true multi-table
/// atomicity would need a two-phase commit protocol this engine doesn't have.
#[derive(Debug)]
pub struct Database {
    tables: Vec<(String, TableStorage)>,
    /// View definitions: name -> defining query. Views have no storage of
    /// their own; `FROM <view>` expands to running the defining query.
    views: Vec<(String, SelectStatement)>,
    /// Whether we are inside an open transaction (BEGIN without COMMIT/ROLLBACK).
    in_transaction: bool,
    /// Per-table transactions, lazily begun on first touch within the
    /// currently open transaction (empty when `!in_transaction`).
    active_txns: Vec<(String, mvcc::Transaction)>,
}

impl Database {
    /// Creates an empty database with the given schema (legacy single-table
    /// constructor; prefer `Database::empty()` + `CREATE TABLE`).
    pub fn new(schema: Schema) -> Self {
        let mut db = Self::empty();
        db.tables.push((
            "t".to_string(),
            TableStorage {
                schema,
                tree: BTree::new(),
                next_row_id: 0,
                mvcc: mvcc::MvccStore::new(),
                vector_indexes: Vec::new(),
            },
        ));
        db
    }

    /// Creates an empty database with no tables.
    pub fn empty() -> Self {
        Self {
            tables: Vec::new(),
            views: Vec::new(),
            in_transaction: false,
            active_txns: Vec::new(),
        }
    }

    /// Ensures a per-table transaction exists for `table_name` while an outer
    /// `BEGIN` is open, lazily beginning one on first touch.
    fn ensure_txn(&mut self, table_name: &str) {
        if self.active_txns.iter().any(|(n, _)| n == table_name) {
            return;
        }
        if let Some(ts) = self.table(table_name) {
            let txn = ts.mvcc.begin();
            self.active_txns.push((table_name.to_string(), txn));
        }
    }

    /// Looks up a table by name.
    fn table(&self, name: &str) -> Option<&TableStorage> {
        self.tables.iter().find(|(n, _)| n == name).map(|(_, t)| t)
    }

    /// Looks up a table by name (mutable).
    fn table_mut(&mut self, name: &str) -> Option<&mut TableStorage> {
        self.tables
            .iter_mut()
            .find(|(n, _)| n == name)
            .map(|(_, t)| t)
    }

    /// Number of rows across all tables.
    pub fn len(&self) -> usize {
        self.tables.iter().map(|(_, t)| t.tree.len()).sum()
    }

    /// Whether the database has no rows.
    pub fn is_empty(&self) -> bool {
        self.tables.iter().all(|(_, t)| t.tree.is_empty())
    }

    /// Returns the names of all tables in the database.
    pub fn table_names(&self) -> Vec<&str> {
        self.tables.iter().map(|(n, _)| n.as_str()).collect()
    }

    /// Returns the schema of the named table, if it exists.
    pub fn table_schema(&self, name: &str) -> Option<&Schema> {
        self.table(name).map(|ts| &ts.schema)
    }

    /// Executes a parsed [`Statement`], returning a [`ResultSet`] for queries.
    pub fn execute(
        &mut self,
        stmt: &Statement,
        params: &[Vec<f32>],
    ) -> Result<ResultSet, DbError> {
        match stmt {
            Statement::Select(s) => self.run_select(s, params),
            Statement::Insert(i) => {
                self.run_insert_stmt(i)?;
                Ok(empty_result_set())
            }
            Statement::Update(u) => {
                self.run_update(u)?;
                Ok(empty_result_set())
            }
            Statement::Delete(d) => {
                self.run_delete(d)?;
                Ok(empty_result_set())
            }
            Statement::CreateTable(ct) => {
                self.run_create_table(ct)?;
                Ok(empty_result_set())
            }
            Statement::CreateView(cv) => {
                self.run_create_view(cv)?;
                Ok(empty_result_set())
            }
            Statement::DropView(name) => {
                self.run_drop_view(name)?;
                Ok(empty_result_set())
            }
            Statement::AlterTable(at) => {
                self.run_alter_table(at)?;
                Ok(empty_result_set())
            }
            Statement::Begin => {
                self.run_begin()?;
                Ok(empty_result_set())
            }
            Statement::Commit => {
                self.run_commit()?;
                Ok(empty_result_set())
            }
            Statement::Rollback => {
                self.run_rollback()?;
                Ok(empty_result_set())
            }
        }
    }

    /// Like [`execute`](Database::execute) but takes ownership of the statement,
    /// used by callers that build statements directly (e.g. arity tests).
    pub fn execute_checked(&mut self, stmt: &Statement) -> Result<ResultSet, DbError> {
        self.execute(stmt, &[])
    }
}

fn empty_result_set() -> ResultSet {
    ResultSet {
        columns: Vec::new(),
        rows: Vec::new(),
    }
}

/// Whether `stmt`'s `FROM`/`JOIN` clauses reference the (not-yet-created)
/// table/view name `name` — used to reject a self-referencing `CREATE VIEW`
/// up front, since forward references are otherwise impossible (a view can
/// only reference tables/views that already exist).
fn select_references_table(stmt: &SelectStatement, name: &str) -> bool {
    if let TableRef::Named { name: n, .. } = &stmt.table {
        if n == name {
            return true;
        }
    }
    stmt.joins
        .iter()
        .any(|j| matches!(&j.table, TableRef::Named { name: n, .. } if n == name))
}
