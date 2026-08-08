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

/// A foreign-key constraint: this table's `columns` must reference a unique (or
/// primary) key `ref_columns` in `ref_table`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKey {
    /// The constrained columns in the owning table.
    pub columns: Vec<String>,
    /// The referenced table.
    pub ref_table: String,
    /// The referenced columns in `ref_table`.
    pub ref_columns: Vec<String>,
}

/// A table schema: ordered column names and their types, plus any declared
/// integrity constraints.
#[derive(Debug, Clone, Default)]
pub struct Schema {
    /// Column names in order.
    pub columns: Vec<String>,
    /// Column types, positionally aligned with `columns`.
    pub types: Vec<ColumnType>,
    /// Columns declared `NOT NULL` (or that are part of a primary key).
    pub not_null: Vec<String>,
    /// The primary-key columns, if a primary key is declared.
    pub primary_key: Option<Vec<String>>,
    /// Composite `UNIQUE` column groups (excluding the primary key).
    pub unique: Vec<Vec<String>>,
    /// Foreign-key constraints.
    pub foreign_keys: Vec<ForeignKey>,
    /// `CHECK` predicate expressions rows must satisfy.
    pub checks: Vec<crate::parser::Expr>,
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
    /// A `NOT NULL` constraint was violated on insert/update.
    NotNullViolation(String),
    /// A `UNIQUE` or `PRIMARY KEY` constraint was violated (duplicate key).
    UniqueViolation(String),
    /// A `CHECK` constraint evaluated to false (or unknown) on insert/update.
    CheckViolation(String),
    /// A `FOREIGN KEY` constraint was violated (referenced key missing).
    ForeignKeyViolation(String),
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
            DbError::NotNullViolation(c) => write!(f, "NOT NULL constraint violated: {}", c),
            DbError::UniqueViolation(c) => write!(f, "UNIQUE constraint violated: {}", c),
            DbError::CheckViolation(c) => write!(f, "CHECK constraint violated: {}", c),
            DbError::ForeignKeyViolation(c) => write!(f, "FOREIGN KEY constraint violated: {}", c),
            DbError::Exec(e) => write!(f, "execution error: {:?}", e),
        }
    }
}

impl core::error::Error for DbError {}
