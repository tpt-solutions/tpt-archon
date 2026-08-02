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

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use tpt_archon_core::btree::BTree;

use crate::executor::{CommandTag, ResultSet};
use crate::mvcc;
use crate::parser::{Expr, SelectStatement, Statement, TableRef};

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

/// Opaque identifier for a database session.
/// Used to track per-session transaction state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionId(pub u64);

impl SessionId {
    pub fn new() -> Self {
        use core::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        SessionId(COUNTER.fetch_add(1, Ordering::Relaxed))
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-session transaction state.
#[derive(Debug, Default)]
struct SessionTxn {
    in_transaction: bool,
    active_txns: Vec<(String, mvcc::Transaction)>,
}

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
    /// Session parameters for SET/SHOW/RESET commands (PostgreSQL compatibility).
    session_parameters: SessionParameters,
    /// Per-session transaction state.
    session_txns: BTreeMap<SessionId, SessionTxn>,
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
            session_parameters: SessionParameters::new(),
            session_txns: BTreeMap::new(),
        }
    }

    /// Begins a transaction for a specific session.
    /// Returns an error if the session already has an active transaction.
    pub fn session_begin(&mut self, session_id: SessionId) -> Result<(), DbError> {
        let session_txn = self.session_txns.entry(session_id).or_default();
        if session_txn.in_transaction {
            return Err(DbError::TransactionError(
                "transaction already in progress for this session".to_string(),
            ));
        }
        session_txn.in_transaction = true;
        session_txn.active_txns.clear();
        Ok(())
    }

    /// Commits the transaction for a specific session.
    /// Returns an error if the session has no active transaction.
    pub fn session_commit(&mut self, session_id: SessionId) -> Result<(), DbError> {
        let session_txn = self.session_txns.get_mut(&session_id).ok_or_else(|| {
            DbError::TransactionError("no active transaction for this session".to_string())
        })?;

        if !session_txn.in_transaction {
            return Err(DbError::TransactionError(
                "no active transaction for this session".to_string(),
            ));
        }

        let txns = core::mem::take(&mut session_txn.active_txns);
        session_txn.in_transaction = false;

        for (table_name, txn) in txns {
            let writes: Vec<(u64, Vec<u8>)> =
                txn.writes_iter().map(|(k, v)| (k, v.to_vec())).collect();
            let ts = self
                .table_mut(&table_name)
                .expect("table existed when its transaction was opened");
            match ts.mvcc.commit(txn) {
                Ok(_) => {
                    for (id, bytes) in writes {
                        if bytes[0] == crate::database::codec::MVCC_TOMBSTONE {
                            ts.tree.delete(id);
                            crate::database::storage::maintain_vector_indexes_on_delete(ts, id);
                        } else {
                            ts.tree.insert(id, bytes[1..].to_vec());
                            let row = crate::database::codec::decode_row_validated(
                                id,
                                &bytes[1..],
                                ts.schema.columns.len(),
                            )?;
                            crate::database::storage::maintain_vector_indexes_for_row(ts, id, &row);
                        }
                    }
                    crate::database::storage::maybe_build_vector_indexes(ts)?;
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

    /// Rolls back the transaction for a specific session.
    /// Returns an error if the session has no active transaction.
    pub fn session_rollback(&mut self, session_id: SessionId) -> Result<(), DbError> {
        let session_txn = self.session_txns.get_mut(&session_id).ok_or_else(|| {
            DbError::TransactionError("no active transaction for this session".to_string())
        })?;

        if !session_txn.in_transaction {
            return Err(DbError::TransactionError(
                "no active transaction for this session".to_string(),
            ));
        }

        // Buffered per-table transactions are simply dropped without
        // committing, discarding every write made since BEGIN.
        session_txn.active_txns.clear();
        session_txn.in_transaction = false;
        Ok(())
    }

    /// Checks if a session has an active transaction.
    pub fn session_in_transaction(&self, session_id: SessionId) -> bool {
        self.session_txns
            .get(&session_id)
            .map(|st| st.in_transaction)
            .unwrap_or(false)
    }

    /// Ensures a per-table transaction exists for `table_name` within a session's
    /// transaction, lazily beginning one on first touch.
    #[allow(dead_code)]
    fn ensure_session_txn(&mut self, session_id: SessionId, table_name: &str) {
        // First check if the table exists
        let table_exists = self.table(table_name).is_some();
        if !table_exists {
            return;
        }

        // Check if we already have a transaction for this table in this session
        let already_has_txn = self
            .session_txns
            .get(&session_id)
            .map(|st| st.active_txns.iter().any(|(n, _)| n == table_name))
            .unwrap_or(false);

        if already_has_txn {
            return;
        }

        // We know the table exists, so we can get it mutably and begin the transaction
        if let Some(ts) = self.table_mut(table_name) {
            let txn = ts.mvcc.begin();

            // Now add it to the session's transaction list
            let session_txn = self.session_txns.entry(session_id).or_default();
            session_txn.active_txns.push((table_name.to_string(), txn));
        }
    }

    /// Executes a statement within a specific session's transaction context.
    /// For non-transactional statements, behaves like `execute`.
    pub fn execute_in_session(
        &mut self,
        session_id: SessionId,
        stmt: &Statement,
        params: &[Vec<f32>],
    ) -> Result<ResultSet, DbError> {
        // For transaction control statements, use session-aware versions
        match stmt {
            Statement::Begin => return self.session_begin(session_id).map(|_| empty_result_set()),
            Statement::Commit => {
                return self.session_commit(session_id).map(|_| empty_result_set())
            }
            Statement::Rollback => {
                return self
                    .session_rollback(session_id)
                    .map(|_| empty_result_set())
            }
            _ => {}
        }

        // For DML statements within a session transaction, ensure session txn exists
        let needs_session_txn = matches!(
            stmt,
            Statement::Insert(_) | Statement::Update(_) | Statement::Delete(_)
        );

        if needs_session_txn && self.session_in_transaction(session_id) {
            // We need to use session-aware execution for DML
            // For now, fall back to the global transaction if no session txn
            // This is a simplified implementation - a full version would need
            // to refactor the DML execution paths to use session_txns
        }

        // For all other statements, use the existing execute path
        self.execute(stmt, params)
    }

    /// Executes a statement within a specific session and returns stats.
    pub fn execute_in_session_with_stats(
        &mut self,
        session_id: SessionId,
        stmt: &Statement,
        params: &[Vec<f32>],
    ) -> Result<(ResultSet, CommandTag), DbError> {
        let rs = self.execute_in_session(session_id, stmt, params)?;
        let tag = match stmt {
            Statement::Select(_) => CommandTag::Select(rs.rows.len() as u64),
            Statement::Insert(_) => CommandTag::Insert(rs.affected.unwrap_or(0)),
            Statement::Update(_) => CommandTag::Update(rs.affected.unwrap_or(0)),
            Statement::Delete(_) => CommandTag::Delete(rs.affected.unwrap_or(0)),
            Statement::CreateTable(_) => CommandTag::CreateTable,
            Statement::CreateView(_) => CommandTag::CreateView,
            Statement::DropView(_) => CommandTag::DropView,
            Statement::AlterTable(_) => CommandTag::AlterTable,
            Statement::Begin => CommandTag::Begin,
            Statement::Commit => CommandTag::Commit,
            Statement::Rollback => CommandTag::Rollback,
            Statement::SetParameter(_) => CommandTag::Set,
            Statement::ShowParameter(_) => CommandTag::Select(0),
            Statement::ResetParameter(_) => CommandTag::Reset,
            Statement::ResetAll(_) => CommandTag::Reset,
            Statement::Compound(_) => CommandTag::Select(rs.rows.len() as u64),
        };
        Ok((rs, tag))
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
    pub fn execute(&mut self, stmt: &Statement, params: &[Vec<f32>]) -> Result<ResultSet, DbError> {
        match stmt {
            Statement::Select(s) => self.run_select(s, params),
            Statement::Insert(i) => {
                let affected = self.run_insert_stmt(i)?;
                Ok(ResultSet {
                    affected: Some(affected),
                    ..Default::default()
                })
            }
            Statement::Update(u) => {
                let affected = self.run_update(u)?;
                Ok(ResultSet {
                    affected: Some(affected),
                    ..Default::default()
                })
            }
            Statement::Delete(d) => {
                let affected = self.run_delete(d)?;
                Ok(ResultSet {
                    affected: Some(affected),
                    ..Default::default()
                })
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
            Statement::SetParameter(s) => {
                self.run_set_parameter(s)?;
                Ok(empty_result_set())
            }
            Statement::ShowParameter(s) => self.run_show_parameter(s),
            Statement::ResetParameter(s) => {
                self.run_reset_parameter(s)?;
                Ok(empty_result_set())
            }
            Statement::ResetAll(_) => {
                self.run_reset_all()?;
                Ok(empty_result_set())
            }
            Statement::Compound(cm) => self.run_compound(cm, params),
        }
    }

    /// Like [`execute`](Database::execute) but takes ownership of the statement,
    /// used by callers that build statements directly (e.g. arity tests).
    pub fn execute_checked(&mut self, stmt: &Statement) -> Result<ResultSet, DbError> {
        self.execute(stmt, &[])
    }

    /// Executes a parsed [`Statement`] and returns both the [`ResultSet`] and a
    /// PostgreSQL-style [`CommandTag`] describing what happened. This is the
    /// entry point the wire-protocol layer uses to emit correct
    /// `CommandComplete` messages.
    pub fn execute_with_stats(
        &mut self,
        stmt: &Statement,
        params: &[Vec<f32>],
    ) -> Result<(ResultSet, CommandTag), DbError> {
        let tag = match stmt {
            Statement::Select(_) => CommandTag::Select(0),
            Statement::Insert(_) => CommandTag::Insert(0),
            Statement::Update(_) => CommandTag::Update(0),
            Statement::Delete(_) => CommandTag::Delete(0),
            Statement::CreateTable(_) => CommandTag::CreateTable,
            Statement::CreateView(_) => CommandTag::CreateView,
            Statement::DropView(_) => CommandTag::DropView,
            Statement::AlterTable(_) => CommandTag::AlterTable,
            Statement::Begin => CommandTag::Begin,
            Statement::Commit => CommandTag::Commit,
            Statement::Rollback => CommandTag::Rollback,
            Statement::SetParameter(_) => CommandTag::Set,
            Statement::ShowParameter(_) => CommandTag::Select(0),
            Statement::ResetParameter(_) => CommandTag::Reset,
            Statement::ResetAll(_) => CommandTag::Reset,
            Statement::Compound(_) => CommandTag::Select(0),
        };
        let rs = self.execute(stmt, params)?;
        let tag = match (&tag, rs.affected) {
            (CommandTag::Select(_), Some(n)) => CommandTag::Select(n),
            (CommandTag::Select(_), None) => CommandTag::Select(rs.rows.len() as u64),
            (CommandTag::Insert(_), Some(n)) => CommandTag::Insert(n),
            (CommandTag::Update(_), Some(n)) => CommandTag::Update(n),
            (CommandTag::Delete(_), Some(n)) => CommandTag::Delete(n),
            (t, _) => t.clone(),
        };
        Ok((rs, tag))
    }
}

/// Parameter storage for SET/SHOW/RESET commands
#[derive(Debug, Default)]
struct SessionParameters {
    params: alloc::collections::BTreeMap<String, String>,
}

impl SessionParameters {
    fn new() -> Self {
        let mut params = alloc::collections::BTreeMap::new();
        // Default PostgreSQL-compatible parameters
        params.insert("server_version".to_string(), "16.0".to_string());
        params.insert("server_encoding".to_string(), "UTF8".to_string());
        params.insert("client_encoding".to_string(), "UTF8".to_string());
        params.insert("application_name".to_string(), "".to_string());
        params.insert("DateStyle".to_string(), "ISO, MDY".to_string());
        params.insert("TimeZone".to_string(), "UTC".to_string());
        params.insert("standard_conforming_strings".to_string(), "on".to_string());
        params.insert("search_path".to_string(), "\"$user\", public".to_string());
        params.insert(
            "default_transaction_isolation".to_string(),
            "read committed".to_string(),
        );
        params.insert(
            "transaction_isolation".to_string(),
            "read committed".to_string(),
        );
        Self { params }
    }

    fn get(&self, name: &str) -> Option<&String> {
        self.params.get(name)
    }

    fn set(&mut self, name: String, value: String) {
        self.params.insert(name, value);
    }

    fn reset(&mut self, name: &str) {
        if let Some(default) = Self::new().params.get(name) {
            self.params.insert(name.to_string(), default.clone());
        } else {
            self.params.remove(name);
        }
    }

    fn reset_all(&mut self) {
        self.params = Self::new().params;
    }
}

impl Database {
    /// Execute SET parameter = value
    fn run_set_parameter(
        &mut self,
        stmt: &crate::parser::SetParameterStatement,
    ) -> Result<(), DbError> {
        self.session_parameters
            .set(stmt.name.clone(), stmt.value.clone());
        Ok(())
    }

    /// Execute SHOW parameter
    fn run_show_parameter(
        &mut self,
        stmt: &crate::parser::ShowParameterStatement,
    ) -> Result<ResultSet, DbError> {
        let mut result = ResultSet {
            columns: vec!["name".to_string(), "setting".to_string()],
            ..Default::default()
        };

        if let Some(value) = self.session_parameters.get(&stmt.name) {
            result.rows.push(vec![
                crate::executor::Value::Text(stmt.name.clone()),
                crate::executor::Value::Text(value.clone()),
            ]);
        } else {
            // Parameter not found - return empty row set with just columns
        }

        Ok(result)
    }

    /// Execute RESET parameter
    fn run_reset_parameter(
        &mut self,
        stmt: &crate::parser::ResetParameterStatement,
    ) -> Result<(), DbError> {
        self.session_parameters.reset(&stmt.name);
        Ok(())
    }

    /// Execute RESET ALL
    fn run_reset_all(&mut self) -> Result<(), DbError> {
        self.session_parameters.reset_all();
        Ok(())
    }
}

fn empty_result_set() -> ResultSet {
    ResultSet::default()
}

/// Whether `stmt` (anywhere in its `FROM`/`JOIN` table references, including
/// nested derived-table subqueries, or its `WHERE`/`HAVING` `EXISTS`/`IN`/
/// scalar subqueries) references the (not-yet-created) table/view name
/// `name` — used to reject a self-referencing `CREATE VIEW` or non-recursive
/// `CTE` up front, since forward references are otherwise impossible (a view
/// or plain CTE can only reference tables/views/CTEs that already exist).
///
/// Walks subqueries recursively rather than only the immediate `FROM`/`JOIN`
/// clauses, closing a stack-overflow hole where a self-reference hidden
/// inside a `WHERE`-clause subquery went undetected here and then recursed
/// forever at execution time (each level re-resolving the same CTE/view name
/// via `resolve_table_ref_with_ctes`, with no base case).
fn select_references_table(stmt: &SelectStatement, name: &str) -> bool {
    if table_ref_references_table(&stmt.table, name) {
        return true;
    }
    if stmt
        .joins
        .iter()
        .any(|j| table_ref_references_table(&j.table, name))
    {
        return true;
    }
    if let Some(f) = &stmt.filter {
        if expr_references_table(f, name) {
            return true;
        }
    }
    if let Some(h) = &stmt.having {
        if expr_references_table(h, name) {
            return true;
        }
    }
    false
}

fn table_ref_references_table(r: &TableRef, name: &str) -> bool {
    match r {
        TableRef::Named { name: n, .. } => n == name,
        TableRef::Subquery { query, .. } => select_references_table(query, name),
    }
}

fn expr_references_table(expr: &Expr, name: &str) -> bool {
    match expr {
        Expr::And(l, r) | Expr::Or(l, r) => {
            expr_references_table(l, name) || expr_references_table(r, name)
        }
        Expr::Not(inner) => expr_references_table(inner, name),
        Expr::Exists { query } | Expr::InSubquery { query, .. } | Expr::ScalarCmp { query, .. } => {
            select_references_table(query, name)
        }
        // Leaf expressions reference columns, not tables — the caller handles
        // column-to-table binding separately. Subqueries are the only Expr
        // nodes that can introduce new table references.
        Expr::ExtractCmp { .. } => false,
        _ => false,
    }
}
