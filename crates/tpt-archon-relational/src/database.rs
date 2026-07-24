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
    AlterTableOp, AlterTableStatement, CmpOp, CreateTableStatement, CreateViewStatement,
    DeleteStatement, Expr, InsertStatement, OrderByCosine, SelectStatement, Statement, TableRef,
    UpdateStatement, CTE,
};
use crate::planner::{plan_select, TableStats};
use crate::vector_index;

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
    /// A scalar or `IN` subquery in a `WHERE` clause did not return the
    /// required shape: a scalar subquery must return exactly one row and one
    /// column; an `IN` subquery must return exactly one column.
    SubqueryCardinality(String),
    /// Execution error propagated from the executor.
    Exec(executor::ExecError),
}

impl From<executor::ExecError> for DbError {
    fn from(e: executor::ExecError) -> Self {
        match e {
            executor::ExecError::UnknownColumn(c) => DbError::UnknownColumn(c),
            executor::ExecError::TypeMismatch => DbError::TypeMismatch,
            executor::ExecError::GroupByColumnNotFound(c) => DbError::UnknownColumn(c),
            executor::ExecError::UnresolvedSubquery => DbError::Unsupported(
                "internal: subquery reached the pure evaluator unresolved".to_string(),
            ),
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
    /// Column name -> IVFFlat index, built lazily once a vector column's live
    /// row count crosses `vector_index::MIN_ROWS_FOR_INDEX` and incrementally
    /// maintained from then on (see `maintain_vector_indexes_for_row`,
    /// `maintain_vector_indexes_on_delete`, and `maybe_build_vector_indexes`
    /// below).
    vector_indexes: Vec<(String, vector_index::IvfFlatIndex)>,
}

/// Updates every vector index on `ts` to reflect `row`'s current value at
/// `id`: inserts/replaces if the indexed column holds a vector, removes if
/// not (e.g. set to `NULL` by an `UPDATE`). No-op if `ts` has no indexes yet.
fn maintain_vector_indexes_for_row(ts: &mut TableStorage, id: u64, row: &[Value]) {
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
fn maintain_vector_indexes_on_delete(ts: &mut TableStorage, id: u64) {
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
fn maybe_build_vector_indexes(ts: &mut TableStorage) -> Result<(), DbError> {
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
        let row = Database::decode_row_validated(id, bytes, col_count)?;
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

/// Pre-computed result for an uncorrelated subquery node, indexed by DFS
/// order. Correlated nodes get `Uncached` and are re-evaluated per row.
enum CacheEntry {
    /// Subquery is correlated (references outer columns) — not cached.
    Uncached,
    /// `EXISTS(...)` result.
    Exists(bool),
    /// `column IN (SELECT ...)` — all values from the subquery's single column.
    In(Vec<Value>),
    /// `column <op> (SELECT ...)` — the single scalar value.
    Scalar(Value),
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
            Statement::AlterTable(at) => {
                self.run_alter_table(at)?;
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
                                    maintain_vector_indexes_on_delete(ts, id);
                                } else {
                                    ts.tree.insert(id, bytes[1..].to_vec());
                                    let row = Self::decode_row_validated(
                                        id,
                                        &bytes[1..],
                                        ts.schema.columns.len(),
                                    )?;
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
                vector_indexes: Vec::new(),
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

    fn run_alter_table(&mut self, at: &AlterTableStatement) -> Result<(), DbError> {
        let ts = self
            .table_mut(&at.table)
            .ok_or_else(|| DbError::UnknownTable(at.table.clone()))?;

        match &at.op {
            AlterTableOp::AddColumn(col) => {
                // Reject duplicate column names.
                if ts.schema.columns.iter().any(|c| c == &col.name) {
                    return Err(DbError::Unsupported(alloc::format!(
                        "column '{}' already exists",
                        col.name
                    )));
                }
                let ctype = match col.ctype {
                    crate::parser::ColumnType::Int => ColumnType::Int,
                    crate::parser::ColumnType::Text => ColumnType::Text,
                    crate::parser::ColumnType::Vector => ColumnType::Vector,
                };
                // Re-encode every row with the new column appended (default: Null).
                let mut rows_to_reencode = Vec::new();
                for id in 0..ts.next_row_id {
                    if let Some(bytes) = ts.tree.get(id) {
                        let mut values = Self::try_decode_row(bytes)?;
                        values.push(Value::Null);
                        rows_to_reencode.push((id, Self::encode_row(&values)));
                    }
                }
                for (id, encoded) in &rows_to_reencode {
                    ts.tree.insert(*id, encoded.to_vec());
                }
                ts.schema.columns.push(col.name.clone());
                ts.schema.types.push(ctype);
            }
            AlterTableOp::DropColumn(name) => {
                let idx = ts
                    .schema
                    .columns
                    .iter()
                    .position(|c| c == name)
                    .ok_or_else(|| DbError::UnknownColumn(name.clone()))?;
                // Cannot drop the implicit id column.
                if idx == 0 {
                    return Err(DbError::Unsupported(
                        "cannot drop the implicit id column".to_string(),
                    ));
                }
                // Re-encode every row without the dropped column.
                let mut rows_to_reencode = Vec::new();
                for id in 0..ts.next_row_id {
                    if let Some(bytes) = ts.tree.get(id) {
                        let mut values = Self::try_decode_row(bytes)?;
                        values.remove(idx);
                        rows_to_reencode.push((id, Self::encode_row(&values)));
                    }
                }
                for (id, encoded) in &rows_to_reencode {
                    ts.tree.insert(*id, encoded.to_vec());
                }
                ts.schema.columns.remove(idx);
                ts.schema.types.remove(idx);
            }
            AlterTableOp::RenameColumn { old_name, new_name } => {
                let idx = ts
                    .schema
                    .columns
                    .iter()
                    .position(|c| c == old_name)
                    .ok_or_else(|| DbError::UnknownColumn(old_name.clone()))?;
                // Cannot rename the implicit id column.
                if idx == 0 {
                    return Err(DbError::Unsupported(
                        "cannot rename the implicit id column".to_string(),
                    ));
                }
                if ts.schema.columns.iter().any(|c| c == new_name) {
                    return Err(DbError::Unsupported(alloc::format!(
                        "column '{}' already exists",
                        new_name
                    )));
                }
                // Rename is metadata-only — the TLV codec is position-based.
                ts.schema.columns[idx] = new_name.clone();
            }
        }
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
            maintain_vector_indexes_for_row(ts, id, &row);
            maybe_build_vector_indexes(ts)?;
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
                maintain_vector_indexes_for_row(ts, id, &row);
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
                maintain_vector_indexes_on_delete(ts, id);
            }
        }
        Ok(())
    }

    /// Mirrors the local-only resolution logic of [`executor::find_value`]:
    /// does `name` resolve against `own_columns` without walking outer scopes?
    ///
    /// For qualified names (e.g. `"t.id"`), only exact matches are accepted —
    /// no suffix fallback, because that would be ambiguous across table
    /// qualifiers. For unqualified names, suffix matching is used (e.g. `"id"`
    /// matches `"t.id"`).
    fn column_resolves_locally(name: &str, own_columns: &[String]) -> bool {
        if own_columns.iter().any(|c| c == name) {
            return true;
        }
        if name.contains('.') {
            return false;
        }
        own_columns
            .iter()
            .any(|c| c.rfind('.').map_or(c.as_str(), |i| &c[i + 1..]) == name)
    }

    /// Returns `true` when `expr` references a column that isn't in
    /// `own_columns` (i.e. it references an outer scope — correlated).
    fn expr_references_outer(expr: &Expr, own_columns: &[String]) -> bool {
        match expr {
            Expr::Cmp { column, .. } => !Self::column_resolves_locally(column, own_columns),
            Expr::CmpColumn { left, right, .. } => {
                !Self::column_resolves_locally(left, own_columns)
                    || !Self::column_resolves_locally(right, own_columns)
            }
            Expr::IsNull { column, .. } => !Self::column_resolves_locally(column, own_columns),
            Expr::Like { column, .. } => !Self::column_resolves_locally(column, own_columns),
            Expr::InInt { column, .. } => !Self::column_resolves_locally(column, own_columns),
            Expr::BetweenInt { column, .. } => !Self::column_resolves_locally(column, own_columns),
            Expr::And(l, r) | Expr::Or(l, r) => {
                Self::expr_references_outer(l, own_columns)
                    || Self::expr_references_outer(r, own_columns)
            }
            Expr::Not(inner) => Self::expr_references_outer(inner, own_columns),
            Expr::Agg { .. } => false,
            Expr::Exists { query }
            | Expr::InSubquery { query, .. }
            | Expr::ScalarCmp { query, .. } => {
                if let Some(ref w) = query.filter {
                    let own = Self::resolve_query_own_columns(query, own_columns);
                    Self::expr_references_outer(w, &own)
                } else {
                    false
                }
            }
        }
    }

    /// Derives the column names visible from a subquery's own `FROM` + `JOIN`
    /// clauses, using the parent scope's column names as input. Columns are
    /// re-qualified with the subquery's own table aliases so that
    /// [`Self::column_resolves_locally`] can detect outer-scope references
    /// precisely.
    fn resolve_query_own_columns(
        query: &SelectStatement,
        parent_columns: &[String],
    ) -> Vec<String> {
        let mut cols = Vec::new();
        match &query.table {
            TableRef::Named { name, alias } => {
                let qualifier = alias.as_ref().unwrap_or(name);
                for c in parent_columns {
                    if let Some(dot) = c.find('.') {
                        let base_table = &c[..dot];
                        if base_table == name || alias.as_deref() == Some(base_table) {
                            cols.push(alloc::format!("{}.{}", qualifier, &c[dot + 1..]));
                        }
                    } else {
                        cols.push(alloc::format!("{}.{}", qualifier, c));
                    }
                }
            }
            TableRef::Subquery { .. } => cols.extend(parent_columns.iter().cloned()),
        }
        for join in &query.joins {
            let jt_name = join.table.table_name();
            let jt_qualifier = match &join.table {
                TableRef::Named { alias, .. } => {
                    alias.clone().unwrap_or_else(|| jt_name.to_string())
                }
                TableRef::Subquery { alias, .. } => alias.clone(),
            };
            for c in parent_columns {
                if let Some(dot) = c.find('.') {
                    let base_table = &c[..dot];
                    if base_table == jt_name || base_table == jt_qualifier.as_str() {
                        cols.push(alloc::format!("{}.{}", jt_qualifier, &c[dot + 1..]));
                    }
                }
            }
        }
        if cols.is_empty() {
            parent_columns.to_vec()
        } else {
            cols
        }
    }

    /// DFS-walks `expr`, assigning incrementing indices to each
    /// `Exists`/`InSubquery`/`ScalarCmp` node (matching the order used
    /// by [`Database::build_subquery_cache`] and [`Database::eval_where`]).
    fn walk_subqueries<F: FnMut(&mut usize, &Expr)>(
        expr: &mut Expr,
        counter: &mut usize,
        f: &mut F,
    ) {
        match expr {
            Expr::And(l, r) | Expr::Or(l, r) => {
                Self::walk_subqueries(l, counter, f);
                Self::walk_subqueries(r, counter, f);
            }
            Expr::Not(inner) => Self::walk_subqueries(inner, counter, f),
            Expr::Exists { .. } | Expr::InSubquery { .. } | Expr::ScalarCmp { .. } => {
                f(counter, expr);
            }
            _ => {}
        }
    }

    /// Pre-computes cache entries for uncorrelated subqueries in a `WHERE`
    /// expression tree. Each `Exists`/`InSubquery`/`ScalarCmp` node is
    /// visited in DFS order; if the subquery doesn't reference outer columns,
    /// its result is computed once and stored. Correlated nodes get
    /// `CacheEntry::Uncached`.
    fn build_subquery_cache(
        &self,
        where_expr: &Expr,
        own_columns: &[String],
        params: &[Vec<f32>],
        outer_ctes: &[CTE],
    ) -> Result<Vec<CacheEntry>, DbError> {
        let mut cache = Vec::new();
        Self::walk_subqueries(&mut where_expr.clone(), &mut 0usize, &mut |_, _| {
            cache.push(CacheEntry::Uncached);
        });
        // Walk again with the same DFS order to populate.
        Self::walk_subqueries(
            &mut where_expr.clone(),
            &mut 0usize,
            &mut |counter, node| {
                let idx = *counter;
                *counter += 1;
                match node {
                    Expr::Exists { query }
                        if !Self::expr_references_outer(
                            &Expr::Exists {
                                query: query.clone(),
                            },
                            own_columns,
                        ) =>
                    {
                        if let Ok(rs) = self.run_select_scoped(query, params, &[], outer_ctes) {
                            cache[idx] = CacheEntry::Exists(!rs.rows.is_empty());
                        }
                    }
                    Expr::InSubquery { column: _, query }
                        if !Self::expr_references_outer(
                            &Expr::InSubquery {
                                column: String::new(),
                                query: query.clone(),
                            },
                            own_columns,
                        ) =>
                    {
                        if let Ok(rs) = self.run_select_scoped(query, params, &[], outer_ctes) {
                            let vals: Vec<Value> =
                                rs.rows.into_iter().map(|r| r[0].clone()).collect();
                            cache[idx] = CacheEntry::In(vals);
                        }
                    }
                    Expr::ScalarCmp {
                        column: _,
                        op: _,
                        query,
                    } if !Self::expr_references_outer(
                        &Expr::ScalarCmp {
                            column: String::new(),
                            op: CmpOp::Eq,
                            query: query.clone(),
                        },
                        own_columns,
                    ) =>
                    {
                        if let Ok(rs) = self.run_select_scoped(query, params, &[], outer_ctes) {
                            if rs.rows.len() == 1 && rs.columns.len() == 1 {
                                cache[idx] = CacheEntry::Scalar(rs.rows[0][0].clone());
                            }
                        }
                    }
                    _ => {}
                }
            },
        );
        Ok(cache)
    }

    /// Evaluates a `WHERE`/`HAVING`-style predicate against a row, with
    /// database access for `Exists`/`InSubquery`/`ScalarCmp` nodes.
    ///
    /// `And`/`Or`/`Not` recursion happens here (not in `executor::eval_expr`)
    /// so a subquery nested inside a boolean combinator still gets database
    /// access. `outer`, if given, is the enclosing query's `(columns, row)`
    /// — passed down so a correlated subquery's own `WHERE` can resolve a
    /// column that isn't in its own `FROM` (see `executor::find_value`).
    ///
    /// `outer_ctes` carries CTE definitions from enclosing queries so
    /// subqueries can reference them (subquery's own CTEs shadow outer ones).
    ///
    /// `cache` holds pre-computed results for uncorrelated subqueries (indexed
    /// by a DFS order over `Exists`/`InSubquery`/`ScalarCmp` nodes); `counter`
    /// advances through the cache as each node is visited.
    #[allow(clippy::too_many_arguments)]
    fn eval_where(
        &self,
        expr: &Expr,
        columns: &[String],
        row: &[Value],
        params: &[Vec<f32>],
        outer: &[(&[String], &[Value])],
        outer_ctes: &[CTE],
        scope_columns: &[String],
        cache: &[CacheEntry],
        counter: &mut usize,
    ) -> Result<bool, DbError> {
        match expr {
            Expr::And(l, r) => Ok(self.eval_where(
                l,
                columns,
                row,
                params,
                outer,
                outer_ctes,
                scope_columns,
                cache,
                counter,
            )? && self.eval_where(
                r,
                columns,
                row,
                params,
                outer,
                outer_ctes,
                scope_columns,
                cache,
                counter,
            )?),
            Expr::Or(l, r) => Ok(self.eval_where(
                l,
                columns,
                row,
                params,
                outer,
                outer_ctes,
                scope_columns,
                cache,
                counter,
            )? || self.eval_where(
                r,
                columns,
                row,
                params,
                outer,
                outer_ctes,
                scope_columns,
                cache,
                counter,
            )?),
            Expr::Not(inner) => Ok(!self.eval_where(
                inner,
                columns,
                row,
                params,
                outer,
                outer_ctes,
                scope_columns,
                cache,
                counter,
            )?),
            Expr::Exists { query } => {
                let idx = *counter;
                *counter += 1;
                if let Some(CacheEntry::Exists(result)) = cache.get(idx) {
                    return Ok(*result);
                }
                let mut stack: [(&[String], &[Value]); 8] = [(&[], &[]); 8];
                let depth = 1 + outer.len();
                assert!(depth <= 8, "correlation nesting too deep");
                stack[0] = (scope_columns, row);
                stack[1..depth].copy_from_slice(outer);
                let rs = self.run_select_scoped(query, params, &stack[..depth], outer_ctes)?;
                Ok(!rs.rows.is_empty())
            }
            Expr::InSubquery { column, query } => {
                let lhs = executor::find_value(column, columns, row, outer)
                    .ok_or_else(|| DbError::UnknownColumn(column.clone()))?;
                let idx = *counter;
                *counter += 1;
                if let Some(CacheEntry::In(vals)) = cache.get(idx) {
                    return Ok(vals.iter().any(|v| v == lhs));
                }
                let mut stack: [(&[String], &[Value]); 8] = [(&[], &[]); 8];
                let depth = 1 + outer.len();
                assert!(depth <= 8, "correlation nesting too deep");
                stack[0] = (scope_columns, row);
                stack[1..depth].copy_from_slice(outer);
                let rs = self.run_select_scoped(query, params, &stack[..depth], outer_ctes)?;
                if rs.columns.len() != 1 {
                    return Err(DbError::SubqueryCardinality(alloc::format!(
                        "IN subquery must return exactly one column, got {}",
                        rs.columns.len()
                    )));
                }
                Ok(rs.rows.iter().any(|r| &r[0] == lhs))
            }
            Expr::ScalarCmp { column, op, query } => {
                let lhs = executor::find_value(column, columns, row, outer)
                    .ok_or_else(|| DbError::UnknownColumn(column.clone()))?;
                let idx = *counter;
                *counter += 1;
                if let Some(CacheEntry::Scalar(rhs)) = cache.get(idx) {
                    if matches!(lhs, Value::Null) || matches!(rhs, Value::Null) {
                        return Ok(false);
                    }
                    let ord = lhs.cmp(rhs);
                    return Ok(match op {
                        CmpOp::Eq => ord == core::cmp::Ordering::Equal,
                        CmpOp::Ne => ord != core::cmp::Ordering::Equal,
                        CmpOp::Lt => ord == core::cmp::Ordering::Less,
                        CmpOp::Le => ord != core::cmp::Ordering::Greater,
                        CmpOp::Gt => ord == core::cmp::Ordering::Greater,
                        CmpOp::Ge => ord != core::cmp::Ordering::Less,
                    });
                }
                let mut stack: [(&[String], &[Value]); 8] = [(&[], &[]); 8];
                let depth = 1 + outer.len();
                assert!(depth <= 8, "correlation nesting too deep");
                stack[0] = (scope_columns, row);
                stack[1..depth].copy_from_slice(outer);
                let rs = self.run_select_scoped(query, params, &stack[..depth], outer_ctes)?;
                if rs.columns.len() != 1 || rs.rows.len() != 1 {
                    return Err(DbError::SubqueryCardinality(alloc::format!(
                        "scalar subquery must return exactly one row and one column, got {} row(s) and {} column(s)",
                        rs.rows.len(),
                        rs.columns.len()
                    )));
                }
                let rhs = &rs.rows[0][0];
                if matches!(lhs, Value::Null) || matches!(rhs, Value::Null) {
                    return Ok(false);
                }
                let ord = lhs.cmp(rhs);
                Ok(match op {
                    CmpOp::Eq => ord == core::cmp::Ordering::Equal,
                    CmpOp::Ne => ord != core::cmp::Ordering::Equal,
                    CmpOp::Lt => ord == core::cmp::Ordering::Less,
                    CmpOp::Le => ord != core::cmp::Ordering::Greater,
                    CmpOp::Gt => ord == core::cmp::Ordering::Greater,
                    CmpOp::Ge => ord != core::cmp::Ordering::Less,
                })
            }
            _ => Ok(executor::eval_expr_scoped(expr, columns, row, outer)?),
        }
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
                Self::decode_row_validated(id, &buffered[1..], ts.schema.columns.len())?
            } else if let Some(bytes) = ts.tree.get(id) {
                Self::decode_row_validated(id, bytes, ts.schema.columns.len())?
            } else {
                continue;
            };
            let keep = match filter {
                None => true,
                Some(expr) => self.eval_where(
                    expr,
                    &ts.schema.columns,
                    &row,
                    &[],
                    &[],
                    &[],
                    &ts.schema.columns,
                    &cache,
                    &mut 0usize,
                )?,
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

    /// Resolves a [`TableRef`] to `(columns, rows)` without CTE context.
    fn resolve_table_ref(&self, r: &TableRef) -> Result<(Vec<String>, Vec<Vec<Value>>), DbError> {
        self.resolve_table_ref_with_ctes(r, &[])
    }

    /// Resolves a [`TableRef`] to `(columns, rows)`: checks CTEs first, then
    /// views, then real tables for `Named`; executes the inner query for
    /// `Subquery`.
    fn resolve_table_ref_with_ctes(
        &self,
        r: &TableRef,
        ctes: &[CTE],
    ) -> Result<(Vec<String>, Vec<Vec<Value>>), DbError> {
        match r {
            TableRef::Named { name, .. } => {
                if let Some(cte) = ctes.iter().find(|c| &c.name == name) {
                    let rs = self.run_select_scoped(&cte.query, &[], &[], ctes)?;
                    return Ok((rs.columns, rs.rows));
                }
                if let Some((_, query)) = self.views.iter().find(|(n, _)| n == name) {
                    let rs = self.run_select(query, &[])?;
                    return Ok((rs.columns, rs.rows));
                }
                self.scan_table(name)
            }
            TableRef::Subquery { query, alias } => {
                let rs = self.run_select_scoped(query, &[], &[], ctes)?;
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

    /// Runs a top-level (non-correlated) `SELECT`.
    fn run_select(
        &self,
        stmt: &SelectStatement,
        params: &[Vec<f32>],
    ) -> Result<executor::ResultSet, DbError> {
        self.run_select_scoped(stmt, params, &[], &[])
    }

    /// Runs a `SELECT`, optionally scoped inside an enclosing (`outer`) row —
    /// used to evaluate a correlated subquery once per outer row. `outer` is
    /// threaded down into this query's own `WHERE` evaluation via
    /// [`Database::eval_where`], so a subquery nested inside this one only
    /// ever sees its immediate parent's row (single level of correlation).
    ///
    /// `outer_ctes` carries CTE definitions from enclosing queries so
    /// subqueries can reference them. The subquery's own CTEs shadow any
    /// matching outer CTE names (standard SQL scoping).
    fn run_select_scoped(
        &self,
        stmt: &SelectStatement,
        params: &[Vec<f32>],
        outer: &[(&[String], &[Value])],
        outer_ctes: &[CTE],
    ) -> Result<executor::ResultSet, DbError> {
        if let Some(ob) = &stmt.order_by_cosine {
            return self.run_vector_topk(stmt, ob, params);
        }

        // Merge outer CTEs with this statement's CTEs. Subquery's own CTEs
        // shadow outer ones (standard SQL scoping).
        let mut merged_ctes: Vec<CTE> = outer_ctes.to_vec();
        for cte in &stmt.with_ctes {
            // Remove any outer CTE with the same name (subquery shadows).
            merged_ctes.retain(|c| c.name != cte.name);
            merged_ctes.push(cte.clone());
        }

        // Validate CTEs: no duplicates, no shadowing, no self-references.
        for cte in &stmt.with_ctes {
            if self.views.iter().any(|(n, _)| n == &cte.name) {
                return Err(DbError::ViewAlreadyExists(cte.name.clone()));
            }
            if self.tables.iter().any(|(n, _)| n == &cte.name) {
                return Err(DbError::ViewAlreadyExists(cte.name.clone()));
            }
            if select_references_table(&cte.query, &cte.name) {
                return Err(DbError::RecursiveView(cte.name.clone()));
            }
        }

        // Build an in-memory table from the source table + optional JOINs.
        let (mut columns, mut rows) =
            self.resolve_table_ref_with_ctes(&stmt.table, &merged_ctes)?;

        // Process JOINs (nested-loop inner join).
        for join in &stmt.joins {
            let (join_cols, join_rows) =
                self.resolve_table_ref_with_ctes(&join.table, &merged_ctes)?;
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

        // Build scope-qualified column names for correlated subquery
        // resolution. The table qualifier (alias or name) is prepended to
        // each column so that `find_value("t.id")` in an inner subquery
        // matches the correct outer scope via exact match instead of
        // accidentally suffix-matching the local unqualified "id".
        let table_qualifier = stmt.table.name();
        let scope_columns: Vec<String> = table
            .columns
            .iter()
            .map(|c| {
                if c.contains('.') {
                    c.clone()
                } else {
                    alloc::format!("{table_qualifier}.{c}")
                }
            })
            .collect();

        // Apply WHERE filter.
        if let Some(expr) = &stmt.filter {
            let subquery_cache =
                self.build_subquery_cache(expr, &table.columns, params, &merged_ctes)?;
            let mut filtered = Vec::new();
            for row in &table.rows {
                if self.eval_where(
                    expr,
                    &table.columns,
                    row,
                    params,
                    outer,
                    &merged_ctes,
                    &scope_columns,
                    &subquery_cache,
                    &mut 0usize,
                )? {
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

        // Apply HAVING filter after aggregation.
        if let Some(hv) = &stmt.having {
            let hv_cache = self.build_subquery_cache(hv, &table.columns, params, &merged_ctes)?;
            let mut filtered = Vec::new();
            for row in &table.rows {
                if self.eval_where(
                    hv,
                    &table.columns,
                    row,
                    params,
                    outer,
                    &merged_ctes,
                    &table.columns,
                    &hv_cache,
                    &mut 0usize,
                )? {
                    filtered.push(row.clone());
                }
            }
            table.rows = filtered;
        }

        let plan = {
            let mut plan_stmt = stmt.clone();
            plan_stmt.group_by.clear();
            plan_stmt.aggregates.clear();
            plan_stmt.having = None;
            // The WHERE filter was already applied above with full
            // DB-aware/correlated-subquery semantics via `eval_where`;
            // clearing it here stops `plan_select` from re-wrapping it in a
            // `PlanNode::Filter`, which would otherwise re-run it through
            // `executor::execute`'s pure (non-DB-aware) evaluator and fail on
            // any subquery node.
            plan_stmt.filter = None;
            plan_select(
                &plan_stmt,
                TableStats {
                    row_count: table.rows.len() as u64,
                },
            )
        };
        executor::execute(&plan, &table).map_err(DbError::from)
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
                // Apply WHERE filter before extracting embeddings.
                if let Some(expr) = &stmt.filter {
                    if !self.eval_where(
                        expr,
                        &columns,
                        row,
                        params,
                        &[],
                        &[],
                        &columns,
                        &[],
                        &mut 0usize,
                    )? {
                        continue;
                    }
                }
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
            TableRef::Named { name, .. } => name.as_str(),
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

        if let Some((_, idx)) = ts.vector_indexes.iter().find(|(c, _)| c == &ob.column) {
            // Fast path: probe the IVFFlat index instead of scanning every
            // row. Oversample beyond `k` so a WHERE filter still has enough
            // candidates left to rank after filtering — recall stays
            // approximate either way (see `vector_index` module docs), the
            // same trade pgvector's own IVFFlat index type makes.
            let fetch_k = (ob.k as usize).saturating_mul(4).max(ob.k as usize);
            for id in idx.search(query, fetch_k, vector_index::DEFAULT_NPROBE) {
                let Some(bytes) = ts.tree.get(id) else {
                    continue;
                };
                let row = Self::decode_row_validated(id, bytes, ts.schema.columns.len())?;
                if let Some(expr) = &stmt.filter {
                    if !self.eval_where(
                        expr,
                        &ts.schema.columns,
                        &row,
                        params,
                        &[],
                        &[],
                        &ts.schema.columns,
                        &[],
                        &mut 0usize,
                    )? {
                        continue;
                    }
                }
                if let Value::Vector(v) = &row[slot] {
                    embeddings.push(v.clone());
                    rows.push(row);
                }
            }
        } else {
            // No index yet for this column (table hasn't crossed
            // `vector_index::MIN_ROWS_FOR_INDEX`, or all writes so far went
            // through a transaction not yet committed) — exact brute-force
            // scan. `0..next_row_id` (not "until the first missing id")
            // because deleted rows leave holes in the middle of the range.
            for id in 0..ts.next_row_id {
                let Some(bytes) = ts.tree.get(id) else {
                    continue;
                };
                let row = Self::decode_row_validated(id, bytes, ts.schema.columns.len())?;
                if let Some(expr) = &stmt.filter {
                    if !self.eval_where(
                        expr,
                        &ts.schema.columns,
                        &row,
                        params,
                        &[],
                        &[],
                        &ts.schema.columns,
                        &[],
                        &mut 0usize,
                    )? {
                        continue;
                    }
                }
                if let Value::Vector(v) = &row[slot] {
                    embeddings.push(v.clone());
                    rows.push(row);
                }
            }
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
    if let TableRef::Named { name: n, .. } = &stmt.table {
        if n == name {
            return true;
        }
    }
    stmt.joins
        .iter()
        .any(|j| matches!(&j.table, TableRef::Named { name: n, .. } if n == name))
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
    fn vector_topk_with_where_filter() {
        let schema = Schema {
            columns: alloc::vec!["id".to_string(), "emb".to_string(), "tag".to_string(),],
            types: alloc::vec![ColumnType::Int, ColumnType::Vector, ColumnType::Text],
        };
        let mut d = Database::new(schema);
        // id=0: tag=a, closest to [1,0]
        // id=1: tag=b, closest to [0,1]
        // id=2: tag=a, second closest to [1,0]
        // id=3: tag=b, closest to [1,0] but filtered out by WHERE tag='a'
        let data = &[
            (0, "[1.0, 0.0]", "a"),
            (1, "[0.0, 1.0]", "b"),
            (2, "[0.9, 0.1]", "a"),
            (3, "[0.95, 0.05]", "b"),
        ];
        for (id, emb, tag) in data {
            let sql = alloc::format!("INSERT INTO t (id, emb, tag) VALUES ({id}, {emb}, '{tag}')");
            d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
        }
        // Without WHERE: top-2 by cosine to [1,0] would be id=0, id=3.
        let sel =
            parse_statement("SELECT id FROM t WHERE tag = 'a' ORDER BY cosine(emb, ?) LIMIT 2")
                .unwrap();
        let r = d.execute(&sel, &[alloc::vec![1.0, 0.0]]).unwrap();
        assert_eq!(r.rows.len(), 2);
        // tag='a' rows: id=0 ([1,0]), id=2 ([0.9,0.1]) → both kept
        assert_eq!(r.rows[0][0], Value::Int(0));
        assert_eq!(r.rows[1][0], Value::Int(2));
    }

    #[test]
    fn vector_topk_uses_ivfflat_index_past_threshold() {
        // One past `vector_index::MIN_ROWS_FOR_INDEX` so the lazy build
        // triggers on the row that crosses it, exercising the index path in
        // `run_vector_topk` instead of the brute-force scan.
        let n = vector_index::MIN_ROWS_FOR_INDEX + 1;
        let schema = Schema {
            columns: alloc::vec!["id".to_string(), "emb".to_string()],
            types: alloc::vec![ColumnType::Int, ColumnType::Vector],
        };
        let mut d = Database::new(schema);
        for i in 0..n {
            // Unique one-hot embeddings (dim == n) so nearest-neighbor
            // results are unambiguous regardless of cluster assignment.
            let mut emb = alloc::vec!["0.0".to_string(); n];
            emb[i] = "1.0".to_string();
            let sql = alloc::format!("INSERT INTO t (id, emb) VALUES ({i}, [{}])", emb.join(", "));
            d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
        }
        assert!(
            d.table("t")
                .unwrap()
                .vector_indexes
                .iter()
                .any(|(c, _)| c == "emb"),
            "index should have been built once the table crossed MIN_ROWS_FOR_INDEX"
        );
        let mut query = alloc::vec![0.0f32; n];
        query[5] = 1.0;
        let sel = parse_statement("SELECT id FROM t ORDER BY cosine(emb, ?) LIMIT 1").unwrap();
        let r = d.execute(&sel, &[query]).unwrap();
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0][0], Value::Int(5));
    }

    #[test]
    fn vector_index_maintained_on_update_and_delete() {
        let n = vector_index::MIN_ROWS_FOR_INDEX + 1;
        // dim = n + 1: one spare slot no initial row occupies, so moving a
        // row onto it via UPDATE can't tie with an existing row's vector.
        let dim = n + 1;
        let schema = Schema {
            columns: alloc::vec!["id".to_string(), "emb".to_string()],
            types: alloc::vec![ColumnType::Int, ColumnType::Vector],
        };
        let mut d = Database::new(schema);
        for i in 0..n {
            let mut emb = alloc::vec!["0.0".to_string(); dim];
            emb[i] = "1.0".to_string();
            let sql = alloc::format!("INSERT INTO t (id, emb) VALUES ({i}, [{}])", emb.join(", "));
            d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
        }
        // Delete row 5, then query for its old embedding: it must be gone.
        d.execute(&parse_statement("DELETE FROM t WHERE id = 5").unwrap(), &[])
            .unwrap();
        let mut old_query = alloc::vec![0.0f32; dim];
        old_query[5] = 1.0;
        let sel = parse_statement("SELECT id FROM t ORDER BY cosine(emb, ?) LIMIT 3").unwrap();
        let r = d.execute(&sel, &[old_query]).unwrap();
        assert!(r.rows.iter().all(|row| row[0] != Value::Int(5)));

        // Update row 6's embedding onto the spare slot, then confirm the
        // index itself was updated (not just the tree). Checked directly
        // against the index with an exhaustive nprobe rather than through
        // SQL: the SQL path uses `vector_index::DEFAULT_NPROBE`, and a
        // one-hot spare dimension with zero training signal makes every
        // centroid tie at dot-product 0 against it, which is a pathological
        // case for *approximate* recall, not a maintenance bug — this
        // assertion is about whether `id, vector` moved to where it should
        // be inside the index, not about IVF recall under adversarial input.
        let mut new_emb = alloc::vec!["0.0".to_string(); dim];
        new_emb[n] = "1.0".to_string();
        let update_sql = alloc::format!("UPDATE t SET emb = [{}] WHERE id = 6", new_emb.join(", "));
        d.execute(&parse_statement(&update_sql).unwrap(), &[])
            .unwrap();
        let mut moved_query = alloc::vec![0.0f32; dim];
        moved_query[n] = 1.0;
        let ts = d.table("t").unwrap();
        let (_, idx) = ts
            .vector_indexes
            .iter()
            .find(|(c, _)| c == "emb")
            .expect("index should still exist after the update");
        let top = idx.search(&moved_query, 1, usize::MAX);
        assert_eq!(top[0], 6);
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
            let sql = alloc::format!(
                "INSERT INTO t (id, name, age) VALUES ({i}, 'u{i}', {})",
                i * 10
            );
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
            d.execute(
                &parse_statement("CREATE VIEW t AS SELECT * FROM t").unwrap(),
                &[]
            ),
            Err(DbError::ViewAlreadyExists(_))
        ));
    }

    #[test]
    fn subquery_in_from() {
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
                &parse_statement("SELECT * FROM (SELECT id, name FROM t WHERE age >= 20) AS sub")
                    .unwrap(),
                &[],
            )
            .unwrap();
        assert_eq!(r.rows.len(), 3);
        assert_eq!(
            r.columns,
            alloc::vec!["sub.id".to_string(), "sub.name".to_string()]
        );
    }

    #[test]
    fn subquery_with_alias_in_outer_where() {
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
            let sql = alloc::format!(
                "INSERT INTO t (id, name, age) VALUES ({i}, 'u{i}', {})",
                i * 10
            );
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

    #[test]
    fn cte_basic() {
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
                &parse_statement(
                    "WITH cte AS (SELECT id, name FROM t WHERE age >= 20) SELECT * FROM cte",
                )
                .unwrap(),
                &[],
            )
            .unwrap();
        assert_eq!(r.rows.len(), 3);
    }

    #[test]
    fn cte_multiple() {
        let mut d = db();
        for i in 0..5 {
            let sql = alloc::format!(
                "INSERT INTO t (id, name, age) VALUES ({i}, 'u{i}', {})",
                i * 10
            );
            d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
        }
        // Use a CTE to filter, then query it.
        let r = d
            .execute(
                &parse_statement(
                    "WITH young AS (SELECT id, age FROM t WHERE age < 30) \
                     SELECT * FROM young WHERE age >= 10",
                )
                .unwrap(),
                &[],
            )
            .unwrap();
        // young has ages 0,10,20; WHERE age >= 10 keeps ages 10,20 = 2 rows.
        assert_eq!(r.rows.len(), 2);
    }

    #[test]
    fn cte_recursive_self_reference_errors() {
        let mut d = db();
        assert!(matches!(
            d.execute(
                &parse_statement("WITH cte AS (SELECT id FROM cte) SELECT * FROM cte").unwrap(),
                &[],
            ),
            Err(DbError::RecursiveView(_))
        ));
    }

    #[test]
    fn cte_shadow_existing_table_errors() {
        let mut d = db();
        assert!(matches!(
            d.execute(
                &parse_statement("WITH t AS (SELECT id FROM t) SELECT * FROM t").unwrap(),
                &[],
            ),
            Err(DbError::ViewAlreadyExists(_))
        ));
    }

    #[test]
    fn cte_visible_in_subquery_from() {
        let mut d = db();
        for i in 0..5 {
            let sql = alloc::format!(
                "INSERT INTO t (id, name, age) VALUES ({i}, 'u{i}', {})",
                i * 10
            );
            d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
        }
        // CTE "adults" is defined in the outer query; the subquery in FROM
        // should be able to reference it.
        let r = d
            .execute(
                &parse_statement(
                    "WITH adults AS (SELECT id, age FROM t WHERE age >= 20) \
                     SELECT * FROM (SELECT id FROM adults) AS sub",
                )
                .unwrap(),
                &[],
            )
            .unwrap();
        assert_eq!(r.rows.len(), 3);
    }

    #[test]
    fn cte_visible_in_where_subquery() {
        let mut d = db();
        for i in 0..5 {
            let sql = alloc::format!(
                "INSERT INTO t (id, name, age) VALUES ({i}, 'u{i}', {})",
                i * 10
            );
            d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
        }
        // CTE "adults" is used in an EXISTS subquery in WHERE.
        let r = d
            .execute(
                &parse_statement(
                    "WITH adults AS (SELECT id FROM t WHERE age >= 20) \
                     SELECT id FROM t WHERE EXISTS (SELECT id FROM adults)",
                )
                .unwrap(),
                &[],
            )
            .unwrap();
        // CTE is non-empty so EXISTS is true for all 5 outer rows.
        assert_eq!(r.rows.len(), 5);
    }

    #[test]
    fn correlated_subquery_sees_outer_row() {
        let mut d = db();
        for i in 0..5 {
            let sql = alloc::format!(
                "INSERT INTO t (id, name, age) VALUES ({i}, 'u{i}', {})",
                i * 10
            );
            d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
        }
        // Non-correlated EXISTS works.
        let r = d
            .execute(
                &parse_statement("SELECT id FROM t WHERE EXISTS (SELECT id FROM t WHERE id = 1)")
                    .unwrap(),
                &[],
            )
            .unwrap();
        assert_eq!(r.rows.len(), 5);

        // Correlated: inner references outer.id via column-to-column comparison.
        let r2 = d
            .execute(
                &parse_statement(
                    "SELECT id FROM t WHERE EXISTS (SELECT id FROM t AS inner_t WHERE inner_t.id < t.id)",
                )
                .unwrap(),
                &[],
            )
            .unwrap();
        assert_eq!(r2.rows.len(), 4);
    }

    #[test]
    fn column_to_column_comparison() {
        let mut d = db();
        for i in 0..5 {
            let sql = alloc::format!(
                "INSERT INTO t (id, name, age) VALUES ({i}, 'u{i}', {})",
                i * 10
            );
            d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
        }
        // Simple column-to-column without alias.
        let r = d
            .execute(
                &parse_statement("SELECT id FROM t WHERE id < age").unwrap(),
                &[],
            )
            .unwrap();
        // id < age: id=0 age=0 no, id=1 age=10 yes, id=2 age=20 yes, ...
        assert_eq!(r.rows.len(), 4);
    }

    #[test]
    fn correlated_subquery_two_levels_deep() {
        let mut d = db();
        for i in 0..5 {
            let sql = alloc::format!(
                "INSERT INTO t (id, name, age) VALUES ({i}, 'u{i}', {})",
                i * 10
            );
            d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
        }
        // Verify nested EXISTS works (non-correlated).
        let r = d
            .execute(
                &parse_statement("SELECT id FROM t WHERE EXISTS (SELECT id FROM t WHERE id = 1)")
                    .unwrap(),
                &[],
            )
            .unwrap();
        assert_eq!(r.rows.len(), 5);

        // Two-level correlation: outer → mid → inner.
        // For each outer row `t`, check if there exists a mid row where
        // mid.id > t.id AND that mid row has an inner row with
        // inner.id > mid.id.  Returns true for id=0..2 (0<1<2, 0<1<3, etc.).
        let r2 = d
            .execute(
                &parse_statement(
                    "SELECT id FROM t WHERE EXISTS \
                     (SELECT id FROM t AS mid WHERE mid.id > t.id \
                      AND EXISTS \
                      (SELECT id FROM t AS inner_t WHERE inner_t.id > mid.id))",
                )
                .unwrap(),
                &[],
            )
            .unwrap();
        // id=0: mid can be 1,2,3,4; inner exists (e.g. mid=1 inner=2) ✓
        // id=1: mid can be 2,3,4; inner exists (e.g. mid=2 inner=3) ✓
        // id=2: mid can be 3,4; inner exists (e.g. mid=3 inner=4) ✓
        // id=3: mid=4; no inner > 4 ✗
        // id=4: no mid > 4 ✗
        assert_eq!(r2.rows.len(), 3);
    }

    #[test]
    fn alter_table_add_column() {
        let mut d = db();
        d.execute(
            &parse_statement("INSERT INTO t (id, name, age) VALUES (1, 'alice', 30)").unwrap(),
            &[],
        )
        .unwrap();
        d.execute(
            &parse_statement("ALTER TABLE t ADD COLUMN email TEXT").unwrap(),
            &[],
        )
        .unwrap();
        let r = d
            .execute(&parse_statement("SELECT * FROM t").unwrap(), &[])
            .unwrap();
        assert_eq!(r.columns.len(), 4); // id, name, age, email
        assert_eq!(r.rows[0][3], Value::Null); // default value
    }

    #[test]
    fn alter_table_drop_column() {
        let mut d = db();
        d.execute(
            &parse_statement("INSERT INTO t (id, name, age) VALUES (1, 'alice', 30)").unwrap(),
            &[],
        )
        .unwrap();
        d.execute(
            &parse_statement("ALTER TABLE t DROP COLUMN age").unwrap(),
            &[],
        )
        .unwrap();
        let r = d
            .execute(&parse_statement("SELECT * FROM t").unwrap(), &[])
            .unwrap();
        assert_eq!(r.columns.len(), 2); // id, name only
        assert_eq!(r.rows[0][0], Value::Int(1));
        assert_eq!(r.rows[0][1], Value::Text("alice".to_string()));
    }

    #[test]
    fn alter_table_rename_column() {
        let mut d = db();
        d.execute(
            &parse_statement("INSERT INTO t (id, name, age) VALUES (1, 'alice', 30)").unwrap(),
            &[],
        )
        .unwrap();
        d.execute(
            &parse_statement("ALTER TABLE t RENAME COLUMN name TO full_name").unwrap(),
            &[],
        )
        .unwrap();
        let r = d
            .execute(&parse_statement("SELECT * FROM t").unwrap(), &[])
            .unwrap();
        assert_eq!(r.columns[1], "full_name");
    }

    #[test]
    fn alter_table_unknown_column_errors() {
        let mut d = db();
        assert!(matches!(
            d.execute(
                &parse_statement("ALTER TABLE t DROP COLUMN nonexistent").unwrap(),
                &[],
            ),
            Err(DbError::UnknownColumn(_))
        ));
    }

    #[test]
    fn alter_table_add_duplicate_column_errors() {
        let mut d = db();
        assert!(matches!(
            d.execute(
                &parse_statement("ALTER TABLE t ADD COLUMN name TEXT").unwrap(),
                &[],
            ),
            Err(DbError::Unsupported(_))
        ));
    }

    #[test]
    fn alter_table_unknown_table_errors() {
        let mut d = db();
        assert!(matches!(
            d.execute(
                &parse_statement("ALTER TABLE nonexistent ADD COLUMN x INT").unwrap(),
                &[],
            ),
            Err(DbError::UnknownTable(_))
        ));
    }

    #[test]
    fn having_filters_groups() {
        let mut d = Database::empty();
        d.execute(
            &parse_statement("CREATE TABLE t (dept TEXT, salary INT)").unwrap(),
            &[],
        )
        .unwrap();
        for (dept, salary) in [("eng", 100), ("eng", 200), ("sales", 150), ("sales", 50)] {
            let sql = alloc::format!("INSERT INTO t (dept, salary) VALUES ('{dept}', {salary})");
            d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
        }
        let r = d
            .execute(
                &parse_statement(
                    "SELECT dept, COUNT(*) AS cnt FROM t GROUP BY dept HAVING cnt >= 2",
                )
                .unwrap(),
                &[],
            )
            .unwrap();
        assert_eq!(r.rows.len(), 2);
    }

    #[test]
    fn having_filters_groups_with_sum() {
        let mut d = Database::empty();
        d.execute(
            &parse_statement("CREATE TABLE t (dept TEXT, salary INT)").unwrap(),
            &[],
        )
        .unwrap();
        for (dept, salary) in [("eng", 100), ("eng", 200), ("sales", 150), ("sales", 50)] {
            let sql = alloc::format!("INSERT INTO t (dept, salary) VALUES ('{dept}', {salary})");
            d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
        }
        let r = d
            .execute(
                &parse_statement(
                    "SELECT dept, SUM(salary) AS total FROM t GROUP BY dept HAVING total > 200",
                )
                .unwrap(),
                &[],
            )
            .unwrap();
        // eng total = 300 (>200), sales total = 200 (not >200)
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0][0], Value::Text("eng".to_string()));
    }

    #[test]
    fn having_without_group_by_filters_single_aggregate() {
        let mut d = Database::empty();
        d.execute(&parse_statement("CREATE TABLE t (x INT)").unwrap(), &[])
            .unwrap();
        for i in 0..5 {
            let sql = alloc::format!("INSERT INTO t (x) VALUES ({i})");
            d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
        }
        // COUNT(*) = 5, HAVING 5 > 10 should filter it out.
        let r = d
            .execute(
                &parse_statement("SELECT COUNT(*) AS cnt FROM t HAVING cnt > 10").unwrap(),
                &[],
            )
            .unwrap();
        assert_eq!(r.rows.len(), 0);
    }

    #[test]
    fn having_with_and_or() {
        let mut d = Database::empty();
        d.execute(
            &parse_statement("CREATE TABLE t (dept TEXT, salary INT)").unwrap(),
            &[],
        )
        .unwrap();
        for (dept, salary) in [("eng", 100), ("eng", 200), ("sales", 150), ("hr", 50)] {
            let sql = alloc::format!("INSERT INTO t (dept, salary) VALUES ('{dept}', {salary})");
            d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
        }
        let r = d
            .execute(
                &parse_statement(
                    "SELECT dept, COUNT(*) AS cnt FROM t GROUP BY dept HAVING cnt >= 2 AND cnt <= 2",
                )
                .unwrap(),
                &[],
            )
            .unwrap();
        // Only eng has cnt=2.
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0][0], Value::Text("eng".to_string()));
    }

    #[test]
    fn uncorrelated_exists_is_cached() {
        let mut d = Database::empty();
        d.execute(
            &parse_statement("CREATE TABLE t (id INT, name TEXT)").unwrap(),
            &[],
        )
        .unwrap();
        d.execute(&parse_statement("CREATE TABLE t2 (val INT)").unwrap(), &[])
            .unwrap();
        for i in 0..5 {
            let sql = alloc::format!("INSERT INTO t (id, name) VALUES ({i}, 'u{i}')");
            d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
        }
        d.execute(
            &parse_statement("INSERT INTO t2 (val) VALUES (10)").unwrap(),
            &[],
        )
        .unwrap();
        // Uncorrelated EXISTS — inner query doesn't reference outer columns.
        // Should return all rows since t2 has at least one row.
        let r = d
            .execute(
                &parse_statement(
                    "SELECT id FROM t WHERE EXISTS (SELECT val FROM t2 WHERE val > 5)",
                )
                .unwrap(),
                &[],
            )
            .unwrap();
        assert_eq!(r.rows.len(), 5);
        // No matching row: t2.val = 10, not < 5.
        let r2 = d
            .execute(
                &parse_statement(
                    "SELECT id FROM t WHERE EXISTS (SELECT val FROM t2 WHERE val < 5)",
                )
                .unwrap(),
                &[],
            )
            .unwrap();
        assert_eq!(r2.rows.len(), 0);
    }

    #[test]
    fn uncorrelated_in_subquery_is_cached() {
        let mut d = Database::empty();
        d.execute(
            &parse_statement("CREATE TABLE t (id INT, name TEXT)").unwrap(),
            &[],
        )
        .unwrap();
        d.execute(&parse_statement("CREATE TABLE t2 (val INT)").unwrap(), &[])
            .unwrap();
        for i in 0..5 {
            let sql = alloc::format!("INSERT INTO t (id, name) VALUES ({i}, 'u{i}')");
            d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
        }
        d.execute(
            &parse_statement("INSERT INTO t2 (val) VALUES (1)").unwrap(),
            &[],
        )
        .unwrap();
        d.execute(
            &parse_statement("INSERT INTO t2 (val) VALUES (3)").unwrap(),
            &[],
        )
        .unwrap();
        // Uncorrelated IN subquery.
        let r = d
            .execute(
                &parse_statement("SELECT id FROM t WHERE id IN (SELECT val FROM t2)").unwrap(),
                &[],
            )
            .unwrap();
        assert_eq!(r.rows.len(), 2);
        assert_eq!(r.rows[0][0], Value::Int(1));
        assert_eq!(r.rows[1][0], Value::Int(3));
    }

    #[test]
    fn mixed_correlated_uncorrelated_subqueries() {
        let mut d = Database::empty();
        d.execute(
            &parse_statement("CREATE TABLE t (id INT, name TEXT)").unwrap(),
            &[],
        )
        .unwrap();
        d.execute(&parse_statement("CREATE TABLE t2 (val INT)").unwrap(), &[])
            .unwrap();
        for i in 0..5 {
            let sql = alloc::format!("INSERT INTO t (id, name) VALUES ({i}, 'u{i}')");
            d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
        }
        d.execute(
            &parse_statement("INSERT INTO t2 (val) VALUES (10)").unwrap(),
            &[],
        )
        .unwrap();
        // Both correlated AND uncorrelated subqueries in one WHERE.
        // uncorrelated: EXISTS (t2 WHERE val > 5) → always true
        // correlated: t.id < 3 → filters to id 0,1,2
        let r = d
            .execute(
                &parse_statement(
                    "SELECT id FROM t WHERE EXISTS \
                     (SELECT val FROM t2 WHERE val > 5) \
                     AND id < 3",
                )
                .unwrap(),
                &[],
            )
            .unwrap();
        assert_eq!(r.rows.len(), 3);
        assert_eq!(r.rows[0][0], Value::Int(0));
        assert_eq!(r.rows[1][0], Value::Int(1));
        assert_eq!(r.rows[2][0], Value::Int(2));
    }

    #[test]
    fn uncorrelated_scalar_subquery_is_cached() {
        let mut d = Database::empty();
        d.execute(
            &parse_statement("CREATE TABLE t (id INT, name TEXT)").unwrap(),
            &[],
        )
        .unwrap();
        d.execute(&parse_statement("CREATE TABLE t2 (val INT)").unwrap(), &[])
            .unwrap();
        for i in 0..5 {
            let sql = alloc::format!("INSERT INTO t (id, name) VALUES ({i}, 'u{i}')");
            d.execute(&parse_statement(&sql).unwrap(), &[]).unwrap();
        }
        d.execute(
            &parse_statement("INSERT INTO t2 (val) VALUES (3)").unwrap(),
            &[],
        )
        .unwrap();
        // Uncorrelated scalar subquery: id > (SELECT val FROM t2) → id > 3 → id=4 only.
        let r = d
            .execute(
                &parse_statement("SELECT id FROM t WHERE id > (SELECT val FROM t2)").unwrap(),
                &[],
            )
            .unwrap();
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0][0], Value::Int(4));
    }
}
