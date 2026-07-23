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

use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use tpt_archon_core::btree::BTree;

use crate::executor::{self, Value};
use crate::mvcc;
use crate::parser::{
    CreateTableStatement, CreateViewStatement, DeleteStatement, Expr, InsertStatement,
    OrderByCosine, SelectStatement, Statement, TableRef, UpdateStatement,
};
use crate::planner::{plan_select, TableStats};

/// MVCC-buffered-write status tag: the row bytes that follow are live.
const MVCC_LIVE: u8 = 0;
/// MVCC-buffered-write status tag: the row was deleted within the transaction.
const MVCC_TOMBSTONE: u8 = 1;

/// A column's logical type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColumnType {
    /// 64-bit integer.
    Int,
    /// UTF-8 text.
    Text,
    /// Fixed-width `f32` embedding vector (`f32[]`).
    Vector,
}

/// A table schema: ordered column names and their types.
#[derive(Debug, Clone)]
pub struct Schema {
    /// Column names in order.
    pub columns: Vec<String>,
    /// Column types, positionally aligned with `columns`.
    pub types: Vec<ColumnType>,
}

impl Schema {
    /// Looks up a column index by name.
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c == name)
    }
}

/// Errors from executing a statement against a [`Database`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbError {
    /// A referenced column does not exist in the schema.
    UnknownColumn(String),
    /// A `WHERE` predicate compared against a non-integer column.
    TypeMismatch,
    /// A value literal did not match the column's declared type.
    ColumnTypeMismatch(String),
    /// A `VALUES` list had a different arity than the column list.
    ArityMismatch,
    /// `ORDER BY cosine(col, ?)` referenced a column that is not a vector.
    NotAVectorColumn(String),
    /// A `?` query parameter was expected but not supplied.
    MissingParam,
    /// A row id referenced during update/delete was not found in the B-Link tree.
    RowNotFound(u64),
    /// Raw bytes from the B-Link tree failed to decode as a valid row.
    CorruptRow(u64),
    /// Referenced table does not exist.
    UnknownTable(String),
    /// Transaction error.
    TransactionError(String),
    /// Table already exists (CREATE TABLE).
    TableAlreadyExists(String),
    /// A view (or table) with this name already exists (CREATE VIEW).
    ViewAlreadyExists(String),
    /// Referenced view does not exist (DROP VIEW).
    UnknownView(String),
    /// A view's defining query references its own not-yet-existing name.
    RecursiveView(String),
    /// A parsed feature is recognized but not yet supported by this engine.
    Unsupported(String),
    /// Execution error propagated from the executor.
    Exec(executor::ExecError),
}

impl From<executor::ExecError> for DbError {
    fn from(e: executor::ExecError) -> Self {
        match e {
            executor::ExecError::UnknownColumn(c) => DbError::UnknownColumn(c),
            executor::ExecError::TypeMismatch => DbError::TypeMismatch,
            executor::ExecError::GroupByColumnNotFound(c) => DbError::UnknownColumn(c),
        }
    }
}

/// A table's storage: its schema, B-Link tree, and per-table MVCC store used
/// while a transaction is open on it.
#[derive(Debug)]
struct TableStorage {
    schema: Schema,
    tree: BTree,
    next_row_id: u64,
    mvcc: mvcc::MvccStore,
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

    /// Wraps encoded row bytes with the MVCC live-row status tag.
    fn mvcc_wrap_row(values: &[Value]) -> Vec<u8> {
        let mut out = vec![MVCC_LIVE];
        out.extend_from_slice(&Self::encode_row(values));
        out
    }

    /// The MVCC tombstone marker for a row deleted within a transaction.
    fn mvcc_wrap_tombstone() -> Vec<u8> {
        vec![MVCC_TOMBSTONE]
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
    ) -> Result<executor::ResultSet, DbError> {
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
            Statement::Begin => {
                if self.in_transaction {
                    return Err(DbError::TransactionError(
                        "transaction already in progress".to_string(),
                    ));
                }
                self.in_transaction = true;
                self.active_txns.clear();
                Ok(empty_result_set())
            }
            Statement::Commit => {
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
                                } else {
                                    ts.tree.insert(id, bytes[1..].to_vec());
                                }
                            }
                        }
                        Err(mvcc::CommitError::Conflict) => {
                            return Err(DbError::TransactionError(alloc::format!(
                                "commit conflict on table '{table_name}'"
                            )));
                        }
                    }
                }
                Ok(empty_result_set())
            }
            Statement::Rollback => {
                if !self.in_transaction {
                    return Err(DbError::TransactionError(
                        "no active transaction".to_string(),
                    ));
                }
                // Buffered per-table transactions are simply dropped without
                // committing, discarding every write made since BEGIN.
                self.active_txns.clear();
                self.in_transaction = false;
                Ok(empty_result_set())
            }
        }
    }

    /// Like [`execute`](Database::execute) but takes ownership of the statement,
    /// used by callers that build statements directly (e.g. arity tests).
    pub fn execute_checked(&mut self, stmt: &Statement) -> Result<executor::ResultSet, DbError> {
        self.execute(stmt, &[])
    }

    // --- DDL ----------------------------------------------------------------

    fn run_create_table(&mut self, ct: &CreateTableStatement) -> Result<(), DbError> {
        if self.table(&ct.table).is_some() {
            return Err(DbError::TableAlreadyExists(ct.table.clone()));
        }
        let mut columns = Vec::new();
        let mut types = Vec::new();
        // First column is always the implicit row_id.
        columns.push("id".to_string());
        types.push(ColumnType::Int);
        for c in &ct.columns {
            columns.push(c.name.clone());
            types.push(match c.ctype {
                crate::parser::ColumnType::Int => ColumnType::Int,
                crate::parser::ColumnType::Text => ColumnType::Text,
                crate::parser::ColumnType::Vector => ColumnType::Vector,
            });
        }
        self.tables.push((
            ct.table.clone(),
            TableStorage {
                schema: Schema { columns, types },
                tree: BTree::new(),
                next_row_id: 0,
                mvcc: mvcc::MvccStore::new(),
            },
        ));
        Ok(())
    }

    fn run_create_view(&mut self, cv: &CreateViewStatement) -> Result<(), DbError> {
        if self.table(&cv.name).is_some() || self.views.iter().any(|(n, _)| n == &cv.name) {
            return Err(DbError::ViewAlreadyExists(cv.name.clone()));
        }
        if select_references_table(&cv.query, &cv.name) {
            return Err(DbError::RecursiveView(cv.name.clone()));
        }
        self.views.push((cv.name.clone(), cv.query.clone()));
        Ok(())
    }

    fn run_drop_view(&mut self, name: &str) -> Result<(), DbError> {
        let pos = self
            .views
            .iter()
            .position(|(n, _)| n == name)
            .ok_or_else(|| DbError::UnknownView(name.to_string()))?;
        self.views.remove(pos);
        Ok(())
    }

    // --- INSERT -------------------------------------------------------------

    fn run_insert_stmt(&mut self, stmt: &InsertStatement) -> Result<(), DbError> {
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
            row[*slot] = ts.literal_to_value(*slot, lit)?;
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
            let wrapped = Self::mvcc_wrap_row(&row);
            ts.mvcc.write(txn, id, wrapped);
        } else {
            let encoded = ts.encode_row(&row);
            ts.tree.insert(id, encoded);
        }
        Ok(())
    }

    // --- row codec ----------------------------------------------------------

    fn encode_row(values: &[Value]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(values.len() as u16).to_le_bytes());
        for v in values {
            match v {
                Value::Int(i) => {
                    out.push(0);
                    out.extend_from_slice(&i.to_le_bytes());
                }
                Value::Text(t) => {
                    out.push(1);
                    out.extend_from_slice(&(t.len() as u32).to_le_bytes());
                    out.extend_from_slice(t.as_bytes());
                }
                Value::Vector(vec) => {
                    out.push(2);
                    out.extend_from_slice(&(vec.len() as u32).to_le_bytes());
                    for f in vec {
                        out.extend_from_slice(&f.to_le_bytes());
                    }
                }
                Value::Null => {
                    out.push(3);
                }
            }
        }
        out
    }

    fn try_decode_row(bytes: &[u8]) -> Result<Vec<Value>, DbError> {
        if bytes.len() < 2 {
            return Err(DbError::CorruptRow(0));
        }
        let mut pos = 0usize;
        let n = u16::from_le_bytes([bytes[pos], bytes[pos + 1]]) as usize;
        pos += 2;
        let mut row = Vec::with_capacity(n);
        for _ in 0..n {
            if pos >= bytes.len() {
                return Err(DbError::CorruptRow(0));
            }
            let tag = bytes[pos];
            pos += 1;
            match tag {
                0 => {
                    if pos + 8 > bytes.len() {
                        return Err(DbError::CorruptRow(0));
                    }
                    let mut b = [0u8; 8];
                    b.copy_from_slice(&bytes[pos..pos + 8]);
                    pos += 8;
                    row.push(Value::Int(i64::from_le_bytes(b)));
                }
                1 => {
                    if pos + 4 > bytes.len() {
                        return Err(DbError::CorruptRow(0));
                    }
                    let len = u32::from_le_bytes([
                        bytes[pos],
                        bytes[pos + 1],
                        bytes[pos + 2],
                        bytes[pos + 3],
                    ]) as usize;
                    pos += 4;
                    if pos + len > bytes.len() {
                        return Err(DbError::CorruptRow(0));
                    }
                    let s = String::from_utf8_lossy(&bytes[pos..pos + len]).into_owned();
                    pos += len;
                    row.push(Value::Text(s));
                }
                2 => {
                    if pos + 4 > bytes.len() {
                        return Err(DbError::CorruptRow(0));
                    }
                    let len = u32::from_le_bytes([
                        bytes[pos],
                        bytes[pos + 1],
                        bytes[pos + 2],
                        bytes[pos + 3],
                    ]) as usize;
                    pos += 4;
                    let vec_bytes = len * 4;
                    if pos + vec_bytes > bytes.len() {
                        return Err(DbError::CorruptRow(0));
                    }
                    let mut vec = Vec::with_capacity(len);
                    for _ in 0..len {
                        let mut b = [0u8; 4];
                        b.copy_from_slice(&bytes[pos..pos + 4]);
                        pos += 4;
                        vec.push(f32::from_le_bytes(b));
                    }
                    row.push(Value::Vector(vec));
                }
                3 => row.push(Value::Null),
                _ => row.push(Value::Int(0)),
            }
        }
        Ok(row)
    }

    fn decode_row_validated(
        id: u64,
        bytes: &[u8],
        col_count: usize,
    ) -> Result<Vec<Value>, DbError> {
        let row = Self::try_decode_row(bytes)?;
        if row.len() != col_count {
            return Err(DbError::CorruptRow(id));
        }
        Ok(row)
    }

    // --- DML ----------------------------------------------------------------

    fn run_update(&mut self, stmt: &UpdateStatement) -> Result<(), DbError> {
        let matching: Vec<u64> = self.matching_row_ids(&stmt.table, stmt.filter.as_ref())?;
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
                    Self::decode_row_validated(id, &buffered[1..], ts.schema.columns.len())?
                } else {
                    let bytes = ts.tree.get(id).ok_or(DbError::RowNotFound(id))?.to_vec();
                    Self::decode_row_validated(id, &bytes, ts.schema.columns.len())?
                };

            for a in &stmt.assignments {
                let slot = ts
                    .schema
                    .index_of(&a.column)
                    .ok_or_else(|| DbError::UnknownColumn(a.column.clone()))?;
                if slot == 0 {
                    continue;
                }
                row[slot] = ts.literal_to_value(slot, &a.value)?;
            }

            if in_txn {
                let txn = active_txns
                    .iter_mut()
                    .find(|(n, _)| n == &stmt.table)
                    .map(|(_, t)| t)
                    .expect("ensure_txn guarantees a transaction exists");
                let wrapped = Self::mvcc_wrap_row(&row);
                ts.mvcc.write(txn, id, wrapped);
            } else {
                let encoded = Self::encode_row(&row);
                ts.tree.insert(id, encoded);
            }
        }
        Ok(())
    }

    fn run_delete(&mut self, stmt: &DeleteStatement) -> Result<(), DbError> {
        let matching = self.matching_row_ids(&stmt.table, stmt.filter.as_ref())?;
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
                ts.mvcc.write(txn, id, Self::mvcc_wrap_tombstone());
            } else {
                ts.tree.delete(id);
            }
        }
        Ok(())
    }

    /// Returns row ids from `table` whose rows satisfy the predicate.
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
        let mut out = Vec::new();
        for id in 0..ts.next_row_id {
            let row = if let Some(buffered) = txn.and_then(|t| t.get_write(id)) {
                if buffered[0] == MVCC_TOMBSTONE {
                    continue;
                }
                Self::decode_row_validated(id, &buffered[1..], ts.schema.columns.len())?
            } else if let Some(bytes) = ts.tree.get(id) {
                Self::decode_row_validated(id, bytes, ts.schema.columns.len())?
            } else {
                continue;
            };
            let keep = match filter {
                None => true,
                Some(expr) => executor::eval_expr(expr, &ts.schema.columns, &row)?,
            };
            if keep {
                out.push(id);
            }
        }
        Ok(out)
    }

    /// Scans all rows from a table, returning (columns, rows).
    fn scan_table(&self, table_name: &str) -> Result<(Vec<String>, Vec<Vec<Value>>), DbError> {
        let ts = self
            .table(table_name)
            .ok_or_else(|| DbError::UnknownTable(table_name.to_string()))?;
        let txn = self
            .active_txns
            .iter()
            .find(|(n, _)| n == table_name)
            .map(|(_, t)| t);
        let mut rows = Vec::new();
        for id in 0..ts.next_row_id {
            if let Some(buffered) = txn.and_then(|t| t.get_write(id)) {
                if buffered[0] == MVCC_TOMBSTONE {
                    continue;
                }
                let row = Self::decode_row_validated(id, &buffered[1..], ts.schema.columns.len())?;
                rows.push(row);
                continue;
            }
            if let Some(bytes) = ts.tree.get(id) {
                let row = Self::decode_row_validated(id, bytes, ts.schema.columns.len())?;
                rows.push(row);
            }
        }
        Ok((ts.schema.columns.clone(), rows))
    }

    /// Resolves a [`TableRef`] to `(columns, rows)`: a view substitution (if
    /// `name` names a view), else a real table scan, for `Named`; a
    /// derived-table substitution (executing the inner query) for `Subquery`.
    fn resolve_table_ref(&self, r: &TableRef) -> Result<(Vec<String>, Vec<Vec<Value>>), DbError> {
        match r {
            TableRef::Named(name) => {
                if let Some((_, query)) = self.views.iter().find(|(n, _)| n == name) {
                    let rs = self.run_select(query, &[])?;
                    return Ok((rs.columns, rs.rows));
                }
                self.scan_table(name)
            }
            TableRef::Subquery { query, alias } => {
                let rs = self.run_select(query, &[])?;
                let columns: Vec<String> = rs
                    .columns
                    .iter()
                    .map(|c| alloc::format!("{alias}.{c}"))
                    .collect();
                Ok((columns, rs.rows))
            }
        }
    }

    // --- SELECT -------------------------------------------------------------

    fn run_select(
        &self,
        stmt: &SelectStatement,
        params: &[Vec<f32>],
    ) -> Result<executor::ResultSet, DbError> {
        if let Some(ob) = &stmt.order_by_cosine {
            return self.run_vector_topk(stmt, ob, params);
        }

        // Build an in-memory table from the source table + optional JOINs.
        let (mut columns, mut rows) = self.resolve_table_ref(&stmt.table)?;

        // Process JOINs (nested-loop inner join).
        for join in &stmt.joins {
            let (join_cols, join_rows) = self.resolve_table_ref(&join.table)?;
            let left_idx = columns
                .iter()
                .position(|c| c == &join.left_col)
                .ok_or_else(|| DbError::UnknownColumn(join.left_col.clone()))?;
            let right_idx = join_cols
                .iter()
                .position(|c| c == &join.right_col)
                .ok_or_else(|| DbError::UnknownColumn(join.right_col.clone()))?;

            // Rename right columns with table prefix to avoid collisions.
            let mut new_cols = columns.clone();
            let mut new_rows = Vec::new();
            for rcol in &join_cols {
                let name = alloc::format!("{}.{}", join.table.name(), rcol);
                new_cols.push(name);
            }
            for lrow in &rows {
                for jrow in &join_rows {
                    if lrow[left_idx] == jrow[right_idx] {
                        let mut combined = lrow.clone();
                        combined.extend_from_slice(jrow);
                        new_rows.push(combined);
                    }
                }
            }
            columns = new_cols;
            rows = new_rows;
        }

        // Build a Table for the executor.
        let mut table = executor::Table::new(columns);
        for row in rows {
            table.insert(row);
        }

        // Apply WHERE filter.
        if let Some(expr) = &stmt.filter {
            let mut filtered = Vec::new();
            for row in &table.rows {
                if executor::eval_expr(expr, &table.columns, row)? {
                    filtered.push(row.clone());
                }
            }
            table.rows = filtered;
        }

        // Apply GROUP BY + aggregates.
        if !stmt.group_by.is_empty() || !stmt.aggregates.is_empty() {
            let rs = executor::aggregate_table(
                &table.columns,
                &table.rows,
                &stmt.group_by,
                &stmt.aggregates,
            )?;
            table = executor::Table {
                columns: rs.columns,
                rows: rs.rows,
            };
        }

        let plan = {
            let mut plan_stmt = stmt.clone();
            plan_stmt.group_by.clear();
            plan_stmt.aggregates.clear();
            plan_select(
                &plan_stmt,
                TableStats {
                    row_count: table.rows.len() as u64,
                },
            )
        };
        executor::execute(&plan, &table).map_err(|e| match e {
            executor::ExecError::UnknownColumn(c) => DbError::UnknownColumn(c),
            executor::ExecError::TypeMismatch => DbError::TypeMismatch,
            executor::ExecError::GroupByColumnNotFound(c) => DbError::UnknownColumn(c),
        })
    }

    fn run_vector_topk(
        &self,
        stmt: &SelectStatement,
        ob: &OrderByCosine,
        params: &[Vec<f32>],
    ) -> Result<executor::ResultSet, DbError> {
        let query = params.get(ob.param - 1).ok_or(DbError::MissingParam)?;

        // For subqueries, resolve to an in-memory table first.
        if let TableRef::Subquery { .. } = &stmt.table {
            let (columns, rows) = self.resolve_table_ref(&stmt.table)?;
            let slot = columns
                .iter()
                .position(|c| c == &ob.column)
                .ok_or_else(|| DbError::UnknownColumn(ob.column.clone()))?;
            let mut embeddings = Vec::new();
            let mut data_rows = Vec::new();
            for row in &rows {
                if let Value::Vector(v) = &row[slot] {
                    embeddings.push(v.clone());
                    data_rows.push(row.clone());
                }
            }
            let top = executor::vector_topk(&embeddings, query, ob.k as usize);
            let out_rows: Vec<Vec<Value>> = top.into_iter().map(|i| data_rows[i].clone()).collect();
            let out_columns = if stmt.star || stmt.columns.is_empty() {
                columns
            } else {
                stmt.columns.clone()
            };
            return Ok(executor::ResultSet {
                columns: out_columns,
                rows: out_rows,
            });
        }

        let table_name = match &stmt.table {
            TableRef::Named(name) => name.as_str(),
            TableRef::Subquery { .. } => unreachable!(),
        };
        let ts = self
            .table(table_name)
            .ok_or_else(|| DbError::UnknownTable(table_name.to_string()))?;
        let slot = ts
            .schema
            .index_of(&ob.column)
            .ok_or_else(|| DbError::UnknownColumn(ob.column.clone()))?;
        if ts.schema.types[slot] != ColumnType::Vector {
            return Err(DbError::NotAVectorColumn(ob.column.clone()));
        }
        let mut embeddings: Vec<Vec<f32>> = Vec::new();
        let mut rows: Vec<Vec<Value>> = Vec::new();
        let mut id = 0u64;
        while let Some(bytes) = ts.tree.get(id) {
            let row = Self::decode_row_validated(id, bytes, ts.schema.columns.len())?;
            if let Value::Vector(v) = &row[slot] {
                embeddings.push(v.clone());
                rows.push(row);
            }
            id += 1;
        }
        let top = executor::vector_topk(&embeddings, query, ob.k as usize);
        let mut out_rows = Vec::new();
        for &i in &top {
            out_rows.push(rows[i].clone());
        }
        let columns = if stmt.star || stmt.columns.is_empty() {
            ts.schema.columns.clone()
        } else {
            stmt.columns.clone()
        };
        Ok(executor::ResultSet {
            columns,
            rows: out_rows,
        })
    }

    // --- helpers for schema access ------------------------------------------

    fn literal_to_value(
        schema: &Schema,
        slot: usize,
        lit: &crate::parser::Literal,
    ) -> Result<Value, DbError> {
        let expected = &schema.types[slot];
        match (expected, lit) {
            (_, crate::parser::Literal::Null) => Ok(Value::Null),
            (ColumnType::Int, crate::parser::Literal::Int(i)) => Ok(Value::Int(*i)),
            (ColumnType::Text, crate::parser::Literal::Text(t)) => Ok(Value::Text(t.clone())),
            (ColumnType::Vector, crate::parser::Literal::Vector(v)) => Ok(Value::Vector(v.clone())),
            (ColumnType::Int, _) | (ColumnType::Text, _) | (ColumnType::Vector, _) => {
                Err(DbError::ColumnTypeMismatch(schema.columns[slot].clone()))
            }
        }
    }
}

impl TableStorage {
    fn literal_to_value(
        &self,
        slot: usize,
        lit: &crate::parser::Literal,
    ) -> Result<Value, DbError> {
        Database::literal_to_value(&self.schema, slot, lit)
    }

    fn encode_row(&self, values: &[Value]) -> Vec<u8> {
        Database::encode_row(values)
    }
}

fn empty_result_set() -> executor::ResultSet {
    executor::ResultSet {
        columns: Vec::new(),
        rows: Vec::new(),
    }
}

/// Whether `stmt`'s `FROM`/`JOIN` clauses reference the (not-yet-created)
/// table/view name `name` — used to reject a self-referencing `CREATE VIEW`
/// up front, since forward references are otherwise impossible (a view can
/// only reference tables/views that already exist).
fn select_references_table(stmt: &SelectStatement, name: &str) -> bool {
    if let TableRef::Named(n) = &stmt.table {
        if n == name {
            return true;
        }
    }
    stmt.joins
        .iter()
        .any(|j| matches!(&j.table, TableRef::Named(n) if n == name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_statement;

    fn schema() -> Schema {
        Schema {
            columns: alloc::vec!["id".to_string(), "name".to_string(), "age".to_string()],
            types: alloc::vec![ColumnType::Int, ColumnType::Text, ColumnType::Int],
        }
    }

    fn db() -> Database {
        Database::new(schema())
    }

    #[test]
    fn execute_dispatch_insert_select_update_delete() {
        let mut d = db();
        d.execute(
            &parse_statement("INSERT INTO t (id, name, age) VALUES (1, 'alice', 30)").unwrap(),
            &[],
        )
        .unwrap();
        assert_eq!(d.len(), 1);

        let r = d
            .execute(
                &parse_statement("SELECT id, name FROM t WHERE age >= 30").unwrap(),
                &[],
            )
            .unwrap();
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0][0], Value::Int(1));

        d.execute(
            &parse_statement("UPDATE t SET age = 99 WHERE age < 50").unwrap(),
            &[],
        )
        .unwrap();
        let r2 = d
            .execute(
                &parse_statement("SELECT id FROM t WHERE age = 99").unwrap(),
                &[],
            )
            .unwrap();
        assert_eq!(r2.rows.len(), 1);

        d.execute(
            &parse_statement("DELETE FROM t WHERE age = 99").unwrap(),
            &[],
        )
        .unwrap();
        assert_eq!(d.len(), 0);
    }

    #[test]
    fn arity_and_type_errors() {
        let mut d = db();
        let ins = InsertStatement {
            table: "t".to_string(),
            columns: alloc::vec!["id".to_string()],
            values: alloc::vec![
                crate::parser::Literal::Int(1),
                crate::parser::Literal::Int(2),
            ],
        };
        assert!(matches!(
            d.execute_checked(&Statement::Insert(ins)),
            Err(DbError::ArityMismatch)
        ));

        let bad_ty = parse_statement("INSERT INTO t (id, name, age) VALUES (1, 5, 30)").unwrap();
        assert_eq!(
            d.execute(&bad_ty, &[]),
            Err(DbError::ColumnTypeMismatch("name".to_string()))
        );
    }

    #[test]
    fn vector_topk_query() {
        let schema = Schema {
            columns: alloc::vec!["id".to_string(), "emb".to_string()],
            types: alloc::vec![ColumnType::Int, ColumnType::Vector],
        };
        let mut d = Database::new(schema);
        let rows = ["[1.0, 0.0]", "[0.0, 1.0]", "[0.9, 0.1]"];
        for (i, emb) in rows.iter().enumerate() {
            let sql = alloc::format!("INSERT INTO t (id, emb) VALUES ({i}, {emb})");
            d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
        }
        let sel = parse_statement("SELECT id FROM t ORDER BY cosine(emb, ?) LIMIT 2").unwrap();
        let r = d.execute(&sel, &[alloc::vec![1.0, 0.0]]).unwrap();
        assert_eq!(r.rows.len(), 2);
        assert_eq!(r.rows[0][0], Value::Int(0));
        assert_eq!(r.rows[1][0], Value::Int(2));
    }

    #[test]
    fn create_table_and_insert() {
        let mut d = Database::empty();
        d.execute(
            &parse_statement("CREATE TABLE users (name TEXT, age INT)").unwrap(),
            &[],
        )
        .unwrap();
        d.execute(
            &parse_statement("INSERT INTO users (name, age) VALUES ('alice', 30)").unwrap(),
            &[],
        )
        .unwrap();
        let r = d
            .execute(
                &parse_statement("SELECT * FROM users WHERE age >= 30").unwrap(),
                &[],
            )
            .unwrap();
        assert_eq!(r.rows.len(), 1);
    }

    #[test]
    fn create_table_duplicate_errors() {
        let mut d = Database::new(schema());
        assert!(matches!(
            d.execute(&parse_statement("CREATE TABLE t (x INT)").unwrap(), &[]),
            Err(DbError::TableAlreadyExists(_))
        ));
    }

    #[test]
    fn unknown_table_errors() {
        let mut d = Database::empty();
        assert!(matches!(
            d.execute(&parse_statement("SELECT * FROM nope").unwrap(), &[]),
            Err(DbError::UnknownTable(_))
        ));
    }

    #[test]
    fn begin_commit_rollback() {
        let mut d = db();
        d.execute(&parse_statement("BEGIN").unwrap(), &[]).unwrap();
        assert!(matches!(
            d.execute(&parse_statement("BEGIN").unwrap(), &[]),
            Err(DbError::TransactionError(_))
        ));
        d.execute(&parse_statement("COMMIT").unwrap(), &[]).unwrap();
        d.execute(&parse_statement("BEGIN").unwrap(), &[]).unwrap();
        d.execute(&parse_statement("ROLLBACK").unwrap(), &[])
            .unwrap();
    }

    #[test]
    fn rollback_actually_undoes_writes() {
        // Regression test: ROLLBACK used to be a bare no-op (just flipped
        // in_transaction back to false) — writes made inside the transaction
        // were never undone. Now they must be, via the real mvcc store.
        let mut d = db();
        d.execute(
            &parse_statement("INSERT INTO t (id, name, age) VALUES (0, 'seed', 1)").unwrap(),
            &[],
        )
        .unwrap();

        d.execute(&parse_statement("BEGIN").unwrap(), &[]).unwrap();
        d.execute(
            &parse_statement("INSERT INTO t (id, name, age) VALUES (1, 'ghost', 2)").unwrap(),
            &[],
        )
        .unwrap();
        d.execute(
            &parse_statement("UPDATE t SET age = 99 WHERE id = 0").unwrap(),
            &[],
        )
        .unwrap();
        d.execute(&parse_statement("ROLLBACK").unwrap(), &[])
            .unwrap();

        let r = d
            .execute(&parse_statement("SELECT id, age FROM t").unwrap(), &[])
            .unwrap();
        // Only the seed row should remain, with its original age.
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0][0], Value::Int(0));
        assert_eq!(r.rows[0][1], Value::Int(1));
    }

    #[test]
    fn commit_applies_writes_made_during_transaction() {
        let mut d = db();
        d.execute(&parse_statement("BEGIN").unwrap(), &[]).unwrap();
        d.execute(
            &parse_statement("INSERT INTO t (id, name, age) VALUES (0, 'alice', 30)").unwrap(),
            &[],
        )
        .unwrap();
        d.execute(&parse_statement("COMMIT").unwrap(), &[]).unwrap();

        let r = d
            .execute(&parse_statement("SELECT id FROM t").unwrap(), &[])
            .unwrap();
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0][0], Value::Int(0));
    }

    #[test]
    fn reads_within_transaction_see_own_writes() {
        let mut d = db();
        d.execute(&parse_statement("BEGIN").unwrap(), &[]).unwrap();
        d.execute(
            &parse_statement("INSERT INTO t (id, name, age) VALUES (0, 'alice', 30)").unwrap(),
            &[],
        )
        .unwrap();
        // Not yet committed, but should be visible within the same transaction.
        let r = d
            .execute(&parse_statement("SELECT id FROM t").unwrap(), &[])
            .unwrap();
        assert_eq!(r.rows.len(), 1);
        d.execute(&parse_statement("ROLLBACK").unwrap(), &[])
            .unwrap();
    }

    #[test]
    fn delete_within_transaction_rolls_back() {
        let mut d = db();
        d.execute(
            &parse_statement("INSERT INTO t (id, name, age) VALUES (0, 'alice', 30)").unwrap(),
            &[],
        )
        .unwrap();
        d.execute(&parse_statement("BEGIN").unwrap(), &[]).unwrap();
        d.execute(&parse_statement("DELETE FROM t WHERE id = 0").unwrap(), &[])
            .unwrap();
        let mid = d
            .execute(&parse_statement("SELECT id FROM t").unwrap(), &[])
            .unwrap();
        assert_eq!(mid.rows.len(), 0);
        d.execute(&parse_statement("ROLLBACK").unwrap(), &[])
            .unwrap();
        let after = d
            .execute(&parse_statement("SELECT id FROM t").unwrap(), &[])
            .unwrap();
        assert_eq!(after.rows.len(), 1);
    }

    #[test]
    fn and_or_where_filter() {
        let mut d = db();
        for i in 0..5 {
            let sql = alloc::format!(
                "INSERT INTO t (id, name, age) VALUES ({i}, 'u{i}', {})",
                i * 10
            );
            d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
        }
        let r = d
            .execute(
                &parse_statement("SELECT * FROM t WHERE age > 5 AND age < 35").unwrap(),
                &[],
            )
            .unwrap();
        // ages: 0, 10, 20, 30, 40; >5 and <35 → 10, 20, 30
        assert_eq!(r.rows.len(), 3);
    }

    #[test]
    fn in_predicate() {
        let mut d = db();
        for i in 0..5 {
            let sql = alloc::format!("INSERT INTO t (id, name, age) VALUES ({i}, 'u{i}', {i})");
            d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
        }
        let r = d
            .execute(
                &parse_statement("SELECT * FROM t WHERE age IN (1, 3)").unwrap(),
                &[],
            )
            .unwrap();
        assert_eq!(r.rows.len(), 2);
    }

    #[test]
    fn between_predicate() {
        let mut d = db();
        for i in 0..10 {
            let sql = alloc::format!("INSERT INTO t (id, name, age) VALUES ({i}, 'u{i}', {i})");
            d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
        }
        let r = d
            .execute(
                &parse_statement("SELECT * FROM t WHERE age BETWEEN 3 AND 7").unwrap(),
                &[],
            )
            .unwrap();
        assert_eq!(r.rows.len(), 5);
    }

    #[test]
    fn order_by_column() {
        let mut d = db();
        d.execute(
            &parse_statement("INSERT INTO t (id, name, age) VALUES (0, 'c', 30)").unwrap(),
            &[],
        )
        .unwrap();
        d.execute(
            &parse_statement("INSERT INTO t (id, name, age) VALUES (1, 'a', 10)").unwrap(),
            &[],
        )
        .unwrap();
        d.execute(
            &parse_statement("INSERT INTO t (id, name, age) VALUES (2, 'b', 20)").unwrap(),
            &[],
        )
        .unwrap();
        let r = d
            .execute(
                &parse_statement("SELECT * FROM t ORDER BY age ASC").unwrap(),
                &[],
            )
            .unwrap();
        assert_eq!(r.rows[0][2], Value::Int(10));
        assert_eq!(r.rows[1][2], Value::Int(20));
        assert_eq!(r.rows[2][2], Value::Int(30));
    }

    #[test]
    fn group_by_with_count() {
        let mut d = Database::empty();
        d.execute(
            &parse_statement("CREATE TABLE t (dept TEXT, salary INT)").unwrap(),
            &[],
        )
        .unwrap();
        d.execute(
            &parse_statement("INSERT INTO t (dept, salary) VALUES ('eng', 100)").unwrap(),
            &[],
        )
        .unwrap();
        d.execute(
            &parse_statement("INSERT INTO t (dept, salary) VALUES ('eng', 200)").unwrap(),
            &[],
        )
        .unwrap();
        d.execute(
            &parse_statement("INSERT INTO t (dept, salary) VALUES ('sales', 150)").unwrap(),
            &[],
        )
        .unwrap();
        let r = d
            .execute(
                &parse_statement("SELECT dept, COUNT(*) FROM t GROUP BY dept").unwrap(),
                &[],
            )
            .unwrap();
        assert_eq!(r.rows.len(), 2);
        // Each group should have count 2 and 1.
        let counts: Vec<i64> = r
            .rows
            .iter()
            .map(|row| match &row[1] {
                Value::Int(v) => *v,
                _ => panic!("expected int"),
            })
            .collect();
        assert!(counts.contains(&2));
        assert!(counts.contains(&1));
    }

    #[test]
    fn create_view_select_and_filter_through() {
        let mut d = db();
        for i in 0..5 {
            let sql = alloc::format!("INSERT INTO t (id, name, age) VALUES ({i}, 'u{i}', {})", i * 10);
            d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
        }
        d.execute(
            &parse_statement("CREATE VIEW adults AS SELECT * FROM t WHERE age >= 20").unwrap(),
            &[],
        )
        .unwrap();
        let r = d
            .execute(&parse_statement("SELECT id FROM adults").unwrap(), &[])
            .unwrap();
        // ages 0,10,20,30,40 -> >=20 keeps ids 2,3,4
        assert_eq!(r.rows.len(), 3);

        let r2 = d
            .execute(
                &parse_statement("SELECT id FROM adults WHERE id = 3").unwrap(),
                &[],
            )
            .unwrap();
        assert_eq!(r2.rows.len(), 1);
        assert_eq!(r2.rows[0][0], Value::Int(3));
    }

    #[test]
    fn drop_view_then_select_errors() {
        let mut d = db();
        d.execute(
            &parse_statement("CREATE VIEW everyone AS SELECT * FROM t").unwrap(),
            &[],
        )
        .unwrap();
        d.execute(&parse_statement("DROP VIEW everyone").unwrap(), &[])
            .unwrap();
        assert!(matches!(
            d.execute(&parse_statement("SELECT * FROM everyone").unwrap(), &[]),
            Err(DbError::UnknownTable(_))
        ));
    }

    #[test]
    fn create_view_self_reference_errors() {
        let mut d = db();
        assert!(matches!(
            d.execute(
                &parse_statement("CREATE VIEW loop AS SELECT * FROM loop").unwrap(),
                &[],
            ),
            Err(DbError::RecursiveView(_))
        ));
    }

    #[test]
    fn create_view_duplicate_name_errors() {
        let mut d = db();
        d.execute(
            &parse_statement("CREATE VIEW everyone AS SELECT * FROM t").unwrap(),
            &[],
        )
        .unwrap();
        assert!(matches!(
            d.execute(
                &parse_statement("CREATE VIEW everyone AS SELECT * FROM t").unwrap(),
                &[],
            ),
            Err(DbError::ViewAlreadyExists(_))
        ));
        assert!(matches!(
            d.execute(&parse_statement("CREATE VIEW t AS SELECT * FROM t").unwrap(), &[]),
            Err(DbError::ViewAlreadyExists(_))
        ));
    }

    #[test]
    fn subquery_in_from() {
        let mut d = db();
        for i in 0..5 {
            let sql = alloc::format!("INSERT INTO t (id, name, age) VALUES ({i}, 'u{i}', {})", i * 10);
            d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
        }
        let r = d
            .execute(
                &parse_statement("SELECT * FROM (SELECT id, name FROM t WHERE age >= 20) AS sub")
                    .unwrap(),
                &[],
            )
            .unwrap();
        assert_eq!(r.rows.len(), 3);
        assert_eq!(r.columns, alloc::vec!["id".to_string(), "name".to_string()]);
    }

    #[test]
    fn subquery_with_alias_in_outer_where() {
        let mut d = db();
        for i in 0..5 {
            let sql = alloc::format!("INSERT INTO t (id, name, age) VALUES ({i}, 'u{i}', {})", i * 10);
            d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
        }
        let r = d
            .execute(
                &parse_statement(
                    "SELECT sub.id FROM (SELECT id, name, age FROM t WHERE age >= 10) AS sub WHERE sub.age < 30",
                )
                .unwrap(),
                &[],
            )
            .unwrap();
        // ages 10, 20 -> ids 1, 2
        assert_eq!(r.rows.len(), 2);
    }

    #[test]
    fn nested_subquery() {
        let mut d = db();
        for i in 0..5 {
            let sql = alloc::format!("INSERT INTO t (id, name, age) VALUES ({i}, 'u{i}', {})", i * 10);
            d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
        }
        let r = d
            .execute(
                &parse_statement(
                    "SELECT * FROM (SELECT * FROM (SELECT id, age FROM t WHERE age > 0) AS inner1) AS outer1",
                )
                .unwrap(),
                &[],
            )
            .unwrap();
        // ages 10, 20, 30, 40 -> 4 rows
        assert_eq!(r.rows.len(), 4);
    }
}
