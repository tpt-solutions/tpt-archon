//! Column types, the table [`Schema`], and [`DbError`] — the small,
//! dependency-light types shared across every other `database` submodule.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use crate::executor;
use crate::parser;

/// A column's logical type.  Re-exported from `crate::parser` so the parser
/// and storage layers share a single `ColumnType` definition (no duplicated
/// enums with manual match-arm bridging between them).
pub use parser::ColumnType;

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

/// Errors from executing a statement against a [`Database`](super::Database).
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
    /// Column count mismatch in a set operation (UNION / INTERSECT / EXCEPT).
    ColumnCountMismatch,
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
            executor::ExecError::Unsupported(msg) => DbError::Unsupported(msg),
        }
    }
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DbError::UnknownColumn(c) => write!(f, "unknown column: {}", c),
            DbError::TypeMismatch => write!(f, "type mismatch"),
            DbError::ColumnTypeMismatch(c) => write!(f, "column type mismatch: {}", c),
            DbError::ArityMismatch => write!(f, "arity mismatch"),
            DbError::NotAVectorColumn(c) => write!(f, "not a vector column: {}", c),
            DbError::MissingParam => write!(f, "missing parameter"),
            DbError::RowNotFound(id) => write!(f, "row not found: {}", id),
            DbError::CorruptRow(id) => write!(f, "corrupt row: {}", id),
            DbError::UnknownTable(t) => write!(f, "unknown table: {}", t),
            DbError::TransactionError(msg) => write!(f, "transaction error: {}", msg),
            DbError::TableAlreadyExists(t) => write!(f, "table already exists: {}", t),
            DbError::ViewAlreadyExists(v) => write!(f, "view already exists: {}", v),
            DbError::UnknownView(v) => write!(f, "unknown view: {}", v),
            DbError::RecursiveView(v) => write!(f, "recursive view: {}", v),
            DbError::Unsupported(msg) => write!(f, "unsupported: {}", msg),
            DbError::ColumnCountMismatch => write!(f, "column count mismatch"),
            DbError::SubqueryCardinality(msg) => write!(f, "subquery cardinality: {}", msg),
            DbError::Exec(e) => write!(f, "execution error: {:?}", e),
        }
    }
}

impl core::error::Error for DbError {}
